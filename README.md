# hsearch

**BM25 full-text search for PostgreSQL 18** — a `pgrx` extension that adds a custom
`bm25` index access method backed by the [Tantivy](https://github.com/quickwit-oss/tantivy)
search engine, with the index stored in **WAL-logged Postgres pages** so it replicates
byte-for-byte over physical streaming replication using [pg_replica](https://github.com/hyperiondb/hyperiondb).

## Features

- **`bm25` index access method** — `CREATE INDEX ... USING bm25 (...) WITH (key_field='_id')`.
- **`&&&` match operator** — `text_column &&& 'query'`, index-accelerated, composes with normal
  SQL / jsonb / PostGIS predicates in the same `WHERE` (via Postgres bitmap scans).
- **`hyper.ngram(min,max,'ascii_folding=true')`** — a tokenizer cast usable inside a `bm25`
  index definition: `(name::hyper.ngram(2,5,'ascii_folding=true'))`.
- **`hyper.score(_id) → double precision`** — BM25 relevance of the current row for the active
  `bm25` scan; valid in the `SELECT` list and `ORDER BY score DESC`.
- **ASCII-folding ngram tokenizer** — Unicode NFKD + combining-mark stripping (`ą`→`a`,
  `ž`→`z`, `ė`→`e`, …) so Lithuanian/diacritic content is matched accent-insensitively, plus
  a few common Latin extras (`ł`→`l`, `ß`→`ss`, …). Index, query, and recheck share one
  tokenizer, so they always agree.
- **WAL-logged storage** — the index lives in the index relation's own Postgres pages, written
  through the buffer manager + generic WAL. It is carried by physical streaming replication and
  recovered by crash recovery, exactly like a btree.
- **MVCC-correct** — bitmap scans return candidate `ctid`s with recheck; heap visibility +
  operator recheck filter dead/updated tuples. `VACUUM` (`ambulkdelete`) reclaims dead entries.
- **`key_field` reloption** — the Mongo-style 24-hex `VARCHAR(24)` `_id` is stored per document
  and used as the scoring key.
- `CREATE EXTENSION hsearch` / `ALTER EXTENSION hsearch UPDATE`, `REINDEX`, and
  `hyper.reindex_all()` all work.

## Install

```sh
curl -fsSL https://hyperiondb.github.io/search/install.sh | sudo bash
sudo apt-get install -y postgresql-18-hsearch
```

`hsearch` installs its hooks at server start, so add it to `shared_preload_libraries`
(alongside whatever else you preload) and restart:

```
shared_preload_libraries = 'hsearch,pg_cron,pg_replica'
```

```sql
CREATE EXTENSION hsearch;
```

## Usage

```sql
CREATE TABLE items (
  _id     varchar(24) PRIMARY KEY,
  name    text,
  summary text
);

CREATE INDEX search_idx ON items
  USING bm25 (
    _id,
    (name::hyper.ngram(2,5,'ascii_folding=true')),
    (summary::hyper.ngram(2,5,'ascii_folding=true'))
  ) WITH (key_field='_id');

-- mixes freely with normal predicates, ordered + paginated by BM25 score:
SELECT _id, hyper.score(_id) AS score
FROM items
WHERE (name &&& 'word1' OR summary &&& 'word1')
  AND (name &&& 'word2' OR summary &&& 'word2')
ORDER BY score DESC
LIMIT 10 OFFSET 0;
```

`key_field` must name one of the indexed columns (the key/`_id`); every other indexed column
is treated as an ngram-tokenized text field. The `&&&` query word is tokenized into the same
ngrams as the indexed text and matches documents that share any of them, ranked by BM25.

## Architecture

Vanilla Tantivy writes its own files and uses worker **threads**, and Postgres APIs are not
thread-safe — so Tantivy never touches Postgres storage directly. Instead:

- **Persistent, replicated truth:** the index bytes live in the `bm25` index relation's own
  pages. `blockstore.rs` implements a tiny WAL-logged file store over those pages (superblock +
  free-list allocator + per-file catalog), writing every change through the buffer manager and
  **generic WAL** (`GenericXLogStart/RegisterBuffer/Finish`). This is what streams to standbys
  and survives crash/failover.
- **Tantivy working copy:** each backend materializes the index into an in-RAM `RamDirectory`
  (`store.rs`); Tantivy and its threads only ever touch RAM. The main backend thread syncs
  *changed* segment files between RAM and the WAL-logged pages — so WAL volume is proportional
  to new data, not to total index size. A superblock **generation counter** triggers reloads
  when another backend, or WAL replay on a standby, advances it.
- **Writes** are buffered per transaction and applied to the page store at `PRE_COMMIT` under a
  per-index advisory lock (Tantivy's single-writer model); reads take a generation-checked
  consistent snapshot.
- **Scoring:** the bitmap scan stashes each matched key's BM25 score in a backend-local
  scoreboard, cleared per top-level statement via an executor hook; `hyper.score(_id)` reads it.

### Replication, crash, failover

Because the index is WAL-logged, a `bm25` index built on the primary appears — byte-identical
and queryable — on physical-streaming standbys, and is present immediately after a `pg_replica`
failover with no rebuild. Crash recovery replays the generic-WAL records, leaving the index
consistent with the heap at Tantivy's commit granularity (the superblock update is the atomic
commit point).

`hyper.reindex_all()` rebuilds every `bm25` index (e.g. as a belt-and-suspenders step in an
operational runbook).

## Building & testing

Requires `cargo-pgrx` 0.18.1 and PostgreSQL 18 server headers.

```sh
cargo pgrx init --pg18 $(which pg_config)
cargo pgrx test --no-default-features --features pg18   # pgrx integration tests
cargo test --no-default-features                         # pure-Rust tokenizer unit tests
make package                                             # cargo pgrx package
bash packaging/build-deb.sh 18                           # build the .deb
```

`docker/Dockerfile.dev` provides a ready toolchain image (postgres:18 + rust + cargo-pgrx).

## License

AGPL-3.0-only. See [LICENCE](LICENCE).
