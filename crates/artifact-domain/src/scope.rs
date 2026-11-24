//! 租户/项目作用域。
//!
//! ART-01 §1.2 不变量 5：客户端传入的 tenant_id / project_id 只是资源定位输入，
//! 不构成授权证明；一切业务查询必须显式限定租户与项目。

use uuid::Uuid;

/// 由委托凭证解析出的可信作用域（非请求体）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantScope {
    pub tenant_id: Uuid,
    pub project_id: Uuid,
}

impl TenantScope {
    pub fn new(tenant_id: Uuid, project_id: Uuid) -> Self {
        Self {
            tenant_id,
            project_id,
        }
    }

    /// 判断某个资源作用域是否与当前委托作用域一致。
    pub fn matches(&self, tenant_id: Uuid, project_id: Uuid) -> bool {
        self.tenant_id == tenant_id && self.project_id == project_id
    }
}

/// 委托凭证中的主体（用户 / Agent / 服务）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub subject: String,
    pub actor_type: ActorType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorType {
    User,
    Agent,
    Service,
}

impl Actor {
    pub fn new(subject: impl Into<String>, actor_type: ActorType) -> Self {
        Self {
            subject: subject.into(),
            actor_type,
        }
    }
}
