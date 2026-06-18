use pgrx::prelude::*;
use std::collections::BTreeMap;

const MAGIC: u32 = 0x6853_4233;
const VERSION: u32 = 1;
const HEADER: usize = 24;
const GENERIC_XLOG_FULL_IMAGE: i32 = 0x0001;

fn blcksz() -> usize {
    pg_sys::BLCKSZ as usize
}
fn payload() -> usize {
    blcksz() - HEADER
}

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub generation: u64,
    pub total_blocks: u32,
    free: Vec<(u32, u32)>,
    files: BTreeMap<String, (u32, u32, u32)>,
}

pub enum Change {
    Write(String, Vec<u8>),
    Delete(String),
}

unsafe fn read_block(rel: pg_sys::Relation, blockno: u32, out: &mut Vec<u8>) {
    let buf = pg_sys::ReadBuffer(rel, blockno);
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
    let page = pg_sys::BufferGetPage(buf) as *const u8;
    let src = page.add(HEADER);
    let start = out.len();
    out.resize(start + payload(), 0);
    std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(start), payload());
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_UNLOCK as i32);
    pg_sys::ReleaseBuffer(buf);
}

unsafe fn write_block(rel: pg_sys::Relation, blockno: u32, data: &[u8], new_page: bool) {
    let buf = pg_sys::ReadBuffer(rel, blockno);
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let state = pg_sys::GenericXLogStart(rel);
    let flags = if new_page { GENERIC_XLOG_FULL_IMAGE } else { 0 };
    let page = pg_sys::GenericXLogRegisterBuffer(state, buf, flags);
    if new_page {
        pg_sys::PageInit(page, blcksz(), 0);
    }
    let used = data.len().min(payload());
    let dst = (page as *mut u8).add(HEADER);
    std::ptr::write_bytes(dst, 0, payload());
    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, used);
    let ph = page as *mut pg_sys::PageHeaderData;
    (*ph).pd_lower = (HEADER + used) as u16;
    pg_sys::GenericXLogFinish(state);
    pg_sys::UnlockReleaseBuffer(buf);
}

unsafe fn extend(rel: pg_sys::Relation) -> u32 {
    let buf = pg_sys::ReadBuffer(rel, pg_sys::InvalidBlockNumber);
    let blockno = pg_sys::BufferGetBlockNumber(buf);
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let state = pg_sys::GenericXLogStart(rel);
    let page = pg_sys::GenericXLogRegisterBuffer(state, buf, GENERIC_XLOG_FULL_IMAGE);
    pg_sys::PageInit(page, blcksz(), 0);
    pg_sys::GenericXLogFinish(state);
    pg_sys::UnlockReleaseBuffer(buf);
    blockno
}

unsafe fn nblocks(rel: pg_sys::Relation) -> u32 {
    pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
}

fn run_blocks(len: usize) -> u32 {
    if len == 0 {
        1
    } else {
        len.div_ceil(payload()) as u32
    }
}

impl Meta {
    fn alloc(&mut self, rel: pg_sys::Relation, need: u32) -> u32 {
        let mut best: Option<usize> = None;
        for (i, (_s, n)) in self.free.iter().enumerate() {
            if *n >= need && best.map(|b| self.free[b].1 > *n).unwrap_or(true) {
                best = Some(i);
            }
        }
        if let Some(i) = best {
            let (start, n) = self.free[i];
            if n == need {
                self.free.remove(i);
            } else {
                self.free[i] = (start + need, n - need);
            }
            return start;
        }
        unsafe {
            let cur = nblocks(rel);
            let start = if cur == 0 { 1 } else { cur };
            let want = start + need;
            let mut b = cur;
            while b < want {
                let got = extend(rel);
                debug_assert_eq!(got, b);
                b += 1;
            }
            if start + need > self.total_blocks {
                self.total_blocks = start + need;
            }
            start
        }
    }

    fn free_run(&mut self, start: u32, n: u32) {
        if n == 0 {
            return;
        }
        self.free.push((start, n));
    }
}

unsafe fn write_run(rel: pg_sys::Relation, start: u32, n: u32, data: &[u8]) {
    let mut offset = 0usize;
    for i in 0..n {
        let end = (offset + payload()).min(data.len());
        let chunk = &data[offset.min(data.len())..end];
        write_block(rel, start + i, chunk, true);
        offset = end;
    }
}

