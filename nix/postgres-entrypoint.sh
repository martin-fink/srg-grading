export PGDATA="${PGDATA:-/var/lib/postgresql/data}"
export PGHOST=/run/postgresql PGUSER=postgres
export PGPASSWORD
PGPASSWORD=$(cat /run/secrets/postgres-password)
if [[ ! -f "$PGDATA/PG_VERSION" ]]; then
  initdb -D "$PGDATA" --username=postgres --pwfile=/run/secrets/postgres-password --auth=scram-sha-256 --encoding=UTF8 --no-locale
fi
# initdb permits only loopback TCP clients. Application containers connect from
# other addresses; deployment networking controls which clients can reach us.
# Keep Unix-socket bootstrap authenticated, and never expose postgres remotely.
cat > "$PGDATA/pg_hba.conf" <<'HBA'
local all all scram-sha-256
host grading grading_owner,grading_web,grading_operator,grading_admin 0.0.0.0/0 scram-sha-256
host grading grading_owner,grading_web,grading_operator,grading_admin ::/0 scram-sha-256
HBA
pg_ctl -D "$PGDATA" -o "-k /run/postgresql -h ''" -w start
trap 'pg_ctl -D "$PGDATA" -m fast -w stop' EXIT
if [[ $(psql -X -At -d postgres -c "SELECT count(*) FROM pg_database WHERE datname='grading'") == 0 ]]; then
  createdb grading
fi
for role in grading_owner grading_web grading_operator grading_admin; do
  export BOOTSTRAP_ROLE="$role" BOOTSTRAP_PASSWORD
  BOOTSTRAP_PASSWORD=$(cat "/run/secrets/$role-password")
  psql -X -v ON_ERROR_STOP=1 -d grading <<'SQL'
\getenv role BOOTSTRAP_ROLE
\getenv password BOOTSTRAP_PASSWORD
SELECT format('CREATE ROLE %I LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION', :'role') WHERE NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname=:'role') \gexec
ALTER ROLE :"role" PASSWORD :'password';
SQL
done
unset BOOTSTRAP_PASSWORD BOOTSTRAP_ROLE
psql -X -v ON_ERROR_STOP=1 -d grading <<'SQL'
REVOKE ALL ON DATABASE grading FROM PUBLIC;
GRANT CONNECT ON DATABASE grading TO grading_owner,grading_web,grading_operator,grading_admin;
REVOKE CREATE ON SCHEMA public FROM PUBLIC;
ALTER SCHEMA public OWNER TO grading_owner;
SQL
pg_ctl -D "$PGDATA" -m fast -w stop
trap - EXIT
unset PGPASSWORD
exec postgres -D "$PGDATA" -k /run/postgresql -h '*' -c password_encryption=scram-sha-256 -c max_connections=50 -c shared_buffers=256MB -c log_statement=none
