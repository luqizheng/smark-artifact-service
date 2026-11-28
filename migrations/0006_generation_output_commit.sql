-- 0006_generation_output_commit.sql — WP-AS-06 输出提交协议与 SeeCode 门禁翻转
--
-- 契约来源：docs/architecture/ART-03A-job-worker-svg-decisions-v1.md §8（单一
-- 受控提交路径：Output 插入 + 目标 Version pending→ready + Job/Attempt
-- succeeded + audit 同事务）；WP-AS-05 实施报告 D-W05-3（补偿删除仅用于
-- AS 自身上传的输出对象）；ART-04 §211（成功 Job 需稳定 READY Version 引用
-- 与已提交 Output；target_version_id 加法性暴露）。
--
-- 仅新增对象；不改写 0001/0002/0003/0004/0005 的任何一行（迁移不可变）。
--
-- 安全模型（与 0002 一致）：
--   * 受控函数 SECURITY DEFINER，owner = migrator（schema owner）；
--     search_path 固定为 pg_catalog, artifact，函数体内全部对象显式限定 schema。
--   * PUBLIC 对所有函数 REVOKE EXECUTE；仅 deploy/db-roles.sql 显式授权 Worker role。
--   * Worker role 不获得任何业务表 DML 权限；其唯一写路径是受控函数。
--   * 租约 token 明文只在 claim 返回值出现一次；数据库仅保存 sha256(token)。

-- ── ① commit_generation_output：输出提交的单一受控路径 ─────────────────
--
-- Worker 完成「渲染 → SVG 安全重建 → file-service 上传（WP-AS-05 适配器，
-- upload_object 恒带 tid，user_id=None）」后调用。函数在**一个事务**内：
--   1. 校验 Attempt 租约（running + token hash 匹配 + 未过期）与 Job 状态；
--   2. 取消检查（cancel_requested_at 非空 → RAISE CANCEL_REQUESTED，与 0002
--      complete/fail 同语义，Worker 侧转 confirm_cancelled）；
--   3. 物化目标 Version：target_version_id 为空则插入 pending Version
--      （generator 三元组取 Job + 注册表），否则校验其为同 scope 的 pending；
--   4. 插入 artifact_output（primary，safety_status='passed' —— SVG 安全
--      重建已由 Worker 完成；sanitizer 策略版本在 manifest 中记录）；
--   5. Version pending→ready（ready_at + manifest，触发器背书终态）；
--   6. Attempt/Job succeeded + input_expires_at=+7d；
--   7. 写 audit_event（allowed, generation.complete, actor=artifact-worker）。
--
-- 返回目标 Version id（Job 的稳定精确结果引用，ART-04 §211）。
-- 任一步失败整体回滚：DB 提交成功但响应丢失时幂等重查即可（Job 已终态）；
-- file-service 上传成功而本函数失败的对象补偿删除由 Worker 负责执行
-- （D-W05-3：补偿仅用于 AS 自身上传的输出对象）。
CREATE FUNCTION artifact.commit_generation_output(
    p_job_id            uuid,
    p_attempt_id        uuid,
    p_lease_token       text,
    p_output_object_id  text,
    p_output_sha256     char(64),
    p_output_size_bytes bigint,
    p_output_mime       text,
    p_output_format     text,
    p_manifest          jsonb
) RETURNS uuid
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, artifact AS $$
DECLARE
    v_attempt   artifact.generation_job_attempt%ROWTYPE;
    v_job       artifact.generation_job%ROWTYPE;
    v_cancel    timestamptz;
    v_version   uuid;
    v_gen_meta  record;
    v_created_by text;
