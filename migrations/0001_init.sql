-- 0001_init.sql — Artifact Service 独立数据库初始 schema
--
-- 契约来源：ART-01 §2（资源模型）、§3（schema/隔离/并发）、ADR-ART-001 §5/§6/§10。
-- 表集合（ART-01 §3.1）：artifact、artifact_version、artifact_output、
--   artifact_reference、generation_job、generator_registry、idempotency_record、
--   audit_event（outbox_event 暂缓，启用事件通知时再增）。
-- 不变量（数据库强制，应用层只是第一道防线）：
--   1. Version READY 后内容/元数据不可变，state 不可离开 ready（触发器）。
--   2. current_version_id 只能指向同 Artifact、同 scope、state=ready 的版本（触发器）。
--   3. version_number 由父行锁串行分配，单调递增（触发器，禁止先读 max 再无锁插入）。
--   4. deleted Artifact 不接受新 Version / Reference / Job（子表插入守卫）。
--   5. audit_event 只追加（触发器 + 运行角色无 UPDATE/DELETE 权限）。
--   6. 跨租户/跨项目引用一律由复合外键（含 tenant_id/project_id）杜绝。

CREATE SCHEMA IF NOT EXISTS artifact;

-- ── ① Generator 注册表 ─────────────────────────────────────────────────

CREATE TABLE artifact.generator_registry (
    generator_id             text PRIMARY KEY,
    display_name             text NOT NULL,
    version                  text NOT NULL DEFAULT '0.0.0',
    adapter_version          text,
    supported_input_formats  text[] NOT NULL DEFAULT '{}',
    supported_output_formats text[] NOT NULL DEFAULT '{}',
    capabilities             jsonb NOT NULL DEFAULT '{}'::jsonb,
    status                   text NOT NULL DEFAULT 'unavailable',
    status_reason            text,
    registered_at            timestamptz NOT NULL DEFAULT now(),
    updated_at               timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT generator_status_check
        CHECK (status IN ('available', 'unavailable', 'disabled'))
);

-- ART-02 事实：两个正式 Generator（Mermaid / SeeCode）Adapter 均未实现，
-- 如实注册为 unavailable；禁止任何"已实现"伪状态。
INSERT INTO artifact.generator_registry
    (generator_id, display_name, supported_input_formats, supported_output_formats,
     capabilities, status, status_reason)
VALUES
    ('mermaid', 'Mermaid Diagram Generator', '{mmd}', '{svg}',
     '{"security_profile": "static_svg_only", "png_enabled": false}',
     'unavailable', 'adapter not implemented (planned post ART-02)'),
    ('seecode', 'SeeCode VizSpec Generator', '{vizspec}', '{svg}',
     '{"security_profile": "server_svg_only", "html_output": false}',
     'unavailable', 'adapter not implemented (planned post ART-02)');

-- ── ② Artifact（逻辑产物容器） ─────────────────────────────────────────

CREATE TABLE artifact.artifact (
    id                 uuid PRIMARY KEY,
    tenant_id          uuid NOT NULL,
    project_id         uuid NOT NULL,
    artifact_key       text,
    name               text NOT NULL,
    description        text,
    artifact_type      text NOT NULL,
    category           text,
    lifecycle_state    text NOT NULL DEFAULT 'active',
    current_version_id uuid,
    created_by         text NOT NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    deleted_at         timestamptz,
    row_revision       bigint NOT NULL DEFAULT 1,
    CONSTRAINT artifact_id_scope_unique UNIQUE (id, tenant_id, project_id),
    CONSTRAINT artifact_name_check CHECK (length(btrim(name)) > 0),
    CONSTRAINT artifact_type_check CHECK (artifact_type IN
        ('document', 'report', 'image', 'diagram', 'visualization',
         'presentation', 'dataset', 'media', 'file')),
    CONSTRAINT artifact_lifecycle_check CHECK (lifecycle_state IN
        ('active', 'archived', 'deleted')),
    CONSTRAINT artifact_deleted_at_check CHECK (
        (deleted_at IS NULL AND lifecycle_state <> 'deleted')
        OR (deleted_at IS NOT NULL AND lifecycle_state = 'deleted'))
);

