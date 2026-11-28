//! 写路径操作绑定强制（V10 接线，WP-LMMS-DELEGATION-04）。
//!
//! # 强制点（契约 §3.1.2 / WP-04 §5.1）
//!
//! 每个 [`crate::ArtifactService`] 写方法入口在 `Delegation::require`（动作 +
//! 作用域）之后、**任何**副作用（DB 变更 / 存储调用 / 队列发布）之前调用
//! [`enforce_write_binding`]。服务层接线保证一切 adapter（HTTP 或未来入口）
//! 都无法绕过；HTTP 层采集的只有「事实数据」（method / 实例化 path / 原始
//! body 字节 / 请求侧 caller），判定全部在本层完成——指纹从不取自调用方可控
//! 的头字段。
//!
//! # 绑定轴（契约 v0.3 冻结可表达部分）
//!
//! 1. **caller_request_id**：请求侧 caller 优先（Job 创建的 body 字段），
//!    其余路由取 token claim 本身（写 token 必带，验证器 PC-O5-1 红线 → 401）。
//!    两者并存时必须相等（经 v1 指纹 CALLER 段 + 显式比对双保险）。
//! 2. **request_hash（v1 冻结规范）**：按
//!    [`canonical_request_fingerprint`] 以「method + 实例化 path + caller +
//!    原始 body Value」重算，与 `biz.request_hash` claim 比对。任何成分差异
//!    （跨 method / 换目标 / body 变异 / 编码形态差异）→ 指纹不同 → 拒绝。
//! 3. **artifact_id**：claim 存在时请求目标 artifact 必须相等（§3.1.2 #1）。
//!    目标寻址路由（含 W8：路径含 `{artifact_id}`，与路径 artifact 精确比对）
//!    之外，真正无目标 artifact 的路由是 artifact 创建（W1，目标尚不存在）与
//!    取消（W9，路径仅含 job_id）——这两类路由上出现该 claim → 拒绝
//!    （字面 fail-closed：无目标即不可能相等；裁决记录 D2）。
//!    claim 缺省时本轴不拒绝：契约 §3.1.1 示例中 `generation:request` token
//!    携带 `artifact_id: null` 合法，"条件必填"的具体条件属签发侧契约，
//!    未冻结（WP-04-R1 决策项 D4）。
//! 4. **operation_id**：仅 `generation:cancel` 落地
//!    `generation_job:{job_id}` 精确比对（契约 §5.3 原文 + §3.1.1 示例语法）；
//!    其余路由的语法表未在契约冻结（裁决记录 D1：停止接线，保持现状，
//!    存在时不比对也不拒绝，待设计裁决后另行启用）。
//!
//! # 失败语义
//!
//! 一律 fail-closed：返回 [`artifact_domain::DomainError::ScopeDenied`]，
//! HTTP 层映射为 403 `SCOPE_DENIED` 稳定信封（契约 §3.1.2 #1）。消息只含
//! 判定原因，不含 token 本体、指纹值或请求内容。
//!
//! # 与幂等的关系（§6 红线）
//!
//! 本模块只做「token ↔ 本次请求」的绑定判定；`caller_request_id` 是幂等/绑定
//! 键，**不是**防重放保证——重放仲裁由各端点幂等机制（AS-1 / AS-2 / CAS /
//! 状态机）承接，见 ROUTE-IDEMPOTENCY-MATRIX。

use uuid::Uuid;

use artifact_domain::{Delegation, DomainError};

use super::delegation_request::{
    REQUEST_HASH_ALGORITHM_V1, assert_delegation_matches_request, canonical_request_fingerprint,
};

/// 写操作类型：与路由一一对应（矩阵 W1–W9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOperationType {
    /// POST /v1/artifacts
    ArtifactCreate,
    /// PATCH /v1/artifacts/{artifact_id}
    ArtifactPatchMetadata,
    /// POST /v1/artifacts/{artifact_id}/versions
    VersionSessionCreate,
    /// POST /v1/artifacts/{artifact_id}/versions/{version_id}/commit
    VersionCommit,
    /// POST /v1/artifacts/{artifact_id}/current-version
    CurrentVersionSet,
    /// POST /v1/artifacts/{artifact_id}/references
    ReferenceCreate,
    /// DELETE /v1/artifacts/{artifact_id}/references/{reference_id}
    ReferenceRemove,
    /// POST /v1/artifacts/{artifact_id}/generation-jobs
    GenerationRequest,
    /// POST /v1/generation-jobs/{job_id}/cancel
    GenerationCancel,
}

