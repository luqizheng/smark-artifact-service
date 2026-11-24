-- 0002_job_coordination.sql — ART-03B-01 Job 输入快照、Attempt/Lease 与受控协调函数
--
-- 契约来源：docs/architecture/ART-03A-job-worker-svg-decisions-v1.md（A3A-01~05/08）。
-- 仅新增对象；不改写 0001_init.sql 的任何一行。
--
-- 安全模型：
--   * 所有 Worker 入口为 SECURITY DEFINER 函数，owner = migrator（schema owner）；
--     search_path 固定为 pg_catalog, artifact，函数体内全部对象显式限定 schema。
--   * PUBLIC 对所有函数 REVOKE EXECUTE；仅 deploy/db-roles.sql 显式授权 Worker role。
--   * Worker role 不获得 generation_job / generation_job_attempt / 任何业务表的 DML 权限；
--     其唯一写路径是本文件中的受控函数。
--   * 租约 token 明文只在 claim 返回值出现一次；数据库仅保存 sha256(token)。

-- ── ⓪ Mermaid seed 清理（A3A-08；绝不改写 0001） ────────────────────
-- DELETE 天然幂等：行不存在时影响 0 行，重复执行不会报错。
-- SeeCode seed 保留，状态仍为 unavailable（Adapter 未实现，未开放生产路径）。
DELETE FROM artifact.generator_registry WHERE generator_id = 'mermaid';

-- ── ① Job 输入快照与协调字段 ──────────────────────────────────────────

ALTER TABLE artifact.generation_job
    ADD COLUMN input_payload       jsonb,
    ADD COLUMN options_payload     jsonb,
    ADD COLUMN input_schema_version text,
    ADD COLUMN input_sha256        char(64),
    ADD COLUMN input_expires_at    timestamptz,
    ADD COLUMN cancel_requested_at timestamptz;

-- 输入 envelope 256 KiB 上限的第二道防线（应用层为第一道）。
-- pg_column_size 计量 JSONB 实际存储，含内部开销，故直接约束字节数。
ALTER TABLE artifact.generation_job ADD CONSTRAINT generation_job_input_size_check
    CHECK (
        (input_payload IS NULL OR pg_column_size(input_payload) <= 262144)
        AND (options_payload IS NULL OR pg_column_size(options_payload) <= 262144)
    );
ALTER TABLE artifact.generation_job ADD CONSTRAINT generation_job_input_sha_check
    CHECK (input_sha256 IS NULL OR length(btrim(input_sha256)) = 64);
ALTER TABLE artifact.generation_job ADD CONSTRAINT generation_job_input_pair_check
    CHECK (
        (input_payload IS NULL AND options_payload IS NULL)
        OR input_payload IS NOT NULL
    );
-- 终态清理期限只能由受控函数在设置 completed_at 时一并写入；
-- cancel_requested_at 与 status 不互相覆盖（running 取消只登记请求）。

CREATE INDEX idx_generation_job_input_expiry
    ON artifact.generation_job (input_expires_at)
    WHERE input_expires_at IS NOT NULL;
CREATE INDEX idx_generation_job_cancel_requested
    ON artifact.generation_job (cancel_requested_at)
    WHERE cancel_requested_at IS NOT NULL;

-- ── ② Attempt ──────────────────────────────────────────────────────────

