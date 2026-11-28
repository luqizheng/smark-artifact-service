//! Artifact Worker 库入口（WP-AS-06）：执行循环与渲染设施供集成测试复用，
//! 与二进制共享同一实现（无测试特化路径）。

pub mod config;
pub mod execute;
pub mod render_sandbox;
pub mod seecode_adapter;
pub mod svg_safety;
