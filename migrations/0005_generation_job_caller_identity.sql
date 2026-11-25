-- 0005_generation_job_caller_identity.sql — AS-2：GenerationJob Durable Caller Identity
--
-- 契约来源（唯一规范性依据）：docs/architecture/ART-04G-independent-review-as2-generation-job-caller-identity.md
--   §5.1 数据模型 / §5.3 创建事务与并发方案 / §9.1 C-1 门禁 / §9.4 C-4 权限信任边界
--   §10 = ART-04I 第三轮挑战后的 v3 冻结修订（优先级高于 §§5–9）：
--     §10.1 C-6 key 规范化先于一切 replay 分支；§10.5 P3-4 权限边界；§10.6 54 项矩阵。
-- 0001–0004 不改一字（CI 与测试按 migrations.sha256 校验）；本迁移为加法式。
--
-- 交付内容：
--   ① generation_job 新增 caller_request_id / request_fingerprint / fingerprint_version 三列；
--   ② CHECK：三列同空或同非空、version>0、fingerprint 为小写 64 位十六进制 SHA-256；
--   ③ 部分唯一索引 (tenant_id, project_id, caller_request_id) WHERE caller_request_id IS NOT NULL；
--   ④ guard_job_transition 触发器函数追加：三列永久不可变（含 NULL→非 NULL 补录拒绝）；
--   ⑤ key holder 硬门禁 helper（读前查与 unique_violation 异常块共用同一实现，§10.1.3）；
--   ⑥ create_generation_job_with_input v2（16 参，返回 (job_id, resolution)）：
--      caller identity 解析 + key 冲突硬门禁 + 并发仲裁；v3 §5.3.1 冻结执行顺序：
--      key 规范化/校验最先（C-6）→ 全量输入校验 → caller 三元校验 → caller 解析+门禁
--      → key 解析 → generator 检查（在一切可判定 replay 之后）→ input_sha256 → INSERT
--      → 异常块（新语句快照 caller 重读 → 同一门禁 → key 重读）。
--   ⑦ 历史清点（不回填）：Legacy Job 三列保持 NULL，不伪造历史 caller_request_id。
--
-- 信任边界（ART-04G v3 §9.4，显式声明，不虚构数据库已完成授权）：
--   本函数无法在数据库内部验证 HTTP 层授权。p_tenant_id / p_project_id 必须由
--   Application 层从经过验证的授权上下文解析（delegation token → tenant；
--   X-Project-Id → project）后传入，不得取自请求体 scope。数据库层只负责
--   scope 内的配对校验、唯一性仲裁、不可变性与事务收敛。身份内容正确性的
--   唯一边界是 Rust typed DTO canonicalizer（§9.4.1）。
--
-- 迁移之后必须重新执行 deploy/db-roles.sql（DROP/CREATE 会丢失函数级 EXECUTE 授权）。

-- ── ① 三列（additive） ───────────────────────────────────────────────────

ALTER TABLE artifact.generation_job
    ADD COLUMN caller_request_id   uuid,
    ADD COLUMN request_fingerprint char(64),
    ADD COLUMN fingerprint_version smallint;

-- ── ② 三元配对 + 格式不变量（数据库层保证，不只依赖 Rust） ───────────────

ALTER TABLE artifact.generation_job
    ADD CONSTRAINT generation_job_caller_identity_pairing_check CHECK (
        (caller_request_id IS NULL
             AND request_fingerprint IS NULL
             AND fingerprint_version IS NULL)
        OR (caller_request_id IS NOT NULL
             AND request_fingerprint IS NOT NULL
             AND fingerprint_version IS NOT NULL
             AND fingerprint_version > 0
             AND request_fingerprint ~ '^[0-9a-f]{64}$')
    );

-- ── ③ 部分唯一索引：同 scope 内一个 caller_request_id 至多绑定一个 Job ───

CREATE UNIQUE INDEX uq_generation_job_caller_request
    ON artifact.generation_job (tenant_id, project_id, caller_request_id)
    WHERE caller_request_id IS NOT NULL;

