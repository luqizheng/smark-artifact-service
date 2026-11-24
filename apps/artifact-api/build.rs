use std::process::Command;

fn main() {
    let sha = std::env::var("ARTIFACT_GIT_SHA").ok().or_else(|| {
        Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    let sha = sha.unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=ARTIFACT_GIT_SHA={sha}");
    println!("cargo:rerun-if-env-changed=ARTIFACT_GIT_SHA");
    println!("cargo:rerun-if-changed=build.rs");
}