CREATE TABLE artifact.generation_job_attempt (
    id                uuid PRIMARY KEY,
    job_id            uuid NOT NULL
        REFERENCES artifact.generation_job (id) ON DELETE RESTRICT,
    attempt_number    integer NOT NULL,
    worker_id         text NOT NULL,
    status            text NOT NULL DEFAULT 'claimed',
    lease_token_hash  char(64),
    lease_expires_at  timestamptz,
    started_at        timestamptz NOT NULL DEFAULT now(),
    heartbeat_at      timestamptz,
    completed_at      timestamptz,
    error_code        text,
    diagnostic_summary text,
    output_sha256     char(64),
    created_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT generation_job_attempt_number_unique UNIQUE (job_id, attempt_number),
    CONSTRAINT generation_job_attempt_number_check CHECK (attempt_number >= 1),
    CONSTRAINT generation_job_attempt_worker_check CHECK (length(btrim(worker_id)) > 0),
    CONSTRAINT generation_job_attempt_status_check CHECK (status IN
        ('claimed', 'running', 'succeeded', 'failed', 'cancelled', 'timed_out', 'lease_expired')),
    CONSTRAINT generation_job_attempt_token_hash_check
        CHECK (lease_token_hash IS NULL OR length(btrim(lease_token_hash)) = 64),
    CONSTRAINT generation_job_attempt_output_sha_check
        CHECK (output_sha256 IS NULL OR length(btrim(output_sha256)) = 64),
    CONSTRAINT generation_job_attempt_diagnostic_len_check
        CHECK (diagnostic_summary IS NULL OR length(diagnostic_summary) <= 2048)
);

-- 同一 Job 同时最多一个有效执行 Attempt；失效 Attempt 不占用该唯一位置。
CREATE UNIQUE INDEX uq_generation_job_active_attempt
    ON artifact.generation_job_attempt (job_id)
    WHERE status IN ('claimed', 'running');
CREATE INDEX idx_generation_job_attempt_lease_expiry
    ON artifact.generation_job_attempt (lease_expires_at)
    WHERE status IN ('claimed', 'running');
CREATE INDEX idx_generation_job_attempt_job
    ON artifact.generation_job_attempt (job_id, attempt_number DESC);

-- Attempt 归属不可变；终态不可离开。任何绕过受控函数的直接写都将被拒绝。
CREATE FUNCTION artifact.guard_attempt_transition() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog, artifact AS $$
BEGIN
    IF NEW.job_id <> OLD.job_id OR NEW.attempt_number <> OLD.attempt_number THEN
        RAISE EXCEPTION 'attempt ownership is immutable' USING ERRCODE = 'P0001';
    END IF;
    IF NEW.status <> OLD.status AND NOT (
        (OLD.status = 'claimed' AND NEW.status IN ('running', 'failed', 'cancelled', 'timed_out', 'lease_expired'))
        OR (OLD.status = 'running' AND NEW.status IN ('succeeded', 'failed', 'cancelled', 'timed_out', 'lease_expired'))
    ) THEN
        RAISE EXCEPTION 'invalid attempt transition % -> %', OLD.status, NEW.status
            USING ERRCODE = 'P0001';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER trg_generation_job_attempt_transition
    BEFORE UPDATE ON artifact.generation_job_attempt
    FOR EACH ROW EXECUTE FUNCTION artifact.guard_attempt_transition();

-- ── ③ 受控函数：输入快照创建（API role 使用，仍不开放生产生成成功路径） ──

-- 入参已由应用层完成 schema/深度/结构/字节校验；此处再校验字节上限，
-- 且 Job 与输入在同一 INSERT 语句/事务中落地。
CREATE FUNCTION artifact.create_generation_job_with_input(
    p_job_id             uuid,
    p_tenant_id          uuid,
    p_project_id         uuid,
    p_artifact_id        uuid,
    p_generator_id       text,
    p_input_payload      jsonb,
    p_options_payload    jsonb,
    p_input_schema_version text,
    p_input_sha256       char(64),
    p_request_hash       char(64),
    p_idempotency_key    text,
    p_max_attempts       integer DEFAULT 3
) RETURNS uuid
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_id uuid;
BEGIN
    IF pg_column_size(p_input_payload) > 262144
       OR pg_column_size(coalesce(p_options_payload, '{}'::jsonb)) > 262144 THEN
        RAISE EXCEPTION 'INPUT_TOO_LARGE' USING ERRCODE = '22001';
    END IF;
    IF length(btrim(p_input_sha256)) <> 64 OR length(btrim(p_request_hash)) <> 64 THEN
        RAISE EXCEPTION 'INVALID_INPUT_HASH' USING ERRCODE = '22023';
    END IF;
    IF coalesce(p_max_attempts, 0) < 1 OR p_max_attempts > 10 THEN
        RAISE EXCEPTION 'INVALID_MAX_ATTEMPTS' USING ERRCODE = '22023';
    END IF;
    INSERT INTO artifact.generation_job
        (id, tenant_id, project_id, artifact_id, generator_id,
         input_payload, options_payload, input_schema_version, input_sha256,
         request_hash, idempotency_key, max_attempts, status)
    VALUES
        (p_job_id, p_tenant_id, p_project_id, p_artifact_id, p_generator_id,
         p_input_payload, p_options_payload, p_input_schema_version, p_input_sha256,
         p_request_hash, p_idempotency_key, p_max_attempts, 'queued')
    RETURNING id INTO v_id;
    RETURN v_id;
