use crate::store::{self, ColumnKind, ColumnSpec, FieldText};
use crate::tokenizer::NgramConfig;
use crate::{options, scoreboard};
use core::ffi::c_void;
use pgrx::datum::FromDatum;
use pgrx::prelude::*;
use std::collections::HashMap;

fn ctid_to_u64(tid: pg_sys::ItemPointerData) -> u64 {
    let block = unsafe { pgrx::itemptr::item_pointer_get_block_number(&tid) } as u64;
    let off = unsafe { pgrx::itemptr::item_pointer_get_offset_number(&tid) } as u64;
    (block << 16) | off
}

fn u64_to_ctid(v: u64) -> pg_sys::ItemPointerData {
    let mut tid = pg_sys::ItemPointerData::default();
    let block = (v >> 16) as pg_sys::BlockNumber;
    let off = (v & 0xFFFF) as u16;
    pgrx::itemptr::item_pointer_set_all(&mut tid, block, off);
    tid
}

unsafe fn columns_of_index(index: pg_sys::Relation) -> (Vec<ColumnSpec>, usize) {
    let tupdesc = (*index).rd_att;
    let natts = (*tupdesc).natts as usize;
    let kf = options::key_field(index);
    let mut key_attno = 1usize;
    if let Some(kf) = &kf {
        for i in 0..natts {
            let att = pg_sys::TupleDescAttr(tupdesc, i as i32);
            let name = pgrx::name_data_to_str(&(*att).attname);
            if name == kf {
                key_attno = i + 1;
                break;
            }
        }
    }
    let mut cols = Vec::with_capacity(natts);
    for i in 0..natts {
        let attno = i + 1;
        if attno == key_attno {
            cols.push(ColumnSpec {
                attno,
                kind: ColumnKind::Key,
            });
        } else {
            cols.push(ColumnSpec {
                attno,
                kind: ColumnKind::Ngram(NgramConfig::default()),
            });
        }
    }
    (cols, key_attno)
}

unsafe fn extract_row(
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    cols: &[ColumnSpec],
    key_attno: usize,
) -> Option<(String, Vec<FieldText>)> {
    let getstr = |pos: usize| -> Option<String> {
        if *isnull.add(pos - 1) {
            return None;
        }
        String::from_datum(*values.add(pos - 1), false)
    };
    let key = getstr(key_attno)?;
    let mut fields = Vec::new();
    for col in cols {
        if let ColumnKind::Ngram(_) = col.kind {
            if let Some(s) = getstr(col.attno) {
                fields.push(FieldText {
                    attno: col.attno,
                    text: s,
                });
            }
        }
    }
    Some((key, fields))
}

struct BuildState {
    cols: Vec<ColumnSpec>,
    key_attno: usize,
    session: Option<store::BuildSession>,
    error: Option<String>,
}

