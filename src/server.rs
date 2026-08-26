#![forbid(unsafe_code)]

use crate::config::ApiConfig;
use crate::routes;

pub fn run(config: &ApiConfig) {
    println!("api bind {}", config.bind);
    println!(
        "{}",
        serde_json::to_string(&routes::health::body()).expect("health json")
    );
}