END $$;

-- ── ④ 受控函数：Worker 领取 ────────────────────────────────────────────

-- 返回输入正文（仅此函数可读），lease token 明文仅在结果中返回一次。
CREATE FUNCTION artifact.claim_generation_job(
    p_worker_id   text,
    p_lease_seconds integer DEFAULT 30
) RETURNS TABLE (
    job_id               uuid,
    attempt_id           uuid,
    attempt_number       integer,
    lease_token          text,
    lease_expires_at     timestamptz,
    input_payload        jsonb,
    options_payload      jsonb,
    input_schema_version text,
    input_sha256         char(64),
    generator_id         text,
    generator_version    text,
    adapter_version      text,
    max_attempts         integer
)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_job artifact.generation_job%ROWTYPE;
    v_token text;
    v_attempt_id uuid;
    v_next_attempt integer;
    v_expires timestamptz;
BEGIN
    IF coalesce(length(btrim(p_worker_id)), 0) = 0 THEN
        RAISE EXCEPTION 'INVALID_WORKER_ID' USING ERRCODE = '22023';
    END IF;
    IF p_lease_seconds < 1 OR p_lease_seconds > 3600 THEN
        RAISE EXCEPTION 'INVALID_LEASE_SECONDS' USING ERRCODE = '22023';
    END IF;

    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j
      JOIN artifact.generator_registry AS g
        ON g.generator_id = j.generator_id
     WHERE j.status IN ('queued', 'retry_wait')
       AND j.cancel_requested_at IS NULL
       AND j.attempt_count < j.max_attempts
       AND j.input_payload IS NOT NULL
       AND g.status = 'available'
     ORDER BY j.created_at, j.id
     FOR UPDATE OF j SKIP LOCKED
     LIMIT 1;

    IF NOT FOUND THEN
        RETURN;
    END IF;

    -- 0001 状态机白名单没有 retry_wait → running，必须经过 queued 中转。
    IF v_job.status = 'retry_wait' THEN
        UPDATE artifact.generation_job SET status = 'queued' WHERE id = v_job.id;
    END IF;

    v_next_attempt := v_job.attempt_count + 1;
    -- 两个随机 UUID 拼接为 72 字符高熵 token；只用 PG core 的 gen_random_uuid()，
    -- 不依赖 pgcrypto 扩展（避免 migrator 需要额外 CREATE EXTENSION 权限）。
    v_token := gen_random_uuid()::text || gen_random_uuid()::text;
    v_expires := clock_timestamp() + (p_lease_seconds || ' seconds')::interval;
    v_attempt_id := gen_random_uuid();

    INSERT INTO artifact.generation_job_attempt
        (id, job_id, attempt_number, worker_id, status,
         lease_token_hash, lease_expires_at, started_at, heartbeat_at)
    VALUES
        (v_attempt_id, v_job.id, v_next_attempt, p_worker_id, 'running',
         encode(sha256(convert_to(v_token, 'UTF8')), 'hex'), v_expires,
         clock_timestamp(), clock_timestamp());

    UPDATE artifact.generation_job
       SET status = 'running',
           attempt_count = v_next_attempt,
           started_at = coalesce(started_at, clock_timestamp()),
           heartbeat_at = clock_timestamp()
     WHERE id = v_job.id;

    job_id := v_job.id;
    attempt_id := v_attempt_id;
    attempt_number := v_next_attempt;
    lease_token := v_token;
    lease_expires_at := v_expires;
    input_payload := v_job.input_payload;
    options_payload := v_job.options_payload;
    input_schema_version := v_job.input_schema_version;
    input_sha256 := v_job.input_sha256;
    generator_id := v_job.generator_id;
    generator_version := v_job.generator_version;
    adapter_version := v_job.adapter_version;
    max_attempts := v_job.max_attempts;
    RETURN NEXT;