-- ── ④ 触发器函数加固：身份三列永久不可变（含 NULL→非 NULL 的普通 UPDATE） ─
-- CREATE OR REPLACE（签名不变，既有触发器引用保持有效）。

CREATE OR REPLACE FUNCTION artifact.guard_job_transition() RETURNS trigger LANGUAGE plpgsql AS $$
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
    -- AS-2（ART-04G v3 §5.1）：caller identity 三列一旦写入永久不可变。
    -- 包括从 legacy NULL 身份补录非 NULL 值的一切普通 UPDATE。
    IF NEW.caller_request_id   IS DISTINCT FROM OLD.caller_request_id
        OR NEW.request_fingerprint IS DISTINCT FROM OLD.request_fingerprint
        OR NEW.fingerprint_version IS DISTINCT FROM OLD.fingerprint_version THEN
        RAISE EXCEPTION 'generation_job caller identity columns are immutable'
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;

-- ── ⑤ key holder 硬门禁 helper（§9.1 / §10.1.3：step 2 与 step 7 共用同一实现，
--      防止两处门禁语义漂移）。v_key IS NULL ⇒ 未提供 key，门禁跳过。
--      不授予任何运行角色；仅在 SECURITY DEFINER 创建函数内部调用。

CREATE FUNCTION artifact.assert_generation_job_key_gate(
    p_tenant_id       uuid,
    p_project_id      uuid,
    p_key             text,
    p_request_hash    text,
    p_existing_job_id uuid
) RETURNS void
LANGUAGE plpgsql SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_holder artifact.generation_job%ROWTYPE;
BEGIN
    IF p_key IS NULL THEN
        RETURN;
    END IF;
    -- 单条 SELECT，READ COMMITTED 语句开始时取快照：一切已提交绑定必然可见
    -- （ART-04I §3.3 论证 1）。gate 通过后的并发绑定属语句快照时点语义下的
    -- 合法串行序（§10.2 P3-1），不得在调用方加锁"修复"。
    SELECT * INTO v_holder
      FROM artifact.generation_job
     WHERE tenant_id = p_tenant_id
       AND project_id = p_project_id
       AND idempotency_key = p_key;
    IF FOUND AND v_holder.id <> p_existing_job_id THEN
        IF v_holder.request_hash <> p_request_hash THEN
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED' USING ERRCODE = '23505';
        END IF;
        -- hash 相同但绑定他行：结构不变量冲突（该分支经 HTTP 结构上不可达——
        -- 同 hash ⇒ 同 body ⇒ 同 caller_request_id ⇒ 同一行；保留 fail-closed 防御）。
        RAISE EXCEPTION 'IDEMPOTENCY_KEY_CROSS_IDENTITY' USING ERRCODE = 'P0001';
    END IF;
END $$;

-- ── ⑥ 创建函数 v2：签名变化（新增 caller 三元参数、返回 resolution），先 DROP ─

DROP FUNCTION artifact.create_generation_job_with_input(
    uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, text, text,
    integer, integer, integer);

CREATE FUNCTION artifact.create_generation_job_with_input(
    p_job_id                 uuid,
    p_tenant_id              uuid,
    p_project_id             uuid,
    p_artifact_id            uuid,
    p_generator_id           text,
    p_input_payload          jsonb,
    p_options_payload        jsonb,
    p_input_schema_version   text,
    p_request_hash           text,
    p_idempotency_key        text,
    p_caller_request_id      uuid DEFAULT NULL,
    p_request_fingerprint    text DEFAULT NULL,
    p_fingerprint_version    smallint DEFAULT NULL,
    p_max_attempts           integer DEFAULT 3,
    p_max_execution_seconds  integer DEFAULT 300,
    p_queue_ttl_seconds      integer DEFAULT 86400
) RETURNS TABLE (job_id uuid, resolution text)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now       timestamptz := clock_timestamp();
    v_key       text;
    v_existing  artifact.generation_job%ROWTYPE;
    v_generator artifact.generator_registry%ROWTYPE;
    v_input_sha text;
