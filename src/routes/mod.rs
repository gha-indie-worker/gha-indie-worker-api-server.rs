#![forbid(unsafe_code)]

//! HTTP surfaces. `health` is unauthenticated by contract; everything under
//! `v1` requires a verified actor except `/v1/capabilities` (a client needs it
//! before it can pick an auth flow) and `/v1/webhooks/github` (which
//! authenticates itself with an HMAC).

pub mod health;
pub mod v1;
