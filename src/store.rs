use crate::blockstore::{self, Change};
use crate::tokenizer::{ngram_tokens, AsciiNgramTokenizer, NgramConfig};
use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::directory::RamDirectory;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::{
    IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED,
};
use tantivy::{Directory, Index, IndexReader, ReloadPolicy, TantivyDocument, Term};

pub const CTID_FIELD: &str = "ctid";
pub const KEY_FIELD: &str = "key";

#[derive(Clone, Debug)]
pub enum ColumnKind {
    Key,
    Ngram(NgramConfig),
}

#[derive(Clone, Debug)]
pub struct ColumnSpec {
    pub attno: usize,
    pub kind: ColumnKind,
}

#[derive(Clone, Debug)]
pub struct FieldText {
    pub attno: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub ctid: u64,
    pub key: String,
    pub score: f32,
}

enum PendingOp {
    Add {
        ctid: u64,
        key: String,
        fields: Vec<FieldText>,
    },
}

struct Cached {
    generation: u64,
    index: Index,
    reader: IndexReader,
}

thread_local! {
    static PENDING: RefCell<HashMap<u32, Vec<PendingOp>>> = RefCell::new(HashMap::new());
    static CACHE: RefCell<HashMap<u32, Cached>> = RefCell::new(HashMap::new());
    static XACT_REGISTERED: RefCell<bool> = RefCell::new(false);
}

fn tokenizer_name(cfg: &NgramConfig) -> String {
    format!(
        "ng_{}_{}_{}",
        cfg.min_gram, cfg.max_gram, cfg.ascii_folding as u8
    )
}

fn parse_tokenizer_name(name: &str) -> Option<NgramConfig> {
    let parts: Vec<&str> = name.split('_').collect();
    if parts.len() != 4 || parts[0] != "ng" {
        return None;
    }
    Some(NgramConfig {
        min_gram: parts[1].parse().ok()?,
        max_gram: parts[2].parse().ok()?,
        ascii_folding: parts[3] != "0",
    })
}

fn register_tokenizers(index: &Index, schema: &Schema) {
    for (_field, entry) in schema.fields() {
        if let tantivy::schema::FieldType::Str(opts) = entry.field_type() {
            if let Some(indexing) = opts.get_indexing_options() {
                let name = indexing.tokenizer();
                if let Some(cfg) = parse_tokenizer_name(name) {
                    index
                        .tokenizers()
                        .register(name, AsciiNgramTokenizer::new(cfg));
                }
            }
        }
    }
}

fn build_schema(columns: &[ColumnSpec]) -> Schema {
    let mut sb = Schema::builder();
    sb.add_u64_field(CTID_FIELD, INDEXED | STORED | FAST);
    sb.add_text_field(KEY_FIELD, STORED);
    for col in columns {
        if let ColumnKind::Ngram(cfg) = &col.kind {
            let indexing = TextFieldIndexing::default()
                .set_tokenizer(&tokenizer_name(cfg))
                .set_index_option(IndexRecordOption::WithFreqsAndPositions);
            let opts = TextOptions::default().set_indexing_options(indexing);
            sb.add_text_field(&format!("f{}", col.attno), opts);
        }
    }
    sb.build()
}

fn read_whole(dir: &RamDirectory, name: &str) -> Result<Vec<u8>, String> {
    let slice = dir
        .open_read(Path::new(name))
        .map_err(|e| format!("open_read {name}: {e}"))?;
    let bytes = slice.read_bytes().map_err(|e| format!("read {name}: {e}"))?;
    Ok(bytes.as_slice().to_vec())
}

fn dir_from_snapshot(snap: &blockstore::Snapshot) -> Result<RamDirectory, String> {
    let dir = RamDirectory::create();
    for (name, bytes) in &snap.files {
        dir.atomic_write(Path::new(name), bytes)
            .map_err(|e| format!("atomic_write {name}: {e}"))?;
    }
    Ok(dir)
}