CREATE UNIQUE INDEX uq_artifact_project_key
    ON artifact.artifact (tenant_id, project_id, artifact_key)
    WHERE artifact_key IS NOT NULL;
CREATE INDEX idx_artifact_scope_state_updated
    ON artifact.artifact (tenant_id, project_id, lifecycle_state, updated_at DESC);
CREATE INDEX idx_artifact_scope_type_created
    ON artifact.artifact (tenant_id, project_id, artifact_type, created_at DESC);

-- 乐观并发锚 + updated_at 自动维护。
CREATE FUNCTION artifact.touch_artifact() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    NEW.row_revision := OLD.row_revision + 1;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_touch BEFORE UPDATE ON artifact.artifact
    FOR EACH ROW EXECUTE FUNCTION artifact.touch_artifact();

-- current_version_id 不变量：只能指向同 Artifact、同 scope、READY 的版本。
CREATE FUNCTION artifact.guard_current_version() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.current_version_id IS NOT NULL THEN
        IF NOT EXISTS (
            SELECT 1 FROM artifact.artifact_version v
            WHERE v.id = NEW.current_version_id
              AND v.artifact_id = NEW.id
              AND v.tenant_id = NEW.tenant_id
              AND v.project_id = NEW.project_id
              AND v.state = 'ready'
        ) THEN
            RAISE EXCEPTION
                'current_version_id must reference a READY version of the same artifact'
                USING ERRCODE = 'P0001';
        END IF;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_current_version BEFORE INSERT OR UPDATE ON artifact.artifact
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_current_version();

-- ── ③ ArtifactVersion（不可变快照） ────────────────────────────────────

