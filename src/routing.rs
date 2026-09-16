#![forbid(unsafe_code)]

use std::fmt::{Display, Formatter};

pub const DEFAULT_ROUTING_SUFFIX: &str = "local.indiebuild.dev";
pub const DEFAULT_PATH_PREFIX: &str = "/p";
pub const DEFAULT_PATH_FALLBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "[::1]"];
pub const MAX_REQUEST_TARGET_BYTES: usize = 8 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteTarget {
    pub project: String,
    pub session: String,
    pub service: String,
    pub upstream_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RoutingError {
    InvalidConfig,
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
            Self::InvalidConfig => "invalid routing configuration",
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
    /// Hosts permitted to use `/p/<project>/<session>/<service>` routing.
    /// Keep this narrow. The default is loopback-only; a tunnel gateway must be
    /// opted in explicitly.
    pub path_fallback_hosts: Vec<String>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            domain_suffix: DEFAULT_ROUTING_SUFFIX.to_owned(),
            path_prefix: DEFAULT_PATH_PREFIX.to_owned(),
            path_fallback_hosts: DEFAULT_PATH_FALLBACK_HOSTS
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }
}

impl RouterConfig {
    pub fn validate(&self) -> Result<(), RoutingError> {
        let suffix = self.domain_suffix.trim_matches('.');
        if suffix.is_empty()
            || suffix.len() > 253
            || suffix
                .split('.')
                .any(|label| validate_label(label, RoutingError::InvalidConfig).is_err())
        {
            return Err(RoutingError::InvalidConfig);
        }

        let prefix = self.path_prefix.trim_end_matches('/');
        if prefix.is_empty()
            || !prefix.starts_with('/')
            || prefix.contains('?')
            || prefix.contains('#')
            || prefix.contains('\\')
        {
            return Err(RoutingError::InvalidConfig);
        }

        for allowed in &self.path_fallback_hosts {
            canonical_host(allowed).map_err(|_| RoutingError::InvalidConfig)?;
        }
        Ok(())
    }

    pub fn resolve(&self, host: &str, path_and_query: &str) -> Result<RouteTarget, RoutingError> {
        self.validate()?;
        validate_request_target(path_and_query)?;

        match self.resolve_subdomain(host, path_and_query) {
            Ok(route) => Ok(route),
            Err(RoutingError::UnsupportedHost) if self.path_fallback_host_allowed(host)? => {
                self.resolve_subpath(path_and_query)
            }
            Err(other) => Err(other),
        }
    }

    pub fn resolve_subdomain(
        &self,
        host: &str,
        path_and_query: &str,
    ) -> Result<RouteTarget, RoutingError> {
        let host = canonical_host(host)?;
        let suffix = self.domain_suffix.trim_matches('.').to_ascii_lowercase();
        let expected_suffix = format!(".{suffix}");
        let labels = host
            .strip_suffix(&expected_suffix)
            .ok_or(RoutingError::UnsupportedHost)?;

        if labels.is_empty() {
            return Err(RoutingError::UnsupportedHost);
        }

        // Canonical shape:
        //   <service>.<project>.<session>.<suffix>
        // Every routing component is a single DNS label so the mapping is
        // unambiguous and can be mirrored in path routing.
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
        validate_request_target(path_and_query)?;
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

    fn path_fallback_host_allowed(&self, host: &str) -> Result<bool, RoutingError> {
        let host = canonical_host(host)?;
        self.path_fallback_hosts
            .iter()
            .try_fold(false, |matched, allowed| {
                if matched {
                    return Ok(true);
                }
                Ok(canonical_host(allowed)? == host)
            })
    }
}

fn canonical_host(host: &str) -> Result<String, RoutingError> {
    let name = strip_port(host)?.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() || name.len() > 253 || name.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RoutingError::InvalidHost);
    }
    Ok(name)
}

fn strip_port(host: &str) -> Result<&str, RoutingError> {
    let host = host.trim();
    if host.is_empty() {
        return Err(RoutingError::InvalidHost);
    }

    if let Some(bracketed) = host.strip_prefix('[') {
        let end = bracketed.find(']').ok_or(RoutingError::InvalidHost)?;
        let name = &bracketed[..end];
        let suffix = &bracketed[end + 1..];
        if name.is_empty() {
            return Err(RoutingError::InvalidHost);
        }
        if suffix.is_empty() {
            return Ok(name);
        }
        let port = suffix.strip_prefix(':').ok_or(RoutingError::InvalidHost)?;
        validate_port(port)?;
        return Ok(name);
    }

    match host.matches(':').count() {
        0 => Ok(host),
        1 => {
            let (name, port) = host.rsplit_once(':').ok_or(RoutingError::InvalidHost)?;
            if name.is_empty() {
                return Err(RoutingError::InvalidHost);
            }
            validate_port(port)?;
            Ok(name)
        }
        _ => Err(RoutingError::InvalidHost),
    }
}

