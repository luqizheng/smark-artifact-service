#!/bin/bash
# 本地开发 compose 专用：初始化开发角色与权限（值均来自 compose env，非生产凭据）。
# 生产环境请使用 deploy/db-roles.sql 并由 secret 管理系统注入口令。
set -euo pipefail

psql -v ON_ERROR_STOP=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<SQL
CREATE ROLE ${ARTIFACT_APP_DB_USER} LOGIN PASSWORD '${ARTIFACT_APP_DB_PASSWORD}';
GRANT USAGE ON SCHEMA artifact TO ${ARTIFACT_APP_DB_USER};
GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA artifact TO ${ARTIFACT_APP_DB_USER};
ALTER DEFAULT PRIVILEGES IN SCHEMA artifact GRANT SELECT, INSERT, UPDATE ON TABLES TO ${ARTIFACT_APP_DB_USER};
REVOKE UPDATE, DELETE ON artifact.audit_event FROM ${ARTIFACT_APP_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.create_generation_job_with_input(uuid, uuid, uuid, uuid, text, jsonb, jsonb, text, char(64), char(64), text, integer) TO ${ARTIFACT_APP_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.request_generation_job_cancel(uuid) TO ${ARTIFACT_APP_DB_USER};

-- 独立 Worker 角色：仅受控函数 EXECUTE，无任何业务表 DML/DDL，NOINHERIT。
-- 本地 dev 口令来自 compose env，与 API 角色不同；生产由 deploy/db-roles.sql + secret 管理。
CREATE ROLE ${ARTIFACT_WORKER_DB_USER} LOGIN NOINHERIT PASSWORD '${ARTIFACT_WORKER_DB_PASSWORD}';
GRANT USAGE ON SCHEMA artifact TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.claim_generation_job(text, integer) TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.heartbeat_generation_job(uuid, uuid, text, integer) TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.complete_generation_job_attempt(uuid, uuid, text, char(64)) TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.fail_generation_job_attempt(uuid, uuid, text, text, text, boolean) TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.confirm_generation_job_cancelled(uuid, uuid, text) TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.recover_expired_generation_jobs() TO ${ARTIFACT_WORKER_DB_USER};
GRANT EXECUTE ON FUNCTION artifact.purge_expired_generation_job_input() TO ${ARTIFACT_WORKER_DB_USER};
REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA artifact FROM ${ARTIFACT_WORKER_DB_USER};
ALTER DEFAULT PRIVILEGES IN SCHEMA artifact REVOKE ALL ON TABLES FROM ${ARTIFACT_WORKER_DB_USER};
REVOKE CREATE ON SCHEMA artifact FROM ${ARTIFACT_WORKER_DB_USER};
SQL

echo "dev roles initialized (migrator=${POSTGRES_USER}, app=${ARTIFACT_APP_DB_USER}, worker=${ARTIFACT_WORKER_DB_USER})"
