//! Simple test server for OxidePM testing

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    println!("Test server starting...");

    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind");
    let addr = listener.local_addr().expect("Failed to get address");
    println!("Listening on {}", addr);

    // Set a timeout so the test can terminate
    listener.set_nonblocking(true).ok();

    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0; 512];
                if let Ok(n) = stream.read(&mut buffer) {
                    if n > 0 {
                        let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
                        stream.write_all(response.as_bytes()).ok();
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }
}
