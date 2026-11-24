-- 0003_job_coordination_fixes.sql — ART-03B-01 评审修复：租约、重试、超时、取消与一致性
--
-- 契约来源：docs/architecture/ART-03A-job-worker-svg-decisions-v1.md §11（评审后补充裁决）。
-- 0001 / 0002 不改一字（CI 与测试按 migrations.sha256 校验）。
--
-- 本迁移取代 0002 中的全部协调函数。签名或返回类型变化的函数先 DROP 再 CREATE；
-- DROP 会连同 EXECUTE 授权一起丢失，所以迁移之后必须重新执行 deploy/db-roles.sql。
--
-- 修复的问题：
--   1. lease_seconds 为 NULL 时产生永不过期、也无法恢复或取消的 running Job；
--   2. retry_wait 没有退避，暂时性故障会瞬间耗尽 attempt；
--   3. heartbeat 可以无限续租，没有最大执行时长；
--   4. 运行中的 Worker 无法感知取消请求；
--   5. complete 可以在没有任何 READY Version / Output 的情况下把 Job 置为 succeeded；
--   6. Job 创建不固定 generator / adapter 版本、没有幂等约束、接受空输入、排队无时限；
--   7. Worker 侧的状态演进不写审计；claim 排序没有索引支撑；错误码没有约束。

-- ── ① 结构：新增列 ─────────────────────────────────────────────────────

ALTER TABLE artifact.generation_job
    ADD COLUMN next_attempt_at       timestamptz,
    ADD COLUMN expires_at            timestamptz,
    ADD COLUMN max_execution_seconds integer NOT NULL DEFAULT 300;

ALTER TABLE artifact.generation_job_attempt
    ADD COLUMN deadline_at timestamptz;

-- ── ② 一次性修复 0002 已造成的不一致数据 ──────────────────────────────

-- 租约为 NULL 的活跃 Attempt（0002 的 NULL lease_seconds 缺陷）：视为租约已失效。
UPDATE artifact.generation_job_attempt
   SET status = 'lease_expired',
       completed_at = clock_timestamp(),
       error_code = coalesce(error_code, 'LEASE_EXPIRED')
 WHERE status IN ('claimed', 'running')
   AND lease_expires_at IS NULL;

-- 没有任何活跃 Attempt 却仍是 running 的 Job：还能重试的回到队列，其余收尾。
UPDATE artifact.generation_job
   SET status = 'retry_wait',
       next_attempt_at = clock_timestamp() + interval '5 seconds',
       heartbeat_at = clock_timestamp()
 WHERE status = 'running'
   AND cancel_requested_at IS NULL
   AND attempt_count < max_attempts
   AND NOT EXISTS (
       SELECT 1 FROM artifact.generation_job_attempt AS a
        WHERE a.job_id = generation_job.id AND a.status IN ('claimed', 'running'));

UPDATE artifact.generation_job
   SET status = CASE WHEN cancel_requested_at IS NOT NULL THEN 'cancelled' ELSE 'timed_out' END,
       completed_at = clock_timestamp(),
       error_code = CASE WHEN cancel_requested_at IS NOT NULL THEN 'CANCELLED' ELSE 'MAX_ATTEMPTS_EXCEEDED' END,
       next_attempt_at = NULL,
       heartbeat_at = clock_timestamp(),
       input_expires_at = clock_timestamp() + interval '7 days'
 WHERE status = 'running'
   AND NOT EXISTS (
       SELECT 1 FROM artifact.generation_job_attempt AS a
        WHERE a.job_id = generation_job.id AND a.status IN ('claimed', 'running'));

-- 0002 的 Job 既没有固定 generator / adapter 版本，也没有排队时限；在 0003 的领取条件下
-- 它们永远排不上队。用注册表当前版本补齐，并给一个默认排队时限：仍无人能处理的旧 Job
-- 会在时限到期后由恢复扫描以 cancelled / QUEUE_EXPIRED 收尾，而不是无限期挂着。
UPDATE artifact.generation_job AS j
   SET generator_version = g.version,
       adapter_version   = g.adapter_version,
       expires_at        = COALESCE(j.expires_at, clock_timestamp() + interval '24 hours')
  FROM artifact.generator_registry AS g
 WHERE g.generator_id = j.generator_id
   AND j.status IN ('queued', 'retry_wait');

-- 0002 使用过小写 snake_case 的系统错误码，统一为稳定机器码。
UPDATE artifact.generation_job
   SET error_code = 'CANCELLED'
 WHERE error_code IN ('cancelled_by_request', 'cancelled_by_worker');

-- 0002 对 Idempotency-Key 没有唯一约束：同 scope 内重复的键只保留最早一个 Job 上的，
-- 其余置空（Job 本身保留，只是不再受幂等保护），保证下面的唯一索引一定能建成。
UPDATE artifact.generation_job
   SET idempotency_key = NULL
 WHERE idempotency_key IS NOT NULL AND btrim(idempotency_key) = '';

