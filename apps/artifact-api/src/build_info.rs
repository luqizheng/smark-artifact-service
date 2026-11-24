//! 版本与构建信息（ART-02 §4.1：提供版本和构建信息）。

#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: &'static str,
    pub git_sha: &'static str,
    pub built_at: &'static str,
}

pub fn build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        git_sha: match option_env!("ARTIFACT_GIT_SHA") {
            Some(sha) => sha,
            None => "unknown",
        },
        built_at: match option_env!("ARTIFACT_BUILD_TIME") {
            Some(ts) => ts,
            None => "unknown",
        },
    }
}
