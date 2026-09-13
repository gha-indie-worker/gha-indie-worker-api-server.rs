#![forbid(unsafe_code)]

use crate::routing::{RouteTarget, RouterConfig, RoutingError};

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectIngress {
    /// An authority reachable only from the laptop-side ingress process, for
    /// example `127.0.0.1:39123`. The ores-compose session owns allocation.
    pub authority: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyPlan {
    pub project: String,
    pub session: String,
    pub service: String,
    pub upstream_authority: String,
    pub upstream_path: String,
    pub routing_headers: Vec<(String, String)>,
}

pub trait ProjectRuntime {
    type Error;

    /// Ensure the requested ores-compose project/session is running and return
    /// the session's Rust load-balancer ingress. Implementations may lazily
    /// launch a project before returning.
    fn ensure_project(&self, key: &ProjectSession) -> Result<ProjectIngress, Self::Error>;
}

#[derive(Debug)]
pub enum IngressError<E> {
    Routing(RoutingError),
    Runtime(E),
}

pub fn plan_request<R: ProjectRuntime>(
    router: &RouterConfig,
    runtime: &R,
    host: &str,
    path: &str,
) -> Result<ProxyPlan, IngressError<R::Error>> {
    let route = router.resolve(host, path).map_err(IngressError::Routing)?;
    let session = ProjectSession::from(&route);
    let ingress = runtime
        .ensure_project(&session)
        .map_err(IngressError::Runtime)?;

    Ok(ProxyPlan {
        project: route.project.clone(),
        session: route.session.clone(),
        service: route.service.clone(),
        upstream_authority: ingress.authority,
        upstream_path: route.upstream_path,
        routing_headers: vec![
            ("x-ores-project".into(), route.project),
            ("x-ores-session".into(), route.session),
            ("x-ores-service".into(), route.service),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeRuntime {
        ensured: RefCell<Vec<ProjectSession>>,
    }

    impl ProjectRuntime for FakeRuntime {
        type Error = ();

        fn ensure_project(&self, key: &ProjectSession) -> Result<ProjectIngress, Self::Error> {
            self.ensured.borrow_mut().push(key.clone());
            Ok(ProjectIngress {
                authority: "127.0.0.1:39123".into(),
            })
        }
    }

    #[test]
    fn project_is_ensured_before_proxy_plan_is_returned() {
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
        assert_eq!(plan.upstream_authority, "127.0.0.1:39123");
        assert_eq!(plan.service, "api");
        assert_eq!(plan.upstream_path, "/v1/packages");
        assert_eq!(
            plan.routing_headers,
            vec![
                ("x-ores-project".into(), "zed-pkg".into()),
                ("x-ores-session".into(), "pr-481".into()),
                ("x-ores-service".into(), "api".into()),
            ]
        );
    }
}
