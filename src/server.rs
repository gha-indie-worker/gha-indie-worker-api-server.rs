#![forbid(unsafe_code)]

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::routes;

const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportKind {
    Http,
    StatefulTcp,
    DurableNats,
}

#[derive(Clone, Eq, PartialEq)]
pub struct ListenerBinding {
    pub transport: TransportKind,
    pub endpoint: String,
}

impl std::fmt::Debug for ListenerBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ListenerBinding")
            .field("transport", &self.transport)
            .field(
                "endpoint",
                &match self.transport {
                    TransportKind::DurableNats => "[redacted]",
                    TransportKind::Http | TransportKind::StatefulTcp => &self.endpoint,
                },
            )
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupPlan {
    pub listeners: Vec<ListenerBinding>,
}

pub fn startup_plan(config: &ApiConfig) -> Result<StartupPlan, ApiError> {
    let required = [(
        TransportKind::Http,
        "GHA_INDIE_WORKER_API_BIND",
        &config.bind,
    )];
    let optional = [
        config.tcp_bind.as_ref().map(|endpoint| {
            (
                TransportKind::StatefulTcp,
                "GHA_INDIE_WORKER_API_TCP_BIND",
                endpoint,
            )
        }),
        config.nats_url.as_ref().map(|endpoint| {
            (
                TransportKind::DurableNats,
                "GHA_INDIE_WORKER_NATS_URL",
                endpoint,
            )
        }),
    ];

    let listeners = required
        .into_iter()
        .chain(optional.into_iter().flatten())
        .map(|(transport, field, endpoint)| listener_binding(transport, field, endpoint))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(StartupPlan { listeners })
}

fn listener_binding(
    transport: TransportKind,
    field: &'static str,
    endpoint: &str,
) -> Result<ListenerBinding, ApiError> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(ApiError::InvalidConfiguration(field));
    }

    Ok(ListenerBinding {
        transport,
        endpoint: endpoint.to_owned(),
    })
}

fn http_binding(plan: &StartupPlan) -> Result<&ListenerBinding, ApiError> {
    plan.listeners
        .iter()
        .find(|listener| listener.transport == TransportKind::Http)
        .ok_or(ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_BIND"))
}

fn loopback_socket(endpoint: &str) -> Result<SocketAddr, ApiError> {
    let address = endpoint
        .parse::<SocketAddr>()
        .map_err(|_| ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_BIND"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_BIND"));
    }
    Ok(address)
}

pub fn run(config: &ApiConfig) -> Result<(), ApiError> {
    let plan = startup_plan(config)?;
    let binding = http_binding(&plan)?;
    let address = loopback_socket(&binding.endpoint)?;
    let listener = TcpListener::bind(address)?;
    eprintln!("gha-indie-worker-api-server listening on http://{address}");

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream) {
                    eprintln!("gha-indie-worker-api-server connection error: {error}");
                }
            }
            Err(error) => eprintln!("gha-indie-worker-api-server accept error: {error}"),
        }
    }

    Ok(())
}

fn handle_connection(stream: &mut TcpStream) -> Result<(), ApiError> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;

    let mut bytes = [0_u8; MAX_REQUEST_HEAD_BYTES];
    let count = stream.read(&mut bytes)?;
    if count == 0 {
        return Ok(());
    }

    let head = std::str::from_utf8(&bytes[..count]).unwrap_or_default();
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or(target);

    let response = route(method, path)?;
    stream.write_all(&response)?;
    stream.flush()?;
    Ok(())
}

fn route(method: &str, path: &str) -> Result<Vec<u8>, ApiError> {
    match (method, path) {
        ("GET", "/healthz") | ("GET", "/readyz") => {
            json_response(200, &routes::health::body())
        }
        ("GET", "/v1/catalog") => json_response(200, &routes::v1::catalog()),
        ("GET", "/v1/status") => json_response(
            200,
            &serde_json::json!({
                "ok": true,
                "service": "gha-indie-worker-api-server",
                "runtime": "standalone-http",
            }),
        ),
        ("HEAD", "/healthz") | ("HEAD", "/readyz") => empty_response(200),
        ("GET" | "HEAD", _) => json_response(404, &serde_json::json!({"error": "not_found"})),
        _ => json_response(405, &serde_json::json!({"error": "method_not_allowed"})),
    }
}

fn json_response<T: serde::Serialize>(status: u16, body: &T) -> Result<Vec<u8>, ApiError> {
    let body = serde_json::to_vec(body).map_err(|_| ApiError::Serialization)?;
    Ok(http_response(
        status,
        "application/json; charset=utf-8",
        &body,
    ))
}

fn empty_response(status: u16) -> Result<Vec<u8>, ApiError> {
    Ok(http_response(status, "application/json; charset=utf-8", &[]))
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

#[cfg(test)]
mod tests {
    use super::{loopback_socket, route, startup_plan, ListenerBinding, StartupPlan, TransportKind};
    use crate::{config::ApiConfig, error::ApiError};

    #[test]
    fn startup_plan_is_a_pure_ordered_transformation() {
        let config = ApiConfig {
            bind: " 127.0.0.1:8080 ".into(),
            tcp_bind: Some("127.0.0.1:8082".into()),
            nats_url: Some("nats://127.0.0.1:4222".into()),
        };

        let plan = startup_plan(&config).expect("valid startup plan");

        assert_eq!(
            plan,
            StartupPlan {
                listeners: vec![
                    ListenerBinding {
                        transport: TransportKind::Http,
                        endpoint: "127.0.0.1:8080".into(),
                    },
                    ListenerBinding {
                        transport: TransportKind::StatefulTcp,
                        endpoint: "127.0.0.1:8082".into(),
                    },
                    ListenerBinding {
                        transport: TransportKind::DurableNats,
                        endpoint: "nats://127.0.0.1:4222".into(),
                    },
                ],
            }
        );
        assert_eq!(config.bind, " 127.0.0.1:8080 ");
    }

    #[test]
    fn startup_plan_rejects_invalid_optional_bindings_without_partial_output() {
        let error = startup_plan(&ApiConfig {
            bind: "127.0.0.1:8080".into(),
            tcp_bind: Some("   ".into()),
            nats_url: Some("nats://127.0.0.1:4222".into()),
        })
        .expect_err("blank TCP binding must fail closed");

        assert!(matches!(
            error,
            ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_TCP_BIND")
        ));
    }

    #[test]
    fn nats_credentials_are_redacted_from_the_printable_plan() {
        let plan = startup_plan(&ApiConfig {
            bind: "127.0.0.1:8080".into(),
            tcp_bind: None,
            nats_url: Some("nats://worker:credential@nats.internal:4222".into()),
        })
        .expect("valid NATS plan");

        let debug = format!("{plan:?}");
        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("credential"));
    }

    #[test]
    fn listener_must_be_literal_loopback() {
        assert!(loopback_socket("127.0.0.1:18090").is_ok());
        assert!(loopback_socket("[::1]:18090").is_ok());
        assert!(loopback_socket("0.0.0.0:18090").is_err());
        assert!(loopback_socket("127.0.0.1:0").is_err());
    }

    #[test]
    fn health_and_status_are_real_http_responses() {
        let health = String::from_utf8(route("GET", "/healthz").expect("health response"))
            .expect("utf8 response");
        assert!(health.starts_with("HTTP/1.1 200 OK"));
        assert!(health.contains("gha-indie-worker-api-server"));

        let status = String::from_utf8(route("GET", "/v1/status").expect("status response"))
            .expect("utf8 response");
        assert!(status.contains("standalone-http"));
    }
}