END $$;

-- ── ⑤ 受控函数：Heartbeat / Complete / Fail ────────────────────────────

-- 三要素必须同时匹配 job_id、attempt_id、lease token hash，且租约仍有效。
CREATE FUNCTION artifact.heartbeat_generation_job(
    p_job_id      uuid,
    p_attempt_id  uuid,
    p_lease_token text,
    p_lease_seconds integer DEFAULT 30
) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_job_status text;
BEGIN
    IF p_lease_seconds < 1 OR p_lease_seconds > 3600 THEN
        RAISE EXCEPTION 'INVALID_LEASE_SECONDS' USING ERRCODE = '22023';
    END IF;
    IF p_lease_token IS NULL OR length(p_lease_token) = 0 THEN
        RETURN false;
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;
    IF v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_expires_at IS NULL
       OR v_attempt.lease_expires_at <= clock_timestamp()
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN false;
    END IF;
    SELECT j.status INTO v_job_status
      FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF v_job_status IS DISTINCT FROM 'running' THEN
        RETURN false;
    END IF;

    UPDATE artifact.generation_job_attempt
       SET heartbeat_at = clock_timestamp(),
           lease_expires_at = clock_timestamp() + (p_lease_seconds || ' seconds')::interval
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET heartbeat_at = clock_timestamp()
     WHERE id = p_job_id;
    RETURN true;
END $$;

-- Complete：验证未取消 + 租约有效 + attempt 有效；终态与输入过期期限同事务写入。
CREATE FUNCTION artifact.complete_generation_job_attempt(
    p_job_id           uuid,
    p_attempt_id       uuid,
    p_lease_token      text,
    p_output_sha256    char(64)
) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_cancel  timestamptz;
BEGIN
    IF p_lease_token IS NULL OR length(p_lease_token) = 0 THEN
        RETURN false;
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;

    SELECT j.cancel_requested_at INTO v_cancel
      FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF v_cancel IS NOT NULL THEN
        RAISE EXCEPTION 'CANCEL_REQUESTED' USING ERRCODE = 'P0001';
    END IF;
    IF v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_expires_at IS NULL
       OR v_attempt.lease_expires_at <= clock_timestamp()
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN false;
    END IF;
    IF length(btrim(coalesce(p_output_sha256, ''))) <> 64 THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_SHA256' USING ERRCODE = '22023';
    END IF;

    UPDATE artifact.generation_job_attempt
       SET status = 'succeeded', completed_at = clock_timestamp(),
           heartbeat_at = clock_timestamp(), output_sha256 = p_output_sha256
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET status = 'succeeded', completed_at = clock_timestamp(),
           error_code = NULL,
           input_expires_at = clock_timestamp() + interval '7 days'
     WHERE id = p_job_id;
    RETURN true;
END $$;