unsafe fn read_run(rel: pg_sys::Relation, start: u32, n: u32, len: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(n as usize * payload());
    for i in 0..n {
        read_block(rel, start + i, &mut out);
    }
    out.truncate(len as usize);
    out
}

fn ser_catalog(meta: &Meta) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(meta.free.len() as u32).to_le_bytes());
    for (s, n) in &meta.free {
        b.extend_from_slice(&s.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
    }
    b.extend_from_slice(&(meta.files.len() as u32).to_le_bytes());
    for (name, (s, n, l)) in &meta.files {
        let nb = name.as_bytes();
        b.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        b.extend_from_slice(nb);
        b.extend_from_slice(&s.to_le_bytes());
        b.extend_from_slice(&n.to_le_bytes());
        b.extend_from_slice(&l.to_le_bytes());
    }
    b
}

fn de_catalog(b: &[u8], meta: &mut Meta) {
    let mut p = 0usize;
    let rd_u32 = |b: &[u8], p: &mut usize| -> u32 {
        let v = u32::from_le_bytes([b[*p], b[*p + 1], b[*p + 2], b[*p + 3]]);
        *p += 4;
        v
    };
    let rd_u16 = |b: &[u8], p: &mut usize| -> u16 {
        let v = u16::from_le_bytes([b[*p], b[*p + 1]]);
        *p += 2;
        v
    };
    if b.len() < 4 {
        return;
    }
    let fc = rd_u32(b, &mut p);
    for _ in 0..fc {
        let s = rd_u32(b, &mut p);
        let n = rd_u32(b, &mut p);
        meta.free.push((s, n));
    }
    let nfiles = rd_u32(b, &mut p);
    for _ in 0..nfiles {
        let nl = rd_u16(b, &mut p) as usize;
        let name = String::from_utf8_lossy(&b[p..p + nl]).into_owned();
        p += nl;
        let s = rd_u32(b, &mut p);
        let n = rd_u32(b, &mut p);
        let l = rd_u32(b, &mut p);
        meta.files.insert(name, (s, n, l));
    }
}

unsafe fn read_superblock(rel: pg_sys::Relation) -> Option<(u64, u32, u32, u32, u32)> {
    if nblocks(rel) == 0 {
        return None;
    }
    let mut sb = Vec::new();
    read_block(rel, 0, &mut sb);
    let rd_u32 = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let rd_u64 = |b: &[u8], o: usize| {
        u64::from_le_bytes([
            b[o], b[o + 1], b[o + 2], b[o + 3], b[o + 4], b[o + 5], b[o + 6], b[o + 7],
        ])
    };
    if rd_u32(&sb, 0) != MAGIC || rd_u32(&sb, 4) != VERSION {
        return None;
    }
    let generation = rd_u64(&sb, 8);
    let total_blocks = rd_u32(&sb, 16);
    let catalog_start = rd_u32(&sb, 20);
    let catalog_blocks = rd_u32(&sb, 24);
    let catalog_len = rd_u32(&sb, 28);
    Some((
        generation,
        total_blocks,
        catalog_start,
        catalog_blocks,
        catalog_len,
    ))
}

unsafe fn read_meta(rel: pg_sys::Relation) -> Meta {
    let mut meta = Meta::default();
    let Some((generation, total_blocks, cs, cb, cl)) = read_superblock(rel) else {
        return meta;
    };
    meta.generation = generation;
    meta.total_blocks = total_blocks;
    if cb > 0 && cl > 0 {
        let blob = read_run(rel, cs, cb, cl);
        de_catalog(&blob, &mut meta);
    }
    meta
}

pub unsafe fn generation(rel: pg_sys::Relation) -> u64 {
    read_superblock(rel).map(|s| s.0).unwrap_or(0)
}

