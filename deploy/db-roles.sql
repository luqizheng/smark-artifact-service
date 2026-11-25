-- deploy/db-roles.sql — 迁移角色与运行角色分离（ART-02 §4.3 / ART-01 §3.2）。
--
-- __MIGRATOR_ROLE__：执行迁移（DDL）。schema 由迁移创建，owner 归该角色。
-- __APP_ROLE__    ：API 运行时连接。仅 SELECT/INSERT/UPDATE；无 DDL、无 DELETE
--                   （删除一律软删除）、审计表不可 UPDATE。
-- __WORKER_ROLE__ ：独立 Worker 运行时连接。仅可执行受控协调函数（SECURITY DEFINER），
--                   对业务表无任何 SELECT/INSERT/UPDATE/DELETE、无 DDL；
--                   不继承 APP/MIGRATOR，不获得用户 delegation 或对象存储凭据。
-- __MAINTENANCE_ROLE__：Idempotency-Key 过期清理主体（ART-04E §6.2）。仅有
--                   清理函数（SECURITY DEFINER）的 EXECUTE 权限，对任何业务表
--                   都没有 DELETE（也没有 SELECT/INSERT/UPDATE）。
--
-- 执行顺序（每次迁移之后都必须重新执行本脚本）：
--   1. 以 __MIGRATOR_ROLE__ 执行迁移；
--   2. 以管理员或 __MIGRATOR_ROLE__ 本身执行本脚本；
--   3. 再启动 API 与 Worker。
-- 迁移若 DROP / CREATE 过函数，函数上的 EXECUTE 授权会随旧函数一起丢失，第 2 步负责补回。
--
-- 用法（由管理员执行；占位符必须替换为实际角色名）：
--   sed -e 's/__MIGRATOR_ROLE__/artifact_migrator/g' \
--       -e 's/__APP_ROLE__/artifact_app/g' \
--       -e 's/__WORKER_ROLE__/artifact_worker/g' deploy/db-roles.sql \
--     | psql -v ON_ERROR_STOP=1
--
-- 口令策略：本脚本不设置口令。由运维以
--   ALTER ROLE artifact_app    PASSWORD :'...';
--   ALTER ROLE artifact_migrator PASSWORD :'...';
-- 注入（从 secret 管理系统获取，禁止写入代码/脚本/镜像）。
-- 本脚本幂等，可以重复执行。

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
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '__MAINTENANCE_ROLE__') THEN
        CREATE ROLE "__MAINTENANCE_ROLE__" LOGIN NOINHERIT;
    END IF;
END $$;

-- 运行角色：schema usage + DML。
GRANT USAGE ON SCHEMA artifact TO "__APP_ROLE__";
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA artifact TO "__APP_ROLE__";
-- 默认权限只对「某一位角色之后创建的对象」生效：必须写明 FOR ROLE 迁移角色，
-- 否则管理员执行本脚本时，迁移角色之后新建的表拿不到任何授权。
ALTER DEFAULT PRIVILEGES FOR ROLE "__MIGRATOR_ROLE__" IN SCHEMA artifact
    GRANT SELECT, INSERT, UPDATE ON TABLES TO "__APP_ROLE__";

-- 审计表对运行角色只允许追加（数据库触发器是第二道防线）。
REVOKE UPDATE, DELETE ON artifact.audit_event FROM "__APP_ROLE__";
ALTER DEFAULT PRIVILEGES FOR ROLE "__MIGRATOR_ROLE__" IN SCHEMA artifact
    REVOKE UPDATE, DELETE ON TABLES FROM "__APP_ROLE__";

-- 持久化创建身份只允许追加（ART-04E §4.1 / #4.8）。即使 ON ALL TABLES 的
-- 历史授权给了 UPDATE，这里也按表显式撤销，身份行在运行期不可改写或删除。
REVOKE UPDATE, DELETE ON artifact.artifact_creation_identity FROM "__APP_ROLE__";

-- ── Worker role：最小权限边界 ──────────────────────────────────────────
-- 迁移对所有函数 REVOKE 了 PUBLIC 的 EXECUTE；这里按函数逐个精确授权，
-- 不授予任何表 DML、不授予 DDL、不授予 CREATE EXTENSION。
GRANT USAGE ON SCHEMA artifact TO "__WORKER_ROLE__";

-- 每一条都是「函数级」授权，绝不使用 ON ALL TABLES / ON ALL FUNCTIONS 的宽授权。
-- 函数签名必须与 migrations/0003_job_coordination_fixes.sql 一致。
GRANT EXECUTE ON FUNCTION artifact.claim_generation_job(text, integer, jsonb) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.heartbeat_generation_job(uuid, uuid, text, integer) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.complete_generation_job_attempt(uuid, uuid, text, text) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.fail_generation_job_attempt(uuid, uuid, text, text, text, boolean) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.confirm_generation_job_cancelled(uuid, uuid, text) TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.recover_expired_generation_jobs() TO "__WORKER_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.purge_expired_generation_job_input() TO "__WORKER_ROLE__";

-- API role 需要调用「创建（同事务输入快照）」与「取消协调」两个受控函数；
-- 其余 Worker 专用函数与内部辅助函数不授予 API role。
GRANT EXECUTE ON FUNCTION artifact.create_generation_job_with_input(
    uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, text, text, integer, integer, integer) TO "__APP_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.request_generation_job_cancel(uuid, uuid, uuid) TO "__APP_ROLE__";

-- 显式兜底：Worker role 不拥有任何业务表的 DML/DDL（即便将来 ALTER DEFAULT PRIVILEGES
-- 放宽，也通过 REVOKE 保持拒绝；受控函数以 SECURITY DEFINER 身份代为写入）。
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA artifact FROM "__WORKER_ROLE__";
ALTER DEFAULT PRIVILEGES FOR ROLE "__MIGRATOR_ROLE__" IN SCHEMA artifact REVOKE ALL ON TABLES FROM "__WORKER_ROLE__";
REVOKE CREATE ON SCHEMA artifact FROM "__WORKER_ROLE__";

-- ── Maintenance role：只拥有 Idempotency-Key 清理函数的执行权 ───────────
-- 清理主体不能直接 DELETE 任何表：删除只发生在 SECURITY DEFINER 函数内部，
-- 且函数固定 search_path、限制批次、只删除「已完成且已过期」的记录。
GRANT USAGE ON SCHEMA artifact TO "__MAINTENANCE_ROLE__";
GRANT EXECUTE ON FUNCTION artifact.purge_expired_idempotency_records(integer) TO "__MAINTENANCE_ROLE__";
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA artifact FROM "__MAINTENANCE_ROLE__";
ALTER DEFAULT PRIVILEGES FOR ROLE "__MIGRATOR_ROLE__" IN SCHEMA artifact
    REVOKE ALL ON TABLES FROM "__MAINTENANCE_ROLE__";
REVOKE CREATE ON SCHEMA artifact FROM "__MAINTENANCE_ROLE__";

-- 迁移角色：schema 全权（owner 身份执行迁移）。
GRANT ALL ON SCHEMA artifact TO "__MIGRATOR_ROLE__";