fn open_from_dir(dir: RamDirectory) -> Result<Index, String> {
    let index = Index::open(dir).map_err(|e| format!("tantivy open: {e}"))?;
    register_tokenizers(&index, &index.schema());
    Ok(index)
}

fn sync_to_blockstore(
    rel: pg_sys::Relation,
    dir: &RamDirectory,
    index: &Index,
    prev: &blockstore::Snapshot,
) -> Result<(), String> {
    let metas = index.load_metas().map_err(|e| format!("load_metas: {e}"))?;
    let mut live: HashSet<String> = HashSet::new();
    for seg in &metas.segments {
        for f in seg.list_files() {
            live.insert(f.to_string_lossy().into_owned());
        }
    }
    live.insert("meta.json".to_string());
    live.insert(".managed.json".to_string());

    let prev_map: HashMap<&String, usize> =
        prev.files.iter().map(|(n, b)| (n, b.len())).collect();

    let mut changes: Vec<Change> = Vec::new();
    for name in &live {
        let bytes = match read_whole(dir, name) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let always = name == "meta.json" || name == ".managed.json";
        match prev_map.get(name) {
            Some(&len) if !always && len == bytes.len() => {}
            _ => changes.push(Change::Write(name.clone(), bytes)),
        }
    }
    for (name, _b) in &prev.files {
        if !live.contains(name) {
            changes.push(Change::Delete(name.clone()));
        }
    }
    unsafe {
        blockstore::apply(rel, changes);
    }
    Ok(())
}

fn ensure_current(rel: pg_sys::Relation) -> Result<(), String> {
    let relid = unsafe { (*rel).rd_id.to_u32() };
    let gen = unsafe { blockstore::generation(rel) };
    let need_reload = CACHE.with(|c| match c.borrow().get(&relid) {
        Some(cached) => cached.generation != gen,
        None => true,
    });
    if !need_reload {
        return Ok(());
    }
    if gen == 0 {
        return Err("index not built".to_string());
    }
    let snap = unsafe { blockstore::snapshot(rel) };
    let dir = dir_from_snapshot(&snap)?;
    let index = open_from_dir(dir)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| format!("reader: {e}"))?;
    CACHE.with(|c| {
        c.borrow_mut().insert(
            relid,
            Cached {
                generation: gen,
                index,
                reader,
            },
        );
    });
    Ok(())
}

fn invalidate(relid: u32) {
    CACHE.with(|c| {
        c.borrow_mut().remove(&relid);
    });
}

pub struct BuildSession {
    dir: RamDirectory,
    index: Index,
    writer: tantivy::IndexWriter<TantivyDocument>,
    ctid_field: tantivy::schema::Field,
    key_field: tantivy::schema::Field,
    count: u64,
}

pub fn begin_build(rel: pg_sys::Relation, columns: &[ColumnSpec]) -> Result<BuildSession, String> {
    unsafe {
        blockstore::init(rel);
    }
    let schema = build_schema(columns);
    let dir = RamDirectory::create();
    let index = Index::create(dir.clone(), schema, tantivy::IndexSettings::default())
        .map_err(|e| format!("tantivy create: {e}"))?;
    register_tokenizers(&index, &index.schema());
    let schema = index.schema();
    let ctid_field = schema.get_field(CTID_FIELD).unwrap();
    let key_field = schema.get_field(KEY_FIELD).unwrap();
    let writer = index
        .writer_with_num_threads(1, 64_000_000)
        .map_err(|e| format!("writer: {e}"))?;
    Ok(BuildSession {
        dir,
        index,
        writer,
        ctid_field,
        key_field,
        count: 0,
    })
}

impl BuildSession {
    pub fn add(&mut self, ctid: u64, key: &str, fields: &[FieldText]) -> Result<(), String> {
        let schema = self.index.schema();
        let mut doc = TantivyDocument::default();
        doc.add_u64(self.ctid_field, ctid);
        doc.add_text(self.key_field, key);
        for ft in fields {
            if let Ok(f) = schema.get_field(&format!("f{}", ft.attno)) {
                doc.add_text(f, &ft.text);
            }
        }
        self.writer
            .add_document(doc)
            .map_err(|e| format!("add: {e}"))?;
        self.count += 1;
        Ok(())
    }
}

