#![forbid(unsafe_code)]

use crate::ingress::{ProxyPlan, RESERVED_REQUEST_HEADERS};
use std::net::IpAddr;

pub const MAX_PROXY_HEADERS: usize = 128;
pub const MAX_PROXY_HEADER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct HttpListener {
    pub bind: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeaderSanitizeError {
    InvalidName,
    InvalidValue,
    InvalidForwardingContext,
    TooManyHeaders,
    HeadersTooLarge,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForwardingContext {
    pub client_ip: IpAddr,
    pub original_host: String,
    pub proto: String,
}

fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn valid_header_value(value: &str) -> bool {
    !value
        .bytes()
        .any(|byte| matches!(byte, b'\r' | b'\n' | 0))
}

fn reserved_header(name: &str) -> bool {
    RESERVED_REQUEST_HEADERS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn validate_forwarding_context(context: &ForwardingContext) -> Result<(), HeaderSanitizeError> {
    if !matches!(context.proto.as_str(), "http" | "https")
        || context.original_host.is_empty()
        || !valid_header_value(&context.original_host)
        || context
            .original_host
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(HeaderSanitizeError::InvalidForwardingContext);
    }
    Ok(())
}

/// Strip spoofable hop-by-hop, forwarding, Cloudflare identity, and internal
/// routing headers, then append only the trusted routing metadata created by
/// `plan_request`. Admission is bounded before allocations grow without limit.
pub fn sanitize_proxy_headers(
    incoming: impl IntoIterator<Item = (String, String)>,
    plan: &ProxyPlan,
) -> Result<Vec<(String, String)>, HeaderSanitizeError> {
    let mut result = Vec::new();
    let mut count = 0usize;
    let mut bytes = 0usize;
    for (name, value) in incoming {
        count = count.saturating_add(1);
        if count > MAX_PROXY_HEADERS {
            return Err(HeaderSanitizeError::TooManyHeaders);
        }
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        if bytes > MAX_PROXY_HEADER_BYTES {
            return Err(HeaderSanitizeError::HeadersTooLarge);
        }
        if !valid_header_name(&name) {
            return Err(HeaderSanitizeError::InvalidName);
        }
        if !valid_header_value(&value) {
            return Err(HeaderSanitizeError::InvalidValue);
        }
        if reserved_header(&name) {
            continue;
        }
        result.push((name, value));
    }

    for (name, value) in &plan.routing_headers {
        if !valid_header_name(name) || !valid_header_value(value) {
            return Err(HeaderSanitizeError::InvalidValue);
        }
        bytes = bytes.saturating_add(name.len()).saturating_add(value.len());
        if bytes > MAX_PROXY_HEADER_BYTES {
            return Err(HeaderSanitizeError::HeadersTooLarge);
        }
        result.push((name.clone(), value.clone()));
    }
    Ok(result)
}

/// Add forwarding metadata only after untrusted forwarding headers have been
/// removed. The caller must supply transport-observed values rather than values
/// copied from the incoming request.
pub fn append_trusted_forwarding_headers(
    headers: &mut Vec<(String, String)>,
    context: &ForwardingContext,
) -> Result<(), HeaderSanitizeError> {
    validate_forwarding_context(context)?;
    headers.push(("x-forwarded-for".into(), context.client_ip.to_string()));
    headers.push(("x-forwarded-host".into(), context.original_host.clone()));
    headers.push(("x-forwarded-proto".into(), context.proto.clone()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingress::LocalUpstream;

    fn plan() -> ProxyPlan {
        ProxyPlan {
            project: "zed-pkg".into(),
            session: "pr-481".into(),
            service: "api".into(),
            generation: 9,
            upstream: LocalUpstream::Tcp("127.0.0.1:39123".parse().unwrap()),
            upstream_path: "/healthz".into(),
            routing_headers: vec![
                ("x-ores-project".into(), "zed-pkg".into()),
                ("x-ores-session".into(), "pr-481".into()),
                ("x-ores-service".into(), "api".into()),
                ("x-ores-generation".into(), "9".into()),
            ],
            headers_to_remove: RESERVED_REQUEST_HEADERS
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
        }
    }

    #[test]
    fn strips_spoofable_headers_case_insensitively() {
        let headers = vec![
            ("X-Ores-Project".into(), "attacker".into()),
            ("x-forwarded-for".into(), "203.0.113.5".into()),
            ("CF-Access-JWT-Assertion".into(), "fake".into()),
            ("accept".into(), "application/json".into()),
        ];
        let sanitized = sanitize_proxy_headers(headers, &plan()).unwrap();

        assert!(sanitized.contains(&("accept".into(), "application/json".into())));
        assert!(sanitized.contains(&("x-ores-project".into(), "zed-pkg".into())));
        assert!(!sanitized
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("x-ores-project") && value == "attacker"));
        assert!(!sanitized
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-forwarded-for")));
        assert!(!sanitized
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("cf-access-jwt-assertion")));
    }

    #[test]
    fn trusted_forwarding_headers_are_rebuilt_from_transport_context() {
        let mut headers = sanitize_proxy_headers(Vec::new(), &plan()).unwrap();
        let context = ForwardingContext {
            client_ip: "198.51.100.9".parse().unwrap(),
            original_host: "api.zed-pkg.pr-481.local.indiebuild.dev".into(),
            proto: "https".into(),
        };
        append_trusted_forwarding_headers(&mut headers, &context).unwrap();
        assert!(headers.contains(&("x-forwarded-for".into(), "198.51.100.9".into())));
        assert!(headers.contains(&("x-forwarded-proto".into(), "https".into())));
    }

    #[test]
    fn invalid_forwarding_context_fails_closed() {
        let mut headers = Vec::new();
        let context = ForwardingContext {
            client_ip: "127.0.0.1".parse().unwrap(),
            original_host: "bad host".into(),
            proto: "ftp".into(),
        };
        assert_eq!(
            append_trusted_forwarding_headers(&mut headers, &context),
            Err(HeaderSanitizeError::InvalidForwardingContext)
        );
    }

    #[test]
    fn rejects_header_value_injection() {
        let headers = vec![("x-safe".into(), "ok\r\nx-evil: true".into())];
        assert_eq!(
            sanitize_proxy_headers(headers, &plan()),
            Err(HeaderSanitizeError::InvalidValue)
        );
    }

    #[test]
    fn rejects_invalid_header_names() {
        let headers = vec![("bad header".into(), "value".into())];
        assert_eq!(
            sanitize_proxy_headers(headers, &plan()),
            Err(HeaderSanitizeError::InvalidName)
        );
    }

    #[test]
    fn bounds_header_count() {
        let headers = (0..=MAX_PROXY_HEADERS)
            .map(|index| (format!("x-test-{index}"), "ok".into()))
            .collect::<Vec<_>>();
        assert_eq!(
            sanitize_proxy_headers(headers, &plan()),
            Err(HeaderSanitizeError::TooManyHeaders)
        );
    }

    #[test]
    fn bounds_total_header_bytes() {
        let headers = vec![("x-large".into(), "a".repeat(MAX_PROXY_HEADER_BYTES))];
        assert_eq!(
            sanitize_proxy_headers(headers, &plan()),
            Err(HeaderSanitizeError::HeadersTooLarge)
        );
    }
}
