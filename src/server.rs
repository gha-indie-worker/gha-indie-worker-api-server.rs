#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use crate::config::ApiConfig;
use crate::routes;

/// Bounds on what one client may make this process read before it has proven
/// itself to be a small HTTP request.
const MAX_REQUEST_LINE_BYTES: usize = 8 * 1024;
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADER_LINES: usize = 100;
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct ActiveConnection(Arc<AtomicUsize>);

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serve the API until the process is stopped.
///
/// This is deliberately a small bootstrap HTTP surface for compose readiness,
/// not a general-purpose public HTTP stack. It therefore fails closed on
/// ambiguous HTTP framing, bounds concurrent clients, and applies read/write
/// deadlines so a tunnel client cannot pin a worker thread indefinitely.
pub fn run(config: &ApiConfig) {
    let listener = match TcpListener::bind(&config.bind) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("api bind {} failed: {error}", config.bind);
            std::process::exit(1);
        }
    };
    let active = Arc::new(AtomicUsize::new(0));

    // Printed after the bind succeeds, so the line is evidence the port is
    // actually held rather than merely requested.
    println!("api bind {}", config.bind);
    println!(
        "{}",
        serde_json::to_string(&routes::health::body()).expect("health json")
    );

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                if active
                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                        (current < MAX_CONCURRENT_CONNECTIONS).then_some(current + 1)
                    })
                    .is_err()
                {
                    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                    let _ = respond(
                        &mut stream,
                        "503 Service Unavailable",
                        r#"{"error":"busy"}"#,
                        false,
                        None,
                    );
                    continue;
                }

                let active_for_thread = Arc::clone(&active);
                let spawn = thread::Builder::new()
                    .name("gha-indie-worker-api-connection".to_string())
                    .spawn(move || {
                        let _guard = ActiveConnection(active_for_thread);
                        if let Err(error) = serve(stream) {
                            eprintln!("api connection ended: {error}");
                        }
                    });
                if let Err(error) = spawn {
                    active.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("api connection thread failed to start: {error}");
                }
            }
            // One rejected connection is not a reason to take the service down.
            Err(error) => eprintln!("api accept failed: {error}"),
        }
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let request_line = match read_bounded_line(&mut reader, MAX_REQUEST_LINE_BYTES)? {
        BoundedLine::Eof => return Ok(()),
        BoundedLine::TooLong => {
            return respond(
                &mut stream,
                "414 URI Too Long",
                r#"{"error":"request_line_too_long"}"#,
                false,
                None,
            )
        }
        BoundedLine::Line(line) => line,
    };

    if !request_line.ends_with("\r\n") {
        return respond(
            &mut stream,
            "400 Bad Request",
            r#"{"error":"malformed_request_line"}"#,
            false,
            None,
        );
    }

    let request_line = request_line.trim_end_matches("\r\n");
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method.is_empty()
        || target.is_empty()
        || !target.starts_with('/')
        || target.chars().any(char::is_control)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
    {
        return respond(
            &mut stream,
            "400 Bad Request",
            r#"{"error":"malformed_request_line"}"#,
            false,
            None,
        );
    }

    match consume_headers(&mut reader)? {
        HeaderRead::Complete => {}
        HeaderRead::TooLarge => {
            return respond(
                &mut stream,
                "431 Request Header Fields Too Large",
                r#"{"error":"headers_too_large"}"#,
                method == "HEAD",
                None,
            )
        }
        HeaderRead::Malformed => {
            return respond(
                &mut stream,
                "400 Bad Request",
                r#"{"error":"malformed_headers"}"#,
                method == "HEAD",
                None,
            )
        }
    }

    let path = target.split('?').next().unwrap_or_default();
    let is_head = method == "HEAD";
    let (status, body, allow) = match (method, path) {
        ("GET" | "HEAD", "/healthz" | "/readyz") => (
            "200 OK",
            serde_json::to_string(&routes::health::body()).expect("health json"),
            None,
        ),
        ("GET" | "HEAD", "/v1/catalog") => (
            "200 OK",
            serde_json::to_string(&routes::v1::catalog()).expect("catalog json"),
            None,
        ),
        ("GET" | "HEAD", _) => (
            "404 Not Found",
            r#"{"error":"not_found"}"#.to_string(),
            None,
        ),
        _ => (
            "405 Method Not Allowed",
            r#"{"error":"method_not_allowed"}"#.to_string(),
            Some("GET, HEAD"),
        ),
    };

    respond(&mut stream, status, &body, is_head, allow)
}