pub fn finish_build(rel: pg_sys::Relation, mut session: BuildSession) -> Result<u64, String> {
    let relid = unsafe { (*rel).rd_id.to_u32() };
    session.writer.commit().map_err(|e| format!("commit: {e}"))?;
    let count = session.count;
    let dir = session.dir.clone();
    let index = session.index.clone();
    drop(session);
    let empty = blockstore::Snapshot::default();
    sync_to_blockstore(rel, &dir, &index, &empty)?;
    invalidate(relid);
    Ok(count)
}

pub fn search(
    rel: pg_sys::Relation,
    attno: usize,
    query_text: &str,
    cfg: &NgramConfig,
    limit: usize,
) -> Result<Vec<Hit>, String> {
    if ensure_current(rel).is_err() {
        return Ok(Vec::new());
    }
    let relid = unsafe { (*rel).rd_id.to_u32() };
    CACHE.with(|c| {
        let borrow = c.borrow();
        let cached = match borrow.get(&relid) {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let searcher = cached.reader.searcher();
        let schema = cached.index.schema();
        let field = match schema.get_field(&format!("f{attno}")) {
            Ok(f) => f,
            Err(_) => return Ok(Vec::new()),
        };
        let ctid_field = schema.get_field(CTID_FIELD).unwrap();
        let key_field = schema.get_field(KEY_FIELD).unwrap();

        let grams = ngram_tokens(query_text, cfg);
        if grams.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for g in grams {
            if !seen.insert(g.clone()) {
                continue;
            }
            let term = Term::from_field_text(field, &g);
            clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)),
            ));
        }
        let query = BooleanQuery::new(clauses);
        let collector = TopDocs::with_limit(limit).order_by_score();
        let top = searcher
            .search(&query, &collector)
            .map_err(|e| format!("search: {e}"))?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr).map_err(|e| format!("doc: {e}"))?;
            let ctid = doc.get_first(ctid_field).and_then(|v| v.as_u64()).unwrap_or(0);
            let key = doc
                .get_first(key_field)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            hits.push(Hit { ctid, key, score });
        }
        Ok(hits)
    })
}

pub fn buffer_add(relid: u32, ctid: u64, key: String, fields: Vec<FieldText>) {
    ensure_xact_registered();
    PENDING.with(|p| {
        p.borrow_mut()
            .entry(relid)
            .or_default()
            .push(PendingOp::Add { ctid, key, fields });
    });
}

pub fn discard_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

fn ensure_xact_registered() {
    XACT_REGISTERED.with(|f| {
        let mut done = f.borrow_mut();
        if !*done {
            unsafe {
                pg_sys::RegisterXactCallback(Some(flush_xact_callback), std::ptr::null_mut());
            }
            *done = true;
        }
    });
}

fn flush_one(relid: u32, ops: &[PendingOp]) -> Result<(), String> {
    unsafe {
        let oid = pg_sys::Oid::from(relid);
        let rel = pg_sys::relation_open(oid, pg_sys::RowExclusiveLock as i32);
        if rel.is_null() {
            return Ok(());
        }
        let _guard = blockstore::lock_writer(relid);
        let snap = blockstore::snapshot(rel);
        if snap.generation == 0 {
            pg_sys::relation_close(rel, pg_sys::RowExclusiveLock as i32);
            return Ok(());
        }
        let dir = dir_from_snapshot(&snap)?;
        let index = open_from_dir(dir.clone())?;
        let schema = index.schema();
        let ctid_field = schema.get_field(CTID_FIELD).unwrap();
        let key_field = schema.get_field(KEY_FIELD).unwrap();
        let mut writer = index
            .writer_with_num_threads(1, 64_000_000)
            .map_err(|e| format!("writer: {e}"))?;
        for op in ops {
            let PendingOp::Add { ctid, key, fields } = op;
            writer.delete_term(Term::from_field_u64(ctid_field, *ctid));
            let mut doc = TantivyDocument::default();
            doc.add_u64(ctid_field, *ctid);
            doc.add_text(key_field, key);
            for ft in fields {
                if let Ok(f) = schema.get_field(&format!("f{}", ft.attno)) {
                    doc.add_text(f, &ft.text);
                }
            }
            writer.add_document(doc).map_err(|e| format!("add: {e}"))?;
        }
        writer.commit().map_err(|e| format!("commit: {e}"))?;
        drop(writer);
        sync_to_blockstore(rel, &dir, &index, &snap)?;
        pg_sys::relation_close(rel, pg_sys::RowExclusiveLock as i32);
    }
    invalidate(relid);
    Ok(())
}

