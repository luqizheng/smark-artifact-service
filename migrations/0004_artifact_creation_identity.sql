-- 0004_artifact_creation_identity.sql — AS-1 持久化 Artifact 创建身份
--
-- 契约来源：ART-04E《Durable Artifact Creation Identity and Crash-Recovery Contract》。
-- 本迁移只做加法：不重写 0001–0003，不回填任何历史 Artifact 行。
--
-- 不变量（数据库强制）：
--   1. (tenant_id, project_id, artifact_key) 上的创建身份唯一且只追加，
--      运行角色无 UPDATE/DELETE 权限（见 deploy/db-roles.sql 的显式 REVOKE）。
--   2. 创建身份行与其 Artifact 行必须同事务提交：两个方向的
--      DEFERRABLE INITIALLY DEFERRED 约束触发器在提交时校验配对。
--   3. 已有非 NULL artifact_key 不可修改；V1 也不允许把 NULL 改成非 NULL。
--      历史 keyed Artifact 的身份迁移不属于普通创建流程，必须另行审批。
--   4. Idempotency-Key 清理函数只删除「已完成且已过期」的行，
--      使用行锁 + SKIP LOCKED，批次上限固定，SECURITY DEFINER 固定 search_path。

-- ── ① 持久化创建身份（append-only） ────────────────────────────────────

