#![forbid(unsafe_code)]

//! The four interaction avenues, one module each.
//!
//! 1. [`db`] — direct SeaORM reads through the canonical and auth pools.
//! 2. [`http`] — the stateless JSON API (`axum`), including graceful shutdown.
//! 3. [`tcp`] and [`ws`] — stateful carriers speaking one command vocabulary.
//! 4. [`nats`] — asynchronous domain events on `giw.<env>.<domain>.<event>`.

pub mod db;
pub mod http;
pub mod nats;
#[cfg(feature = "tcp-transport")]
pub mod tcp;
pub mod ws;
