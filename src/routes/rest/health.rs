#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize)]
pub struct HealthBody {
    pub ok: bool,
    pub service: &'static str,
}

#[allow(clippy::needless_return)]
pub fn body() -> HealthBody {
    return HealthBody {
        ok: true,
        service: "gha-indie-worker-api-server",
    };
}