CREATE TABLE artifact.artifact_version (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL,
    project_id        uuid NOT NULL,
    artifact_id       uuid NOT NULL,
    version_number    bigint NOT NULL DEFAULT 0,
    state             text NOT NULL DEFAULT 'pending',
    source_format     text,
    source_object_id  text,
    source_sha256     char(64),
    generator_id      text REFERENCES artifact.generator_registry (generator_id),
    generator_version text,
    adapter_version   text,
    manifest          jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_by        text NOT NULL,
    created_at        timestamptz NOT NULL DEFAULT now(),
    ready_at          timestamptz,
    failure_code      text,
    CONSTRAINT artifact_version_number_unique UNIQUE (artifact_id, version_number),
    CONSTRAINT artifact_version_id_scope_unique UNIQUE (id, artifact_id, tenant_id, project_id),
    CONSTRAINT artifact_version_scope_fk FOREIGN KEY (artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact (id, tenant_id, project_id),
    CONSTRAINT artifact_version_state_check CHECK (state IN
        ('pending', 'ready', 'failed', 'quarantined')),
    CONSTRAINT artifact_version_number_check CHECK (version_number >= 1),
    CONSTRAINT artifact_version_sha_check CHECK (
        source_sha256 IS NULL OR length(btrim(source_sha256)) = 64)
);

CREATE INDEX idx_artifact_version_artifact
    ON artifact.artifact_version (artifact_id, version_number DESC);
CREATE INDEX idx_artifact_version_scope
    ON artifact.artifact_version (tenant_id, project_id, artifact_id);

-- version_number 分配：锁父行（事务级串行），取 max+1。
-- ART-01 §3.3：禁止先读 max(version_number) 再无锁插入。
CREATE FUNCTION artifact.assign_version_number() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    parent_lifecycle text;
BEGIN
    SELECT lifecycle_state INTO parent_lifecycle
        FROM artifact.artifact
        WHERE id = NEW.artifact_id
          AND tenant_id = NEW.tenant_id
          AND project_id = NEW.project_id
        FOR UPDATE;
    IF parent_lifecycle IS NULL THEN
        RAISE EXCEPTION 'parent artifact % not found in scope', NEW.artifact_id
            USING ERRCODE = 'P0001';
    END IF;
    IF parent_lifecycle = 'deleted' THEN
        RAISE EXCEPTION 'deleted artifact does not accept new versions'
            USING ERRCODE = 'P0001';
    END IF;
    SELECT COALESCE(MAX(version_number), 0) + 1 INTO NEW.version_number
        FROM artifact.artifact_version
        WHERE artifact_id = NEW.artifact_id;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_version_number BEFORE INSERT ON artifact.artifact_version
    FOR EACH ROW EXECUTE FUNCTION artifact.assign_version_number();

-- Version 状态机 + READY 不可变（ART-01 §2.3：READY 后 source/manifest/
-- generator metadata/对象引用/created 元数据均不可改，ready_at 不可改，
-- 不得离开 ready）。归属（artifact/scope）对一切状态不可改。
CREATE FUNCTION artifact.guard_version_immutability() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.id <> OLD.id
        OR NEW.artifact_id <> OLD.artifact_id
        OR NEW.tenant_id <> OLD.tenant_id
        OR NEW.project_id <> OLD.project_id THEN
        RAISE EXCEPTION 'artifact_version ownership columns are immutable'
            USING ERRCODE = 'P0001';
    END IF;
    IF OLD.state = 'ready' THEN
        IF NEW.state <> 'ready' THEN
            RAISE EXCEPTION 'READY version is terminal' USING ERRCODE = 'P0001';
        END IF;
        IF NEW.manifest IS DISTINCT FROM OLD.manifest
            OR NEW.source_format IS DISTINCT FROM OLD.source_format
            OR NEW.source_object_id IS DISTINCT FROM OLD.source_object_id
            OR NEW.source_sha256 IS DISTINCT FROM OLD.source_sha256
            OR NEW.generator_id IS DISTINCT FROM OLD.generator_id
            OR NEW.generator_version IS DISTINCT FROM OLD.generator_version
            OR NEW.adapter_version IS DISTINCT FROM OLD.adapter_version
            OR NEW.created_by IS DISTINCT FROM OLD.created_by
            OR NEW.created_at IS DISTINCT FROM OLD.created_at
            OR NEW.ready_at IS DISTINCT FROM OLD.ready_at THEN
            RAISE EXCEPTION 'READY version content is immutable'
                USING ERRCODE = 'P0001';
        END IF;
    ELSE
        IF NOT (
            (OLD.state = 'pending' AND NEW.state IN ('pending', 'ready', 'failed', 'quarantined'))
            OR (OLD.state IN ('failed', 'quarantined') AND NEW.state = OLD.state)
        ) THEN
            RAISE EXCEPTION 'invalid version state transition % -> %', OLD.state, NEW.state
                USING ERRCODE = 'P0001';
        END IF;
        IF NEW.state = 'ready' AND OLD.state = 'pending' AND NEW.ready_at IS NULL THEN
            RAISE EXCEPTION 'READY version requires ready_at' USING ERRCODE = 'P0001';
        END IF;
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_version_immutable BEFORE UPDATE ON artifact.artifact_version
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_version_immutability();

-- ── ④ ArtifactOutput（可查询派生输出） ─────────────────────────────────

CREATE TABLE artifact.artifact_output (
    id             uuid PRIMARY KEY,
    tenant_id      uuid NOT NULL,
    project_id     uuid NOT NULL,
    artifact_id    uuid NOT NULL,
    version_id     uuid NOT NULL,
    output_role    text NOT NULL,
    format         text NOT NULL,
    mime_type      text NOT NULL,
    size_bytes     bigint NOT NULL,
    sha256         char(64) NOT NULL,
    file_object_id text NOT NULL,
    safety_status  text NOT NULL DEFAULT 'pending',
    created_at     timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT artifact_output_version_role_format_unique UNIQUE (version_id, output_role, format),
    CONSTRAINT artifact_output_version_scope_fk FOREIGN KEY (version_id, artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact_version (id, artifact_id, tenant_id, project_id),
    CONSTRAINT artifact_output_role_check CHECK (output_role IN
        ('primary', 'preview', 'attachment', 'source-map')),
    CONSTRAINT artifact_output_size_check CHECK (size_bytes >= 0),
    CONSTRAINT artifact_output_sha_check CHECK (length(btrim(sha256)) = 64),
    CONSTRAINT artifact_output_safety_check CHECK (safety_status IN
        ('pending', 'passed', 'rejected'))
);

CREATE INDEX idx_artifact_output_version ON artifact.artifact_output (version_id);

-- ── ⑤ ArtifactReference（外部业务引用，无跨服务外键） ──────────────────

CREATE TABLE artifact.artifact_reference (
    id            uuid PRIMARY KEY,
    tenant_id     uuid NOT NULL,
    project_id    uuid NOT NULL,
    artifact_id   uuid NOT NULL,
    owner_type    text NOT NULL,
    owner_id      text NOT NULL,
    relation_type text NOT NULL,
    created_by    text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    removed_at    timestamptz,
    CONSTRAINT artifact_reference_scope_fk FOREIGN KEY (artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact (id, tenant_id, project_id),
    CONSTRAINT artifact_reference_owner_check CHECK (owner_type IN
        ('requirement', 'design', 'adr', 'task', 'knowledge', 'project')),
    CONSTRAINT artifact_reference_relation_check CHECK (relation_type IN
        ('generated_from', 'illustrates', 'supports', 'references',
         'evidence_for', 'delivered_by')),
    CONSTRAINT artifact_reference_owner_id_check CHECK (length(btrim(owner_id)) > 0)
);

-- ART-01 §2.5：唯一约束仅作用于未解除（removed_at IS NULL）的引用。
CREATE UNIQUE INDEX uq_artifact_reference_active
    ON artifact.artifact_reference
        (tenant_id, project_id, artifact_id, owner_type, owner_id, relation_type)
    WHERE removed_at IS NULL;
CREATE INDEX idx_artifact_reference_owner
    ON artifact.artifact_reference (tenant_id, project_id, owner_type, owner_id);

CREATE FUNCTION artifact.guard_reference_parent() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    parent_lifecycle text;
BEGIN
    SELECT lifecycle_state INTO parent_lifecycle
        FROM artifact.artifact
        WHERE id = NEW.artifact_id
          AND tenant_id = NEW.tenant_id
          AND project_id = NEW.project_id;
    IF parent_lifecycle = 'deleted' THEN
        RAISE EXCEPTION 'deleted artifact does not accept new references'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_artifact_reference_parent BEFORE INSERT ON artifact.artifact_reference
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_reference_parent();

-- ── ⑥ GenerationJob（状态机与 Artifact 生命周期分离） ──────────────────

CREATE TABLE artifact.generation_job (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL,
    project_id        uuid NOT NULL,
    artifact_id       uuid NOT NULL,
    target_version_id uuid,
    generator_id      text NOT NULL REFERENCES artifact.generator_registry (generator_id),
    generator_version text,
    adapter_version   text,
    status            text NOT NULL DEFAULT 'queued',
    idempotency_key   text,
    request_hash      char(64),
    attempt_count     integer NOT NULL DEFAULT 0,
    max_attempts      integer NOT NULL DEFAULT 3,
    created_at        timestamptz NOT NULL DEFAULT now(),
    started_at        timestamptz,
    heartbeat_at      timestamptz,
    completed_at      timestamptz,
    error_code        text,
    error_message     text,
    trace_id          text,
    CONSTRAINT generation_job_artifact_scope_fk FOREIGN KEY (artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact (id, tenant_id, project_id),
    CONSTRAINT generation_job_target_scope_fk FOREIGN KEY (target_version_id, artifact_id, tenant_id, project_id)
        REFERENCES artifact.artifact_version (id, artifact_id, tenant_id, project_id),
    CONSTRAINT generation_job_status_check CHECK (status IN
        ('queued', 'running', 'retry_wait', 'succeeded', 'failed', 'cancelled', 'timed_out')),
    CONSTRAINT generation_job_attempts_check CHECK
        (attempt_count >= 0 AND max_attempts >= 1)
);

CREATE INDEX idx_generation_job_scope_status
    ON artifact.generation_job (tenant_id, project_id, status, created_at);
-- Worker 调度恢复（ART-01 §3.2）。
CREATE INDEX idx_generation_job_status_heartbeat
    ON artifact.generation_job (status, heartbeat_at);

CREATE FUNCTION artifact.guard_job_transition() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NEW.artifact_id <> OLD.artifact_id
        OR NEW.tenant_id <> OLD.tenant_id
        OR NEW.project_id <> OLD.project_id THEN
        RAISE EXCEPTION 'generation_job ownership columns are immutable'
            USING ERRCODE = 'P0001';
    END IF;
    IF NEW.status <> OLD.status AND NOT (
        (OLD.status = 'queued'     AND NEW.status IN ('running', 'cancelled'))
        OR (OLD.status = 'running' AND NEW.status IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'retry_wait'))
        OR (OLD.status = 'retry_wait' AND NEW.status IN ('queued', 'cancelled'))
    ) THEN
        RAISE EXCEPTION 'invalid generation_job transition % -> %', OLD.status, NEW.status
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_generation_job_transition BEFORE UPDATE ON artifact.generation_job
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_job_transition();

CREATE FUNCTION artifact.guard_job_parent() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    parent_lifecycle text;
BEGIN
    SELECT lifecycle_state INTO parent_lifecycle
        FROM artifact.artifact
        WHERE id = NEW.artifact_id
          AND tenant_id = NEW.tenant_id
          AND project_id = NEW.project_id;
    IF parent_lifecycle = 'deleted' THEN
        RAISE EXCEPTION 'deleted artifact does not accept generation jobs'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;

CREATE TRIGGER trg_generation_job_parent BEFORE INSERT ON artifact.generation_job
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_job_parent();

-- ── ⑦ 幂等记录（ART-01 §4.1：创建类写请求支持 Idempotency-Key） ────────

CREATE TABLE artifact.idempotency_record (
    id                uuid PRIMARY KEY,
    tenant_id         uuid NOT NULL,
    project_id        uuid NOT NULL,
    endpoint          text NOT NULL,
    idempotency_key   text NOT NULL,
    request_hash      char(64) NOT NULL,
    response_snapshot jsonb,
    resource_id       uuid,
    created_at        timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL DEFAULT now() + interval '24 hours',
    CONSTRAINT idempotency_key_scope_unique UNIQUE
        (tenant_id, project_id, endpoint, idempotency_key),
    CONSTRAINT idempotency_key_check CHECK (length(btrim(idempotency_key)) > 0),
    CONSTRAINT idempotency_hash_check CHECK (length(btrim(request_hash)) = 64)
);

CREATE INDEX idx_idempotency_expiry ON artifact.idempotency_record (expires_at);

-- ── ⑧ AuditEvent（追加式） ─────────────────────────────────────────────

CREATE TABLE artifact.audit_event (
    id            uuid PRIMARY KEY,
    tenant_id     uuid,
    project_id    uuid,
    actor_subject text,
    actor_type    text,
    action        text NOT NULL,
    object_type   text NOT NULL,
    object_id     uuid,
    outcome       text NOT NULL,
    error_code    text,
    detail        jsonb NOT NULL DEFAULT '{}'::jsonb,
    request_id    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT audit_outcome_check CHECK (outcome IN ('allowed', 'denied', 'error'))
);

CREATE INDEX idx_audit_scope_created
    ON artifact.audit_event (tenant_id, project_id, created_at DESC);
CREATE INDEX idx_audit_object ON artifact.audit_event (object_type, object_id);

-- append-only：数据库层最后防线（运行角色另行 REVOKE UPDATE/DELETE）。
CREATE FUNCTION artifact.guard_audit_append_only() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit_event is append-only' USING ERRCODE = 'P0001';
END $$;

CREATE TRIGGER trg_audit_append_only BEFORE UPDATE OR DELETE ON artifact.audit_event
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_audit_append_only();
