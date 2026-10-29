//! A server that speaks raw HTTP/1.1 on one connection, for answers the mock
//! server cannot produce (no `Content-Length`, a truncated body).

use std::io::Read as _;
use std::net::{TcpListener, TcpStream};
use std::thread;

/// Serves the next connection with `respond`, once it has read the whole
/// request; returns the server's base URL.
pub fn serve_once(respond: impl FnOnce(TcpStream) + Send + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).unwrap() == 0 {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .map_or(0, |value| value.trim().parse().unwrap());
        let mut body = vec![0_u8; length];
        stream.read_exact(&mut body).unwrap();
        respond(stream);
    });
    uri
}
