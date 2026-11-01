//! A local HTTP server that answers slowly, for cancellation tests.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

/// Bytes the stub claims each file has.
pub(crate) const STUB_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Serve every request on a fresh port: HEAD answers with the size and no
/// range support, GET streams 64 KiB every 50 ms. Returns the base URL.
pub(crate) fn slow_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).ok();
                let mut line = String::new();
                while reader.read_line(&mut line).map(|n| n > 2).unwrap_or(false) {
                    line.clear();
                }
                let mut stream = stream;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {STUB_FILE_BYTES}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(head.as_bytes()).is_err() || request_line.starts_with("HEAD") {
                    return;
                }
                let chunk = vec![0u8; 64 * 1024];
                while stream.write_all(&chunk).is_ok() {
                    thread::sleep(Duration::from_millis(50));
                }
            });
        }
    });
    url
}