fn validate_port(port: &str) -> Result<(), RoutingError> {
    match port.parse::<u16>() {
        Ok(value) if value > 0 => Ok(()),
        _ => Err(RoutingError::InvalidHost),
    }
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
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(RoutingError::InvalidHost);
    }
    Ok(())
}

fn validate_request_target(path_and_query: &str) -> Result<(), RoutingError> {
    if path_and_query.is_empty()
        || path_and_query.len() > MAX_REQUEST_TARGET_BYTES
        || !path_and_query.starts_with('/')
        || path_and_query.contains('\\')
        || path_and_query.contains('#')
        || path_and_query
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(RoutingError::InvalidPath);
    }

    let (path, _) = split_path_and_query(path_and_query);
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(RoutingError::InvalidPath);
    }

    // Reject percent-encoded path separators, dot segments, NUL and CR/LF.
    // Different proxy stacks normalize these differently; fail closed rather
    // than allow the edge, laptop router and upstream application to disagree.
    let lower = path.to_ascii_lowercase();
    if ["%2e", "%2f", "%5c", "%00", "%0d", "%0a"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return Err(RoutingError::InvalidPath);
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
    path_and_query.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_service_project_session_subdomain() {
        let route = RouterConfig::default()
            .resolve(
                "API.ZED-PKG.PR-481.LOCAL.INDIEBUILD.DEV",
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
    fn falls_back_to_subpath_routing_on_loopback() {
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
    fn supports_bracketed_ipv6_loopback_for_path_fallback() {
        let route = RouterConfig::default()
            .resolve("[::1]:8080", "/p/zed-pkg/pr-481/api/healthz")
            .unwrap();
        assert_eq!(route.service, "api");
    }

    #[test]
    fn arbitrary_hosts_cannot_use_path_fallback() {
        assert_eq!(
            RouterConfig::default().resolve("attacker.example", "/p/zed-pkg/pr-481/api/healthz",),
            Err(RoutingError::UnsupportedHost)
        );
    }

    #[test]
    fn configured_gateway_can_use_path_fallback() {
        let mut config = RouterConfig::default();
        config
            .path_fallback_hosts
            .push("gateway.indiebuild.dev".into());
        let route = config
            .resolve(
                "gateway.indiebuild.dev:443",
                "/p/zed-pkg/pr-481/api/healthz",
            )
            .unwrap();
        assert_eq!(route.project, "zed-pkg");
    }

    #[test]
    fn preserves_query_when_path_route_targets_service_root() {
        let route = RouterConfig::default()
            .resolve("localhost", "/p/zed-pkg/pr-481/api?limit=20&cursor=abc")
            .unwrap();
        assert_eq!(route.upstream_path, "/?limit=20&cursor=abc");
    }

    #[test]
    fn rejects_ambiguous_or_invalid_labels() {
        assert_eq!(
            RouterConfig::default().resolve("api.bad_project.pr-1.local.indiebuild.dev", "/",),
            Err(RoutingError::InvalidHost)
        );
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

    #[test]
    fn rejects_malformed_ports() {
        assert_eq!(
            RouterConfig::default().resolve("localhost:not-a-port", "/p/a/b/c"),
            Err(RoutingError::InvalidHost)
        );
    }

    #[test]
    fn rejects_path_normalization_ambiguity() {
        for target in [
            "/p/zed-pkg/pr-1/api/../admin",
            "/p/zed-pkg/pr-1/api/%2e%2e/admin",
            "/p/zed-pkg/pr-1/api/a%2fb",
            "/p/zed-pkg/pr-1/api/a\\b",
        ] {
            assert_eq!(
                RouterConfig::default().resolve("localhost", target),
                Err(RoutingError::InvalidPath),
                "target={target}"
            );
        }
    }

    #[test]
    fn rejects_invalid_router_configuration() {
        let config = RouterConfig {
            domain_suffix: "".into(),
            path_prefix: "/p".into(),
            path_fallback_hosts: vec!["localhost".into()],
        };
        assert_eq!(
            config.resolve("localhost", "/p/a/b/c"),
            Err(RoutingError::InvalidConfig)
        );
    }
}