CREATE TABLE artifact.artifact_creation_identity (
    tenant_id           uuid        NOT NULL,
    project_id          uuid        NOT NULL,
    artifact_key        text        NOT NULL,
    artifact_id         uuid        NOT NULL,
    fingerprint_version smallint    NOT NULL,
    request_fingerprint char(64)    NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now(),
    created_by          text        NOT NULL,

    CONSTRAINT artifact_creation_identity_pk
        PRIMARY KEY (tenant_id, project_id, artifact_key),
    CONSTRAINT artifact_creation_identity_artifact_unique
        UNIQUE (tenant_id, project_id, artifact_id),
    CONSTRAINT artifact_creation_identity_artifact_fk
        FOREIGN KEY (artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact (id, tenant_id, project_id),
    CONSTRAINT artifact_creation_identity_key_check
        CHECK (length(btrim(artifact_key)) BETWEEN 1 AND 200),
    CONSTRAINT artifact_creation_identity_fingerprint_version_check
        CHECK (fingerprint_version > 0),
    CONSTRAINT artifact_creation_identity_fingerprint_check
        CHECK (request_fingerprint ~ '^[0-9a-f]{64}$'),
    CONSTRAINT artifact_creation_identity_created_by_check
        CHECK (length(btrim(created_by)) > 0)
);

-- 数据库层最后防线：身份行只允许追加，不能改写、不能删除、不能改绑。
CREATE FUNCTION artifact.guard_creation_identity_append_only() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'artifact_creation_identity is append-only' USING ERRCODE = 'P0001';
END $$;

CREATE TRIGGER trg_creation_identity_append_only
    BEFORE UPDATE OR DELETE ON artifact.artifact_creation_identity
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_creation_identity_append_only();

-- 仓库约定：所有函数都撤销 PUBLIC EXECUTE（触发器函数由所有者隐式执行，
-- 不需要运行角色持有 EXECUTE）。
REVOKE ALL ON FUNCTION artifact.guard_creation_identity_append_only() FROM PUBLIC;

-- ── ② 双向配对不变量（提交时校验） ─────────────────────────────────────

-- 方向 A：新插入或 key 发生变化的 Artifact（非 NULL key）必须有配对身份行。
-- 未在本事务中触碰的历史行不参与校验，因此无需回填历史数据。
CREATE FUNCTION artifact.check_artifact_identity_pairing() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.artifact_key IS NULL THEN
        RETURN NULL;
    END IF;
    -- 普通元数据更新不会改变 key；历史 keyed 行保持原样合法。
    IF TG_OP = 'UPDATE' AND NEW.artifact_key IS NOT DISTINCT FROM OLD.artifact_key THEN
        RETURN NULL;
    END IF;
    IF NOT EXISTS (
        SELECT 1
        FROM artifact.artifact_creation_identity i
        WHERE i.tenant_id = NEW.tenant_id
          AND i.project_id = NEW.project_id
          AND i.artifact_key = NEW.artifact_key
          AND i.artifact_id = NEW.id
    ) THEN
        RAISE EXCEPTION
            'artifact % with artifact_key has no matching creation identity', NEW.id
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER trg_artifact_identity_pairing
    AFTER INSERT OR UPDATE OF artifact_key ON artifact.artifact
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION artifact.check_artifact_identity_pairing();

REVOKE ALL ON FUNCTION artifact.check_artifact_identity_pairing() FROM PUBLIC;

-- 方向 B：身份行插入时，被引用 Artifact 必须存在且 artifact_key 完全一致。
CREATE FUNCTION artifact.check_identity_artifact_pairing() RETURNS trigger
    LANGUAGE plpgsql AS $$
DECLARE
    actual_key text;
BEGIN
    SELECT a.artifact_key INTO actual_key
    FROM artifact.artifact a
    WHERE a.id = NEW.artifact_id
      AND a.tenant_id = NEW.tenant_id
      AND a.project_id = NEW.project_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'creation identity references an artifact outside its scope'
            USING ERRCODE = 'P0001';
    END IF;
    IF actual_key IS DISTINCT FROM NEW.artifact_key THEN
        RAISE EXCEPTION 'creation identity artifact_key does not match the referenced artifact'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NULL;
END $$;

CREATE CONSTRAINT TRIGGER trg_creation_identity_artifact_pairing
    AFTER INSERT ON artifact.artifact_creation_identity
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW EXECUTE FUNCTION artifact.check_identity_artifact_pairing();

REVOKE ALL ON FUNCTION artifact.check_identity_artifact_pairing() FROM PUBLIC;

-- ── ③ artifact_key 不可变 ──────────────────────────────────────────────

CREATE FUNCTION artifact.guard_artifact_key_immutable() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.artifact_key IS NOT NULL AND NEW.artifact_key IS DISTINCT FROM OLD.artifact_key THEN
        RAISE EXCEPTION 'artifact_key is immutable once set' USING ERRCODE = 'P0001';
    END IF;
    IF OLD.artifact_key IS NULL AND NEW.artifact_key IS NOT NULL THEN
        RAISE EXCEPTION
            'artifact_key cannot be assigned to an existing artifact; a separately approved adoption flow is required'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_key_immutable
    BEFORE UPDATE ON artifact.artifact
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_artifact_key_immutable();

REVOKE ALL ON FUNCTION artifact.guard_artifact_key_immutable() FROM PUBLIC;

-- ── ④ Idempotency-Key 物理清理（受限维护函数） ─────────────────────────

-- expires_at 只是「可清理时间」，不是逻辑过期或可复用时间：
--   * 已完成（response_snapshot 非空）且已过期的行才会被删除；
--   * 未完成行永不自动删除（须在旧写入者排空后人工对账）；
--   * 行锁 + SKIP LOCKED 保证清理不会与正在解析同一 key 的请求同时获胜。
CREATE FUNCTION artifact.purge_expired_idempotency_records(batch_size integer)
    RETURNS integer
    LANGUAGE plpgsql
    SECURITY DEFINER
    SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    capped integer;
    removed integer;
BEGIN
    IF batch_size IS NULL OR batch_size < 1 THEN
        RAISE EXCEPTION 'batch_size must be a positive integer' USING ERRCODE = '22023';
    END IF;
    capped := LEAST(batch_size, 1000);

    DELETE FROM artifact.idempotency_record r
    WHERE r.id IN (
        SELECT c.id
        FROM artifact.idempotency_record c
        WHERE c.response_snapshot IS NOT NULL
          AND c.expires_at <= now()
        ORDER BY c.expires_at
        LIMIT capped
        FOR UPDATE SKIP LOCKED
    );

    GET DIAGNOSTICS removed = ROW_COUNT;
    RETURN removed;
END $$;

REVOKE ALL ON FUNCTION artifact.purge_expired_idempotency_records(integer) FROM PUBLIC;

-- ── ⑤ 历史数据清点（只记录，绝不回填） ────────────────────────────────

DO $$
DECLARE
    legacy_count bigint;
BEGIN
    SELECT count(*) INTO legacy_count
    FROM artifact.artifact a
    WHERE a.artifact_key IS NOT NULL
      AND NOT EXISTS (
          SELECT 1
          FROM artifact.artifact_creation_identity i
          WHERE i.tenant_id = a.tenant_id
            AND i.project_id = a.project_id
            AND i.artifact_key = a.artifact_key
            AND i.artifact_id = a.id
      );
    RAISE NOTICE
        'AS-1 migration inventory: % keyed artifact(s) without creation identity (not backfilled)',
        legacy_count;
END $$;