unsafe fn write_superblock(
    rel: pg_sys::Relation,
    meta: &Meta,
    catalog_start: u32,
    catalog_blocks: u32,
    catalog_len: u32,
) {
    let mut sb = vec![0u8; payload().min(64)];
    sb[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    sb[4..8].copy_from_slice(&VERSION.to_le_bytes());
    sb[8..16].copy_from_slice(&meta.generation.to_le_bytes());
    sb[16..20].copy_from_slice(&meta.total_blocks.to_le_bytes());
    sb[20..24].copy_from_slice(&catalog_start.to_le_bytes());
    sb[24..28].copy_from_slice(&catalog_blocks.to_le_bytes());
    sb[28..32].copy_from_slice(&catalog_len.to_le_bytes());
    let new_page = nblocks(rel) == 0;
    if new_page {
        extend(rel);
    }
    write_block(rel, 0, &sb, false);
}

unsafe fn flush_meta(rel: pg_sys::Relation, meta: &mut Meta, old_catalog: (u32, u32)) {
    if old_catalog.1 > 0 {
        meta.free_run(old_catalog.0, old_catalog.1);
    }
    let mut blob = ser_catalog(meta);
    let mut need = run_blocks(blob.len());
    let mut start = meta.alloc(rel, need);
    loop {
        blob = ser_catalog(meta);
        let need2 = run_blocks(blob.len());
        if need2 <= need {
            break;
        }
        meta.free_run(start, need);
        need = need2;
        start = meta.alloc(rel, need);
    }
    let len = blob.len() as u32;
    write_run(rel, start, need, &blob);
    write_superblock(rel, meta, start, need, len);
}

pub unsafe fn init(rel: pg_sys::Relation) {
    let mut meta = Meta {
        generation: 1,
        total_blocks: 1,
        free: Vec::new(),
        files: BTreeMap::new(),
    };
    write_superblock(rel, &meta, 0, 0, 0);
    flush_meta(rel, &mut meta, (0, 0));
}

#[derive(Default)]
pub struct Snapshot {
    pub generation: u64,
    pub files: Vec<(String, Vec<u8>)>,
}

pub unsafe fn snapshot(rel: pg_sys::Relation) -> Snapshot {
    loop {
        let gen0 = generation(rel);
        if gen0 == 0 {
            return Snapshot::default();
        }
        let meta = read_meta(rel);
        let mut files = Vec::with_capacity(meta.files.len());
        for (name, (s, n, l)) in &meta.files {
            let bytes = read_run(rel, *s, *n, *l);
            files.push((name.clone(), bytes));
        }
        let gen1 = generation(rel);
        if gen0 == gen1 {
            return Snapshot {
                generation: gen0,
                files,
            };
        }
        check_for_interrupts!();
    }
}

pub unsafe fn apply(rel: pg_sys::Relation, changes: Vec<Change>) {
    let mut meta = read_meta(rel);
    if meta.generation == 0 {
        init(rel);
        meta = read_meta(rel);
    }
    let old_catalog = {
        let sb = read_superblock(rel).unwrap_or((0, 0, 0, 0, 0));
        (sb.2, sb.3)
    };
    for ch in changes {
        match ch {
            Change::Delete(name) => {
                if let Some((s, n, _l)) = meta.files.remove(&name) {
                    meta.free_run(s, n);
                }
            }
            Change::Write(name, bytes) => {
                if let Some((s, n, _l)) = meta.files.remove(&name) {
                    meta.free_run(s, n);
                }
                let need = run_blocks(bytes.len());
                let start = meta.alloc(rel, need);
                write_run(rel, start, need, &bytes);
                meta.files.insert(name, (start, need, bytes.len() as u32));
            }
        }
    }
    meta.generation += 1;
    flush_meta(rel, &mut meta, old_catalog);
}

pub struct WriterGuard {
    tag: pg_sys::LOCKTAG,
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        unsafe {
            pg_sys::LockRelease(&mut self.tag, pg_sys::ExclusiveLock as i32, true);
        }
    }
}

pub unsafe fn lock_writer(relid: u32) -> WriterGuard {
    let mut tag = pg_sys::LOCKTAG {
        locktag_field1: pg_sys::MyDatabaseId.to_u32(),
        locktag_field2: relid,
        locktag_field3: 0,
        locktag_field4: 1,
        locktag_type: pg_sys::LockTagType::LOCKTAG_ADVISORY as u8,
        locktag_lockmethodid: pg_sys::USER_LOCKMETHOD as u8,
    };
    pg_sys::LockAcquire(&mut tag, pg_sys::ExclusiveLock as i32, true, false);
    WriterGuard { tag }
}
