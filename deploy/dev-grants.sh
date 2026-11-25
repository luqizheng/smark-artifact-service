#!/bin/bash
# deploy/dev-grants.sh — 本地 compose 专用（非生产）：在迁移之后创建运行角色并应用授权。
#
# 为什么不在 postgres 的 initdb 阶段做授权：initdb 先于迁移执行，那时 schema artifact
# 与受控函数都还不存在。角色与授权必须在 migrate 之后应用，并且每次迁移后都要重新应用
# （迁移重建函数会丢失其 EXECUTE 授权）。
#
# 幂等，可重复执行。生产使用 deploy/db-roles.sql + secret 管理系统注入口令，顺序相同：
# 先迁移，后授权，再启动服务。
#
# 环境变量：
#   PGHOST / PGPORT / PGDATABASE / PGUSER / PGPASSWORD   以迁移角色连接（需要能建角色）
#   ARTIFACT_MIGRATOR_DB_USER / ARTIFACT_APP_DB_USER / ARTIFACT_WORKER_DB_USER
#   ARTIFACT_APP_DB_PASSWORD  / ARTIFACT_WORKER_DB_PASSWORD   本地 dev 口令
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${PGHOST:?PGHOST is required}"
: "${PGDATABASE:?PGDATABASE is required}"
: "${PGUSER:?PGUSER is required}"
: "${ARTIFACT_MIGRATOR_DB_USER:?ARTIFACT_MIGRATOR_DB_USER is required}"
: "${ARTIFACT_APP_DB_USER:?ARTIFACT_APP_DB_USER is required}"
: "${ARTIFACT_APP_DB_PASSWORD:?ARTIFACT_APP_DB_PASSWORD is required}"
: "${ARTIFACT_WORKER_DB_USER:?ARTIFACT_WORKER_DB_USER is required}"
: "${ARTIFACT_WORKER_DB_PASSWORD:?ARTIFACT_WORKER_DB_PASSWORD is required}"
# 清理主体（Idempotency-Key 过期清理）。默认值保持本地 compose 无需额外配置。
ARTIFACT_MAINTENANCE_DB_USER="${ARTIFACT_MAINTENANCE_DB_USER:-artifact_maintenance}"

sed -e "s/__MIGRATOR_ROLE__/${ARTIFACT_MIGRATOR_DB_USER}/g" \
    -e "s/__APP_ROLE__/${ARTIFACT_APP_DB_USER}/g" \
    -e "s/__WORKER_ROLE__/${ARTIFACT_WORKER_DB_USER}/g" \
    -e "s/__MAINTENANCE_ROLE__/${ARTIFACT_MAINTENANCE_DB_USER}/g" \
    "${here}/db-roles.sql" | psql -v ON_ERROR_STOP=1

# db-roles.sql 不设置口令；本地 dev 的口令来自 compose env。
psql -v ON_ERROR_STOP=1 \
    -v app_user="$ARTIFACT_APP_DB_USER" -v app_password="$ARTIFACT_APP_DB_PASSWORD" \
    -v worker_user="$ARTIFACT_WORKER_DB_USER" -v worker_password="$ARTIFACT_WORKER_DB_PASSWORD" <<'SQL'
ALTER ROLE :"app_user" PASSWORD :'app_password';
ALTER ROLE :"worker_user" PASSWORD :'worker_password';
SQL

echo "roles and grants applied (migrator=${ARTIFACT_MIGRATOR_DB_USER}, app=${ARTIFACT_APP_DB_USER}, worker=${ARTIFACT_WORKER_DB_USER})"
