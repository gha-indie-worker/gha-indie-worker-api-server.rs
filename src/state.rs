#![forbid(unsafe_code)]

use crate::config::ApiConfig;

#[derive(Clone, Debug)]
pub struct AppState {
    pub config: ApiConfig,
}

