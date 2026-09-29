#![forbid(unsafe_code)]

pub mod rest;

// Preserve the existing public module paths while the physical source authority
// lives under src/routes/rest/** as required by the ores-stack publication
// contract.
pub use rest::{health, v1};
