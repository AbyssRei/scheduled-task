//! std mpsc 可靠传递状态与唤醒，日志正文使用有界缓冲；发送不等待界面。

use std::collections::VecDeque;
use std::sync::mpsc::{self, RecvError, RecvTimeoutError, SendError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::DaemonEvent;

const LOG_BYTES: usize = 1024 * 1024;
const LOG_LINES: usize = 1024;
type Stamped = (u64, DaemonEvent);

enum Message {
    Control(Stamped),
    LogsReady,
}

struct Queue {
    logs: VecDeque<Stamped>,
    bytes: usize,
    sequence: u64,
    dropped: u64,
    receiver: bool,
    wake_pending: bool,
}

struct Shared {
    queue: Mutex<Queue>,
}

#[derive(Clone)]
pub struct EventSender(Arc<Shared>, mpsc::Sender<Message>);

pub struct EventReceiver {
    shared: Arc<Shared>,
    control: mpsc::Receiver<Message>,
    pending: Mutex<Option<Stamped>>,
}

pub fn channel() -> (EventSender, EventReceiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue {
            logs: VecDeque::new(),
            bytes: 0,
            sequence: 0,
            dropped: 0,
            receiver: true,
            wake_pending: false,
        }),
    });
    let (tx, rx) = mpsc::channel();
    (
        EventSender(shared.clone(), tx),
        EventReceiver {
            shared,
            control: rx,
            pending: Mutex::new(None),
        },
    )
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        let mut queue = self.shared.queue.lock().unwrap();
        queue.receiver = false;
        queue.logs.clear();
        queue.bytes = 0;
    }
}

impl EventSender {
    pub fn send(&self, event: DaemonEvent) -> Result<(), SendError<DaemonEvent>> {
        let mut queue = self.0.queue.lock().unwrap();
        if !queue.receiver {
            return Err(SendError(event));
        }
        let sequence = queue.sequence;
        queue.sequence += 1;
        if let DaemonEvent::LogLine { line, .. } = &event {
            let size = line.len();
            while queue.bytes.saturating_add(size) > LOG_BYTES || queue.logs.len() >= LOG_LINES {
                let Some((_, DaemonEvent::LogLine { line, .. })) = queue.logs.pop_front() else {
                    break;
                };
                queue.bytes -= line.len();
                queue.dropped = queue.dropped.saturating_add(1);
            }
            if size > LOG_BYTES {
                queue.dropped = queue.dropped.saturating_add(1);
            } else {
                queue.bytes += size;
                queue.logs.push_back((sequence, event));
            }
            // 合并唤醒，慢界面不会令 std mpsc 堆积每一条日志的通知。
            if !queue.wake_pending {
                queue.wake_pending = true;
                let _ = self.1.send(Message::LogsReady);
            }
        } else if let Err(SendError(Message::Control((_, event)))) =
            self.1.send(Message::Control((sequence, event)))
        {
            return Err(SendError(event));
        }
        Ok(())
    }
}

impl Queue {
    fn pop(&mut self, pending: &mut Option<Stamped>) -> Option<DaemonEvent> {
        if self.dropped > 0 {
            return Some(DaemonEvent::LogsDropped {
                count: std::mem::take(&mut self.dropped),
            });
        }
        let take_log = match (self.logs.front(), pending.as_ref()) {
            (Some((log, _)), Some((control, _))) => log < control,
            (Some(_), None) => true,
            _ => false,
        };
        if take_log {
            let (_, event) = self.logs.pop_front()?;
            if let DaemonEvent::LogLine { line, .. } = &event {
                self.bytes -= line.len();
            }
            Some(event)
        } else {
            pending.take().map(|(_, event)| event)
        }
    }
}

impl EventReceiver {
    fn accept(&self, message: Message) {
        let mut queue = self.shared.queue.lock().unwrap();
        match message {
            Message::LogsReady => queue.wake_pending = false,
            Message::Control(event) => *self.pending.lock().unwrap() = Some(event),
        }
    }

