-- deploy/db-roles.sql — 迁移角色与运行角色分离（ART-02 §4.3 / ART-01 §3.2）。
--
-- __MIGRATOR_ROLE__：执行 `artifact-api migrate`（DDL）。schema 由迁移创建，owner 归该角色。
-- __APP_ROLE__    ：服务运行时连接。仅 SELECT/INSERT/UPDATE；无 DDL、无 DELETE
--                   （删除一律软删除）、审计表不可 UPDATE。
--
-- 用法（由管理员执行，替换占位符）：
--   psql -v ON_ERROR_STOP=1 -f deploy/db-roles.sql \
--     -v migrator=artifact_migrator -v app=artifact_app
-- 或先 sed 替换 __MIGRATOR_ROLE__ / __APP_ROLE__ 再执行。
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

-- 迁移角色：schema 全权（owner 身份执行迁移）。
GRANT ALL ON SCHEMA artifact TO "__MIGRATOR_ROLE__";
