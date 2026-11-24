-- deploy/db-roles.sql — 迁移角色与运行角色分离（ART-02 §4.3 / ART-01 §3.2）。
--
-- __MIGRATOR_ROLE__：执行 `artifact-api migrate`（DDL）。schema 由迁移创建，owner 归该角色。
-- __APP_ROLE__    ：服务运行时连接。仅 SELECT/INSERT/UPDATE；无 DDL、无 DELETE
--                   （删除一律软删除）、审计表不可 UPDATE。
-- __WORKER_ROLE__ ：独立 Worker 运行时连接。仅可执行受控协调函数（SECURITY DEFINER），
--                   对业务表无任何 SELECT/INSERT/UPDATE/DELETE、无 DDL；
--                   不继承 APP/MIGRATOR，不获得用户 delegation 或对象存储凭据。
--
-- 用法（由管理员执行，替换占位符）：
--   psql -v ON_ERROR_STOP=1 -f deploy/db-roles.sql \
--     -v migrator=artifact_migrator -v app=artifact_app -v worker=artifact_worker
-- 或先 sed 替换三个占位符再执行。
--
-- 口令策略：本脚本不设置口令。由运维以
--   ALTER ROLE artifact_app    PASSWORD :'...';
--   ALTER ROLE artifact_migrator PASSWORD :'...';
-- 注入（从 secret 管理系统获取，禁止写入代码/脚本/镜像）。

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '__MIGRATOR_ROLE__') THEN
        CREATE ROLE "__MIGRATOR_ROLE__" LOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '__APP_ROLE__') THEN
        CREATE ROLE "__APP_ROLE__" LOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '__WORKER_ROLE__') THEN
        -- NOINHERIT：即使将来误加成员关系也不会自动继承其他角色的权限。
        CREATE ROLE "__WORKER_ROLE__" LOGIN NOINHERIT;
    END IF;
END $$;

-- 运行角色：schema usage + DML。
GRANT USAGE ON SCHEMA artifact TO "__APP_ROLE__";
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA artifact TO "__APP_ROLE__";
ALTER DEFAULT PRIVILEGES IN SCHEMA artifact
    GRANT SELECT, INSERT, UPDATE ON TABLES TO "__APP_ROLE__";

-- 审计表对运行角色只允许追加（数据库触发器是第二道防线）。
REVOKE UPDATE, DELETE ON artifact.audit_event FROM "__APP_ROLE__";
ALTER DEFAULT PRIVILEGES IN SCHEMA artifact
    REVOKE UPDATE, DELETE ON TABLES FROM "__APP_ROLE__";

-- ── Worker role：最小权限边界 ──────────────────────────────────────────
-- 仅 schema USAGE + 受控函数 EXECUTE。migration 0002 已对 PUBLIC REVOKE EXECUTE，
-- 这里按函数逐个精确授权；不授予任何表 DML、不授予 DDL、不授予 CREATE EXTENSION。
GRANT USAGE ON SCHEMA artifact TO "__WORKER_ROLE__";

-- 每一条都是「函数级」授权，绝不使用 ON ALL TABLES / ON ALL FUNCTIONS 的宽授权。
GRANT EXECUTE ON FUNCTION artifact.claim_generation_job(text, integer) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.heartbeat_generation_job(uuid, uuid, text, integer) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.complete_generation_job_attempt(uuid, uuid, text, char(64)) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.fail_generation_job_attempt(uuid, uuid, text, text, text, boolean) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.confirm_generation_job_cancelled(uuid, uuid, text) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.recover_expired_generation_jobs() TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.purge_expired_generation_job_input() TO "__WORKER_ROLE__";

-- API role 需要调用「创建（同事务输入快照）」与「取消协调」两个受控函数；
-- 其余 Worker 专用函数不授予 API role。
GRANT EXECUTE ON FUNCTION artifact.create_generation_job_with_input(uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, char(64), char(64), text, integer) TO "__APP_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.request_generation_job_cancel(uuid) TO "__APP_ROLE__";

-- 显式兜底：Worker role 不拥有任何业务表的 DML/DDL（即便将来 ALTER DEFAULT PRIVILEGES
-- 放宽，也通过 REVOKE 保持拒绝；受控函数以 SECURITY DEFINER 身份代为写入）。
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA artifact FROM "__WORKER_ROLE__";
ALTER DEFAULT PRIVILEGES IN SCHEMA artifact REVOKE ALL ON TABLES FROM "__WORKER_ROLE__";
REVOKE CREATE ON SCHEMA artifact FROM "__WORKER_ROLE__";

-- 迁移角色：schema 全权（owner 身份执行迁移）。
GRANT ALL ON SCHEMA artifact TO "__MIGRATOR_ROLE__";
