#!/usr/bin/env bash
# Restore ONLY into a fresh random database, never an existing destination.
set -euo pipefail
umask 077
if [[ $# != 1 || -z ${PGDATABASE:-} ]]; then
    echo 'Usage: PGDATABASE=postgres [other PG* settings] scripts/restore-check.sh backup.dump' >&2
    exit 2
fi
restore_archive=$(realpath "$1")
(cd "$(dirname "$restore_archive")" && sha256sum --check "$(basename "$restore_archive").sha256")
restore_db="gameboxd_restore_$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
# Only this invocation's created database can ever be dropped.
created=false
cleanup() {
    if $created; then dropdb --if-exists "$restore_db"; fi
}
trap cleanup EXIT
createdb --template=template0 "$restore_db"
created=true
PGDATABASE="$restore_db" pg_restore --exit-on-error --single-transaction --no-owner --no-acl --dbname="$restore_db" "$restore_archive"
PGDATABASE="$restore_db" psql -X --set=ON_ERROR_STOP=1 <<'SQL'
DO $$ BEGIN
 IF (SELECT count(*) FROM gameboxd._sqlx_migrations WHERE success) = 0
 OR (SELECT count(*) FROM gameboxd_auth._sqlx_migrations WHERE success) = 0 THEN
   RAISE EXCEPTION 'Missing migration history';
 END IF;
 IF EXISTS (SELECT 1 FROM pg_constraint c JOIN pg_namespace n ON n.oid=c.connamespace
            WHERE n.nspname IN ('gameboxd','gameboxd_auth') AND NOT c.convalidated) THEN
   RAISE EXCEPTION 'Unvalidated restored constraint';
 END IF;
END $$;
SELECT count(*) AS restored_profiles FROM gameboxd.profiles;
SELECT count(*) AS restored_receipts FROM gameboxd.request_receipts;
SELECT count(*) AS restored_public_reviews FROM gameboxd.public_reviews;
SQL
echo 'Restore, migration histories, constraints and public projection smoke checks passed.'
# Optional controlled application smoke hook; PGDATABASE points at disposable DB.
# It must use disabled email/provider egress and must never serve public traffic.
if [[ -n ${RESTORE_CHECK_HOOK:-} ]]; then
    PGDATABASE="$restore_db" "$RESTORE_CHECK_HOOK"
fi