UPDATE artifact.generation_job AS j
   SET idempotency_key = NULL
 WHERE j.idempotency_key IS NOT NULL
   AND EXISTS (
       SELECT 1 FROM artifact.generation_job AS earlier
        WHERE earlier.tenant_id = j.tenant_id
          AND earlier.project_id = j.project_id
          AND earlier.idempotency_key = j.idempotency_key
          AND (earlier.created_at, earlier.id) < (j.created_at, j.id));

-- ── ③ 约束与索引 ────────────────────────────────────────────────────────

ALTER TABLE artifact.generation_job
    ADD CONSTRAINT generation_job_max_execution_check
        CHECK (max_execution_seconds BETWEEN 1 AND 3600),
    ADD CONSTRAINT generation_job_error_code_check
        CHECK (error_code IS NULL OR error_code ~ '^[A-Z][A-Z0-9_]{0,63}$');

ALTER TABLE artifact.generation_job_attempt
    ADD CONSTRAINT generation_job_attempt_active_lease_check
        CHECK (status NOT IN ('claimed', 'running') OR lease_expires_at IS NOT NULL),
    ADD CONSTRAINT generation_job_attempt_error_code_check
        CHECK (error_code IS NULL OR error_code ~ '^[A-Z][A-Z0-9_]{0,63}$'),
    ADD CONSTRAINT generation_job_attempt_deadline_check
        CHECK (deadline_at IS NULL OR deadline_at >= started_at);

-- claim 按 (generator_id, adapter_version) 分区、再按 created_at, id 取最早的 Job：
-- 与 claim 的窗口查询完全一致，规划器不需要估算任何表达式。
CREATE INDEX idx_generation_job_claimable
    ON artifact.generation_job (generator_id, adapter_version, created_at, id)
    WHERE status IN ('queued', 'retry_wait');

CREATE INDEX idx_generation_job_queue_expiry
    ON artifact.generation_job (expires_at)
    WHERE status IN ('queued', 'retry_wait') AND expires_at IS NOT NULL;

