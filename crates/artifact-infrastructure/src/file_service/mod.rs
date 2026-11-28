//! LMMS file-service 适配层（WP-AS-05）。
//!
//! * [`service_jwt`]：短时 `aud=file-service` 服务令牌（bootstrap 凭证 →
//!   token-service 按 `(aud, tid)` mint，进程内缓存；F-24：恒带 tid）。
//! * [`storage`]：`StoragePort` 的真实适配器 —— 契约 v0.3 §12 裁决的
//!   "上传 + 校验 + 补偿"近似（LMMS 无 upload-intent/finalize 协议，
//!   P1 差异记录在案；生产姿态 fail-closed 不变）。

pub mod service_jwt;
pub mod storage;