fn flush_pending() -> Result<(), String> {
    let drained: Vec<(u32, Vec<PendingOp>)> =
        PENDING.with(|p| p.borrow_mut().drain().collect());
    for (relid, ops) in drained {
        if ops.is_empty() {
            continue;
        }
        flush_one(relid, &ops)?;
    }
    Ok(())
}

pub fn bulk_delete<F: Fn(u64) -> bool>(rel: pg_sys::Relation, is_dead: F) -> Result<u64, String> {
    let relid = unsafe { (*rel).rd_id.to_u32() };
    let snap = unsafe { blockstore::snapshot(rel) };
    if snap.generation == 0 {
        return Ok(0);
    }
    let _guard = unsafe { blockstore::lock_writer(relid) };
    let snap = unsafe { blockstore::snapshot(rel) };
    let dir = dir_from_snapshot(&snap)?;
    let index = open_from_dir(dir.clone())?;
    let schema = index.schema();
    let ctid_field = schema.get_field(CTID_FIELD).unwrap();
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| format!("reader: {e}"))?;
    reader.reload().map_err(|e| format!("reload: {e}"))?;
    let searcher = reader.searcher();
    let mut dead: Vec<u64> = Vec::new();
    for seg in searcher.segment_readers() {
        let store = seg.get_store_reader(0).map_err(|e| format!("store: {e}"))?;
        let alive = seg.alive_bitset();
        for doc_id in 0..seg.max_doc() {
            if let Some(bs) = alive {
                if !bs.is_alive(doc_id) {
                    continue;
                }
            }
            let doc: TantivyDocument = match store.get(doc_id) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if let Some(ctid) = doc.get_first(ctid_field).and_then(|v| v.as_u64()) {
                if is_dead(ctid) {
                    dead.push(ctid);
                }
            }
        }
    }
    if dead.is_empty() {
        return Ok(0);
    }
    let mut writer: tantivy::IndexWriter<TantivyDocument> = index
        .writer_with_num_threads(1, 64_000_000)
        .map_err(|e| format!("writer: {e}"))?;
    for ctid in &dead {
        writer.delete_term(Term::from_field_u64(ctid_field, *ctid));
    }
    writer.commit().map_err(|e| format!("commit: {e}"))?;
    drop(writer);
    sync_to_blockstore(rel, &dir, &index, &snap)?;
    invalidate(relid);
    Ok(dead.len() as u64)
}

#[pg_guard]
unsafe extern "C-unwind" fn flush_xact_callback(
    event: pg_sys::XactEvent::Type,
    _arg: *mut core::ffi::c_void,
) {
    match event {
        pg_sys::XactEvent::XACT_EVENT_PRE_COMMIT => {
            if let Err(e) = flush_pending() {
                discard_pending();
                error!("hsearch: failed to flush bm25 index: {e}");
            }
        }
        pg_sys::XactEvent::XACT_EVENT_ABORT | pg_sys::XactEvent::XACT_EVENT_PARALLEL_ABORT => {
            discard_pending();
        }
        _ => {}
    }
}
