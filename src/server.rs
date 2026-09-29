#![forbid(unsafe_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::routes;

const MAX_REQUEST_HEAD: usize = 16 * 1024;

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

fn response_for(method: &str, path: &str) -> Result<(u16, &'static str, String), ApiError> {
    if method != "GET" {
        return Ok((
            405,
            "application/json; charset=utf-8",
            r#"{"error":"method_not_allowed"}"#.to_owned(),
        ));
    }

    match path {
        "/healthz" | "/readyz" => Ok((
            200,
            "application/json; charset=utf-8",
            serde_json::to_string(&routes::health::body()).map_err(|_| ApiError::Serialization)?,
        )),
        "/v1" | "/v1/catalog" => Ok((
            200,
            "application/json; charset=utf-8",
            serde_json::to_string(&routes::v1::catalog()).map_err(|_| ApiError::Serialization)?,
        )),
        _ => Ok((
            404,
            "application/json; charset=utf-8",
            r#"{"error":"not_found"}"#.to_owned(),
        )),
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    }
}

fn handle_http(mut stream: TcpStream) -> Result<(), ApiError> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;

    let mut buffer = [0_u8; MAX_REQUEST_HEAD];
    let read = stream.read(&mut buffer)?;
    if read == 0 {
        return Ok(());
    }
    let request = std::str::from_utf8(&buffer[..read]).unwrap_or_default();
    let mut parts = request.lines().next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or(target);

    let (status, content_type, body) = response_for(method, path)?;
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;
    Ok(())
}

pub fn run(config: &ApiConfig) -> Result<(), ApiError> {
    let plan = startup_plan(config)?;
    let http = plan
        .listeners
        .iter()
        .find(|listener| listener.transport == TransportKind::Http)
        .ok_or(ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_BIND"))?;

    for listener in &plan.listeners {
        match listener.transport {
            TransportKind::Http | TransportKind::StatefulTcp => {
                eprintln!("api {:?} endpoint {}", listener.transport, listener.endpoint);
            }
            TransportKind::DurableNats => eprintln!("api DurableNats configured"),
        }
    }

    let listener = TcpListener::bind(&http.endpoint)?;
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if let Err(error) = handle_http(stream) {
                    eprintln!("api http connection failed: {error}");
                }
            }
            Err(error) => eprintln!("api http accept failed: {error}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{response_for, startup_plan, ListenerBinding, StartupPlan, TransportKind};
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
    fn health_and_readiness_are_real_http_routes() {
        for path in ["/healthz", "/readyz"] {
            let (status, content_type, body) = response_for("GET", path).expect("response");
            assert_eq!(status, 200);
            assert_eq!(content_type, "application/json; charset=utf-8");
            assert!(body.contains("gha-indie-worker-api-server"));
        }
    }

    #[test]
    fn unknown_routes_fail_closed() {
        let (status, _, _) = response_for("GET", "/not-a-route").expect("response");
        assert_eq!(status, 404);
    }
}
