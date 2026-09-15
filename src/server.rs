#![forbid(unsafe_code)]

use crate::config::ApiConfig;
use crate::routes;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(config: &ApiConfig) {
    let listener = TcpListener::bind(&config.bind)
        .unwrap_or_else(|error| panic!("failed to bind API listener at {}: {error}", config.bind));
    eprintln!("gha-indie-worker API listening on {}", config.bind);

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream) {
                    eprintln!("API connection failed: {error}");
                }
            }
            Err(error) => eprintln!("API accept failed: {error}"),
        }
    }
}

fn handle_connection(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut request = [0_u8; MAX_REQUEST_BYTES];
    let size = stream.read(&mut request)?;
    if size == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&request[..size]);
    let request_line = request.lines().next().unwrap_or_default();
    let (status, content_type, body) = response_for(request_line);
    write_response(stream, status, content_type, &body)
}

fn response_for(request_line: &str) -> (&'static str, &'static str, Vec<u8>) {
    match request_line {
        line if line.starts_with("GET /healthz ") || line.starts_with("GET /health ") => (
            "200 OK",
            "application/json",
            serde_json::to_vec(&routes::health::body()).expect("health response must serialize"),
        ),
        line if line.starts_with("GET / ") => (
            "200 OK",
            "application/json",
            br#"{"service":"gha-indie-worker-api-server","ok":true}"#.to_vec(),
        ),
        line if line.starts_with("GET ") => (
            "404 Not Found",
            "application/json",
            br#"{"error":"not_found"}"#.to_vec(),
        ),
        _ => (
            "405 Method Not Allowed",
            "application/json",
            br#"{"error":"method_not_allowed"}"#.to_vec(),
        ),
    }
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_endpoint_is_json_and_successful() {
        let (status, content_type, body) = response_for("GET /healthz HTTP/1.1");
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "application/json");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["service"], "gha-indie-worker-api-server");
    }

    #[test]
    fn unknown_get_is_not_found() {
        let (status, _, _) = response_for("GET /missing HTTP/1.1");
        assert_eq!(status, "404 Not Found");
    }
}