BEGIN
    -- ── step 1：key 规范化与长度校验必须最先执行（v3 §10.1 C-6：早于一切
    --    caller lookup / key lookup / replay return / 异常块 replay 处理）。
    --    btrim(text) 默认仅剥离 ASCII SP U+0020（0003 原语义，不得扩展为
    --    HTAB/CR/LF 或 Unicode whitespace）；HTTP adapter 的 OWS 处理与数据库
    --    btrim 规范化分开处理。
    v_key := nullif(btrim(p_idempotency_key), '');
    IF v_key IS NOT NULL AND length(v_key) > 255 THEN
        RAISE EXCEPTION 'INVALID_IDEMPOTENCY_KEY' USING ERRCODE = '22023';
    END IF;

    -- ── step 1（续）：现有全量输入校验逐字保留（0003 L294-323）。
    IF p_job_id IS NULL OR p_tenant_id IS NULL OR p_project_id IS NULL
       OR p_artifact_id IS NULL OR coalesce(length(btrim(p_generator_id)), 0) = 0 THEN
        RAISE EXCEPTION 'INVALID_JOB_REQUEST' USING ERRCODE = '22023';
    END IF;
    IF p_input_payload IS NULL OR jsonb_typeof(p_input_payload) <> 'object'
       OR (p_options_payload IS NOT NULL AND jsonb_typeof(p_options_payload) <> 'object') THEN
        RAISE EXCEPTION 'INVALID_INPUT_PAYLOAD' USING ERRCODE = '22023';
    END IF;
    IF coalesce(length(btrim(p_input_schema_version)), 0) = 0
       OR length(p_input_schema_version) > 32 THEN
        RAISE EXCEPTION 'INVALID_INPUT_SCHEMA_VERSION' USING ERRCODE = '22023';
    END IF;
    IF p_request_hash IS NULL OR p_request_hash !~ '^[0-9a-f]{64}$' THEN
        RAISE EXCEPTION 'INVALID_REQUEST_HASH' USING ERRCODE = '22023';
    END IF;
    IF p_max_attempts IS NULL OR p_max_attempts < 1 OR p_max_attempts > 10 THEN
        RAISE EXCEPTION 'INVALID_MAX_ATTEMPTS' USING ERRCODE = '22023';
    END IF;
    IF p_max_execution_seconds IS NULL
       OR p_max_execution_seconds < 1 OR p_max_execution_seconds > 3600 THEN
        RAISE EXCEPTION 'INVALID_MAX_EXECUTION_SECONDS' USING ERRCODE = '22023';
    END IF;
    IF p_queue_ttl_seconds IS NULL
       OR p_queue_ttl_seconds < 1 OR p_queue_ttl_seconds > 604800 THEN
        RAISE EXCEPTION 'INVALID_QUEUE_TTL' USING ERRCODE = '22023';
    END IF;
    IF pg_column_size(p_input_payload) > 262144
       OR pg_column_size(coalesce(p_options_payload, '{}'::jsonb)) > 262144 THEN
        RAISE EXCEPTION 'INPUT_TOO_LARGE' USING ERRCODE = '22001';
    END IF;

    -- ── step 1（续）：caller 三元配对与格式校验（三者同空或同非空，DB CHECK 双保险）。
    IF p_caller_request_id IS NOT NULL THEN
        IF p_request_fingerprint IS NULL OR p_fingerprint_version IS NULL
           OR p_request_fingerprint !~ '^[0-9a-f]{64}$'
           OR p_fingerprint_version < 1 THEN
            RAISE EXCEPTION 'INVALID_JOB_REQUEST' USING ERRCODE = '22023';
        END IF;
    ELSIF p_request_fingerprint IS NOT NULL OR p_fingerprint_version IS NOT NULL THEN
        RAISE EXCEPTION 'INVALID_JOB_REQUEST' USING ERRCODE = '22023';
    END IF;

    -- ── step 2：caller identity 解析 + Idempotency-Key 冲突硬门禁（§9.1 / §10.1）。
    --    优先级冻结：version → fingerprint → key holder gate。
    --    重放分支不重评 generator 可用性与乐观前置条件（replay 零写入）。
    IF p_caller_request_id IS NOT NULL THEN
        SELECT * INTO v_existing
          FROM artifact.generation_job
         WHERE tenant_id = p_tenant_id
           AND project_id = p_project_id
           AND caller_request_id = p_caller_request_id;
        IF FOUND THEN
            IF v_existing.fingerprint_version <> p_fingerprint_version THEN
                RAISE EXCEPTION 'CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH'
                    USING ERRCODE = 'P0001';
            END IF;
            IF v_existing.request_fingerprint::text <> p_request_fingerprint THEN
                RAISE EXCEPTION 'CALLER_REQUEST_ID_REUSED' USING ERRCODE = '23505';
            END IF;
            -- 不得在成功 replay 前跳过 key gate（与异常块共用同一 helper）。
            PERFORM artifact.assert_generation_job_key_gate(
                p_tenant_id, p_project_id, v_key, p_request_hash, v_existing.id);
            RETURN QUERY SELECT v_existing.id, 'caller_request_replay'::text;
            RETURN;
        END IF;
    END IF;

    -- ── step 3：Idempotency-Key 解析（仅 caller 未命中时；v3 §10.1.3 禁用原始参数）。
    IF v_key IS NOT NULL THEN
        SELECT * INTO v_existing
          FROM artifact.generation_job
         WHERE tenant_id = p_tenant_id AND project_id = p_project_id AND idempotency_key = v_key;
        IF FOUND THEN
            IF v_existing.request_hash = p_request_hash THEN
                RETURN QUERY SELECT v_existing.id, 'idempotency_replay'::text;
                RETURN;
            END IF;
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED' USING ERRCODE = '23505';
        END IF;
    END IF;

    -- ── step 4：generator registry 检查（v3 §5.3.1：必须位于一切可判定 replay 之后；
    --    已存在的合法 Job 即使 generator 当前 unavailable 也必须可 replay）。
    SELECT * INTO v_generator
      FROM artifact.generator_registry WHERE generator_id = p_generator_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'UNKNOWN_GENERATOR' USING ERRCODE = '22023';
    END IF;
    -- 生成器不可用时不入队：排队的 Job 也不会被领取，API 应直接返回 503。
    IF v_generator.status <> 'available' THEN
        RAISE EXCEPTION 'GENERATOR_UNAVAILABLE' USING ERRCODE = 'P0001';
    END IF;

    -- ── step 5：input_sha256（原逻辑不变）。
    v_input_sha := encode(sha256(convert_to(
        p_input_schema_version || E'\n'
        || p_input_payload::text || E'\n'
        || coalesce(p_options_payload, '{}'::jsonb)::text, 'UTF8')), 'hex');

    -- ── step 6：单条 INSERT（caller 为空时三列为 NULL；identity 写入仅此受控路径）。
    BEGIN
        INSERT INTO artifact.generation_job
            (id, tenant_id, project_id, artifact_id, generator_id,
             generator_version, adapter_version,
             input_payload, options_payload, input_schema_version, input_sha256,
             request_hash, idempotency_key,
             caller_request_id, request_fingerprint, fingerprint_version,
             max_attempts, max_execution_seconds,
             status, expires_at)
        VALUES
            (p_job_id, p_tenant_id, p_project_id, p_artifact_id, p_generator_id,
             v_generator.version, v_generator.adapter_version,
             p_input_payload, p_options_payload, p_input_schema_version, v_input_sha,
             p_request_hash, v_key,
             p_caller_request_id, p_request_fingerprint, p_fingerprint_version,
             p_max_attempts, p_max_execution_seconds,
             'queued', v_now + make_interval(secs => p_queue_ttl_seconds::double precision));
    EXCEPTION WHEN unique_violation THEN
        -- ── step 7（ART-04I §3.3 时序 C/E/F；§10.3）：
        --    子事务已自动回滚；以下每条语句都在违例点之后取新语句快照（READ COMMITTED），
        --    并发胜者必然可见（绑定只增不改不消，重读结果无"过期"问题）。
        --    与 step 2 完全相同的 caller 判定 + key holder gate（共用 helper，不绕过）。
        IF p_caller_request_id IS NOT NULL THEN
            SELECT * INTO v_existing
              FROM artifact.generation_job
             WHERE tenant_id = p_tenant_id
               AND project_id = p_project_id
               AND caller_request_id = p_caller_request_id;
            IF FOUND THEN
                IF v_existing.fingerprint_version <> p_fingerprint_version THEN
                    RAISE EXCEPTION 'CALLER_REQUEST_ID_FINGERPRINT_VERSION_MISMATCH'
                        USING ERRCODE = 'P0001';
                END IF;
                IF v_existing.request_fingerprint::text <> p_request_fingerprint THEN
                    RAISE EXCEPTION 'CALLER_REQUEST_ID_REUSED' USING ERRCODE = '23505';
                END IF;
                PERFORM artifact.assert_generation_job_key_gate(
                    p_tenant_id, p_project_id, v_key, p_request_hash, v_existing.id);
                RETURN QUERY SELECT v_existing.id, 'caller_request_replay'::text;
                RETURN;
            END IF;
        END IF;
        IF v_key IS NULL THEN
            RAISE;
        END IF;
        SELECT * INTO v_existing
          FROM artifact.generation_job
         WHERE tenant_id = p_tenant_id AND project_id = p_project_id AND idempotency_key = v_key;
        IF NOT FOUND THEN
            RAISE;
        END IF;
        IF v_existing.request_hash = p_request_hash THEN
            -- 防御条款（结构上经 HTTP 不可达，注释声明）：同 hash ⇒ 同 body ⇒ 同
            -- caller_request_id；胜者 caller 与传入不同（含 NULL/非 NULL 组合）即
            -- 结构不变量破坏，fail-closed，绝不返回跨身份重放。
            IF p_caller_request_id IS NOT NULL
               AND v_existing.caller_request_id IS DISTINCT FROM p_caller_request_id THEN
                RAISE EXCEPTION 'IDEMPOTENCY_KEY_CROSS_IDENTITY' USING ERRCODE = 'P0001';
            END IF;
            RETURN QUERY SELECT v_existing.id, 'idempotency_replay'::text;
            RETURN;
        END IF;
        RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED' USING ERRCODE = '23505';
    END;

    -- ── step 8：正常路径。
    RETURN QUERY SELECT p_job_id, 'created'::text;
