#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::config::ApiConfig;
use crate::routes;

/// Bounds on what one client may make this process read before it has proven
/// itself to be an HTTP request at all.
const MAX_REQUEST_LINE_BYTES: u64 = 8 * 1024;
const MAX_HEADER_BYTES: u64 = 32 * 1024;
const MAX_HEADER_LINES: usize = 100;

/// Serve the API until the process is stopped.
///
/// The service is supervised by `ores-compose`, which starts it, polls
/// `/healthz`, and treats an exit before the first successful probe as a
/// failed start — so this function must not return while the listener is
/// healthy.
pub fn run(config: &ApiConfig) {
    let listener = match TcpListener::bind(&config.bind) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("api bind {} failed: {error}", config.bind);
            std::process::exit(1);
        }
    };

    // Printed after the bind succeeds, so the line is evidence the port is
    // actually held rather than merely requested.
    println!("api bind {}", config.bind);
    println!(
        "{}",
        serde_json::to_string(&routes::health::body()).expect("health json")
    );

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(error) = serve(stream) {
                        eprintln!("api connection ended: {error}");
                    }
                });
            }
            // One rejected connection is not a reason to take the service down.
            Err(error) => eprintln!("api accept failed: {error}"),
        }
    }
}

fn serve(stream: TcpStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if (&mut reader)
        .take(MAX_REQUEST_LINE_BYTES)
        .read_line(&mut request_line)?
        == 0
    {
        return Ok(());
    }

    // Headers are read and discarded: no route depends on them yet, but they
    // must be consumed so the client sees a complete exchange.
    let mut header_reader = (&mut reader).take(MAX_HEADER_BYTES);
    for _ in 0..MAX_HEADER_LINES {
        let mut header = String::new();
        if header_reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default();

    let (status, body) = match (method, path) {
        ("GET" | "HEAD", "/healthz" | "/readyz") => (
            "200 OK",
            serde_json::to_string(&routes::health::body()).expect("health json"),
        ),
        ("GET" | "HEAD", "/v1/catalog") => (
            "200 OK",
            serde_json::to_string(&routes::v1::catalog()).expect("catalog json"),
        ),
        ("GET" | "HEAD", _) => (
            "404 Not Found",
            r#"{"error":"not_found"}"#.to_string(),
        ),
        _ => (
            "405 Method Not Allowed",
            r#"{"error":"method_not_allowed"}"#.to_string(),
        ),
    };

    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    if method != "HEAD" {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}
