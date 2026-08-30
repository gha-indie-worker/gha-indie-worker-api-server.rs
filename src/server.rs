#![forbid(unsafe_code)]

use crate::config::ApiConfig;
use crate::error::ApiError;
use crate::routes;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportKind {
    Http,
    StatefulTcp,
    DurableNats,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenerBinding {
    pub transport: TransportKind,
    pub endpoint: String,
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

pub fn run(config: &ApiConfig) -> Result<(), ApiError> {
    let plan = startup_plan(config)?;
    for listener in &plan.listeners {
        println!(
            "api {:?} endpoint {}",
            listener.transport, listener.endpoint
        );
    }
    println!(
        "{}",
        serde_json::to_string(&routes::health::body()).map_err(|_| ApiError::Serialization)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{startup_plan, ListenerBinding, StartupPlan, TransportKind};
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
}
