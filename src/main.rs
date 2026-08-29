#![forbid(unsafe_code)]

use gha_indie_worker_api_server::{config::ApiConfig, flags, server};

fn main() {
    let env = match flags::apply_cli_flags() {
        Ok(env) => env,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let cfg = ApiConfig::from_env_map(&env);
    server::run(&cfg);
}
