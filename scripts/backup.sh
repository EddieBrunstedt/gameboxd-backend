#!/usr/bin/env bash
# PG* libpq environment (prefer PGSERVICE/PGPASSFILE); never put secrets in argv.
set -euo pipefail
umask 077
if [[ $# != 1 || -z ${PGDATABASE:-} ]]; then
    echo 'Usage: PGDATABASE=gameboxd [other PG* settings] scripts/backup.sh /encrypted/backup/directory' >&2
    exit 2
fi
backup_dir=$1
[[ -d "$backup_dir" ]] || { echo 'Backup directory must already exist on encrypted storage' >&2; exit 2; }
backup_dir=$(cd "$backup_dir" && pwd)
backup_tmp=$(mktemp "$backup_dir/.gameboxd-XXXXXX.dump")
trap 'rm -f -- "$backup_tmp"' EXIT
# Entire database: auth, app, both migration histories, extensions and sequences.
pg_dump --format=custom --no-owner --no-acl --file="$backup_tmp"
pg_restore --list "$backup_tmp" >/dev/null
backup_name="gameboxd-$(date -u +%Y%m%dT%H%M%SZ)-${backup_tmp##*-}"
backup_path="$backup_dir/$backup_name"
mv -- "$backup_tmp" "$backup_path"
(cd "$backup_dir" && sha256sum "$backup_name" > "$backup_name.sha256")
echo "$backup_path"
