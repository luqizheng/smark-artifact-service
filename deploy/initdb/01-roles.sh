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
SQL

echo "dev roles initialized (migrator=${POSTGRES_USER}, app=${ARTIFACT_APP_DB_USER})"