-- 一个生成请求对应一个 Job：同 scope 内 Idempotency-Key 唯一（ART-03 §7）。
CREATE UNIQUE INDEX uq_generation_job_idempotency_key
    ON artifact.generation_job (tenant_id, project_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- 0002 的 cancel_requested_at 索引没有任何查询使用。
DROP INDEX artifact.idx_generation_job_cancel_requested;

-- ── ④ 内部辅助函数（不授权给任何运行角色） ─────────────────────────────

-- 受控函数内部写审计：Worker 角色没有 audit_event 写权限，审计只能在 SECURITY DEFINER
-- 函数内完成。detail 不得包含输入正文。
CREATE FUNCTION artifact.audit_generation_job_event(
    p_job_id         uuid,
    p_action         text,
    p_outcome        text,
    p_error_code     text,
    p_actor_subject  text DEFAULT NULL,
    p_actor_type     text DEFAULT 'service',
    p_detail         jsonb DEFAULT '{}'::jsonb
) RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
BEGIN
    INSERT INTO artifact.audit_event
        (id, tenant_id, project_id, actor_subject, actor_type, action, object_type,
         object_id, outcome, error_code, detail, request_id, created_at)
    SELECT gen_random_uuid(), j.tenant_id, j.project_id, p_actor_subject, p_actor_type,
           p_action, 'generation_job', j.id, p_outcome, p_error_code,
           coalesce(p_detail, '{}'::jsonb), j.trace_id, clock_timestamp()
      FROM artifact.generation_job AS j
     WHERE j.id = p_job_id;
END $$;

-- 可重试的错误码白名单。Worker 自报的 retryable 只有落在白名单内才生效：
-- 重试只针对明确的暂时性故障，未知错误码一律不重试。
CREATE FUNCTION artifact.is_retryable_generation_error(p_error_code text)
RETURNS boolean
LANGUAGE sql IMMUTABLE SET search_path = pg_catalog AS $$
    SELECT p_error_code = ANY (ARRAY['GENERATION_TIMEOUT', 'STORAGE_UNAVAILABLE'])
$$;

-- Attempt 失败后的统一收尾：更新 Attempt，按重试策略推进 Job，并写审计。
-- 调用方必须已经锁定 Attempt 与 Job 行。返回 Job 的下一个状态。
-- 退避：第 n 次失败后等待 min(5s × 2^(n-1), 300s) × [0.8, 1.2)；等不到的下一次重试直接终结。
CREATE FUNCTION artifact.settle_failed_attempt(
    p_job            artifact.generation_job,
    p_attempt_id     uuid,
    p_attempt_status text,
    p_error_code     text,
    p_diagnostic     text,
    p_retryable      boolean,
    p_actor_subject  text,
    p_actor_type     text
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now        timestamptz := clock_timestamp();
    v_delay      double precision;
    v_next_at    timestamptz;
    v_attempt_no integer;
    v_next       text;
    v_job_code   text;
BEGIN
    SELECT attempt_number INTO v_attempt_no
      FROM artifact.generation_job_attempt WHERE id = p_attempt_id;

    UPDATE artifact.generation_job_attempt
       SET status = p_attempt_status,
           completed_at = v_now,
           heartbeat_at = v_now,
           error_code = p_error_code,
           diagnostic_summary = nullif(left(btrim(coalesce(p_diagnostic, '')), 2048), '')
     WHERE id = p_attempt_id;

    IF coalesce(p_retryable, false) AND p_job.attempt_count < p_job.max_attempts THEN
        v_delay := LEAST(5.0 * power(2, greatest(p_job.attempt_count - 1, 0)), 300.0)
                   * (0.8 + random() * 0.4);
        v_next_at := v_now + make_interval(secs => v_delay);
        IF p_job.expires_at IS NULL OR v_next_at < p_job.expires_at THEN
            UPDATE artifact.generation_job
               SET status = 'retry_wait',
                   error_code = p_error_code,
                   error_message = NULL,
                   next_attempt_at = v_next_at,
                   heartbeat_at = v_now
             WHERE id = p_job.id;
            PERFORM artifact.audit_generation_job_event(
                p_job.id, 'generation.retry', 'error', p_error_code, p_actor_subject, p_actor_type,
                jsonb_build_object('attempt_number', v_attempt_no,
                                   'attempt_status', p_attempt_status,
                                   'next_attempt_at', v_next_at));
            RETURN 'retry_wait';
        END IF;
    END IF;

    -- Attempt 以 timed_out / lease_expired 收尾时，Job 的终态相应为 timed_out（ART-03 §5.2）。
    v_next := CASE WHEN p_attempt_status IN ('timed_out', 'lease_expired')
                   THEN 'timed_out' ELSE 'failed' END;
    -- Worker 反复失联（租约过期）耗尽 attempt 时，原因未知，使用稳定码而非猜测。
    v_job_code := CASE WHEN p_error_code = 'LEASE_EXPIRED' THEN 'MAX_ATTEMPTS_EXCEEDED' ELSE p_error_code END;
    UPDATE artifact.generation_job
       SET status = v_next,
           error_code = v_job_code,
           error_message = NULL,
           completed_at = v_now,
           heartbeat_at = v_now,
           next_attempt_at = NULL,
           input_expires_at = v_now + interval '7 days'
     WHERE id = p_job.id;
    PERFORM artifact.audit_generation_job_event(
        p_job.id,
        CASE WHEN v_next = 'timed_out' THEN 'generation.timeout' ELSE 'generation.fail' END,
        'error', v_job_code, p_actor_subject, p_actor_type,
        jsonb_build_object('attempt_number', v_attempt_no, 'attempts', p_job.attempt_count));
    RETURN v_next;
END $$;

-- ── ⑤ 取代 0002 的协调函数 ─────────────────────────────────────────────

DROP FUNCTION artifact.create_generation_job_with_input(
    uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, char(64), char(64), text, integer);
DROP FUNCTION artifact.claim_generation_job(text, integer);
DROP FUNCTION artifact.heartbeat_generation_job(uuid, uuid, text, integer);
DROP FUNCTION artifact.complete_generation_job_attempt(uuid, uuid, text, char(64));
DROP FUNCTION artifact.request_generation_job_cancel(uuid);
DROP FUNCTION artifact.confirm_generation_job_cancelled(uuid, uuid, text);

-- 创建：Job 与输入快照同一事务落地。
--   * 入参严格校验：输入、schema 版本、request hash 缺一不可；
--   * 从注册表固定 generator_version / adapter_version，重试中不会无声切换；
--   * input_sha256 由数据库计算（schema 版本 + JSONB 规范文本），调用方不再自报；
--   * Idempotency-Key 在 scope 内唯一：同 key 同 request hash 返回原 Job，不同则 409；
--   * expires_at 为排队时限，超过后未被领取的 Job 会被恢复扫描以 cancelled 收尾。
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
    p_max_attempts           integer DEFAULT 3,
    p_max_execution_seconds  integer DEFAULT 300,
    p_queue_ttl_seconds      integer DEFAULT 86400
) RETURNS TABLE (job_id uuid, replayed boolean)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now       timestamptz := clock_timestamp();
    v_key       text;
    v_existing  artifact.generation_job%ROWTYPE;
    v_generator artifact.generator_registry%ROWTYPE;
    v_input_sha text;
BEGIN
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

    SELECT * INTO v_generator
      FROM artifact.generator_registry WHERE generator_id = p_generator_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'UNKNOWN_GENERATOR' USING ERRCODE = '22023';
    END IF;
    -- 生成器不可用时不入队：排队的 Job 也不会被领取，API 应直接返回 503。
    IF v_generator.status <> 'available' THEN
        RAISE EXCEPTION 'GENERATOR_UNAVAILABLE' USING ERRCODE = 'P0001';
    END IF;

    v_input_sha := encode(sha256(convert_to(
        p_input_schema_version || E'\n'
        || p_input_payload::text || E'\n'
        || coalesce(p_options_payload, '{}'::jsonb)::text, 'UTF8')), 'hex');

    v_key := nullif(btrim(p_idempotency_key), '');
    IF v_key IS NOT NULL THEN
        IF length(v_key) > 255 THEN
            RAISE EXCEPTION 'INVALID_IDEMPOTENCY_KEY' USING ERRCODE = '22023';
        END IF;
        SELECT * INTO v_existing
          FROM artifact.generation_job
         WHERE tenant_id = p_tenant_id AND project_id = p_project_id AND idempotency_key = v_key;
        IF FOUND THEN
            IF v_existing.request_hash = p_request_hash THEN
                RETURN QUERY SELECT v_existing.id, true;
                RETURN;
            END IF;
            RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED' USING ERRCODE = '23505';
        END IF;
    END IF;

    BEGIN
        INSERT INTO artifact.generation_job
            (id, tenant_id, project_id, artifact_id, generator_id,
             generator_version, adapter_version,
             input_payload, options_payload, input_schema_version, input_sha256,
             request_hash, idempotency_key, max_attempts, max_execution_seconds,
             status, expires_at)
        VALUES
            (p_job_id, p_tenant_id, p_project_id, p_artifact_id, p_generator_id,
             v_generator.version, v_generator.adapter_version,
             p_input_payload, p_options_payload, p_input_schema_version, v_input_sha,
             p_request_hash, v_key, p_max_attempts, p_max_execution_seconds,
             'queued', v_now + make_interval(secs => p_queue_ttl_seconds::double precision));
    EXCEPTION WHEN unique_violation THEN
        -- 与并发请求撞上同一个 Idempotency-Key：按既有 Job 判定重放或冲突。
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
            RETURN QUERY SELECT v_existing.id, true;
            RETURN;
        END IF;
        RAISE EXCEPTION 'IDEMPOTENCY_KEY_REUSED' USING ERRCODE = '23505';
    END;

    RETURN QUERY SELECT p_job_id, false;
END $$;

-- 领取：原子锁定最早的、已到期的、未超时、未取消、生成器可用且与 Worker 申报的
-- adapter 版本一致的 Job。Worker 没有申报任何 adapter 时不会领到任何 Job。
-- 每次 claim 产生新 Attempt、新租约 token；租约与 deadline 取较早者。
CREATE FUNCTION artifact.claim_generation_job(
    p_worker_id        text,
    p_lease_seconds    integer DEFAULT 30,
    p_adapter_versions jsonb DEFAULT NULL
) RETURNS TABLE (
    job_id               uuid,
    attempt_id           uuid,
    attempt_number       integer,
    lease_token          text,
    lease_expires_at     timestamptz,
    deadline_at          timestamptz,
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
    v_now        timestamptz := clock_timestamp();
    v_window     integer := 100;
    v_ids        uuid[];
    v_job        artifact.generation_job%ROWTYPE;
    v_token      text;
    v_attempt_id uuid;
    v_next       integer;
    v_expires    timestamptz;
    v_deadline   timestamptz;
BEGIN
    IF coalesce(length(btrim(p_worker_id)), 0) = 0 OR length(p_worker_id) > 128 THEN
        RAISE EXCEPTION 'INVALID_WORKER_ID' USING ERRCODE = '22023';
    END IF;
    IF p_lease_seconds IS NULL OR p_lease_seconds < 1 OR p_lease_seconds > 3600 THEN
        RAISE EXCEPTION 'INVALID_LEASE_SECONDS' USING ERRCODE = '22023';
    END IF;
    IF p_adapter_versions IS NOT NULL AND jsonb_typeof(p_adapter_versions) <> 'object' THEN
        RAISE EXCEPTION 'INVALID_ADAPTER_VERSIONS' USING ERRCODE = '22023';
    END IF;

    -- 分两步，保证每一步的执行计划都不依赖规划器估算：
    -- 第一步：按 Worker 申报的每个 (generator, adapter) 各取一个最早的窗口，只走部分索引。
    --         退避中和已超时的 Job 在窗口内就被排除，不会挡住后面的 Job。
    SELECT array_agg(w.id) INTO v_ids
      FROM jsonb_each_text(p_adapter_versions) AS c
      CROSS JOIN LATERAL (
            SELECT j.id
              FROM artifact.generation_job AS j
             WHERE j.status IN ('queued', 'retry_wait')
               AND j.generator_id = c.key
               AND j.adapter_version = c.value
               AND (j.status = 'queued' OR j.next_attempt_at IS NULL
                    OR j.next_attempt_at <= v_now)
               AND (j.expires_at IS NULL OR j.expires_at > v_now)
             ORDER BY j.created_at, j.id
             LIMIT v_window
      ) AS w;
    IF v_ids IS NULL THEN
        RETURN;
    END IF;

    -- 第二步：按主键加锁并复核全部条件。窗口只是候选，这里的结果才是权威。
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j
      JOIN artifact.generator_registry AS g
        ON g.generator_id = j.generator_id
     WHERE j.id = ANY (v_ids)
       AND j.status IN ('queued', 'retry_wait')
       -- retry_wait 必须等到退避结束（0002 遗留行的 next_attempt_at 为空，视为已到期）。
       AND (j.status = 'queued' OR j.next_attempt_at IS NULL OR j.next_attempt_at <= v_now)
       AND (j.expires_at IS NULL OR j.expires_at > v_now)
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

    v_now := clock_timestamp();
    v_next := v_job.attempt_count + 1;
    -- 两个随机 UUID 拼接为 244 bit 随机 token；只用 PG core 的 gen_random_uuid()。
    v_token := gen_random_uuid()::text || gen_random_uuid()::text;
    v_deadline := v_now + make_interval(secs => v_job.max_execution_seconds::double precision);
    v_expires := LEAST(v_now + make_interval(secs => p_lease_seconds::double precision), v_deadline);
    v_attempt_id := gen_random_uuid();

    INSERT INTO artifact.generation_job_attempt
        (id, job_id, attempt_number, worker_id, status,
         lease_token_hash, lease_expires_at, deadline_at, started_at, heartbeat_at)
    VALUES
        (v_attempt_id, v_job.id, v_next, p_worker_id, 'running',
         encode(sha256(convert_to(v_token, 'UTF8')), 'hex'), v_expires, v_deadline,
         v_now, v_now);

    UPDATE artifact.generation_job
       SET status = 'running',
           attempt_count = v_next,
           started_at = coalesce(started_at, v_now),
           heartbeat_at = v_now,
           next_attempt_at = NULL
     WHERE id = v_job.id;

    job_id := v_job.id;
    attempt_id := v_attempt_id;
    attempt_number := v_next;
    lease_token := v_token;
    lease_expires_at := v_expires;
    deadline_at := v_deadline;
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

-- 续租。返回四态：
--   renewed           已续租；
--   cancel_requested  已续租，但调用方应停止执行并调用 confirm_generation_job_cancelled；
--   deadline_exceeded 已超过本 Attempt 的最大执行时长：不再续租，Attempt 已按超时收尾；
--   stale             租约已不属于调用方（过期、旧 Attempt、错误 token、Job 不在 running）。
-- 先判定租约归属再读取取消状态：旧 Worker 一律得到 stale，不泄露 Job 状态。
CREATE FUNCTION artifact.heartbeat_generation_job(
    p_job_id        uuid,
    p_attempt_id    uuid,
    p_lease_token   text,
    p_lease_seconds integer DEFAULT 30
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now     timestamptz := clock_timestamp();
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_job     artifact.generation_job%ROWTYPE;
    v_expires timestamptz;
BEGIN
    IF p_lease_seconds IS NULL OR p_lease_seconds < 1 OR p_lease_seconds > 3600 THEN
        RAISE EXCEPTION 'INVALID_LEASE_SECONDS' USING ERRCODE = '22023';
    END IF;
    IF p_lease_token IS NULL OR p_lease_token = '' THEN
        RETURN 'stale';
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
       FOR UPDATE;
    IF NOT FOUND
       OR v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN 'stale';
    END IF;

    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id
       FOR UPDATE;
    IF NOT FOUND OR v_job.status <> 'running' THEN
        RETURN 'stale';
    END IF;

    -- 最大总执行时长：超过 deadline 不再续租，由调用方原地收尾为超时。
    IF v_attempt.deadline_at IS NOT NULL AND v_now >= v_attempt.deadline_at THEN
        PERFORM artifact.settle_failed_attempt(
            v_job, p_attempt_id, 'timed_out', 'GENERATION_TIMEOUT',
            'attempt exceeded max_execution_seconds', true, v_attempt.worker_id, 'service');
        RETURN 'deadline_exceeded';
    END IF;

    IF v_attempt.lease_expires_at IS NULL OR v_attempt.lease_expires_at <= v_now THEN
        RETURN 'stale';
    END IF;

    -- 续租不得超过 deadline。
    v_expires := LEAST(
        v_now + make_interval(secs => p_lease_seconds::double precision), v_attempt.deadline_at);
    UPDATE artifact.generation_job_attempt
       SET heartbeat_at = v_now, lease_expires_at = v_expires
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET heartbeat_at = v_now
     WHERE id = p_job_id;

    IF v_job.cancel_requested_at IS NOT NULL THEN
        RETURN 'cancel_requested';
    END IF;
    RETURN 'renewed';
END $$;

-- 完成。一致性不变量（ART-03A §8）：只有当 Job 的目标 Version 已是 READY、且存在
-- 已通过安全检查的主 Output、其 sha256 与上报一致时，Job 才能变为 succeeded。
-- 输出登记（Output + Version pending → ready）由 ART-03D 在同一事务内先行完成，
-- 随后调用本函数收尾；在那之前本函数不可能成功。
-- 返回 succeeded / cancel_requested / stale。
CREATE FUNCTION artifact.complete_generation_job_attempt(
    p_job_id        uuid,
    p_attempt_id    uuid,
    p_lease_token   text,
    p_output_sha256 text
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now     timestamptz := clock_timestamp();
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_job     artifact.generation_job%ROWTYPE;
BEGIN
    IF p_lease_token IS NULL OR p_lease_token = '' THEN
        RETURN 'stale';
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
       FOR UPDATE;
    IF NOT FOUND
       OR v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN 'stale';
    END IF;
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id
       FOR UPDATE;
    IF NOT FOUND OR v_job.status <> 'running'
       OR v_attempt.lease_expires_at IS NULL OR v_attempt.lease_expires_at <= v_now THEN
        RETURN 'stale';
    END IF;
    IF v_job.cancel_requested_at IS NOT NULL THEN
        RETURN 'cancel_requested';
    END IF;
    IF p_output_sha256 IS NULL OR p_output_sha256 !~ '^[0-9a-f]{64}$' THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_SHA256' USING ERRCODE = '22023';
    END IF;
    IF v_job.target_version_id IS NULL OR NOT EXISTS (
        SELECT 1
          FROM artifact.artifact_version AS v
          JOIN artifact.artifact_output AS o ON o.version_id = v.id
         WHERE v.id = v_job.target_version_id
           AND v.artifact_id = v_job.artifact_id
           AND v.tenant_id = v_job.tenant_id
           AND v.project_id = v_job.project_id
           AND v.state = 'ready'
           AND o.safety_status = 'passed'
           AND o.output_role = 'primary'
           AND o.sha256 = p_output_sha256) THEN
        RAISE EXCEPTION 'OUTPUT_NOT_REGISTERED' USING ERRCODE = 'P0001';
    END IF;

    UPDATE artifact.generation_job_attempt
       SET status = 'succeeded', completed_at = v_now, heartbeat_at = v_now,
           output_sha256 = p_output_sha256
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET status = 'succeeded', completed_at = v_now,
           error_code = NULL, error_message = NULL,
           input_expires_at = v_now + interval '7 days'
     WHERE id = p_job_id;
    PERFORM artifact.audit_generation_job_event(
        p_job_id, 'generation.complete', 'allowed', NULL, v_attempt.worker_id, 'service',
        jsonb_build_object('attempt_number', v_attempt.attempt_number,
                           'version_id', v_job.target_version_id));
    RETURN 'succeeded';
END $$;

-- 失败上报。Attempt 的终态随错误码区分：GENERATION_TIMEOUT 记为 timed_out，其余为 failed。
-- 返回 retry_wait / failed / timed_out / cancel_requested / stale_attempt / not_found。
-- 取消请求登记在即：先判定租约归属（错误 token 一律 stale），再返回 cancel_requested。
CREATE OR REPLACE FUNCTION artifact.fail_generation_job_attempt(
    p_job_id            uuid,
    p_attempt_id        uuid,
    p_lease_token       text,
    p_error_code        text,
    p_diagnostic_summary text DEFAULT NULL,
    p_retryable         boolean DEFAULT false
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now          timestamptz := clock_timestamp();
    v_attempt      artifact.generation_job_attempt%ROWTYPE;
    v_job          artifact.generation_job%ROWTYPE;
    v_retryable    boolean;
    v_attempt_state text;
BEGIN
    IF p_lease_token IS NULL OR p_lease_token = '' THEN
        RETURN 'stale_attempt';
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN 'not_found';
    END IF;
    IF v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN 'stale_attempt';
    END IF;
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id
       FOR UPDATE;
    IF NOT FOUND OR v_job.status <> 'running'
       OR v_attempt.lease_expires_at IS NULL OR v_attempt.lease_expires_at <= v_now THEN
        RETURN 'stale_attempt';
    END IF;
    IF v_job.cancel_requested_at IS NOT NULL THEN
        RETURN 'cancel_requested';
    END IF;
    IF p_error_code IS NULL OR p_error_code !~ '^[A-Z][A-Z0-9_]{0,63}$' THEN
        RAISE EXCEPTION 'INVALID_ERROR_CODE' USING ERRCODE = '22023';
    END IF;

    v_retryable := coalesce(p_retryable, false)
                   AND artifact.is_retryable_generation_error(p_error_code);
    v_attempt_state := CASE WHEN p_error_code = 'GENERATION_TIMEOUT' THEN 'timed_out' ELSE 'failed' END;
    RETURN artifact.settle_failed_attempt(
        v_job, p_attempt_id, v_attempt_state, p_error_code,
        p_diagnostic_summary, v_retryable, v_attempt.worker_id, 'service');
END $$;

-- 取消协调。租户与项目在函数内校验：scope 不匹配一律 not_found，不泄露 Job 是否存在。
--   queued / retry_wait → cancelled；running → 只登记 cancel_requested_at，不伪造 Worker 已停止。
-- 返回 cancelled / cancel_requested / terminal / not_found。
CREATE FUNCTION artifact.request_generation_job_cancel(
    p_tenant_id  uuid,
    p_project_id uuid,
    p_job_id     uuid
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now timestamptz := clock_timestamp();
    v_job artifact.generation_job%ROWTYPE;
BEGIN
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j
     WHERE j.id = p_job_id AND j.tenant_id = p_tenant_id AND j.project_id = p_project_id
       FOR UPDATE;
    IF NOT FOUND THEN
        RETURN 'not_found';
    END IF;
    IF v_job.status IN ('succeeded', 'failed', 'cancelled', 'timed_out') THEN
        RETURN 'terminal';
    END IF;

    IF v_job.status IN ('queued', 'retry_wait') THEN
        UPDATE artifact.generation_job
           SET status = 'cancelled', completed_at = v_now, error_code = 'CANCELLED',
               next_attempt_at = NULL, heartbeat_at = v_now,
               input_expires_at = v_now + interval '7 days'
         WHERE id = p_job_id;
        RETURN 'cancelled';
    END IF;

    -- running：仅登记请求。Job 与 Attempt 仍为 running；Worker 通过 heartbeat 得知后停止，
    -- 并调用 confirm_generation_job_cancelled；Worker 失联时由恢复扫描收尾。
    UPDATE artifact.generation_job
       SET cancel_requested_at = coalesce(cancel_requested_at, v_now)
     WHERE id = p_job_id;
    RETURN 'cancel_requested';
END $$;

-- Worker 确认已停止后，才将 Job / Attempt 置为 cancelled。
-- 返回 cancelled / not_requested / stale。
CREATE FUNCTION artifact.confirm_generation_job_cancelled(
    p_job_id      uuid,
    p_attempt_id  uuid,
    p_lease_token text
) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_now     timestamptz := clock_timestamp();
    v_attempt artifact.generation_job_attempt%ROWTYPE;
    v_job     artifact.generation_job%ROWTYPE;
BEGIN
    IF p_lease_token IS NULL OR p_lease_token = '' THEN
        RETURN 'stale';
    END IF;
    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
       FOR UPDATE;
    IF NOT FOUND
       OR v_attempt.status <> 'running'
       OR v_attempt.lease_token_hash IS NULL
       OR v_attempt.lease_token_hash <> encode(sha256(convert_to(p_lease_token, 'UTF8')), 'hex') THEN
        RETURN 'stale';
    END IF;
    SELECT j.* INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id
       FOR UPDATE;
    IF NOT FOUND OR v_job.status <> 'running' THEN
        RETURN 'stale';
    END IF;
    IF v_job.cancel_requested_at IS NULL THEN
        RETURN 'not_requested';
    END IF;

    -- 不检查租约是否过期：取消已登记，Job 的终态不受租约影响。
    UPDATE artifact.generation_job_attempt
       SET status = 'cancelled', completed_at = v_now
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET status = 'cancelled', completed_at = v_now, error_code = 'CANCELLED',
           heartbeat_at = v_now, input_expires_at = v_now + interval '7 days'
     WHERE id = p_job_id;
    PERFORM artifact.audit_generation_job_event(
        p_job_id, 'generation.cancelled', 'allowed', NULL, v_attempt.worker_id, 'service',
        jsonb_build_object('attempt_number', v_attempt.attempt_number, 'confirmed_by', 'worker'));
    RETURN 'cancelled';
END $$;

-- 恢复扫描（幂等，可由任意 Worker 或定时任务调用）：
--   ① 排队超时：未能在 expires_at 之前被领取的 Job 以 cancelled / QUEUE_EXPIRED 收尾；
--   ② 租约失效的活跃 Attempt：按是否到达 deadline 归类为超时或失联，再按重试策略推进 Job。
-- advisory lock 保证同一时刻只有一个扫描器推进；每轮最多处理一批，避免长事务。
CREATE OR REPLACE FUNCTION artifact.recover_expired_generation_jobs()
RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_batch     integer := 500;
    v_count     integer := 0;
    v_now       timestamptz;
    v_job       artifact.generation_job%ROWTYPE;
    v_timed_out boolean;
    v_code      text;
    v_state     text;
    r           record;
    a           record;
BEGIN
    IF NOT pg_try_advisory_xact_lock(hashtext('artifact.recover_expired_generation_jobs')) THEN
        RETURN 0;
    END IF;

    FOR r IN
        SELECT j.id
          FROM artifact.generation_job AS j
         WHERE j.status IN ('queued', 'retry_wait')
           AND j.expires_at IS NOT NULL
           AND j.expires_at <= clock_timestamp()
         ORDER BY j.expires_at, j.id
           FOR UPDATE SKIP LOCKED
         LIMIT v_batch
    LOOP
        v_now := clock_timestamp();
        UPDATE artifact.generation_job
           SET status = 'cancelled', completed_at = v_now, error_code = 'QUEUE_EXPIRED',
               next_attempt_at = NULL, heartbeat_at = v_now,
               input_expires_at = v_now + interval '7 days'
         WHERE id = r.id;
        PERFORM artifact.audit_generation_job_event(
            r.id, 'generation.expired', 'error', 'QUEUE_EXPIRED');
        v_count := v_count + 1;
    END LOOP;

    FOR a IN
        SELECT at.id, at.job_id, at.attempt_number, at.deadline_at, at.lease_expires_at
          FROM artifact.generation_job_attempt AS at
         WHERE at.status IN ('claimed', 'running')
           AND (at.lease_expires_at IS NULL OR at.lease_expires_at <= clock_timestamp())
         ORDER BY at.lease_expires_at
           FOR UPDATE SKIP LOCKED
         LIMIT v_batch
    LOOP
        v_now := clock_timestamp();
        SELECT j.* INTO v_job
          FROM artifact.generation_job AS j
         WHERE j.id = a.job_id
           FOR UPDATE;

        -- 租约与 deadline 重合，说明 Worker 一直存活到时限：按超时归类；否则是失联。
        v_timed_out := a.deadline_at IS NOT NULL
                       AND a.lease_expires_at IS NOT NULL
                       AND a.lease_expires_at >= a.deadline_at;
        v_code := CASE WHEN v_timed_out THEN 'GENERATION_TIMEOUT' ELSE 'LEASE_EXPIRED' END;
        v_state := CASE WHEN v_timed_out THEN 'timed_out' ELSE 'lease_expired' END;

        IF v_job.status = 'running' AND v_job.cancel_requested_at IS NOT NULL THEN
            UPDATE artifact.generation_job_attempt
               SET status = 'lease_expired', completed_at = v_now, error_code = 'LEASE_EXPIRED'
             WHERE id = a.id;
            UPDATE artifact.generation_job
               SET status = 'cancelled', completed_at = v_now, error_code = 'CANCELLED',
                   next_attempt_at = NULL, heartbeat_at = v_now,
                   input_expires_at = v_now + interval '7 days'
             WHERE id = v_job.id;
            PERFORM artifact.audit_generation_job_event(
                v_job.id, 'generation.cancelled', 'allowed', NULL, NULL, 'service',
                jsonb_build_object('attempt_number', a.attempt_number, 'confirmed_by', 'recovery'));
        ELSIF v_job.status = 'running' THEN
            PERFORM artifact.settle_failed_attempt(
                v_job, a.id, v_state, v_code, NULL, true, NULL, 'service');
        ELSE
            -- Job 已不在 running：只收尾遗留的 Attempt。
            UPDATE artifact.generation_job_attempt
               SET status = v_state, completed_at = v_now, error_code = v_code
             WHERE id = a.id;
        END IF;
        v_count := v_count + 1;
    END LOOP;
    RETURN v_count;
END $$;

-- 终态 7 天后清理输入正文；保留 sha256、schema 版本与执行元数据。分批执行。
CREATE OR REPLACE FUNCTION artifact.purge_expired_generation_job_input()
RETURNS integer
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_count integer;
BEGIN
    WITH expired AS (
        SELECT j.id
          FROM artifact.generation_job AS j
         WHERE j.status IN ('succeeded', 'failed', 'cancelled', 'timed_out')
           AND j.input_expires_at IS NOT NULL
           AND j.input_expires_at <= clock_timestamp()
           AND (j.input_payload IS NOT NULL OR j.options_payload IS NOT NULL)
           FOR UPDATE SKIP LOCKED
         LIMIT 1000
    ), purged AS (
        UPDATE artifact.generation_job AS j
           SET input_payload = NULL, options_payload = NULL
          FROM expired
         WHERE j.id = expired.id
        RETURNING 1
    )
    SELECT count(*) INTO v_count FROM purged;
    RETURN v_count;
END $$;

-- ── ⑥ 默认拒绝：PUBLIC 不获得任何函数 EXECUTE ─────────────────────────

REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA artifact FROM PUBLIC;
