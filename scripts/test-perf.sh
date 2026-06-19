#!/usr/bin/env bash
# Performance benchmark for hsearch on a synthetic dataset.
#
# Measures: bulk-load time, bm25 index build time, search latency (p50/p95/p99)
# for the real autocomplete query shape, single-row insert+commit latency (the
# live aminsert -> PRE_COMMIT flush write path), and on-disk index size.
#
# Requires the dev container `hsearch-dev-c` with a completed `cargo pgrx package`
# (staged files under target/release/hsearch-pg18). Tunables via env:
#   ROWS (default 50000)  SEARCH_ITERS (2000)  INSERT_ITERS (300)
set -uo pipefail

ROWS="${ROWS:-50000}"
SEARCH_ITERS="${SEARCH_ITERS:-2000}"
INSERT_ITERS="${INSERT_ITERS:-300}"
IMAGE=hsearch-pg
DEVC=hsearch-dev-c
CTR=hsearch-perf

cleanup() { docker rm -f "$CTR" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# 1) hsearch-equipped postgres image from the staged package files.
STAGE="$(mktemp -d)"
if ! docker cp "${DEVC}:/home/builder/target/release/hsearch-pg18/." "${STAGE}/stage" >/dev/null 2>&1; then
  echo "  FAIL: staged hsearch files not found in ${DEVC} (run 'cargo pgrx package' first)"; exit 1
fi
cat > "${STAGE}/Dockerfile" <<'EOF'
FROM postgres:18-trixie
COPY stage/ /
EOF
docker build -t "$IMAGE" "$STAGE" >/dev/null 2>&1 || { echo "  FAIL: image build"; exit 1; }

# 2) Start postgres with realistic settings.
docker rm -f "$CTR" >/dev/null 2>&1 || true
docker run -d --name "$CTR" \
  -e POSTGRES_PASSWORD=pw -e POSTGRES_USER=postgres -e POSTGRES_DB=app \
  -e POSTGRES_INITDB_ARGS="--locale=C.UTF-8" "$IMAGE" \
  -c shared_preload_libraries=hsearch -c shared_buffers=512MB -c work_mem=64MB \
  -c maintenance_work_mem=512MB -c max_wal_size=4GB -c checkpoint_timeout=30min \
  -c listen_addresses=127.0.0.1 >/dev/null

for _ in $(seq 1 90); do
  docker exec "$CTR" pg_isready -h 127.0.0.1 -U postgres -d app >/dev/null 2>&1 && break; sleep 1
done

echo
echo "============ hsearch perf (ROWS=${ROWS}, SEARCH_ITERS=${SEARCH_ITERS}, INSERT_ITERS=${INSERT_ITERS}) ============"

# 3) Seed + build + search-latency (server-side timing — no client round-trip noise).
docker exec -i "$CTR" psql -U postgres -d app -q -v ON_ERROR_STOP=1 \
  -v rows="$ROWS" -v siters="$SEARCH_ITERS" <<'PERFSQL'
CREATE EXTENSION IF NOT EXISTS hsearch;
DROP TABLE IF EXISTS items;
CREATE TABLE items (_id varchar(24) PRIMARY KEY, name text, summary text, category_path text);

\echo '--- bulk load ---'
\timing on
INSERT INTO items (_id, name, summary, category_path)
SELECT lpad(to_hex(g),24,'0'),
       w[1+floor(random()*30)::int]||' '||w[1+floor(random()*30)::int]||' '||w[1+floor(random()*30)::int],
       w[1+floor(random()*30)::int]||' '||w[1+floor(random()*30)::int],
       w[1+floor(random()*30)::int]||' '||w[1+floor(random()*30)::int]
FROM generate_series(1, :rows) g
CROSS JOIN (SELECT ARRAY['kavos','aparatas','dviratis','kalnu','azuolinis','stalas','kavinukas',
                         'turkiskas','varinis','baldas','espresso','aliuminis','masyvus','lengvas',
                         'virtuve','technika','sportas','dviraciai','indai','stalai','lova','spinta',
                         'kede','seimos','vaiku','zaislai','knygos','telefonas','kompiuteris','ekranas'] AS w) p;
\timing off

\echo '--- index build ---'
\timing on
CREATE INDEX search_idx ON items USING bm25 (
  _id,
  (name::hyper.ngram(2,5,'ascii_folding=true')),
  (summary::hyper.ngram(2,5,'ascii_folding=true')),
  (category_path::hyper.ngram(2,5,'ascii_folding=true'))
) WITH (key_field='_id');
\timing off

\echo
\echo 'SIZES:'
SELECT pg_size_pretty(pg_relation_size('items'))      AS heap,
       pg_size_pretty(pg_relation_size('search_idx')) AS bm25_index;

SET enable_seqscan = off;
SET bench.siters = :siters;
CREATE TEMP TABLE bench_times(ms double precision);
DO $bench$
DECLARE
  i int; t0 timestamptz; w text;
  words text[] := ARRAY['kav','dvir','stal','azuo','kavi','turk','vari','bald','espr','aliu',
                        'virtu','sport','indai','lova','spint','kede','zaisl','knyg','telef','kompiu'];
  n int := current_setting('bench.siters')::int;
BEGIN
  FOR i IN 1..n LOOP
    w := words[1 + floor(random()*array_length(words,1))::int];
    t0 := clock_timestamp();
    PERFORM _id, hyper.score(_id) FROM items
      WHERE (name &&& w OR summary &&& w OR category_path &&& w)
      ORDER BY hyper.score(_id) DESC LIMIT 10;
    INSERT INTO bench_times VALUES (extract(epoch from clock_timestamp()-t0)*1000);
  END LOOP;
END $bench$;

\echo
\echo 'SEARCH LATENCY (ms) — autocomplete shape over the bm25 index:'
SELECT count(*) AS n,
  round(avg(ms)::numeric,3) AS mean,
  round((percentile_cont(0.50) WITHIN GROUP (ORDER BY ms))::numeric,3) AS p50,
  round((percentile_cont(0.95) WITHIN GROUP (ORDER BY ms))::numeric,3) AS p95,
  round((percentile_cont(0.99) WITHIN GROUP (ORDER BY ms))::numeric,3) AS p99,
  round(max(ms)::numeric,3) AS max
FROM bench_times;
PERFSQL

# 4) Single-row insert+commit latency (each INSERT is its own transaction ->
#    exercises aminsert + the PRE_COMMIT flush per write).
echo
echo "INSERT+COMMIT LATENCY — ${INSERT_ITERS} single-row transactions:"
docker exec -i "$CTR" bash -s "$INSERT_ITERS" <<'PERFINS'
set -e
N="$1"
psql -U postgres -d app -tAq -c \
  "SELECT format('INSERT INTO items VALUES (%L,%L,%L,%L) ON CONFLICT DO NOTHING;', lpad(to_hex(g+900000000),24,'0'), 'naujas '||g, 'aprasymas '||g, 'kategorija '||g) FROM generate_series(1,$N) g" \
  > /tmp/ins.sql
t0=$(date +%s%3N)
psql -U postgres -d app -q -f /tmp/ins.sql >/dev/null
t1=$(date +%s%3N)
ms=$((t1 - t0))
awk -v ms="$ms" -v n="$N" 'BEGIN{printf "  %d inserts in %d ms  =>  %.2f ms/insert,  %.0f inserts/sec\n", n, ms, ms/n, n*1000.0/ms}'
PERFINS

echo
echo "  Notes: search latency is measured with the bm25 index forced on (production uses it on a"
echo "  large table). Insert latency includes the per-commit WAL-logged page sync."
