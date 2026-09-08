//! A minimal HTTP/1.1 origin on loopback, for tests that need a page whose
//! document commits but whose `load` event never fires.
//!
//! Three routes:
//!
//! - `/fast` — a complete document that loads normally.
//! - `/slow` — a complete document that references `/hang`, so the parser
//!   finishes and `DOMContentLoaded` fires while `load` stays pending.
//! - `/hang` — accepted and then never answered.
//!
//! Every connection is served by its own task holding a shutdown receiver, so
//! a parked `/hang` socket closes with the server rather than outliving the
//! test that opened it. Dropping [`TestServer`] stops the accept loop and
//! every connection task.

#![allow(dead_code)]

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

const SLOW_BODY: &str = "<!doctype html><html><head><title>slow</title></head>\
<body><h1>slow</h1><img src=\"/hang\" alt=\"\"></body></html>";

const FAST_BODY: &str = "<!doctype html><html><head><title>fast</title></head>\
<body><h1>fast</h1></body></html>";

const NOT_FOUND_BODY: &str = "<!doctype html><html><body>not found</body></html>";

/// Largest request head accepted before the connection is dropped.
const MAX_HEAD: usize = 16 * 1024;

pub struct TestServer {
    addr: SocketAddr,
    shutdown: broadcast::Sender<()>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("listener local addr");
        let (shutdown, mut accept_rx) = broadcast::channel(1);
        let conn_shutdown = shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_rx.recv() => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _peer)) = accepted else { break };
                        let rx = conn_shutdown.subscribe();
                        tokio::spawn(serve(stream, rx));
                    }
                }
            }
        });

        Self { addr, shutdown }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Errors here only mean nothing is listening any more.
        let _ = self.shutdown.send(());
    }
}

async fn serve(mut stream: TcpStream, mut shutdown: broadcast::Receiver<()>) {
    let Some(path) = read_request_path(&mut stream).await else {
        return;
    };

    match path.as_str() {
        "/fast" => respond(&mut stream, "200 OK", FAST_BODY).await,
        "/slow" => respond(&mut stream, "200 OK", SLOW_BODY).await,
        "/hang" => {
            // Answer nothing. The socket stays open until the server stops,
            // which is what keeps `load` from firing on `/slow`.
            let _ = shutdown.recv().await;
        }
        _ => respond(&mut stream, "404 Not Found", NOT_FOUND_BODY).await,
    }
}

/// Reads the request head and returns the request-target, or `None` when the
/// peer hung up or sent something unusable.
async fn read_request_path(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];

    loop {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if head.len() > MAX_HEAD {
            return None;
        }
    }

    let line_end = head.windows(2).position(|w| w == b"\r\n")?;
    let line = std::str::from_utf8(&head[..line_end]).ok()?;
    line.split_whitespace().nth(1).map(str::to_owned)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    // A peer that vanished mid-write is not a test failure.
    if stream.write_all(head.as_bytes()).await.is_err() {
        return;
    }
    if stream.write_all(body.as_bytes()).await.is_err() {
        return;
    }
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}