BEGIN
    IF p_lease_token IS NULL OR length(btrim(p_lease_token)) = 0 THEN
        RETURN NULL;
    END IF;
    IF p_output_object_id IS NULL OR length(btrim(p_output_object_id)) = 0 THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_OBJECT' USING ERRCODE = '22023';
    END IF;
    IF length(btrim(coalesce(p_output_sha256, ''))) <> 64 THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_SHA256' USING ERRCODE = '22023';
    END IF;
    IF p_output_size_bytes IS NULL OR p_output_size_bytes < 0 THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_SIZE' USING ERRCODE = '22023';
    END IF;
    IF length(btrim(coalesce(p_output_mime, ''))) = 0
       OR length(btrim(coalesce(p_output_format, ''))) = 0 THEN
        RAISE EXCEPTION 'INVALID_OUTPUT_METADATA' USING ERRCODE = '22023';
    END IF;

    SELECT a.* INTO v_attempt
      FROM artifact.generation_job_attempt AS a
     WHERE a.id = p_attempt_id AND a.job_id = p_job_id
     FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NULL;
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
        RETURN NULL;
    END IF;

    SELECT * INTO v_job
      FROM artifact.generation_job AS j WHERE j.id = p_job_id FOR UPDATE;
    IF NOT FOUND THEN
        RETURN NULL;
    END IF;
    IF v_job.status <> 'running' THEN
        RETURN NULL;
    END IF;

    -- ③ 物化/校验目标 Version。
    IF v_job.target_version_id IS NULL THEN
        -- generator 三元组：Job 行 + 注册表当前版本（claim 时的 adapter
        -- 版本已由 claim 固定在 Attempt 上，这里以 Job/注册表为准落库）。
        SELECT g.version AS generator_version, v_job.adapter_version AS adapter_version
          INTO v_gen_meta
          FROM artifact.generator_registry AS g
         WHERE g.generator_id = v_job.generator_id;
        INSERT INTO artifact.artifact_version
            (id, tenant_id, project_id, artifact_id, state, source_format,
             generator_id, generator_version, adapter_version, manifest, created_by)
        VALUES
            (gen_random_uuid(), v_job.tenant_id, v_job.project_id, v_job.artifact_id,
             'pending', btrim(p_output_format), v_job.generator_id,
             v_gen_meta.generator_version, v_job.adapter_version, '{}'::jsonb,
             'artifact-worker')
        RETURNING id INTO v_version;
        UPDATE artifact.generation_job
           SET target_version_id = v_version
         WHERE id = p_job_id;
    ELSE
        SELECT id INTO v_version
          FROM artifact.artifact_version
         WHERE id = v_job.target_version_id
           AND artifact_id = v_job.artifact_id
           AND tenant_id = v_job.tenant_id
           AND project_id = v_job.project_id
           AND state = 'pending'
         FOR UPDATE;
        IF NOT FOUND THEN
            RAISE EXCEPTION 'TARGET_VERSION_NOT_PENDING' USING ERRCODE = 'P0001';
        END IF;
        UPDATE artifact.artifact_version
           SET generator_id = v_job.generator_id,
               generator_version = (SELECT version FROM artifact.generator_registry
                                     WHERE generator_id = v_job.generator_id),
               adapter_version = v_job.adapter_version
         WHERE id = v_version;
    END IF;

    -- ④ Output（primary；safety_status='passed'：SVG 安全重建已在 Worker 完成，
    --    策略版本随 manifest 记录）。
    INSERT INTO artifact.artifact_output
        (id, tenant_id, project_id, artifact_id, version_id, output_role,
         format, mime_type, size_bytes, sha256, file_object_id, safety_status)
    VALUES
        (gen_random_uuid(), v_job.tenant_id, v_job.project_id, v_job.artifact_id,
         v_version, 'primary', btrim(p_output_format), btrim(p_output_mime),
         p_output_size_bytes, btrim(p_output_sha256), btrim(p_output_object_id),
         'passed');

    -- ⑤ Version pending→ready（触发器背书：READY 终态、不可变列锁定）。
    UPDATE artifact.artifact_version
       SET state = 'ready', ready_at = clock_timestamp(), manifest = p_manifest
     WHERE id = v_version;

    -- ⑥ Attempt/Job 终态 + 输入保留窗口（与 0002 complete 同语义）。
    UPDATE artifact.generation_job_attempt
       SET status = 'succeeded', completed_at = clock_timestamp(),
           heartbeat_at = clock_timestamp(), output_sha256 = btrim(p_output_sha256)
     WHERE id = p_attempt_id;
    UPDATE artifact.generation_job
       SET status = 'succeeded', completed_at = clock_timestamp(),
           error_code = NULL,
           input_expires_at = clock_timestamp() + interval '7 days'
     WHERE id = p_job_id;

    -- ⑦ 审计（服务身份动作；不含输入正文/租约 token）。
    INSERT INTO artifact.audit_event
        (id, tenant_id, project_id, actor_subject, actor_type, action,
         object_type, object_id, outcome, error_code, detail, request_id)
    VALUES
        (gen_random_uuid(), v_job.tenant_id, v_job.project_id,
         'artifact-worker', 'service', 'generation.complete',
         'generation_job', p_job_id, 'allowed', NULL,
         jsonb_build_object(
             'attempt_id', p_attempt_id,
             'target_version_id', v_version,
             'output_sha256', btrim(p_output_sha256),
             'output_object_id', btrim(p_output_object_id)
         ),
         NULL);

    RETURN v_version;
END $$;

REVOKE EXECUTE ON FUNCTION artifact.commit_generation_output(uuid, uuid, text, text, char(64), bigint, text, text, jsonb) FROM PUBLIC;

-- ── ② SeeCode 门禁翻转（A3A-08：迁移式修改注册表状态） ────────────────────
-- WP-AS-06 落地 SeeCode Adapter + Worker 执行循环后，注册表 status 翻转为
-- available（request_generation 才会放行）。
--
-- adapter_version 必须同迁移钉住（WP-AS-06）：claim v2（0003）以
-- `j.adapter_version = c.value` 等值匹配 Worker 申报的能力表；seed 的
-- adapter_version 为 NULL，等值匹配永假 → Job 无法被领取。该值同时是
-- Worker 侧（apps/artifact-worker）能力申报的契约常量，两侧必须一致；
-- 升级 Adapter 时以新迁移更新此值并同步 Worker 版本。
UPDATE artifact.generator_registry
   SET status = 'available',
       adapter_version = 'svg-as-1',
       status_reason = 'WP-AS-06 SeeCode adapter + worker execution loop landed'
 WHERE generator_id = 'seecode';