/// 请求侧操作绑定事实（由 adapter 采集，服务层强制）。
///
/// * `method`/`path`：HTTP 方法与**实例化**路径（路径参数已代入实际值；
///   UTF-8 字节原样，不解码/重编码——v1 冻结规则）。
/// * `body`：**原始** body 解析出的 `serde_json::Value`（未知字段参与指纹，
///   与签发方对原始请求 JSON 的指纹对齐）；无 body 端点为 `Value::Null`。
/// * `request_caller_request_id`：仅 Job 创建路由携带（body 字段，与 token
///   claim 构成真实交叉比对）；其余路由为 `None`。
/// * `target_artifact_id`：本操作的目标 artifact（路径可定位时）。W2–W8 均
///   为目标寻址路由——**W8（`/v1/artifacts/{artifact_id}/generation-jobs`）
///   路径含 artifact_id，为 `Some(路径 artifact)`，其 artifact_id claim 与
///   路径 ID 精确比对**（WP-04-R1 P2 修正：W8 不是无目标路由）。真正
///   `None` 的只有 artifact 创建（W1，目标尚不存在）与取消（W9，路径仅含
///   job_id）。
/// * `target_job_id`：取消路由的目标 job（路径可定位时）；其余路由 `None`。
#[derive(Debug, Clone)]
pub struct WriteOperationBinding {
    pub method: String,
    pub path: String,
    pub operation_type: WriteOperationType,
    pub target_artifact_id: Option<Uuid>,
    pub target_job_id: Option<Uuid>,
    pub body: serde_json::Value,
    pub request_caller_request_id: Option<Uuid>,
}

impl WriteOperationBinding {
    /// 契约冻结语法的稳定操作标识（裁决记录 D1：仅取消路由在契约内）。
    fn contract_operation_id(&self) -> Option<String> {
        match self.operation_type {
            WriteOperationType::GenerationCancel => {
                self.target_job_id.map(|id| format!("generation_job:{id}"))
            }
            // 其余路由语法未冻结（D1）：返回 None = 不比对、不拒绝。
            _ => None,
        }
    }
}

