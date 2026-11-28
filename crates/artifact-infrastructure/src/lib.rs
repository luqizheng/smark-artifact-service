//! Artifact Service 基础设施实现：PostgreSQL 仓储、委托验证器、存储端口。

pub mod delegation;
pub mod file_service;
pub mod postgres;
pub mod storage;

#[cfg(feature = "test-doubles")]
pub mod test_support;
