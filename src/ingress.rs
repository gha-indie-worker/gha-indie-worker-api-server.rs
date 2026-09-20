#![forbid(unsafe_code)]

use crate::routing::{RouteTarget, RouterConfig, RoutingError};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

pub const RESERVED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-ores-project",
    "x-ores-session",
    "x-ores-service",
    "x-ores-generation",
    "cf-access-authenticated-user-email",
    "cf-access-jwt-assertion",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSession {
    pub project: String,
    pub session: String,
}

impl From<&RouteTarget> for ProjectSession {
    fn from(route: &RouteTarget) -> Self {
        Self {
            project: route.project.clone(),
            session: route.session.clone(),
        }
    }
}

/// A target reachable only from the laptop-side ingress process. Runtime
/// implementations may allocate a loopback TCP port or a Unix-domain socket,
/// but may not redirect the ingress proxy to arbitrary network destinations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LocalUpstream {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl LocalUpstream {
    pub fn validate(&self, unix_socket_root: Option<&Path>) -> Result<(), InvalidUpstream> {
        match self {
            Self::Tcp(addr) if addr.ip().is_loopback() && addr.port() != 0 => Ok(()),
            Self::Tcp(_) => Err(InvalidUpstream::NonLocalTcp),
            Self::Unix(path) => validate_unix_endpoint(path, unix_socket_root),
        }
    }
}

fn safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
}