END $$;

-- ── ⑦ 历史清点（§5.7 / §10.7：不回填、不伪造历史 caller_request_id） ─────

DO $$
DECLARE
    v_total         bigint;
    v_with_identity bigint;
    v_keyed         bigint;
BEGIN
    SELECT count(*) INTO v_total FROM artifact.generation_job;
    SELECT count(*) INTO v_with_identity
      FROM artifact.generation_job WHERE caller_request_id IS NOT NULL;
    SELECT count(*) INTO v_keyed
      FROM artifact.generation_job WHERE idempotency_key IS NOT NULL;
    RAISE NOTICE '0005 caller identity inventory: % job(s) total, % already carry caller identity (expected 0), % idempotency-keyed job(s) preserved untouched',
        v_total, v_with_identity, v_keyed;
END $$;

-- ── ⑧ 默认拒绝：新函数不获得 PUBLIC 的 EXECUTE（运行授权由 deploy/db-roles.sql
--      按 §10.5 精确授予） ────────────────────────────────────────────────

REVOKE EXECUTE ON FUNCTION artifact.assert_generation_job_key_gate(uuid, uuid, text, text, uuid) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION artifact.create_generation_job_with_input(
    uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, text, text,
    uuid, text, smallint, integer, integer, integer) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION artifact.guard_job_transition() FROM PUBLIC;
