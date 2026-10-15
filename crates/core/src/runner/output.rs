//! 标准输出与标准错误的独立读取、字符集解码和逐行合并。

use std::io::Read;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

#[cfg(windows)]
use super::job;
use super::{Charset, RunningProcess};

impl Charset {
    fn encoding(self) -> &'static encoding_rs::Encoding {
        match self {
            Charset::Default => {
                #[cfg(windows)]
                if job::system_charset() == Charset::Gbk {
                    return encoding_rs::GBK;
                }
                encoding_rs::UTF_8
            }
            Charset::Utf8 => encoding_rs::UTF_8,
            Charset::Gbk => encoding_rs::GBK,
        }
    }
}

/// 启动双读取线程；调用方消费行通道后等待线程收尾。
pub(super) fn capture(
    running: &RunningProcess,
    charset: Charset,
) -> (Receiver<String>, [JoinHandle<()>; 2]) {
    // 双读取线程:stdout/stderr → 行通道(行内完整,跨路顺序不保证)
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    let encoding = charset.encoding();
    let mut stdout = running.child.lock().unwrap().stdout.take();
    let mut stderr = running.child.lock().unwrap().stderr.take();
    let tx1 = line_tx.clone();
    let t1 = std::thread::spawn(move || {
        if let Some(stream) = stdout.as_mut() {
            read_lines(stream, encoding, tx1);
        }
    });
    let tx2 = line_tx.clone();
    let t2 = std::thread::spawn(move || {
        if let Some(stream) = stderr.as_mut() {
            read_lines(stream, encoding, tx2);
        }
    });
    drop(line_tx);

    (line_rx, [t1, t2])
}

/// 从字节流按行解码并发送。
fn read_lines(
    stream: &mut impl Read,
    encoding: &'static encoding_rs::Encoding,
    tx: Sender<String>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => {
                tracing::error!("读取任务输出时出错: {e}");
                break;
            }
        }
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let mut line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            line_bytes.pop();
            if line_bytes.last() == Some(&b'\r') {
                line_bytes.pop();
            }
            let (decoded, _, _) = encoding.decode(&line_bytes);
            let _ = tx.send(decoded.into_owned());
        }
    }
    if !buf.is_empty() {
        let (decoded, _, _) = encoding.decode(&buf);
        let _ = tx.send(decoded.into_owned());
    }
}
