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
  cat > "$test_root/course.toml" <<'TOML'
schema_version = 1
[course]
id = "local-file-course"
title = "First local title"
github_organization = "fixture-org"
timezone = "UTC"
TOML
  printf '%s\n' "$TEST_OPERATOR_DATABASE_URL" > "$test_root/operator-url"
  env -u GRADING_GITHUB_CONFIG target/debug/gradingctl --database-url-file "$test_root/operator-url" course apply "$test_root/course.toml" --dry-run
  test "$(psql -X -At -c "SELECT count(*) FROM courses WHERE id='local-file-course'")" = 0
  env -u GRADING_GITHUB_CONFIG target/debug/gradingctl --database-url-file "$test_root/operator-url" course apply "$test_root/course.toml"
  sed -i 's/First local title/Edited local title/' "$test_root/course.toml"
  env -u GRADING_GITHUB_CONFIG target/debug/gradingctl --database-url-file "$test_root/operator-url" course apply "$test_root/course.toml"
  test "$(psql -X -At -c "SELECT title FROM courses WHERE id='local-file-course'")" = 'Edited local title'
  cat > "$test_root/exercises.toml" <<'TOML'
schema_version = 1
course = "local-file-course"
TOML
  env -u GRADING_GITHUB_CONFIG target/debug/gradingctl --database-url-file "$test_root/operator-url" exercise apply "$test_root/exercises.toml" --reason 'Empty catalog preview' --dry-run
  cat > "$test_root/retire.toml" <<'TOML'
schema_version = 1
course = "registered-course"
TOML
  python3 - "$test_root/operator-url" "$test_root/retire.toml" <<'PY'
import os
import pty
import subprocess
import sys

command = ["target/debug/gradingctl", "--database-url-file", sys.argv[1],
           "exercise", "apply", sys.argv[2], "--reason", "Confirmation fixture"]
env = dict(os.environ)
env.pop("GRADING_GITHUB_CONFIG", None)

def active():
    return subprocess.check_output(["psql", "-X", "-At", "-c",
        "SELECT count(*) FROM assignments WHERE course_id='registered-course' AND NOT archived"], text=True).strip()

assert active() == "1"
preview = subprocess.run(command + ["--dry-run"], stdin=subprocess.DEVNULL, capture_output=True, env=env)
assert preview.returncode == 0 and b"WARNING" in preview.stderr
refused = subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, env=env)
assert refused.returncode != 0 and b"interactive confirmation" in refused.stderr
for answer, expected in [(b"no\n", "1"), (b"REMOVE EXERCISES\n", "0")]:
    master, slave = pty.openpty()
    process = subprocess.Popen(command, stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    os.close(slave)
    os.write(master, answer)
    stdout, stderr = process.communicate(timeout=30)
    os.close(master)
    assert b"WARNING" in stderr and b"REMOVE registered-course/echo" in stderr
    assert (process.returncode == 0) == (expected == "0"), (stdout, stderr)
    assert active() == expected
print("Removal preview, terminal confirmation and cancellation checks passed.")
PY
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
