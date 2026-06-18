use core::ffi::c_char;
use pgrx::datum::{FromDatum, IntoDatum};
use pgrx::prelude::*;
use std::ffi::CStr;

mod am;
mod blockstore;
mod options;
mod scoreboard;
mod store;
mod tokenizer;

use tokenizer::NgramConfig;

pgrx::pg_module_magic!();

macro_rules! pg_finfo_v1 {
    ($finfo:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $finfo() -> &'static pg_sys::Pg_finfo_record {
            const RECORD: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
            &RECORD
        }
    };
}

pg_finfo_v1!(pg_finfo_hsearch_ngram_in);
pg_finfo_v1!(pg_finfo_hsearch_ngram_out);
pg_finfo_v1!(pg_finfo_hsearch_ngram_typmod_in);
pg_finfo_v1!(pg_finfo_hsearch_ngram_typmod_out);
pg_finfo_v1!(pg_finfo_hsearch_ngram_match);
pg_finfo_v1!(pg_finfo_hsearch_score);
pg_finfo_v1!(pg_finfo_hsearch_bm25_handler);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    unsafe {
        options::init_reloptions();
        scoreboard::install_hooks();
    }
}

unsafe fn raw_arg(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> pg_sys::Datum {
    let nargs = (*fcinfo).nargs as usize;
    let args = (*fcinfo).args.as_slice(nargs);
    args[n].value
}

unsafe fn arg_is_null(fcinfo: pg_sys::FunctionCallInfo, n: usize) -> bool {
    let nargs = (*fcinfo).nargs as usize;
    let args = (*fcinfo).args.as_slice(nargs);
    args[n].isnull
}

unsafe fn make_cstring(s: &str) -> pg_sys::Datum {
    let bytes = s.as_bytes();
    let out = pg_sys::palloc(bytes.len() + 1) as *mut c_char;
    std::ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out, bytes.len());
    *out.add(bytes.len()) = 0;
    pg_sys::Datum::from(out as usize)
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_ngram_in(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let cstr = raw_arg(fcinfo, 0).cast_mut_ptr::<c_char>();
    let s = CStr::from_ptr(cstr).to_string_lossy().into_owned();
    s.into_datum().unwrap()
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_ngram_out(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let s = String::from_datum(raw_arg(fcinfo, 0), false).unwrap_or_default();
    make_cstring(&s)
}

unsafe fn cstring_array(d: pg_sys::Datum) -> Vec<String> {
    let arr = pg_sys::pg_detoast_datum(d.cast_mut_ptr::<pg_sys::varlena>()) as *mut pg_sys::ArrayType;
    let mut elems: *mut pg_sys::Datum = std::ptr::null_mut();
    let mut nulls: *mut bool = std::ptr::null_mut();
    let mut n: i32 = 0;
    pg_sys::deconstruct_array(
        arr,
        pg_sys::CSTRINGOID,
        -2,
        false,
        b'c' as c_char,
        &mut elems,
        &mut nulls,
        &mut n,
    );
    let slice = std::slice::from_raw_parts(elems, n as usize);
    slice
        .iter()
        .map(|dd| {
            CStr::from_ptr(dd.cast_mut_ptr::<c_char>())
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_ngram_typmod_in(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let parts = cstring_array(raw_arg(fcinfo, 0));
    match tokenizer::pack_typmod(&parts) {
        Ok(packed) => packed.into_datum().unwrap(),
        Err(e) => error!("hyper.ngram type modifier: {e}"),
    }
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_ngram_typmod_out(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let tm = i32::from_datum(raw_arg(fcinfo, 0), false).unwrap_or(-1);
    let cfg = tokenizer::unpack_typmod(tm);
    let s = format!(
        "({},{},'ascii_folding={}')",
        cfg.min_gram, cfg.max_gram, cfg.ascii_folding
    );
    make_cstring(&s)
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_ngram_match(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    if arg_is_null(fcinfo, 0) || arg_is_null(fcinfo, 1) {
        return false.into_datum().unwrap();
    }
    let hay = String::from_datum(raw_arg(fcinfo, 0), false).unwrap_or_default();
    let needle = String::from_datum(raw_arg(fcinfo, 1), false).unwrap_or_default();
    let matched = tokenizer::ngram_match(&hay, &needle, &NgramConfig::default());
    matched.into_datum().unwrap()
}

#[pg_guard]
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn hsearch_score(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum {
    if arg_is_null(fcinfo, 0) {
        return 0.0f64.into_datum().unwrap();
    }
    let key = String::from_datum(raw_arg(fcinfo, 0), false).unwrap_or_default();
    let score = scoreboard::lookup(&key).unwrap_or(0.0) as f64;
    score.into_datum().unwrap()
}

extension_sql!(
    r#"
CREATE SCHEMA IF NOT EXISTS hyper;

CREATE TYPE hyper.ngram;

CREATE FUNCTION hyper.ngram_in(cstring) RETURNS hyper.ngram
    AS 'MODULE_PATHNAME', 'hsearch_ngram_in' LANGUAGE c IMMUTABLE STRICT;
CREATE FUNCTION hyper.ngram_out(hyper.ngram) RETURNS cstring
    AS 'MODULE_PATHNAME', 'hsearch_ngram_out' LANGUAGE c IMMUTABLE STRICT;
CREATE FUNCTION hyper.ngram_typmod_in(cstring[]) RETURNS integer
    AS 'MODULE_PATHNAME', 'hsearch_ngram_typmod_in' LANGUAGE c IMMUTABLE STRICT;
CREATE FUNCTION hyper.ngram_typmod_out(integer) RETURNS cstring
    AS 'MODULE_PATHNAME', 'hsearch_ngram_typmod_out' LANGUAGE c IMMUTABLE STRICT;

CREATE TYPE hyper.ngram (
    INPUT = hyper.ngram_in,
    OUTPUT = hyper.ngram_out,
    TYPMOD_IN = hyper.ngram_typmod_in,
    TYPMOD_OUT = hyper.ngram_typmod_out,
    INTERNALLENGTH = VARIABLE,
    STORAGE = extended,
    CATEGORY = 'S'
);

CREATE CAST (text AS hyper.ngram) WITHOUT FUNCTION AS IMPLICIT;
CREATE CAST (character varying AS hyper.ngram) WITHOUT FUNCTION AS IMPLICIT;

CREATE FUNCTION hyper.ngram_match(text, text) RETURNS boolean
    AS 'MODULE_PATHNAME', 'hsearch_ngram_match' LANGUAGE c IMMUTABLE STRICT PARALLEL SAFE;

CREATE OPERATOR public.&&& (
    LEFTARG = text,
    RIGHTARG = text,
    FUNCTION = hyper.ngram_match,
    RESTRICT = contsel,
    JOIN = contjoinsel
);

CREATE FUNCTION hyper.score(text) RETURNS double precision
    AS 'MODULE_PATHNAME', 'hsearch_score' LANGUAGE c STABLE PARALLEL UNSAFE;

CREATE FUNCTION hyper.bm25_handler(internal) RETURNS index_am_handler
    AS 'MODULE_PATHNAME', 'hsearch_bm25_handler' LANGUAGE c;

CREATE ACCESS METHOD bm25 TYPE INDEX HANDLER hyper.bm25_handler;

CREATE OPERATOR FAMILY hyper.bm25_ops USING bm25;
ALTER OPERATOR FAMILY hyper.bm25_ops USING bm25
    ADD OPERATOR 1 public.&&& (text, text);

CREATE OPERATOR CLASS hyper.text_bm25_ops DEFAULT FOR TYPE text
    USING bm25 FAMILY hyper.bm25_ops AS STORAGE text;
CREATE OPERATOR CLASS hyper.varchar_bm25_ops DEFAULT FOR TYPE character varying
    USING bm25 FAMILY hyper.bm25_ops AS STORAGE character varying;
CREATE OPERATOR CLASS hyper.ngram_bm25_ops DEFAULT FOR TYPE hyper.ngram
    USING bm25 FAMILY hyper.bm25_ops AS STORAGE hyper.ngram;

CREATE FUNCTION hyper.reindex_all() RETURNS integer
    LANGUAGE plpgsql AS $$
DECLARE
    r record;
    n integer := 0;
BEGIN
    FOR r IN
        SELECT c.oid::regclass AS idx
        FROM pg_class c
        JOIN pg_am a ON a.oid = c.relam
        WHERE a.amname = 'bm25' AND c.relkind = 'i'
    LOOP
        EXECUTE 'REINDEX INDEX ' || r.idx::text;
        n := n + 1;
    END LOOP;
    RETURN n;
END;
$$;
"#,
    name = "hsearch_sql",
    finalize
);

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn seed_items() {
        Spi::run(
            "CREATE TABLE items (
                _id varchar(24) PRIMARY KEY,
                name text,
                summary text,
                category_path text
             )",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO items (_id, name, summary, category_path) VALUES
                ('000000000000000000000001', 'Kavos aparatas DeLonghi', 'puikus espresso', 'virtuve technika'),
                ('000000000000000000000002', 'Dviratis kalnu', 'lengvas aliuminis', 'sportas dviraciai'),
                ('000000000000000000000003', 'Ąžuolinis stalas', 'masyvus baldas', 'baldai stalai'),
                ('000000000000000000000004', 'Kavinukas turkiškas', 'varinis', 'virtuve indai')",
        )
        .unwrap();
        Spi::run(
            "CREATE INDEX search_idx ON items USING bm25 (
                _id,
                (name::hyper.ngram(2,5,'ascii_folding=true')),
                (summary::hyper.ngram(2,5,'ascii_folding=true')),
                (category_path::hyper.ngram(2,5,'ascii_folding=true'))
             ) WITH (key_field='_id')",
        )
        .unwrap();
        // Tiny test tables make the planner prefer a seq scan; force the bm25 index
        // path (what runs in production on a large table, and what populates scores).
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
    }

    #[pg_test]
    fn schema_and_am_exist() {
        let n = Spi::get_one::<i64>("SELECT count(*) FROM pg_namespace WHERE nspname='hyper'")
            .unwrap()
            .unwrap();
        assert_eq!(n, 1);
        let am = Spi::get_one::<i64>("SELECT count(*) FROM pg_am WHERE amname='bm25'")
            .unwrap()
            .unwrap();
        assert_eq!(am, 1);
    }

    #[pg_test]
    fn ngram_cast_parses() {
        Spi::run("SELECT 'kava'::hyper.ngram(2,5,'ascii_folding=true')").unwrap();
    }

    #[pg_test]
    fn operator_uses_index_and_matches() {
        seed_items();
        let count = Spi::get_one::<i64>("SELECT count(*) FROM items WHERE name &&& 'kava'")
            .unwrap()
            .unwrap();
        assert!(count >= 1, "expected kava matches, got {count}");
        let plan = Spi::get_one::<String>(
            "EXPLAIN (FORMAT TEXT) SELECT _id FROM items WHERE name &&& 'kava'",
        )
        .unwrap()
        .unwrap_or_default();
        assert!(
            plan.contains("bm25") || plan.to_lowercase().contains("bitmap"),
            "plan did not use the bm25 index: {plan}"
        );
    }

    #[pg_test]
    fn score_orders_results() {
        seed_items();
        let id = Spi::get_one::<String>(
            "SELECT _id FROM items WHERE name &&& 'kava' ORDER BY hyper.score(_id) DESC LIMIT 1",
        )
        .unwrap()
        .unwrap();
        assert!(id == "000000000000000000000001" || id == "000000000000000000000004");
    }

    #[pg_test]
    fn accent_folding_matches() {
        seed_items();
        let count = Spi::get_one::<i64>("SELECT count(*) FROM items WHERE name &&& 'azuol'")
            .unwrap()
            .unwrap();
        assert_eq!(count, 1, "ascii-folded query should match Ąžuolinis");
    }

    #[pg_test]
    fn autocomplete_shape() {
        seed_items();
        let got = Spi::get_one::<i64>(
            "WITH scored_ids AS (
                 SELECT _id, hyper.score(_id) AS score
                 FROM items
                 WHERE (name &&& 'kav' OR summary &&& 'kav' OR category_path &&& 'kav')
                 ORDER BY score DESC
             )
             SELECT count(*) FROM scored_ids s JOIN items i ON i._id = s._id",
        )
        .unwrap()
        .unwrap();
        assert!(got >= 2, "expected kav* autocomplete hits, got {got}");
    }

    #[pg_test]
    fn admin_shape_conjunction() {
        seed_items();
        let n = Spi::get_one::<i64>(
            "SELECT count(*) FROM (
                 SELECT _id, hyper.score(_id) AS score
                 FROM items
                 WHERE (name &&& 'kavos') AND (summary &&& 'espresso')
                 ORDER BY score DESC LIMIT 10 OFFSET 0
             ) q",
        )
        .unwrap()
        .unwrap();
        assert_eq!(n, 1, "conjunction should match exactly the espresso row");
    }

    #[pg_test]
    fn delete_then_search_excludes_row() {
        seed_items();
        Spi::run("DELETE FROM items WHERE _id='000000000000000000000001'").unwrap();
        let still = Spi::get_one::<bool>(
            "SELECT EXISTS(SELECT 1 FROM items WHERE name &&& 'aparatas'
                           AND _id='000000000000000000000001')",
        )
        .unwrap()
        .unwrap_or(false);
        assert!(!still, "deleted row must not be returned by search");
    }

    #[pg_test]
    fn short_word_two_chars() {
        seed_items();
        let count = Spi::get_one::<i64>("SELECT count(*) FROM items WHERE name &&& 'ka'")
            .unwrap()
            .unwrap();
        assert!(count >= 2, "two-char ngram query should match, got {count}");
    }

    #[pg_test]
    fn reindex_all_runs() {
        seed_items();
        let n = Spi::get_one::<i32>("SELECT hyper.reindex_all()").unwrap().unwrap();
        assert!(n >= 1);
        let count = Spi::get_one::<i64>("SELECT count(*) FROM items WHERE name &&& 'kava'")
            .unwrap()
            .unwrap();
        assert!(count >= 1, "search must work after reindex");
    }
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'hsearch'"]
    }
}