    pub fn recv(&self) -> Result<DaemonEvent, RecvError> {
        loop {
            match self.try_recv() {
                Ok(event) => return Ok(event),
                Err(TryRecvError::Disconnected) => return Err(RecvError),
                Err(TryRecvError::Empty) => self.accept(self.control.recv()?),
            }
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<DaemonEvent, RecvTimeoutError> {
        let start = Instant::now();
        loop {
            match self.try_recv() {
                Ok(event) => return Ok(event),
                Err(TryRecvError::Disconnected) => return Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => self.accept(
                    self.control
                        .recv_timeout(timeout.saturating_sub(start.elapsed()))?,
                ),
            }
        }
    }

    pub fn try_recv(&self) -> Result<DaemonEvent, TryRecvError> {
        // 在序号锁内接收控制事件，保证与日志按原始发送顺序合并。
        let mut queue = self.shared.queue.lock().unwrap();
        let mut pending = self.pending.lock().unwrap();
        let mut disconnected = false;
        while pending.is_none() {
            match self.control.try_recv() {
                Ok(Message::LogsReady) => queue.wake_pending = false,
                Ok(Message::Control(event)) => *pending = Some(event),
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        queue.pop(&mut pending).ok_or(if disconnected {
            TryRecvError::Disconnected
        } else {
            TryRecvError::Empty
        })
    }

    pub fn try_iter(&self) -> impl Iterator<Item = DaemonEvent> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskId;
    use crate::state::TaskStatus;

    #[test]
    fn 慢接收端限制日志内存且保留状态和完成事件() {
        let (tx, rx) = channel();
        tx.send(DaemonEvent::Status {
            id: TaskId(1),
            status: TaskStatus::Running,
        })
        .unwrap();
        for _ in 0..20_000 {
            tx.send(DaemonEvent::LogLine {
                id: TaskId(1),
                line: "x".repeat(4096),
            })
            .unwrap();
        }
        tx.send(DaemonEvent::Finished {
            id: TaskId(1),
            returncode: 0,
            retry_triggered: false,
        })
        .unwrap();
        let queue = tx.0.queue.lock().unwrap();
        assert!(queue.bytes <= LOG_BYTES);
        assert!(queue.logs.len() <= LOG_LINES);
        drop(queue);
        drop(tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(matches!(events[0], DaemonEvent::LogsDropped { count } if count > 0));
        assert!(matches!(
            events[1],
            DaemonEvent::Status {
                status: TaskStatus::Running,
                ..
            }
        ));
        assert!(matches!(events.last(), Some(DaemonEvent::Finished { .. })));
        assert!(matches!(rx.recv(), Err(RecvError)));
    }

    #[test]
    fn 发送端关闭唤醒等待且接收端关闭立即拒绝发送() {
        let (tx, rx) = channel();
        assert!(matches!(
            rx.recv_timeout(Duration::ZERO),
            Err(RecvTimeoutError::Timeout)
        ));
        let waiter = std::thread::spawn(move || rx.recv());
        drop(tx);
        assert!(waiter.join().unwrap().is_err());
        let (tx, rx) = channel();
        drop(rx);
        assert!(tx.send(DaemonEvent::ConfigChanged).is_err());
    }

    #[test]
    fn 日志与状态按发送顺序合并且克隆发送端保持连接() {
        let (tx, rx) = channel();
        let clone = tx.clone();
        tx.send(DaemonEvent::Status {
            id: TaskId(1),
            status: TaskStatus::Running,
        })
        .unwrap();
        tx.send(DaemonEvent::LogLine {
            id: TaskId(1),
            line: "第一条".into(),
        })
        .unwrap();
        clone
            .send(DaemonEvent::Status {
                id: TaskId(1),
                status: TaskStatus::Stopping,
            })
            .unwrap();
        clone
            .send(DaemonEvent::LogLine {
                id: TaskId(1),
                line: "第二条".into(),
            })
            .unwrap();
        drop(tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(matches!(
            events[0],
            DaemonEvent::Status {
                status: TaskStatus::Running,
                ..
            }
        ));
        assert!(matches!(&events[1], DaemonEvent::LogLine { line, .. } if line == "第一条"));
        assert!(matches!(
            events[2],
            DaemonEvent::Status {
                status: TaskStatus::Stopping,
                ..
            }
        ));
        assert!(matches!(&events[3], DaemonEvent::LogLine { line, .. } if line == "第二条"));
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Empty);
        drop(clone);
        assert_eq!(rx.try_recv().unwrap_err(), TryRecvError::Disconnected);
    }
}
