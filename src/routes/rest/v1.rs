#![forbid(unsafe_code)]

use serde::Serialize;

#[derive(Serialize)]
pub struct Catalog {
    pub resource: &'static str,
}

pub fn catalog() -> Catalog {
    return Catalog {
        resource: "WorkerLease",
    };
}
