#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize)]
pub struct Catalog {
    pub resource: &'static str,
}

#[allow(clippy::needless_return)]
pub fn catalog() -> Catalog {
    return Catalog {
        resource: "WorkerLease",
    };
}
