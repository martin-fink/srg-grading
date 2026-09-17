#!/usr/bin/env bash
set -euo pipefail
umask 077
: "${GRADING_STATE:?}" "${BORG_REPO:?}" "${BORG_PASSCOMMAND:?}"
backup_dir=$(mktemp -d)
trap 'rm -rf "$backup_dir"' EXIT
# Sources and reports are immutable and durable before their DB references commit.
docker compose -f /opt/grading/compose.yaml exec -T database sh -c 'PGPASSWORD=$(cat /run/secrets/grading_owner-password) pg_dump -h /run/postgresql -U grading_owner --format=custom --no-owner --no-acl grading' > "$backup_dir/database.dump"
borg create --stats "::grading-{now:%Y-%m-%dT%H:%M:%S}" "$backup_dir/database.dump" "$GRADING_STATE/artifacts"
borg prune --glob-archives 'grading-*' --keep-daily 14 --keep-weekly 8 --keep-monthly 12
borg compact
date -u +%s > "$GRADING_STATE/backup-success.timestamp"
