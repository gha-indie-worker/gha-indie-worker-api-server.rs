#![forbid(unsafe_code)]
#![allow(clippy::needless_return)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::routes;

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
        return formatter
            .debug_struct("ListenerBinding")
            .field("transport", &self.transport)
            .field(
                "endpoint",
                &match self.transport {
                    TransportKind::DurableNats => "[redacted]",
                    TransportKind::Http | TransportKind::StatefulTcp => &self.endpoint,
                },
            )
            .finish();
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
            return (
                TransportKind::StatefulTcp,
                "GHA_INDIE_WORKER_API_TCP_BIND",
                endpoint,
            );
        }),
        config.nats_url.as_ref().map(|endpoint| {
            return (
                TransportKind::DurableNats,
                "GHA_INDIE_WORKER_NATS_URL",
                endpoint,
            );
        }),
    ];

    let listeners = required
        .into_iter()
        .chain(optional.into_iter().flatten())
        .map(|(transport, field, endpoint)| {
            return listener_binding(transport, field, endpoint);
        })
        .collect::<Result<Vec<_>, _>>()?;

    return Ok(StartupPlan { listeners });
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

    return Ok(ListenerBinding {
        transport,
        endpoint: endpoint.to_owned(),
    });
}

fn response_for_path(path: &str) -> Result<(&'static str, &'static str, String), ApiError> {
    if matches!(path, "/readyz" | "/healthz") {
        let body =
            serde_json::to_string(&routes::health::body()).map_err(|_| ApiError::Serialization)?;
        return Ok(("200 OK", "application/json", body));
    }

    return Ok((
        "404 Not Found",
        "text/plain; charset=utf-8",
        "not found\n".to_owned(),
    ));
}

fn handle_http_connection(stream: &mut TcpStream) -> Result<(), ApiError> {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream.read(&mut buffer)?;
    if bytes_read == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let (status, content_type, body) = response_for_path(path)?;
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );

    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    return Ok(());
}

fn serve_http(bind: &str) -> Result<(), ApiError> {
    let listener = TcpListener::bind(bind)?;
    println!("api Http endpoint {bind}");

    for connection in listener.incoming() {
        let mut stream = connection?;
        handle_http_connection(&mut stream)?;
    }

    return Ok(());
}

pub fn run(config: &ApiConfig) -> Result<(), ApiError> {
    let plan = startup_plan(config)?;
    for listener in &plan.listeners {
        match listener.transport {
            TransportKind::Http => {}
            TransportKind::StatefulTcp => {
                println!("api StatefulTcp configured at {}", listener.endpoint);
            }
            TransportKind::DurableNats => {
                println!("api DurableNats configured");
            }
        }
    }

    let http = plan
        .listeners
        .iter()
        .find(|listener| listener.transport == TransportKind::Http)
        .ok_or(ApiError::InvalidConfiguration("GHA_INDIE_WORKER_API_BIND"))?;

    return serve_http(&http.endpoint);
}

#[cfg(test)]
mod tests {
    use super::{response_for_path, startup_plan, ListenerBinding, StartupPlan, TransportKind};
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
    fn readiness_is_json_and_unknown_paths_fail_closed() {
        let (status, content_type, body) = response_for_path("/readyz").expect("ready response");
        assert_eq!(status, "200 OK");
        assert_eq!(content_type, "application/json");
        assert!(body.contains("\"ok\":true"));

        let (status, _, body) = response_for_path("/missing").expect("404 response");
        assert_eq!(status, "404 Not Found");
        assert_eq!(body, "not found\n");
    }
}