-- Fail：可重试错误回落 retry_wait（受控恢复后重新排队），不可重试/超限进入 failed。
CREATE FUNCTION artifact.fail_generation_job_attempt(
    p_job_id            uuid,
    p_attempt_id        uuid,
    p_lease_token       text,
    p_error_code        text,
    p_diagnostic_summary text DEFAULT NULL,
    p_retryable         boolean DEFAULT false
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_job artifact.generation_job%ROWTYPE;
    v_next text;
BEGIN
    IF p_lease_token IS NULL OR length(p_lease_token) = 0 THEN
        RETURN 'stale_attempt';
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RETURN 'not_found';
    END IF;
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF v_job.cancel_requested_at IS NOT NULL THEN
        RAISE EXCEPTION 'CANCEL_REQUESTED' USING ERRCODE = 'P0001';
    END IF;
    IF v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_expires_at IS NULL
       OR v_attempt.lease_expires_at <= clock_timestamp()
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN 'stale_attempt';
    END IF;
    IF coalesce(length(btrim(p_error_code)), 0) = 0 THEN
        RAISE EXCEPTION 'INVALID_ERROR_CODE' USING ERRCODE = '22023';
    END IF;

    IF p_retryable AND v_job.attempt_count < v_job.max_attempts THEN
        v_next := 'retry_wait';
    ELSE
        v_next := 'failed';
    END IF;

    UPDATE artifact.generation_job_attempt
       SET status = 'failed',
           completed_at = clock_timestamp(), heartbeat_at = clock_timestamp(),
           error_code = p_error_code, diagnostic_summary = p_diagnostic_summary
     WHERE id = p_attempt_id;

    UPDATE artifact.generation_job
       SET status = v_next,
           error_code = p_error_code,
           error_message = NULL, -- 不写输入正文；仅稳定错误码
           completed_at = CASE WHEN v_next = 'failed' THEN clock_timestamp() ELSE NULL END,
           input_expires_at = CASE WHEN v_next = 'failed'
               THEN clock_timestamp() + interval '7 days' ELSE NULL END
     WHERE id = p_job_id;
    RETURN v_next;
END $$;

-- ── ⑥ 受控函数：取消协调 ──────────────────────────────────────────────

-- running 只登记取消请求，不伪造 Worker 已停止；queued/retry_wait 可直接取消。
CREATE FUNCTION artifact.request_generation_job_cancel(
    p_job_id uuid
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_job artifact.generation_job%ROWTYPE;
    v_result text;
BEGIN
    SELECT j.* INTO v_job FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF NOT FOUND THEN
        RETURN 'not_found';
    END IF;
    IF v_job.status IN ('succeeded', 'failed', 'cancelled', 'timed_out') THEN
        RETURN 'terminal';
    END IF;

    IF v_job.status IN ('queued', 'retry_wait') THEN
        UPDATE artifact.generation_job
           SET status = 'cancelled', completed_at = clock_timestamp(),
               error_code = coalesce(error_code, 'cancelled_by_request'),
               input_expires_at = clock_timestamp() + interval '7 days'
         WHERE id = p_job_id;
        v_result := 'cancelled';
    ELSE
        -- running：仅登记请求。Job 仍为 running，Attempt 仍为 running，
        -- Worker heartbeat/完成前检查该字段并自行终止；最终态由 Worker 确认或恢复扫描裁决。
        UPDATE artifact.generation_job
           SET cancel_requested_at = coalesce(cancel_requested_at, clock_timestamp())
         WHERE id = p_job_id;
        v_result := 'cancel_requested';
    END IF;
    RETURN v_result;
END $$;

-- Worker 确认已停止后，才将 Job/Attempt 置为 cancelled（三要素匹配）。
CREATE FUNCTION artifact.confirm_generation_job_cancelled(
    p_job_id      uuid,
    p_attempt_id  uuid,
    p_lease_token text
) RETURNS boolean
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_cancel timestamptz;
BEGIN
    IF p_lease_token IS NULL OR length(p_lease_token) = 0 THEN
        RETURN false;
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id FOR UPDATE;
    IF NOT FOUND THEN
        RETURN false;
    END IF;
    SELECT j.cancel_requested_at INTO v_cancel
      FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF v_cancel IS NULL THEN
        RETURN false;
    END IF;
    IF v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN false;
    END IF;

    UPDATE artifact.generation_job_attempt
       SET status = 'cancelled', completed_at = clock_timestamp()
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET status = 'cancelled', completed_at = clock_timestamp(),
           error_code = coalesce(error_code, 'cancelled_by_worker'),
           input_expires_at = clock_timestamp() + interval '7 days'
     WHERE id = p_job_id;
    RETURN true;
END $$;

-- ── ⑦ 受控函数：恢复与清理 ────────────────────────────────────────────

-- 租约过期的 Attempt 一律 lease_expired，不复用；Job 按 attempt 上限转 retry_wait 或 timed_out。
-- 只处理 Attempt 的 claimed/running 状态，天然幂等，不会无限重试，终态 Job 不会被触及。
CREATE FUNCTION artifact.recover_expired_generation_jobs()
RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt_id uuid;
    v_job_id uuid;
    v_job artifact.generation_job%ROWTYPE;
    v_count integer := 0;
BEGIN
    FOR v_attempt_id, v_job_id IN
        SELECT a.id, a.job_id
          FROM artifact.generation_job_attempt AS a
         WHERE a.status IN ('claimed', 'running')
           AND a.lease_expires_at IS NOT NULL
           AND a.lease_expires_at <= clock_timestamp()
         ORDER BY a.lease_expires_at
         FOR UPDATE OF a SKIP LOCKED
    LOOP
        UPDATE artifact.generation_job_attempt
           SET status = 'lease_expired', completed_at = clock_timestamp(),
               error_code = coalesce(error_code, 'LEASE_EXPIRED')
         WHERE id = v_attempt_id;

        SELECT j.* INTO v_job
          FROM artifact.generation_job AS j WHERE j.id = v_job_id FOR UPDATE;
        IF v_job.status = 'running' THEN
            IF v_job.cancel_requested_at IS NOT NULL THEN
                UPDATE artifact.generation_job
                   SET status = 'cancelled', completed_at = clock_timestamp(),
                       error_code = coalesce(error_code, 'cancelled_by_request'),
                       input_expires_at = clock_timestamp() + interval '7 days'
                 WHERE id = v_job_id;
            ELSIF v_job.attempt_count < v_job.max_attempts THEN
                -- running → retry_wait；下一次 claim 会经 queued 中转重新领取（新 Attempt）。
                UPDATE artifact.generation_job SET status = 'retry_wait' WHERE id = v_job_id;
            ELSE
                UPDATE artifact.generation_job
                   SET status = 'timed_out', completed_at = clock_timestamp(),
                       error_code = coalesce(error_code, 'MAX_ATTEMPTS_EXCEEDED'),
                       input_expires_at = clock_timestamp() + interval '7 days'
                 WHERE id = v_job_id;
            END IF;
        END IF;
        v_count := v_count + 1;
    END LOOP;
    RETURN v_count;
END $$;

-- 终态 7 天后清理输入正文；保留 sha256、schema version 与执行元数据。
CREATE FUNCTION artifact.purge_expired_generation_job_input()
RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_count integer;
BEGIN
    WITH purged AS (
        UPDATE artifact.generation_job
           SET input_payload = NULL, options_payload = NULL
         WHERE status IN ('succeeded', 'failed', 'cancelled', 'timed_out')
           AND input_expires_at IS NOT NULL
           AND input_expires_at <= clock_timestamp()
           AND (input_payload IS NOT NULL OR options_payload IS NOT NULL)
        RETURNING 1
    )
    SELECT count(*) INTO v_count FROM purged;
    RETURN v_count;
END $$;

-- ── ⑧ 默认拒绝：PUBLIC 不获得任何函数 EXECUTE ─────────────────────────

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA artifact FROM PUBLIC;
