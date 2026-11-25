-- 0006_delegation_token_use.sql — WP-LMMS-DELEGATION-01：委托 Token 单次操作绑定
--
-- 契约来源（唯一规范性依据）：docs/architecture/WP-LMMS-DELEGATION-01-AUTHORIZATION-CONTRACT.md v0.2 §3.4
--   P0-02：一个 Token 对应一个明确业务操作；jti ↔ (scope, caller_request_id,
--   request_hash) 唯一绑定；并发重放由数据库唯一约束原子仲裁；同指纹重试合法
--   （交由业务幂等层收敛）；跨请求复用 jti 确定性拒绝（DELEGATION_TOKEN_REUSED）。
-- 0001–0005 不改一字（CI 与测试按 migrations.sha256 校验）；本迁移为加法式。
--
-- 交付内容：
--   ① artifact.delegation_token_use：jti 唯一约束 + (caller_request_id, request_hash)
--      配对 CHECK（同空或同非空，hash 为 64 位小写十六进制）；
--   ② first_used_at 索引（运维清理用）；
--   ③ purge_delegation_token_use(before)：SECURITY DEFINER 清理函数，
--      PUBLIC 无 EXECUTE；窗口护栏（只允许清理 ≥ 1h 前的行——token TTL ≤ 300s
--      + 时钟偏差 60s，1h 窗口外的行不可能再被消费判定引用）。
--
-- 信任边界：本表是"防重放判定"的持久化状态，不是授权来源。授权仍以委托
-- token + Delegation::require 判定；本表只在 AS-4 接线的写路径入口被
-- PgDelegationBindingStore::consume 原子消费（契约 §3.4.4 事务边界）。
--
-- 迁移之后必须重新执行 deploy/db-roles.sql（APP 角色按 ON ALL TABLES 获得本表
-- DML；本迁移无新函数授予 APP，清理函数授权由运维按需显式执行）。

-- ── ① jti 单次绑定表 ─────────────────────────────────────────────────────

CREATE TABLE artifact.delegation_token_use (
    id                uuid         PRIMARY KEY DEFAULT gen_random_uuid(),
    issuer            text         NOT NULL,
    jti               text         NOT NULL,
    tenant_id         uuid         NOT NULL,
    project_id        uuid         NOT NULL,
    caller_request_id uuid,
    request_hash      char(64),
    first_used_at     timestamptz  NOT NULL DEFAULT now(),
    CONSTRAINT uq_delegation_token_use_jti UNIQUE (jti),
    CONSTRAINT delegation_token_use_binding_pairing_check CHECK (
        (caller_request_id IS NULL AND request_hash IS NULL)
        OR (caller_request_id IS NOT NULL
            AND request_hash IS NOT NULL
            AND request_hash ~ '^[0-9a-f]{64}$')
    )
);

-- ── ② 清理索引用（运维 purge；查询路径走 uq_..._jti 唯一索引） ────────────

CREATE INDEX ix_delegation_token_use_first_used_at
    ON artifact.delegation_token_use (first_used_at);

-- ── ③ 保守清理函数：SECURITY DEFINER + 窗口护栏，默认无任何角色可执行 ─────

CREATE OR REPLACE FUNCTION artifact.purge_delegation_token_use(p_before timestamptz)
RETURNS bigint
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, artifact
AS $$
DECLARE
    v_deleted bigint;
BEGIN
    -- 护栏：拒绝清理过近的行（token TTL ≤ 300s + 60s 偏差，1h 远超任何
    -- 消费判定窗口）。这是不可逆删除的最后防线。
    IF p_before IS NULL OR p_before > now() - interval '1 hour' THEN
        RAISE EXCEPTION 'PURGE_WINDOW_TOO_RECENT' USING ERRCODE = 'P0001';
    END IF;
    DELETE FROM artifact.delegation_token_use WHERE first_used_at < p_before;
    GET DIAGNOSTICS v_deleted = ROW_COUNT;
    RETURN v_deleted;
END $$;

REVOKE ALL ON FUNCTION artifact.purge_delegation_token_use(timestamptz) FROM PUBLIC;

-- ── ④ 历史清点（不回填：本表从零开始，历史无 delegation token 消费记录） ──

DO $$
DECLARE
    v_existing bigint;
BEGIN
    SELECT count(*) INTO v_existing FROM artifact.delegation_token_use;
    RAISE NOTICE '0006 delegation token use inventory: % pre-existing binding row(s) (expected 0)',
        v_existing;
END $$;
