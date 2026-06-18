use core::ffi::c_char;
use pgrx::prelude::*;
use std::ffi::CStr;

#[repr(C)]
pub struct Bm25Options {
    vl_len_: i32,
    key_field_offset: i32,
}

pub static mut RELOPT_KIND_BM25: pg_sys::relopt_kind::Type = 0;

pub unsafe fn init_reloptions() {
    RELOPT_KIND_BM25 = pg_sys::add_reloption_kind();
    pg_sys::add_string_reloption(
        RELOPT_KIND_BM25,
        c"key_field".as_ptr(),
        c"name of the key (id) column for the bm25 index".as_ptr(),
        c"".as_ptr(),
        None,
        pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE,
    );
}

#[pg_guard]
pub unsafe extern "C-unwind" fn amoptions(
    reloptions: pg_sys::Datum,
    validate: bool,
) -> *mut pg_sys::bytea {
    let tab = [pg_sys::relopt_parse_elt {
        optname: c"key_field".as_ptr(),
        opttype: pg_sys::relopt_type::RELOPT_TYPE_STRING,
        offset: core::mem::offset_of!(Bm25Options, key_field_offset) as i32,
        isset_offset: -1,
    }];
    pg_sys::build_reloptions(
        reloptions,
        validate,
        RELOPT_KIND_BM25,
        core::mem::size_of::<Bm25Options>(),
        tab.as_ptr(),
        tab.len() as i32,
    ) as *mut pg_sys::bytea
}

pub unsafe fn key_field(index: pg_sys::Relation) -> Option<String> {
    let opts = (*index).rd_options as *mut Bm25Options;
    if opts.is_null() {
        return None;
    }
    let off = (*opts).key_field_offset;
    if off <= 0 {
        return None;
    }
    let base = opts as *const c_char;
    let ptr = base.add(off as usize);
    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