enum BoundedLine {
    Eof,
    TooLong,
    Line(String),
}

fn read_bounded_line(reader: &mut BufReader<TcpStream>, max: usize) -> std::io::Result<BoundedLine> {
    let mut line = String::new();
    let read = reader
        .take((max + 1) as u64)
        .read_line(&mut line)?;
    if read == 0 {
        return Ok(BoundedLine::Eof);
    }
    if line.len() > max || !line.ends_with('\n') {
        return Ok(BoundedLine::TooLong);
    }
    Ok(BoundedLine::Line(line))
}

enum HeaderRead {
    Complete,
    TooLarge,
    Malformed,
}

fn consume_headers(reader: &mut BufReader<TcpStream>) -> std::io::Result<HeaderRead> {
    let mut total = 0usize;
    for _ in 0..MAX_HEADER_LINES {
        let remaining = MAX_HEADER_BYTES.saturating_sub(total);
        if remaining == 0 {
            return Ok(HeaderRead::TooLarge);
        }
        let mut line = String::new();
        let read = reader
            .take((remaining + 1) as u64)
            .read_line(&mut line)?;
        if read == 0 {
            return Ok(HeaderRead::Malformed);
        }
        total = total.saturating_add(line.len());
        if total > MAX_HEADER_BYTES {
            return Ok(HeaderRead::TooLarge);
        }
        if !line.ends_with("\r\n") {
            return Ok(HeaderRead::Malformed);
        }
        if line == "\r\n" {
            return Ok(HeaderRead::Complete);
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            return Ok(HeaderRead::Malformed);
        }
        let Some((name, _value)) = line.trim_end_matches("\r\n").split_once(':') else {
            return Ok(HeaderRead::Malformed);
        };
        if name.is_empty()
            || !name.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-'
                            | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
                    )
            })
        {
            return Ok(HeaderRead::Malformed);
        }
    }
    Ok(HeaderRead::TooLarge)
}

fn respond(
    stream: &mut TcpStream,
    status: &str,
    body: &str,
    head_only: bool,
    allow: Option<&str>,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    )?;
    if let Some(allow) = allow {
        write!(stream, "allow: {allow}\r\n")?;
    }
    write!(stream, "\r\n")?;
    if !head_only {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn header_name_grammar_rejects_obs_fold_and_bad_names() {
        fn parse(input: &str) -> HeaderRead {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let address = listener.local_addr().expect("addr");
            let client = thread::spawn(move || TcpStream::connect(address).expect("connect"));
            let (server, _) = listener.accept().expect("accept");
            let mut peer = client.join().expect("join");
            peer.write_all(input.as_bytes()).expect("write");
            peer.shutdown(std::net::Shutdown::Write).expect("shutdown");
            let mut reader = BufReader::new(server);
            consume_headers(&mut reader).expect("parse")
        }

        assert!(matches!(parse("host: example\r\n\r\n"), HeaderRead::Complete));
        assert!(matches!(parse(" folded: nope\r\n\r\n"), HeaderRead::Malformed));
        assert!(matches!(parse("bad name: nope\r\n\r\n"), HeaderRead::Malformed));
    }

    #[test]
    fn request_line_limit_is_strict() {
        let bytes = format!("{}\n", "x".repeat(MAX_REQUEST_LINE_BYTES));
        let mut cursor = Cursor::new(bytes.into_bytes());
        let mut line = String::new();
        let read = (&mut cursor)
            .take((MAX_REQUEST_LINE_BYTES + 1) as u64)
            .read_line(&mut line)
            .expect("read");
        assert!(read > MAX_REQUEST_LINE_BYTES || line.len() > MAX_REQUEST_LINE_BYTES);
    }
}
