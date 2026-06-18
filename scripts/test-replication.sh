#!/usr/bin/env bash
# Streaming-replication test for hsearch.
#
# Proves the headline claim: a bm25 index built on the PRIMARY is present and
# queryable on a physical-streaming STANDBY (because the index lives in WAL-logged
# Postgres pages), and incremental updates stream too.
#
# Requires: the dev container `hsearch-dev-c` with a completed
# `cargo pgrx package` (staged files under target/release/hsearch-pg18).
set -uo pipefail

NET=hsearch-repl-net
PRIMARY=hsearch-primary
STANDBY=hsearch-standby
IMAGE=hsearch-pg
DEVC=hsearch-dev-c
PGPASS=replpw
fail=0

cleanup() {
  docker rm -f "$PRIMARY" "$STANDBY" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT

say() { printf '  %s\n' "$*"; }

# 1) Build an hsearch-equipped postgres image from the staged package files.
STAGE="$(mktemp -d)"
if ! docker cp "${DEVC}:/home/builder/target/release/hsearch-pg18/." "${STAGE}/stage" >/dev/null 2>&1; then
  echo "  FAIL: could not copy staged hsearch files from ${DEVC} (run 'cargo pgrx package' first)"
  exit 1
fi
cat > "${STAGE}/Dockerfile" <<'EOF'
FROM postgres:18-trixie
COPY stage/ /
EOF
docker build -t "$IMAGE" "$STAGE" >/dev/null 2>&1 || { echo "  FAIL: image build"; exit 1; }
docker network create "$NET" >/dev/null 2>&1 || true

# 2) Start the primary (streaming-replication ready, hsearch preloaded).
docker rm -f "$PRIMARY" "$STANDBY" >/dev/null 2>&1 || true
docker run -d --name "$PRIMARY" --network "$NET" \
  -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=postgres -e POSTGRES_DB=app \
  -e POSTGRES_HOST_AUTH_METHOD=trust -e POSTGRES_INITDB_ARGS="--locale=C.UTF-8" \
  "$IMAGE" \
  -c shared_preload_libraries=hsearch -c wal_level=replica \
  -c max_wal_senders=10 -c max_replication_slots=10 -c hot_standby=on \
  -c listen_addresses='*' >/dev/null

# enable_seqscan=off via PGOPTIONS so every &&& search goes through the bm25 INDEX
# (a seq scan would read the replicated heap and pass even if the index didn't replicate).
pexec() { docker exec -e PGOPTIONS="-c enable_seqscan=off" "$1" psql -U postgres -d app -tAc "$2" 2>/dev/null; }
wait_ready() {
  # Use TCP (-h 127.0.0.1): the image's temporary init server listens on the unix
  # socket only, so a TCP check only succeeds once the REAL server is up.
  for _ in $(seq 1 90); do
    docker exec "$1" pg_isready -h 127.0.0.1 -p 5432 -U postgres -d app >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}

wait_ready "$PRIMARY" || { echo "  FAIL: primary not ready"; exit 1; }
docker exec -i "$PRIMARY" psql -U postgres -d app -v ON_ERROR_STOP=1 <<SQL || { echo "  FAIL: replication setup"; exit 1; }
CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD '${PGPASS}';
SQL
docker exec "$PRIMARY" bash -lc "echo 'host replication all 0.0.0.0/0 trust' >> \$PGDATA/pg_hba.conf && echo 'host all all 0.0.0.0/0 trust' >> \$PGDATA/pg_hba.conf"
docker exec "$PRIMARY" psql -U postgres -d app -c "SELECT pg_reload_conf()" >/dev/null

# 3) Build the bm25 index + data on the primary, committed.
docker exec -i "$PRIMARY" psql -U postgres -d app -v ON_ERROR_STOP=1 <<'SQL' || { echo "  FAIL: primary build"; exit 1; }
CREATE EXTENSION hsearch;
CREATE TABLE items (_id varchar(24) PRIMARY KEY, name text, summary text);
INSERT INTO items VALUES
  ('000000000000000000000001','Kavos aparatas DeLonghi','espresso'),
  ('000000000000000000000002','Dviratis kalnu','aliuminis'),
  ('000000000000000000000003','Ąžuolinis stalas','baldas');
CREATE INDEX search_idx ON items USING bm25
  (_id, (name::hyper.ngram(2,5,'ascii_folding=true')), (summary::hyper.ngram(2,5,'ascii_folding=true')))
  WITH (key_field='_id');
SET enable_seqscan = off;
SQL
prim_hits="$(pexec "$PRIMARY" "SELECT count(*) FROM items WHERE name &&& 'kava'")"
say "primary: 'kava' hits = ${prim_hits}"
[ "${prim_hits:-0}" -ge 1 ] || { echo "  FAIL: primary search returned no hits"; fail=1; }

# 4) Clone a standby via pg_basebackup and start it.
docker run -d --name "$STANDBY" --network "$NET" -u postgres --entrypoint bash \
  -e PGPASSWORD="$PGPASS" "$IMAGE" -lc '
    set -e
    rm -rf "$PGDATA"/* 2>/dev/null || true
    until pg_isready -h '"$PRIMARY"' -U postgres; do sleep 1; done
    pg_basebackup -h '"$PRIMARY"' -U replicator -D "$PGDATA" -X stream -R -P
    touch "$PGDATA"/standby.signal
    exec /usr/lib/postgresql/18/bin/postgres -D "$PGDATA" \
      -c shared_preload_libraries=hsearch -c hot_standby=on -c listen_addresses=127.0.0.1
  ' >/dev/null

wait_ready "$STANDBY" || { echo "  FAIL: standby not ready"; exit 1; }
in_recovery="$(pexec "$STANDBY" "SELECT pg_is_in_recovery()")"
say "standby in recovery = ${in_recovery}"
[ "$in_recovery" = "t" ] || { echo "  FAIL: standby is not a replica"; fail=1; }

# 5) The index built on the primary must be queryable on the standby (replicated via WAL).
for _ in $(seq 1 30); do
  std_hits="$(pexec "$STANDBY" "SELECT count(*) FROM items WHERE name &&& 'kava'")"
  [ "${std_hits:-0}" -ge 1 ] && break
  sleep 1
done
say "standby: 'kava' hits = ${std_hits} (expected ${prim_hits})"
[ "${std_hits:-0}" = "${prim_hits:-X}" ] || { echo "  FAIL: standby search != primary (index did not replicate)"; fail=1; }

# 6) Incremental update on the primary must stream to the standby.
docker exec "$PRIMARY" psql -U postgres -d app -v ON_ERROR_STOP=1 -c \
  "INSERT INTO items VALUES ('000000000000000000000004','Kavinukas turkiškas','varinis')" >/dev/null 2>&1
for _ in $(seq 1 30); do
  std4="$(pexec "$STANDBY" "SELECT count(*) FROM items WHERE name &&& 'turkiskas'")"
  [ "${std4:-0}" -ge 1 ] && break
  sleep 1
done
say "standby sees streamed insert ('turkiskas' hits) = ${std4}"
[ "${std4:-0}" -ge 1 ] || { echo "  FAIL: incremental index update did not stream to standby"; fail=1; }

# 7) Accent-folded search works on the standby too.
std_fold="$(pexec "$STANDBY" "SELECT count(*) FROM items WHERE name &&& 'azuol'")"
say "standby accent-folded ('azuol' -> Ąžuolinis) hits = ${std_fold}"
[ "${std_fold:-0}" -ge 1 ] || { echo "  FAIL: accent-folded standby search"; fail=1; }

echo
if [ "$fail" -eq 0 ]; then
  echo "  PASS: bm25 index replicates over physical streaming and is queryable on the standby"
else
  echo "  FAIL: replication test had failures"
fi
exit "$fail"