fn validate_unix_endpoint(
    path: &Path,
    unix_socket_root: Option<&Path>,
) -> Result<(), InvalidUpstream> {
    if !safe_absolute_path(path) {
        return Err(InvalidUpstream::UnsafeUnixPath);
    }
    let root = unix_socket_root.ok_or(InvalidUpstream::MissingUnixSocketRoot)?;
    if !safe_absolute_path(root) {
        return Err(InvalidUpstream::UnsafeUnixSocketRoot);
    }
    if path == root || !path.starts_with(root) {
        return Err(InvalidUpstream::UnixOutsideRuntimeRoot);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InvalidUpstream {
    NonLocalTcp,
    UnsafeUnixPath,
    MissingUnixSocketRoot,
    UnsafeUnixSocketRoot,
    UnixOutsideRuntimeRoot,
    InvalidGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectIngress {
    pub endpoint: LocalUpstream,
    /// Monotonic supervisor/session generation. Zero is never a valid live
    /// generation and is rejected before a proxy plan is produced.
    pub generation: u64,
    /// Required for Unix endpoints so the ingress process can prove the socket
    /// is inside the trusted ores-compose runtime tree. TCP endpoints ignore it.
    pub unix_socket_root: Option<PathBuf>,
}

impl ProjectIngress {
    /// Validate all trust-boundary properties of a supervisor-produced ingress
    /// record in one place so every future transport adapter can reuse the same
    /// checks before dialing it.
    pub fn validate(&self) -> Result<(), InvalidUpstream> {
        if self.generation == 0 {
            return Err(InvalidUpstream::InvalidGeneration);
        }
        self.endpoint.validate(self.unix_socket_root.as_deref())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyPlan {
    pub project: String,
    pub session: String,
    pub service: String,
    pub generation: u64,
    pub upstream: LocalUpstream,
    pub upstream_path: String,
    pub routing_headers: Vec<(String, String)>,
    /// The HTTP adapter must remove these case-insensitively from the incoming
    /// request before adding trusted routing/forwarding metadata. WebSocket
    /// Connection/Upgrade headers may then be reconstructed explicitly.
    pub headers_to_remove: Vec<String>,
}

pub trait ProjectRuntime {
    type Error;

    /// Admit a logical route before any lazy-start side effect occurs. A real
    /// runtime should check project membership, service existence, session
    /// ownership, quotas/concurrency, and whether this request may create a
    /// session. Unknown user-controlled project names must fail closed here.
    fn admit_route(&self, route: &RouteTarget) -> Result<(), Self::Error>;

    /// Ensure the admitted ores-compose project/session is running and return
    /// that session's local Rust load-balancer endpoint. Implementations should
    /// coalesce concurrent starts for the same key and bound total startups.
    /// The returned generation must be the current fenced generation.
    fn ensure_project(&self, key: &ProjectSession) -> Result<ProjectIngress, Self::Error>;
}

#[derive(Debug)]
pub enum IngressError<E> {
    Routing(RoutingError),
    Admission(E),
    Runtime(E),
    InvalidUpstream(InvalidUpstream),
}

pub fn plan_request<R: ProjectRuntime>(
    router: &RouterConfig,
    runtime: &R,
    host: &str,
    path_and_query: &str,
) -> Result<ProxyPlan, IngressError<R::Error>> {
    let route = router
        .resolve(host, path_and_query)
        .map_err(IngressError::Routing)?;

    runtime
        .admit_route(&route)
        .map_err(IngressError::Admission)?;

    let session = ProjectSession::from(&route);
    let ingress = runtime
        .ensure_project(&session)
        .map_err(IngressError::Runtime)?;
    ingress.validate().map_err(IngressError::InvalidUpstream)?;

    Ok(ProxyPlan {
        project: route.project.clone(),
        session: route.session.clone(),
        service: route.service.clone(),
        generation: ingress.generation,
        upstream: ingress.endpoint,
        upstream_path: route.upstream_path,
        routing_headers: vec![
            ("x-ores-project".into(), route.project),
            ("x-ores-session".into(), route.session),
            ("x-ores-service".into(), route.service),
            ("x-ores-generation".into(), ingress.generation.to_string()),
        ],
        headers_to_remove: RESERVED_REQUEST_HEADERS
            .iter()
            .map(|header| (*header).to_owned())
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::net::{IpAddr, Ipv4Addr};

    #[derive(Default)]
    struct FakeRuntime {
        deny: Cell<bool>,
        ensured: RefCell<Vec<ProjectSession>>,
    }

    impl ProjectRuntime for FakeRuntime {
        type Error = &'static str;

        fn admit_route(&self, _route: &RouteTarget) -> Result<(), Self::Error> {
            if self.deny.get() {
                Err("denied")
            } else {
                Ok(())
            }
        }

        fn ensure_project(&self, key: &ProjectSession) -> Result<ProjectIngress, Self::Error> {
            self.ensured.borrow_mut().push(key.clone());
            Ok(ProjectIngress {
                endpoint: LocalUpstream::Tcp(SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    39123,
                )),
                generation: 7,
                unix_socket_root: None,
            })
        }
    }

    #[test]
    fn project_is_admitted_and_ensured_before_proxy_plan_is_returned() {
        let runtime = FakeRuntime::default();
        let plan = plan_request(
            &RouterConfig::default(),
            &runtime,
            "api.zed-pkg.pr-481.local.indiebuild.dev",
            "/v1/packages",
        )
        .unwrap();

        assert_eq!(
            runtime.ensured.into_inner(),
            vec![ProjectSession {
                project: "zed-pkg".into(),
                session: "pr-481".into(),
            }]
        );
        assert_eq!(plan.service, "api");
        assert_eq!(plan.generation, 7);
        assert_eq!(plan.upstream_path, "/v1/packages");
        assert!(plan.headers_to_remove.contains(&"x-ores-service".into()));
        assert!(plan.headers_to_remove.contains(&"x-ores-generation".into()));
        assert!(plan.headers_to_remove.contains(&"x-forwarded-for".into()));
        assert!(plan
            .routing_headers
            .contains(&("x-ores-generation".into(), "7".into())));
    }

    #[test]
    fn denied_route_has_no_lazy_start_side_effect() {
        let runtime = FakeRuntime::default();
        runtime.deny.set(true);
        let result = plan_request(
            &RouterConfig::default(),
            &runtime,
            "api.zed-pkg.pr-481.local.indiebuild.dev",
            "/",
        );
        assert!(matches!(result, Err(IngressError::Admission("denied"))));
        assert!(runtime.ensured.into_inner().is_empty());
    }

    struct RemoteRuntime;

    impl ProjectRuntime for RemoteRuntime {
        type Error = ();

        fn admit_route(&self, _route: &RouteTarget) -> Result<(), Self::Error> {
            Ok(())
        }

        fn ensure_project(&self, _key: &ProjectSession) -> Result<ProjectIngress, Self::Error> {
            Ok(ProjectIngress {
                endpoint: LocalUpstream::Tcp("8.8.8.8:443".parse().unwrap()),
                generation: 1,
                unix_socket_root: None,
            })
        }
    }

    #[test]
    fn runtime_cannot_turn_ingress_into_ssrf_proxy() {
        let result = plan_request(
            &RouterConfig::default(),
            &RemoteRuntime,
            "api.zed-pkg.pr-481.local.indiebuild.dev",
            "/",
        );
        assert!(matches!(
            result,
            Err(IngressError::InvalidUpstream(InvalidUpstream::NonLocalTcp))
        ));
    }

    #[test]
    fn safe_absolute_unix_socket_must_be_below_runtime_root() {
        let ingress = ProjectIngress {
            endpoint: LocalUpstream::Unix(PathBuf::from(
                "/tmp/ores-compose/sessions/pr-481/control.sock",
            )),
            generation: 3,
            unix_socket_root: Some(PathBuf::from("/tmp/ores-compose/sessions")),
        };
        assert_eq!(ingress.validate(), Ok(()));
    }

    #[test]
    fn unix_socket_outside_runtime_root_is_rejected() {
        let ingress = ProjectIngress {
            endpoint: LocalUpstream::Unix(PathBuf::from("/var/run/docker.sock")),
            generation: 1,
            unix_socket_root: Some(PathBuf::from("/tmp/ores-compose/sessions")),
        };
        assert_eq!(
            ingress.validate(),
            Err(InvalidUpstream::UnixOutsideRuntimeRoot)
        );
    }

    #[test]
    fn unix_socket_without_runtime_root_is_rejected() {
        let ingress = ProjectIngress {
            endpoint: LocalUpstream::Unix(PathBuf::from(
                "/tmp/ores-compose/sessions/pr-481/control.sock",
            )),
            generation: 1,
            unix_socket_root: None,
        };
        assert_eq!(
            ingress.validate(),
            Err(InvalidUpstream::MissingUnixSocketRoot)
        );
    }

    #[test]
    fn unix_socket_with_parent_traversal_is_rejected() {
        let ingress = ProjectIngress {
            endpoint: LocalUpstream::Unix(PathBuf::from("/tmp/ores-compose/../admin.sock")),
            generation: 1,
            unix_socket_root: Some(PathBuf::from("/tmp/ores-compose")),
        };
        assert_eq!(ingress.validate(), Err(InvalidUpstream::UnsafeUnixPath));
    }

    struct ZeroGenerationRuntime;

    impl ProjectRuntime for ZeroGenerationRuntime {
        type Error = ();

        fn admit_route(&self, _route: &RouteTarget) -> Result<(), Self::Error> {
            Ok(())
        }

        fn ensure_project(&self, _key: &ProjectSession) -> Result<ProjectIngress, Self::Error> {
            Ok(ProjectIngress {
                endpoint: LocalUpstream::Tcp("127.0.0.1:39123".parse().unwrap()),
                generation: 0,
                unix_socket_root: None,
            })
        }
    }

    #[test]
    fn zero_generation_is_never_proxyable() {
        let result = plan_request(
            &RouterConfig::default(),
            &ZeroGenerationRuntime,
            "api.zed-pkg.pr-481.local.indiebuild.dev",
            "/",
        );
        assert!(matches!(
            result,
            Err(IngressError::InvalidUpstream(
                InvalidUpstream::InvalidGeneration
            ))
        ));
    }
}
