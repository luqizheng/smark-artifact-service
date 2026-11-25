//! Artifact Service HTTP Server（库入口：`tests/` 与消费者复用同一 router）。

pub mod auth;
pub mod build_info;
pub mod config;
pub mod error;
pub mod routes;
pub mod state;

pub use build_info::{BuildInfo, build_info};
pub use config::Config;
pub use routes::router;
pub use state::{
    AppState, app_state, app_state_with_verifier, default_verifier, verifier_from_config,
};
