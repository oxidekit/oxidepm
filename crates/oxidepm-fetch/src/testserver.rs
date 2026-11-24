//! A small HTTP server on the loopback interface for the tests of this crate. It speaks just
//! enough HTTP/1.1: one request per connection, `Connection: close`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// What the server does with a request.
pub enum Reply {
    /// A complete answer with a correct `Content-Length`.
    Ok(Vec<u8>),
    /// An answer with this status line and these extra headers and no body.
    Status(u16, Vec<(String, String)>),
    /// Headers announcing `announced` bytes, then silence (the body never comes).
    Announce(u64),
    /// No `Content-Length`: this many bytes of `x`, then the connection closes.
    Unbounded(usize),
}

pub struct TestServer {
    pub base: String,
    pub seen: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub fn requests(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

/// Start a server; `handler` gets the request path and the number of requests so far (from 0).
pub async fn serve<F>(handler: F) -> TestServer
where
    F: Fn(&str, usize) -> Reply + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (log, handler) = (log.clone(), handler.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&request).to_string();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_string();
                let index = {
                    let mut seen = log.lock().unwrap();
                    seen.push(path.clone());
                    seen.len() - 1
                };
                match handler(&path, index) {
                    Reply::Ok(body) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        let _ = stream.write_all(&body).await;
                    }
                    Reply::Status(code, headers) => {
                        let mut head = format!(
                            "HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n"
                        );
                        for (k, v) in headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        let _ = stream.write_all(head.as_bytes()).await;
                    }
                    Reply::Announce(n) => {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {n}\r\nConnection: close\r\n\r\n"
                        );
                        let _ = stream.write_all(head.as_bytes()).await;
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    }
                    Reply::Unbounded(n) => {
                        let _ = stream
                            .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                            .await;
                        let chunk = vec![b'x'; 16 * 1024];
                        let mut left = n;
                        while left > 0 {
                            let take = left.min(chunk.len());
                            if stream.write_all(&chunk[..take]).await.is_err() {
                                return;
                            }
                            left -= take;
                        }
                    }
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    TestServer { base, seen }
}