#[pg_guard]
unsafe extern "C-unwind" fn build_callback(
    _index: pg_sys::Relation,
    tid: pg_sys::ItemPointer,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    _tuple_is_alive: bool,
    state: *mut c_void,
) {
    let state = &mut *(state as *mut BuildState);
    if state.error.is_some() {
        return;
    }
    if let Some((key, fields)) = extract_row(values, isnull, &state.cols, state.key_attno) {
        let ctid = ctid_to_u64(*tid);
        if let Some(session) = state.session.as_mut() {
            if let Err(e) = session.add(ctid, &key, &fields) {
                state.error = Some(e);
            }
        }
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuild(
    heap: pg_sys::Relation,
    index: pg_sys::Relation,
    index_info: *mut pg_sys::IndexInfo,
) -> *mut pg_sys::IndexBuildResult {
    let (cols, key_attno) = columns_of_index(index);
    let session = match store::begin_build(index, &cols) {
        Ok(s) => s,
        Err(e) => error!("hsearch ambuild: {e}"),
    };
    let mut state = BuildState {
        cols: cols.clone(),
        key_attno,
        session: Some(session),
        error: None,
    };
    let state_ptr = &mut state as *mut BuildState as *mut c_void;
    let ntuples = pg_sys::table_index_build_scan(
        heap,
        index,
        index_info,
        true,
        true,
        Some(build_callback),
        state_ptr,
        std::ptr::null_mut(),
    );
    if let Some(e) = state.error.take() {
        error!("hsearch ambuild: {e}");
    }
    let session = state.session.take().unwrap();
    let count = match store::finish_build(index, session) {
        Ok(c) => c,
        Err(e) => error!("hsearch ambuild finish: {e}"),
    };
    let result =
        pg_sys::palloc0(core::mem::size_of::<pg_sys::IndexBuildResult>()) as *mut pg_sys::IndexBuildResult;
    (*result).heap_tuples = ntuples;
    (*result).index_tuples = count as f64;
    result
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambuildempty(index: pg_sys::Relation) {
    let (cols, _ka) = columns_of_index(index);
    if let Ok(session) = store::begin_build(index, &cols) {
        let _ = store::finish_build(index, session);
    }
}

#[pg_guard]
pub unsafe extern "C-unwind" fn aminsert(
    index: pg_sys::Relation,
    values: *mut pg_sys::Datum,
    isnull: *mut bool,
    heap_tid: pg_sys::ItemPointer,
    _heap: pg_sys::Relation,
    _check_unique: pg_sys::IndexUniqueCheck::Type,
    _index_unchanged: bool,
    _index_info: *mut pg_sys::IndexInfo,
) -> bool {
    let (cols, key_attno) = columns_of_index(index);
    if let Some((key, fields)) = extract_row(values, isnull, &cols, key_attno) {
        let relid = (*index).rd_id.to_u32();
        let ctid = ctid_to_u64(*heap_tid);
        store::buffer_add(relid, ctid, key, fields);
    }
    true
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambulkdelete(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
    callback: pg_sys::IndexBulkDeleteCallback,
    callback_state: *mut c_void,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let index = (*info).index;
    let mut stats = stats;
    if stats.is_null() {
        stats = pg_sys::palloc0(core::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
            as *mut pg_sys::IndexBulkDeleteResult;
    }
    let removed = store::bulk_delete(index, |ctid_u64| {
        let mut tid = u64_to_ctid(ctid_u64);
        match callback {
            Some(cb) => cb(&mut tid, callback_state),
            None => false,
        }
    })
    .unwrap_or(0);
    (*stats).tuples_removed += removed as f64;
    stats
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amvacuumcleanup(
    info: *mut pg_sys::IndexVacuumInfo,
    stats: *mut pg_sys::IndexBulkDeleteResult,
) -> *mut pg_sys::IndexBulkDeleteResult {
    let mut stats = stats;
    if stats.is_null() && !(*info).analyze_only {
        stats = pg_sys::palloc0(core::mem::size_of::<pg_sys::IndexBulkDeleteResult>())
            as *mut pg_sys::IndexBulkDeleteResult;
    }
    stats
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amcostestimate(
    _root: *mut pg_sys::PlannerInfo,
    _path: *mut pg_sys::IndexPath,
    _loop_count: f64,
    index_startup_cost: *mut pg_sys::Cost,
    index_total_cost: *mut pg_sys::Cost,
    index_selectivity: *mut pg_sys::Selectivity,
    index_correlation: *mut f64,
    index_pages: *mut f64,
) {
    *index_startup_cost = 0.0;
    *index_total_cost = 0.0;
    *index_selectivity = 0.0001;
    *index_correlation = 0.0;
    *index_pages = 1.0;
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amvalidate(_opclassoid: pg_sys::Oid) -> bool {
    true
}

#[pg_guard]
pub unsafe extern "C-unwind" fn ambeginscan(
    index: pg_sys::Relation,
    nkeys: i32,
    norderbys: i32,
) -> pg_sys::IndexScanDesc {
    pg_sys::RelationGetIndexScan(index, nkeys, norderbys)
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amrescan(
    scan: pg_sys::IndexScanDesc,
    keys: pg_sys::ScanKey,
    nkeys: i32,
    _orderbys: pg_sys::ScanKey,
    _norderbys: i32,
) {
    if nkeys > 0 && !keys.is_null() && !(*scan).keyData.is_null() {
        std::ptr::copy(keys, (*scan).keyData, nkeys as usize);
    }
    (*scan).numberOfKeys = nkeys;
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amgetbitmap(
    scan: pg_sys::IndexScanDesc,
    tbm: *mut pg_sys::TIDBitmap,
) -> i64 {
    let index = (*scan).indexRelation;
    let nkeys = (*scan).numberOfKeys as usize;
    let keys = (*scan).keyData;
    let (cols, _key_attno) = columns_of_index(index);
    let attcfg = |attno: usize| -> NgramConfig {
        for c in &cols {
            if c.attno == attno {
                if let ColumnKind::Ngram(cfg) = &c.kind {
                    return *cfg;
                }
            }
        }
        NgramConfig::default()
    };

    let mut per_key: Vec<HashMap<u64, (String, f32)>> = Vec::new();
    for k in 0..nkeys {
        let key = keys.add(k);
        let attno = (*key).sk_attno as usize;
        let query = match String::from_datum((*key).sk_argument, false) {
            Some(s) => s,
            None => return 0,
        };
        let cfg = attcfg(attno);
        let hits = store::search(index, attno, &query, &cfg, crate::max_matches()).unwrap_or_default();
        let mut m: HashMap<u64, (String, f32)> = HashMap::new();
        for h in hits {
            let e = m.entry(h.ctid).or_insert((h.key.clone(), f32::MIN));
            if h.score > e.1 {
                e.1 = h.score;
            }
            e.0 = h.key;
        }
        per_key.push(m);
    }
    if per_key.is_empty() {
        return 0;
    }

    let mut result = per_key[0].clone();
    for m in &per_key[1..] {
        result.retain(|ctid, _| m.contains_key(ctid));
        for (ctid, val) in result.iter_mut() {
            if let Some((_k, s)) = m.get(ctid) {
                val.1 = val.1.max(*s);
            }
        }
    }

    let mut tids: Vec<pg_sys::ItemPointerData> = Vec::with_capacity(result.len());
    for (ctid, (key, score)) in &result {
        scoreboard::add(key, *score);
        tids.push(u64_to_ctid(*ctid));
    }
    let n = tids.len();
    if n > 0 {
        pg_sys::tbm_add_tuples(tbm, tids.as_mut_ptr(), n as i32, true);
    }
    n as i64
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amendscan(_scan: pg_sys::IndexScanDesc) {}

#[pg_guard]
#[unsafe(no_mangle)]
pub extern "C-unwind" fn hsearch_bm25_handler(_fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    let mut amr = unsafe {
        PgBox::<pg_sys::IndexAmRoutine>::alloc_node(pg_sys::NodeTag::T_IndexAmRoutine)
    };
    amr.amstrategies = 1;
    amr.amsupport = 0;
    amr.amoptsprocnum = 0;
    amr.amcanorder = false;
    amr.amcanorderbyop = false;
    amr.amcanbackward = false;
    amr.amcanunique = false;
    amr.amcanmulticol = true;
    amr.amoptionalkey = true;
    amr.amsearcharray = false;
    amr.amsearchnulls = false;
    amr.amstorage = false;
    amr.amclusterable = false;
    amr.ampredlocks = false;
    amr.amcanparallel = false;
    amr.amcaninclude = false;
    amr.amusemaintenanceworkmem = false;
    amr.amsummarizing = false;
    amr.amparallelvacuumoptions = 0;
    amr.amkeytype = pg_sys::InvalidOid;

    amr.ambuild = Some(ambuild);
    amr.ambuildempty = Some(ambuildempty);
    amr.aminsert = Some(aminsert);
    amr.ambulkdelete = Some(ambulkdelete);
    amr.amvacuumcleanup = Some(amvacuumcleanup);
    amr.amcanreturn = None;
    amr.amcostestimate = Some(amcostestimate);
    amr.amoptions = Some(options::amoptions);
    amr.amproperty = None;
    amr.ambuildphasename = None;
    amr.amvalidate = Some(amvalidate);
    amr.amadjustmembers = None;
    amr.ambeginscan = Some(ambeginscan);
    amr.amrescan = Some(amrescan);
    amr.amgettuple = None;
    amr.amgetbitmap = Some(amgetbitmap);
    amr.amendscan = Some(amendscan);
    amr.ammarkpos = None;
    amr.amrestrpos = None;
    amr.amestimateparallelscan = None;
    amr.aminitparallelscan = None;
    amr.amparallelrescan = None;

    pg_sys::Datum::from(amr.into_pg() as usize)
}
