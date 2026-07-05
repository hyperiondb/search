use pgrx::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::addr_of;

thread_local! {
    static SCOREBOARD: RefCell<HashMap<String, f32>> = RefCell::new(HashMap::new());
    static NESTING: RefCell<i32> = RefCell::new(0);
    static SUBXACT_STACK: RefCell<Vec<(pg_sys::SubTransactionId, i32)>> = RefCell::new(Vec::new());
}

pub fn reset() {
    SCOREBOARD.with(|s| s.borrow_mut().clear());
}

pub fn add(key: &str, score: f32) {
    SCOREBOARD.with(|s| {
        let mut map = s.borrow_mut();
        let entry = map.entry(key.to_string()).or_insert(f32::MIN);
        *entry = entry.max(score);
    });
}

pub fn lookup(key: &str) -> Option<f32> {
    SCOREBOARD.with(|s| s.borrow().get(key).copied())
}

static mut PREV_EXECUTOR_START: pg_sys::ExecutorStart_hook_type = None;
static mut PREV_EXECUTOR_END: pg_sys::ExecutorEnd_hook_type = None;

pub unsafe fn install_hooks() {
    PREV_EXECUTOR_START = pg_sys::ExecutorStart_hook;
    pg_sys::ExecutorStart_hook = Some(executor_start);
    PREV_EXECUTOR_END = pg_sys::ExecutorEnd_hook;
    pg_sys::ExecutorEnd_hook = Some(executor_end);
    pg_sys::RegisterXactCallback(Some(xact_callback), std::ptr::null_mut());
    pg_sys::RegisterSubXactCallback(Some(subxact_callback), std::ptr::null_mut());
}

#[pg_guard]
unsafe extern "C-unwind" fn executor_start(query_desc: *mut pg_sys::QueryDesc, eflags: i32) {
    let level = NESTING.with(|n| *n.borrow());
    if level == 0 {
        reset();
    }
    NESTING.with(|n| *n.borrow_mut() += 1);
    let prev = *addr_of!(PREV_EXECUTOR_START);
    if let Some(prev) = prev {
        prev(query_desc, eflags);
    } else {
        pg_sys::standard_ExecutorStart(query_desc, eflags);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn executor_end(query_desc: *mut pg_sys::QueryDesc) {
    NESTING.with(|n| {
        let mut v = n.borrow_mut();
        if *v > 0 {
            *v -= 1;
        }
    });
    let prev = *addr_of!(PREV_EXECUTOR_END);
    if let Some(prev) = prev {
        prev(query_desc)
    } else {
        pg_sys::standard_ExecutorEnd(query_desc)
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn xact_callback(event: pg_sys::XactEvent::Type, _arg: *mut core::ffi::c_void) {
    match event {
        pg_sys::XactEvent::XACT_EVENT_COMMIT
        | pg_sys::XactEvent::XACT_EVENT_ABORT
        | pg_sys::XactEvent::XACT_EVENT_PREPARE => {
            NESTING.with(|n| *n.borrow_mut() = 0);
            SUBXACT_STACK.with(|s| s.borrow_mut().clear());
            reset();
        }
        _ => {}
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn subxact_callback(
    event: pg_sys::SubXactEvent::Type,
    my_subid: pg_sys::SubTransactionId,
    _parent_subid: pg_sys::SubTransactionId,
    _arg: *mut core::ffi::c_void,
) {
    match event {
        pg_sys::SubXactEvent::SUBXACT_EVENT_START_SUB => {
            let level = NESTING.with(|n| *n.borrow());
            SUBXACT_STACK.with(|s| s.borrow_mut().push((my_subid, level)));
        }
        pg_sys::SubXactEvent::SUBXACT_EVENT_COMMIT_SUB => {
            SUBXACT_STACK.with(|s| {
                let mut stack = s.borrow_mut();
                if let Some(pos) = stack.iter().rposition(|(id, _)| *id == my_subid) {
                    stack.truncate(pos);
                }
            });
        }
        pg_sys::SubXactEvent::SUBXACT_EVENT_ABORT_SUB => {
            SUBXACT_STACK.with(|s| {
                let mut stack = s.borrow_mut();
                if let Some(pos) = stack.iter().rposition(|(id, _)| *id == my_subid) {
                    let (_, saved) = stack[pos];
                    stack.truncate(pos);
                    NESTING.with(|n| *n.borrow_mut() = saved);
                }
            });
        }
        _ => {}
    }
}
