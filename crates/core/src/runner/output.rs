//! 标准输出与标准错误的独立读取、字符集解码和逐行合并。

use std::io::Read;
use std::sync::mpsc::{Receiver, SyncSender};
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
    let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<String>(256);
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
    tx: SyncSender<String>,
) {
    const MAX_LINE_BYTES: usize = 64 * 1024;
    let mut buf = String::new();
    let mut decoder = encoding.new_decoder_with_bom_removal();
    let mut chunk = [0u8; 4096];
    loop {
        let count = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e) => {
                tracing::error!("读取任务输出时出错: {e}");
                break;
            }
        };
        // 增量解码保留跨读取块的多字节字符；超长物理行拆成有界片段。
        let mut decoded = String::with_capacity(count * 4 + 16);
        let _ = decoder.decode_to_string(&chunk[..count], &mut decoded, count == 0);
        buf.push_str(&decoded);
        loop {
            let newline = buf.find('\n');
            let (end, consumed) = if let Some(end) = newline.filter(|end| *end <= MAX_LINE_BYTES) {
                (end, end + 1)
            } else if buf.len() > MAX_LINE_BYTES {
                let mut end = MAX_LINE_BYTES;
                while !buf.is_char_boundary(end) {
                    end -= 1;
                }
                (end, end)
            } else {
                break;
            };
            let mut line = buf[..end].to_owned();
            if consumed != end && line.ends_with('\r') {
                line.pop();
            }
            buf.drain(..consumed);
            if tx.send(line).is_err() {
                return;
            }
        }
        if count == 0 {
            break;
        }
    }
    if !buf.is_empty() {
        let _ = tx.send(buf);
    }
}