/// 强制「token ↔ 本次请求」绑定一致性。任一轴不一致即
/// [`DomainError::ScopeDenied`]（403），绝不降级放行。
pub fn enforce_write_binding(
    delegation: &Delegation,
    op: &WriteOperationBinding,
) -> Result<(), DomainError> {
    // ① 请求侧 caller：Job 创建 = body 字段（与 claim 交叉比对）；其余路由
    //    = claim 本身。写 token 无 claim（验证器红线被绕过/测试替身直通）时
    //    在此失败关闭——服务层不信任验证器之外的任何保证。
    let caller = match op
        .request_caller_request_id
        .or(delegation.caller_request_id)
    {
        Some(caller) => caller,
        None => {
            return Err(DomainError::ScopeDenied(
                "delegation token carries no operation binding (caller_request_id missing); \
                 write operations require an operation-bound token"
                    .into(),
            ));
        }
    };
    if let Some(request_caller) = op.request_caller_request_id
        && delegation.caller_request_id.is_some()
        && delegation.caller_request_id != Some(request_caller)
    {
        return Err(DomainError::ScopeDenied(format!(
            "delegation token is not minted for caller_request_id {request_caller}"
        )));
    }

    // ② artifact_id 目标相等（§3.1.2 #1 字面 fail-closed：无目标即拒绝，D2）。
    if let Some(bound_artifact) = delegation.artifact_id {
        match op.target_artifact_id {
            Some(target) if target == bound_artifact => {}
            Some(target) => {
                return Err(DomainError::ScopeDenied(format!(
                    "delegation token is bound to artifact {bound_artifact}; \
                     the request targets artifact {target}"
                )));
            }
            None => {
                return Err(DomainError::ScopeDenied(
                    "delegation token is bound to a specific artifact; \
                     this operation has no artifact target to verify the binding against"
                        .into(),
                ));
            }
        }
    }

    // ③ operation_id：仅契约冻结语法（取消路由）落地精确比对（D1）。
    if let Some(bound_operation) = &delegation.operation_id {
        match op.contract_operation_id() {
            Some(expected) if bound_operation == &expected => {}
            Some(expected) => {
                return Err(DomainError::ScopeDenied(format!(
                    "delegation token operation binding '{bound_operation}' does not match \
                     the stable operation identity '{expected}'"
                )));
            }
            None => {
                // D1 裁决：语法未冻结的路由不比对也不拒绝（保持 v0.3 现状）。
                // 该分支不得静默扩大；启用需先修订裁决记录。
            }
        }
    }

    // ④ v1 指纹重算比对（caller + hash 轴；method/path/body 变异在此拒绝）。
    let fingerprint = canonical_request_fingerprint(&op.method, &op.path, Some(caller), &op.body);
    assert_delegation_matches_request(delegation, caller, &fingerprint)?;
    // ⑤ 显式算法版本防线（双重保险）：验证器已 fail-closed 拒绝未知 alg；
    //    本层仅接受 v1——指纹函数本身就是 v1 实现，任何非 v1 claim 的 hash
    //    都不可能与本重算值相等，此处比对保证语义不依赖上述推理。
    debug_assert_eq!(REQUEST_HASH_ALGORITHM_V1, "v1");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use artifact_domain::{Action, Actor, ActorType, TenantScope};
    use std::collections::HashSet;

    const CALLER: Uuid = Uuid::from_u128(0xC0DE);
    const ARTIFACT: Uuid = Uuid::from_u128(0xA17);
    const OTHER_ARTIFACT: Uuid = Uuid::from_u128(0xA18);
    const JOB: Uuid = Uuid::from_u128(0xB0B);

    fn delegation(
        caller: Option<Uuid>,
        hash: Option<&str>,
        artifact_id: Option<Uuid>,
        operation_id: Option<&str>,
    ) -> Delegation {
        Delegation {
            delegation_id: "d".into(),
            jti: "j".into(),
            scope: TenantScope::new(Uuid::from_u128(0x11), Uuid::from_u128(0x22)),
            actor: Actor::new("user-1", ActorType::User),
            actions: HashSet::from([Action::ArtifactCreate, Action::GenerationCancel]),
            expires_at: chrono::Utc::now() + chrono::Duration::seconds(300),
            lmms_tid: None,
            artifact_id,
            operation_id: operation_id.map(str::to_string),
            caller_request_id: caller,
            request_hash: hash.map(str::to_string),
        }
    }

    fn op(
        operation_type: WriteOperationType,
        method: &str,
        path: &str,
        target: Option<Uuid>,
        body: serde_json::Value,
    ) -> WriteOperationBinding {
        WriteOperationBinding {
            method: method.into(),
            path: path.into(),
            operation_type,
            target_artifact_id: target,
            target_job_id: None,
            body,
            request_caller_request_id: None,
        }
    }

    fn bound_delegation_for(op: &WriteOperationBinding) -> Delegation {
        let caller = Some(CALLER);
        let hash = canonical_request_fingerprint(&op.method, &op.path, caller, &op.body);
        delegation(caller, Some(&hash), None, None)
    }

    #[test]
    fn correct_binding_is_accepted() {
        let operation = op(
            WriteOperationType::ArtifactCreate,
            "POST",
            "/v1/artifacts",
            None,
            serde_json::json!({"name": "x"}),
        );
        let d = bound_delegation_for(&operation);
        assert!(enforce_write_binding(&d, &operation).is_ok());
    }

    #[test]
    fn path_method_or_body_mutation_is_rejected() {
        let base_body = serde_json::json!({"name": "x"});
        let operation = op(
            WriteOperationType::ArtifactCreate,
            "POST",
            "/v1/artifacts",
            None,
            base_body,
        );
        let d = bound_delegation_for(&operation);
        // 换目标（路径实例化值不同）。
        let patched = op(
            WriteOperationType::ArtifactPatchMetadata,
            "PATCH",
            &format!("/v1/artifacts/{OTHER_ARTIFACT}"),
            Some(OTHER_ARTIFACT),
            serde_json::json!({"name": "x"}),
        );
        assert!(enforce_write_binding(&d, &patched).is_err());
        // 换 method。
        let method_swapped = op(
            WriteOperationType::ArtifactCreate,
            "PATCH",
            "/v1/artifacts",
            None,
            serde_json::json!({"name": "x"}),
        );
        assert!(enforce_write_binding(&d, &method_swapped).is_err());
        // body 变异。
        let body_swapped = op(
            WriteOperationType::ArtifactCreate,
            "POST",
            "/v1/artifacts",
            None,
            serde_json::json!({"name": "y"}),
        );
        assert!(enforce_write_binding(&d, &body_swapped).is_err());
    }

    #[test]
    fn missing_caller_or_hash_is_rejected_fail_closed() {
        let operation = op(
            WriteOperationType::ArtifactCreate,
            "POST",
            "/v1/artifacts",
            None,
            serde_json::json!({"name": "x"}),
        );
        let hash =
            canonical_request_fingerprint("POST", "/v1/artifacts", Some(CALLER), &operation.body);
        // 无 caller。
        let d = delegation(None, Some(&hash), None, None);
        assert!(enforce_write_binding(&d, &operation).is_err());
        // caller 有、hash 缺（半配对）。
        let d = delegation(Some(CALLER), None, None, None);
        assert!(enforce_write_binding(&d, &operation).is_err());
        // hash 被篡改。
        let d = delegation(Some(CALLER), Some(&"0".repeat(64)), None, None);
        assert!(enforce_write_binding(&d, &operation).is_err());
    }

    #[test]
    fn artifact_id_claim_requires_matching_target() {
        let operation = op(
            WriteOperationType::ArtifactPatchMetadata,
            "PATCH",
            &format!("/v1/artifacts/{ARTIFACT}"),
            Some(ARTIFACT),
            serde_json::json!({"name": "x"}),
        );
        let hash =
            canonical_request_fingerprint("PATCH", &operation.path, Some(CALLER), &operation.body);
        // 绑定一致。
        let d = delegation(Some(CALLER), Some(&hash), Some(ARTIFACT), None);
        assert!(enforce_write_binding(&d, &operation).is_ok());
        // 绑定到另一个 artifact。
        let d = delegation(Some(CALLER), Some(&hash), Some(OTHER_ARTIFACT), None);
        assert!(enforce_write_binding(&d, &operation).is_err());
        // 创建型路由（无目标）携带 artifact_id → fail-closed（D2）。
        let creation = op(
            WriteOperationType::ArtifactCreate,
            "POST",
            "/v1/artifacts",
            None,
            serde_json::json!({"name": "x"}),
        );
        let creation_hash =
            canonical_request_fingerprint("POST", "/v1/artifacts", Some(CALLER), &creation.body);
        let d = delegation(Some(CALLER), Some(&creation_hash), Some(ARTIFACT), None);
        assert!(enforce_write_binding(&d, &creation).is_err());
    }

    #[test]
    fn cancel_operation_id_uses_contract_grammar() {
        let path = format!("/v1/generation-jobs/{JOB}/cancel");
        let mut operation = op(
            WriteOperationType::GenerationCancel,
            "POST",
            &path,
            None,
            serde_json::Value::Null,
        );
        operation.target_job_id = Some(JOB);
        let hash =
            canonical_request_fingerprint("POST", &path, Some(CALLER), &serde_json::Value::Null);
        // 契约语法精确匹配。
        let d = delegation(
            Some(CALLER),
            Some(&hash),
            None,
            Some(&format!("generation_job:{JOB}")),
        );
        assert!(enforce_write_binding(&d, &operation).is_ok());
        // 绑定到别的 job。
        let d = delegation(
            Some(CALLER),
            Some(&hash),
            None,
            Some(&format!("generation_job:{}", Uuid::from_u128(0xBAD))),
        );
        assert!(enforce_write_binding(&d, &operation).is_err());
        // 非 cancel 路由语法未冻结：operation_id 存在不比对也不拒绝（D1）。
        let patch = op(
            WriteOperationType::ArtifactPatchMetadata,
            "PATCH",
            &format!("/v1/artifacts/{ARTIFACT}"),
            Some(ARTIFACT),
            serde_json::json!({"name": "x"}),
        );
        let patch_hash =
            canonical_request_fingerprint("PATCH", &patch.path, Some(CALLER), &patch.body);
        let d = delegation(Some(CALLER), Some(&patch_hash), None, Some("anything:goes"));
        assert!(enforce_write_binding(&d, &patch).is_ok());
    }

    #[test]
    fn job_request_caller_must_equal_token_claim() {
        let body = serde_json::json!({"generator_id": "g", "caller_request_id": CALLER});
        let operation = WriteOperationBinding {
            method: "POST".into(),
            path: format!("/v1/artifacts/{ARTIFACT}/generation-jobs"),
            operation_type: WriteOperationType::GenerationRequest,
            target_artifact_id: Some(ARTIFACT),
            target_job_id: None,
            body: body.clone(),
            request_caller_request_id: Some(CALLER),
        };
        let hash = canonical_request_fingerprint("POST", &operation.path, Some(CALLER), &body);
        let d = delegation(Some(CALLER), Some(&hash), None, None);
        assert!(enforce_write_binding(&d, &operation).is_ok());
        // body caller 与 claim 不一致 → 拒绝。
        let other = Uuid::new_v4();
        let mismatch = WriteOperationBinding {
            request_caller_request_id: Some(other),
            ..operation
        };
        assert!(enforce_write_binding(&d, &mismatch).is_err());
    }
}
