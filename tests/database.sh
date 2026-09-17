#!/usr/bin/env bash
set -euo pipefail
umask 077
test_root=$(mktemp -d /tmp/grading-db.XXXXXX)
test_port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')
cleanup() {
  pg_ctl -D "$test_root/data" -m immediate -w stop >/dev/null 2>&1 || true
  rm -rf "$test_root"
}
trap cleanup EXIT
openssl rand -hex 32 > "$test_root/password"
export PGPASSWORD
PGPASSWORD=$(cat "$test_root/password")
initdb -D "$test_root/data" -U postgres --pwfile="$test_root/password" --auth=scram-sha-256 --no-locale --encoding=UTF8 > "$test_root/init.log"
pg_ctl -D "$test_root/data" -l "$test_root/postgres.log" -o "-k $test_root -h 127.0.0.1 -p $test_port" -w start >/dev/null
export PGHOST=127.0.0.1 PGPORT="$test_port" PGUSER=postgres PGDATABASE=grading
createdb grading
psql -X -v ON_ERROR_STOP=1 -f nix/database-roles.sql >/dev/null
for role in grading_owner grading_web grading_operator grading_admin; do
  psql -X -v ON_ERROR_STOP=1 -v role="$role" -v password="$PGPASSWORD" > /dev/null <<'SQL'
ALTER ROLE :"role" PASSWORD :'password';
SQL
done
printf 'postgresql://grading_owner:%s@127.0.0.1:%s/grading\n' "$PGPASSWORD" "$test_port" > "$test_root/owner-url"
export DATABASE_URL="postgresql://grading_owner:$PGPASSWORD@127.0.0.1:$test_port/grading"
export TEST_DATABASE_URL="$DATABASE_URL"
export TEST_WEB_DATABASE_URL="postgresql://grading_web:$PGPASSWORD@127.0.0.1:$test_port/grading"
export TEST_ADMIN_DATABASE_URL="postgresql://grading_admin:$PGPASSWORD@127.0.0.1:$test_port/grading"
export TEST_OPERATOR_DATABASE_URL="postgresql://grading_operator:$PGPASSWORD@127.0.0.1:$test_port/grading"
export TEST_APP_KEY="$test_root/app.pem"
export TEST_ARTIFACT_ROOT="$test_root/artifacts"
mkdir -p "$TEST_ARTIFACT_ROOT"
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$TEST_APP_KEY" 2>/dev/null
if [[ "${1:-test}" == prepare ]]; then
  export SQLX_OFFLINE=false
fi
cargo run --locked --offline -p grading-cli -- --database-url-file "$test_root/owner-url" migrate
PGUSER=grading_owner psql -X -v ON_ERROR_STOP=1 -f nix/database-grants.sql >/dev/null
if [[ "${1:-test}" == prepare ]]; then
  SQLX_OFFLINE=false cargo sqlx prepare --workspace -- --all-targets
else
  cargo test --locked --offline --workspace
  PGUSER=grading_owner pg_dump --format=custom --no-owner --no-acl grading > "$test_root/database.dump"
  createdb grading_restore
  pg_restore --exit-on-error --no-owner --no-acl -d grading_restore "$test_root/database.dump"
  original_count=$(psql -X -At -c 'SELECT count(*) FROM grading_runs')
  restored_count=$(psql -X -At -d grading_restore -c 'SELECT count(*) FROM grading_runs')
  test "$original_count" = "$restored_count"
  cp -a "$TEST_ARTIFACT_ROOT" "$test_root/restored-artifacts"
  psql -X -At -d grading_restore -c 'SELECT digest FROM artifacts ORDER BY digest' > "$test_root/digests"
  while IFS= read -r digest; do
    test -f "$test_root/restored-artifacts/$digest"
    printf '%s  %s\n' "$digest" "$test_root/restored-artifacts/$digest" | sha256sum --check --status
  done < "$test_root/digests"
  echo "Database and artifact restore smoke test passed."
fi
