#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};

pub const DEFAULT_ROUTING_SUFFIX: &str = "local.indiebuild.dev";
pub const DEFAULT_PATH_PREFIX: &str = "/p";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteTarget {
    pub project: String,
    pub session: String,
    pub service: String,
    pub upstream_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoutingError {
    InvalidHost,
    InvalidPath,
    MissingProject,
    MissingSession,
    MissingService,
    UnsupportedHost,
}

impl Display for RoutingError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidHost => "invalid host",
            Self::InvalidPath => "invalid path",
            Self::MissingProject => "missing project",
            Self::MissingSession => "missing session",
            Self::MissingService => "missing service",
            Self::UnsupportedHost => "unsupported routing host",
        };
        f.write_str(message)
    }
}

impl std::error::Error for RoutingError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterConfig {
    pub domain_suffix: String,
    pub path_prefix: String,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            domain_suffix: DEFAULT_ROUTING_SUFFIX.to_owned(),
            path_prefix: DEFAULT_PATH_PREFIX.to_owned(),
        }
    }
}

impl RouterConfig {
    pub fn resolve(&self, host: &str, path_and_query: &str) -> Result<RouteTarget, RoutingError> {
        self.resolve_subdomain(host, path_and_query)
            .or_else(|subdomain_error| match subdomain_error {
                RoutingError::UnsupportedHost => self.resolve_subpath(path_and_query),
                other => Err(other),
            })
    }

    pub fn resolve_subdomain(
        &self,
        host: &str,
        path_and_query: &str,
    ) -> Result<RouteTarget, RoutingError> {
        let host = strip_port(host)?.trim_end_matches('.');
        let suffix = self.domain_suffix.trim_matches('.');
        let expected_suffix = format!(".{suffix}");
        let labels = host
            .strip_suffix(&expected_suffix)
            .ok_or(RoutingError::UnsupportedHost)?;

        // Canonical shape:
        //   <service>.<project>.<session>.<suffix>
        // Project and service names may contain '-', but not '.'. Session follows
        // the same DNS-label rule so the entire route is unambiguous.
        let mut labels = labels.rsplitn(3, '.');
        let session = labels.next().ok_or(RoutingError::MissingSession)?;
        let project = labels.next().ok_or(RoutingError::MissingProject)?;
        let service = labels.next().ok_or(RoutingError::MissingService)?;

        validate_label(service, RoutingError::MissingService)?;
        validate_label(project, RoutingError::MissingProject)?;
        validate_label(session, RoutingError::MissingSession)?;

        Ok(RouteTarget {
            project: project.to_owned(),
            session: session.to_owned(),
            service: service.to_owned(),
            upstream_path: normalize_upstream_path(path_and_query),
        })
    }

    pub fn resolve_subpath(&self, path_and_query: &str) -> Result<RouteTarget, RoutingError> {
        let (path, query) = split_path_and_query(path_and_query);
        let prefix = self.path_prefix.trim_end_matches('/');
        let remainder = path
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('/'))
            .ok_or(RoutingError::InvalidPath)?;

        let mut parts = remainder.splitn(4, '/');
        let project = parts.next().ok_or(RoutingError::MissingProject)?;
        let session = parts.next().ok_or(RoutingError::MissingSession)?;
        let service = parts.next().ok_or(RoutingError::MissingService)?;
        let tail = parts.next().unwrap_or_default();

        validate_label(project, RoutingError::MissingProject)?;
        validate_label(session, RoutingError::MissingSession)?;
        validate_label(service, RoutingError::MissingService)?;

        let upstream_path = if tail.is_empty() {
            "/".to_owned()
        } else {
            format!("/{tail}")
        };

        Ok(RouteTarget {
            project: project.to_owned(),
            session: session.to_owned(),
            service: service.to_owned(),
            upstream_path: append_query(upstream_path, query),
        })
    }
}

fn strip_port(host: &str) -> Result<&str, RoutingError> {
    let host = host.trim();
    if host.is_empty() || host.starts_with('[') {
        return Err(RoutingError::InvalidHost);
    }

    Ok(host.split_once(':').map_or(host, |(name, _)| name))
}

fn validate_label(value: &str, missing: RoutingError) -> Result<(), RoutingError> {
    if value.is_empty() {
        return Err(missing);
    }
    if value.len() > 63
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(RoutingError::InvalidHost);
    }
    Ok(())
}

fn split_path_and_query(path_and_query: &str) -> (&str, Option<&str>) {
    match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query, None),
    }
}

fn append_query(path: String, query: Option<&str>) -> String {
    match query {
        Some(query) => format!("{path}?{query}"),
        None => path,
    }
}

fn normalize_upstream_path(path_and_query: &str) -> String {
    if path_and_query.is_empty() {
        "/".to_owned()
    } else if path_and_query.starts_with('/') {
        path_and_query.to_owned()
    } else {
        format!("/{path_and_query}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_service_project_session_subdomain() {
        let route = RouterConfig::default()
            .resolve(
                "api.zed-pkg.pr-481.local.indiebuild.dev",
                "/v1/packages?limit=20",
            )
            .unwrap();

        assert_eq!(
            route,
            RouteTarget {
                project: "zed-pkg".into(),
                session: "pr-481".into(),
                service: "api".into(),
                upstream_path: "/v1/packages?limit=20".into(),
            }
        );
    }

    #[test]
    fn strips_ingress_port_before_subdomain_resolution() {
        let route = RouterConfig::default()
            .resolve(
                "admin-api.zed-pkg.pr-481.local.indiebuild.dev:8443",
                "/healthz",
            )
            .unwrap();

        assert_eq!(route.service, "admin-api");
    }

    #[test]
    fn falls_back_to_subpath_routing() {
        let route = RouterConfig::default()
            .resolve(
                "127.0.0.1:8080",
                "/p/zed-pkg/pr-481/admin-web/settings/profile",
            )
            .unwrap();

        assert_eq!(route.project, "zed-pkg");
        assert_eq!(route.session, "pr-481");
        assert_eq!(route.service, "admin-web");
        assert_eq!(route.upstream_path, "/settings/profile");
    }

    #[test]
    fn preserves_query_when_path_route_targets_service_root() {
        let route = RouterConfig::default()
            .resolve("localhost", "/p/zed-pkg/pr-481/api?limit=20&cursor=abc")
            .unwrap();

        assert_eq!(route.service, "api");
        assert_eq!(route.upstream_path, "/?limit=20&cursor=abc");
    }

    #[test]
    fn preserves_query_when_rewriting_path_route() {
        let route = RouterConfig::default()
            .resolve(
                "localhost",
                "/p/zed-pkg/pr-481/api/v1/packages?limit=20",
            )
            .unwrap();

        assert_eq!(route.upstream_path, "/v1/packages?limit=20");
    }

    #[test]
    fn path_root_is_forwarded_as_root() {
        let route = RouterConfig::default()
            .resolve("localhost", "/p/fiducia-cloud/dev/api")
            .unwrap();

        assert_eq!(route.upstream_path, "/");
    }

    #[test]
    fn rejects_ambiguous_or_invalid_labels() {
        let result = RouterConfig::default().resolve(
            "api.bad_project.pr-1.local.indiebuild.dev",
            "/",
        );

        assert_eq!(result, Err(RoutingError::InvalidHost));
    }

    #[test]
    fn rejects_dns_labels_over_sixty_three_bytes() {
        let too_long = "a".repeat(64);
        let host = format!("api.{too_long}.pr-1.local.indiebuild.dev");

        assert_eq!(
            RouterConfig::default().resolve(&host, "/"),
            Err(RoutingError::InvalidHost)
        );
    }
}
