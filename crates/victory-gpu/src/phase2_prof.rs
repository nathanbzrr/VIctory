//! Low-overhead Phase 2 performance profiler (`VICTORY_PHASE2_PROFILE=1`).
//!
//! Collects:
//! - Full `victory-submit` / `card_steps` / `card_before` / `run_deferred` phase breakdown
//! - `heap_buf` / `heap_textures` / `victory-aside` / `victory-thin` breakdown
//! - Exact `PORT_CBV` acceptance & refusal funnel
//! - `port_journal` / `victory-heap-sync` & `victory-relay` / `copying back` breakdown
//! - Hot lock contention (`try_lock` fast path: zero timer overhead when uncontended)
//! - Hot D3D12 COM & Win32 OS call counters (TLS-batched)
//! - Background 5s OS sampler (`victory-prof`): per-thread Kernel+User CPU%, RAM/Commit, PageFaults/s, DXGI VRAM/Shared

#![allow(dead_code)]

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard, Once, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

/// Whether Phase 2 deep profiling is active (`VICTORY_PHASE2_PROFILE=1`).
#[inline(always)]
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let enabled = std::env::var_os("VICTORY_PHASE2_PROFILE").is_some_and(|v| v == "1");
        if enabled {
            start_bg_sampler();
        }
        enabled
    })
}

pub(crate) mod t {
    pub(crate) const SUB_TOTAL: usize = 0;
    pub(crate) const SUB_LOCK_WAIT: usize = 1;
    pub(crate) const SUB_OWN_CARD_STEPS: usize = 2;
    pub(crate) const SUB_LATE_BLOCKED: usize = 3;
    pub(crate) const SUB_SWEEP: usize = 4;
    pub(crate) const SUB_WATCH_ONCE: usize = 5;
    pub(crate) const SUB_WATCH_ASK: usize = 6;
    pub(crate) const SUB_FILTER_STALE: usize = 7;
    pub(crate) const SUB_SETTLE_STALE: usize = 8;
    pub(crate) const SUB_REFRESH_FRONT: usize = 9;
    pub(crate) const SUB_RUN_DEFERRED: usize = 10;
    pub(crate) const SUB_PRELUDE: usize = 11;
    pub(crate) const SUB_BATCH_FLUSH: usize = 12;
    pub(crate) const SUB_SNAPSHOTS: usize = 13;
    pub(crate) const SUB_CARD_STEPS_TAIL: usize = 14;
    pub(crate) const SUB_DECLARE: usize = 15;
    pub(crate) const SUB_CARD_BEFORE: usize = 16;
    pub(crate) const SUB_HEAPS_WAIT: usize = 17;
    pub(crate) const SUB_MAKE_WAITING: usize = 18;
    pub(crate) const SUB_EXEC_LISTS: usize = 19;
    pub(crate) const SUB_LATE_END: usize = 20;
    pub(crate) const SUB_LAND_RECYCLE: usize = 21;
    pub(crate) const SUB_PRESENT: usize = 22;
    pub(crate) const BEHIND_IDLE_WAIT: usize = 23;
    pub(crate) const BEHIND_THIN_SETTLE: usize = 24;

    pub(crate) const DEF_BATCH: usize = 25;
    pub(crate) const DEF_COPIES: usize = 26;
    pub(crate) const DEF_FILLS: usize = 27;
    pub(crate) const DEF_HOST_READS: usize = 28;

    pub(crate) const CS_LOCK_LINE: usize = 29;
    pub(crate) const CS_BLOCKED_BY: usize = 30;
    pub(crate) const CS_COPIES: usize = 31;
    pub(crate) const CS_DOES_OF: usize = 32;
    pub(crate) const CS_PLAN: usize = 33;
    pub(crate) const CS_HOLDER: usize = 34;
    pub(crate) const CS_MAPPED_VQ: usize = 35;
    pub(crate) const CS_ELSEWHERE: usize = 36;
    pub(crate) const CS_RECORD: usize = 37;
    pub(crate) const CS_CONST_SLOTS: usize = 38;

    pub(crate) const CB_SETUP: usize = 39;
    pub(crate) const CB_SEND_TAKE: usize = 40;
    pub(crate) const CB_SEND_COPY_ALL: usize = 41;
    pub(crate) const CB_UPQ_SEND: usize = 42;
    pub(crate) const CB_UPQ_DRAW_WAIT: usize = 43;
    pub(crate) const CB_RESET: usize = 44;
    pub(crate) const CB_HASH: usize = 45;
    pub(crate) const CB_COMPARE: usize = 46;
    pub(crate) const CB_CLOSE_EXEC: usize = 47;

    pub(crate) const HB_STATE_TOTAL: usize = 48;
    pub(crate) const HB_ASIDE_BEGIN: usize = 49;
    pub(crate) const HB_TEXTURES: usize = 50;
    pub(crate) const HT_BOOKKEEP: usize = 51;
    pub(crate) const HT_LOOP_UPLOAD: usize = 52;
    pub(crate) const HB_ASIDE_WAIT: usize = 53;
    pub(crate) const HBP_TOTAL: usize = 54;
    pub(crate) const HBP_CHANGES: usize = 55;
    pub(crate) const HBP_SLOTS: usize = 56;
    pub(crate) const HBP_MIRROR: usize = 57;
    pub(crate) const HBP_BUF_COPY: usize = 58;
    pub(crate) const HBP_SPANS: usize = 59;
    pub(crate) const HBP_HOLES: usize = 60;
    pub(crate) const HBP_BATCH_REFRESH: usize = 61;
    pub(crate) const DRAW_TABLES_TOTAL: usize = 62;
    pub(crate) const DRAW_BIND_TOTAL: usize = 63;

    pub(crate) const HS_TOTAL: usize = 64;
    pub(crate) const HS_DESC_TAKE: usize = 65;
    pub(crate) const HS_FALLBACK_ASK: usize = 66;
    pub(crate) const HS_SCAN: usize = 67;
    pub(crate) const HS_MIRROR: usize = 68;
    pub(crate) const HS_IDLE_WAIT: usize = 69;

    pub(crate) const RL_WAIT_CARD: usize = 70;
    pub(crate) const RL_LAND_TOTAL: usize = 71;
    pub(crate) const RL_GATHER: usize = 72;
    pub(crate) const RL_FETCH_WAIT: usize = 73;
    pub(crate) const RL_LAND_CARD: usize = 74;
    pub(crate) const RL_FIND_ALL: usize = 75;
    pub(crate) const RL_LAND_BACK: usize = 76;
    pub(crate) const RL_READ_BACKS: usize = 77;
    pub(crate) const CS_TOTAL: usize = 78;

    // Extended Wave 1.5 sub-phase & D3D12/PSO timers
    pub(crate) const SEND_MEM: usize = 79;
    pub(crate) const SEND_PAIRS: usize = 80;
    pub(crate) const SEND_DIRECT: usize = 81;
    pub(crate) const RF_PLAN: usize = 82;
    pub(crate) const RF_ASK_AHEAD: usize = 83;
    pub(crate) const RF_TEX_REF: usize = 84;
    pub(crate) const RF_BUF_UPD: usize = 85;
    pub(crate) const PREL_RESET: usize = 86;
    pub(crate) const PREL_UPLOADS: usize = 87;
    pub(crate) const PREL_PREFILL: usize = 88;
    pub(crate) const PREL_CLOSE: usize = 89;
    pub(crate) const DEF_STEP_COPY: usize = 90;
    pub(crate) const DEF_STEP_FILL: usize = 91;
    pub(crate) const DEF_STEP_GPU_WR: usize = 92;
    pub(crate) const DEF_STEP_TEX_FIN: usize = 93;
    pub(crate) const D3D_ALLOC_COMMIT: usize = 94;
    pub(crate) const D3D_MAKE_RESIDENT: usize = 95;
    pub(crate) const PSO_CREATE_GFX: usize = 96;
    pub(crate) const PSO_CREATE_CS: usize = 97;
    pub(crate) const PSO_LOAD_CACHE: usize = 98;
    pub(crate) const DRAW_EARLY_TEX: usize = 99;
    pub(crate) const DRAW_READABLE: usize = 100;
    pub(crate) const DRAW_CBV_DEFER: usize = 101;
    pub(crate) const DRAW_LISTED: usize = 102;
    pub(crate) const PREL_MAKE: usize = 103;
    pub(crate) const D3D_ALLOC_LIST: usize = 104;
    pub(crate) const TBL_DIRECT: usize = 105;
    pub(crate) const TBL_HASH: usize = 106;
    pub(crate) const TBL_MAKE_AT: usize = 107;
    pub(crate) const TBL_FLUSH: usize = 108;
    pub(crate) const TBL_DYN: usize = 109;
    pub(crate) const TBL_FRESH_HIT: usize = 110;
    pub(crate) const TBL_MIR_AFTER: usize = 111;

    pub(crate) const COUNT: usize = 112;
}

pub(crate) mod c {
    // PORT_CBV exact funnel
    pub(crate) const CBV_SEEN_IN_PLACE: usize = 0;
    pub(crate) const CBV_SEEN_SHADOW_DIRECT: usize = 1;
    pub(crate) const CBV_SEEN_LISTED_COPY: usize = 2;
    pub(crate) const CBV_SEEN_FALLBACK: usize = 3;
    pub(crate) const CBV_FRESH_HIT: usize = 4;
    pub(crate) const CBV_FRESH_MISS: usize = 5;
    pub(crate) const CBV_FAIL_PORT_OFF: usize = 6;
    pub(crate) const CBV_FAIL_PLACE_OFF: usize = 7;
    pub(crate) const CBV_FAIL_NO_TWINS: usize = 8;
    pub(crate) const CBV_FAIL_FIND_NONE: usize = 9;
    pub(crate) const CBV_FAIL_NOT_INPLACE_SM0: usize = 10;
    pub(crate) const CBV_FAIL_NOT_CARDED: usize = 11;
    pub(crate) const CBV_FAIL_GPU_WRITTEN: usize = 12;
    pub(crate) const CBV_FAIL_WRITES_PENDING: usize = 13;
    pub(crate) const CBV_FAIL_UNALIGNED: usize = 14;
    pub(crate) const CBV_FAIL_LISTED_RECHECK: usize = 15;
    pub(crate) const CBV_FAIL_LISTVIEWS_FULL: usize = 16;
    pub(crate) const CBV_OK_PLACE: usize = 17;
    pub(crate) const CBV_OK_SHADOW: usize = 18;
    pub(crate) const CBV_OK_SHADOW_BYTES: usize = 19;
    pub(crate) const CBV_OK_COPY: usize = 20;
    pub(crate) const CBV_OK_COPY_BYTES: usize = 21;

    // heap_buf & textures
    pub(crate) const HBP_PASS_ALL: usize = 22;
    pub(crate) const HBP_PASS_SPREAD: usize = 23;
    pub(crate) const HBP_PASS_CHANGED: usize = 24;
    pub(crate) const HBP_SLOTS_SCANNED: usize = 25;
    pub(crate) const HBP_SLOTS_SKIP_TABLED: usize = 26;
    pub(crate) const HBP_SLOTS_MIRRORED: usize = 27;
    pub(crate) const HBP_SLOTS_COPIED: usize = 28;
    pub(crate) const HBP_SPANS_BUILT: usize = 29;
    pub(crate) const HBP_HOLES_SCANNED: usize = 30;
    pub(crate) const HB_EARLY_RET: usize = 31;

    // card_steps & card_before
    pub(crate) const CS_FILLS: usize = 32;
    pub(crate) const CS_COPIES: usize = 33;
    pub(crate) const CS_BATCHES: usize = 34;
    pub(crate) const CS_VQ_CALLS: usize = 35;
    pub(crate) const CS_MAPPED_HITS: usize = 36;
    pub(crate) const CS_MAPPED_MISSES: usize = 37;
    pub(crate) const CB_HASH_PASSES: usize = 38;
    pub(crate) const CB_COMPARE_PASSES: usize = 39;
    pub(crate) const CB_HASH_BYTES: usize = 40;
    pub(crate) const CB_COMPARE_BYTES: usize = 41;

    // heap-sync & relay
    pub(crate) const HS_EARLY_ROUNDS: usize = 42;
    pub(crate) const HS_SUB_ROUNDS: usize = 43;
    pub(crate) const HS_PAGES_SCANNED: usize = 44;
    pub(crate) const HS_SLOTS_MIRRORED: usize = 45;
    pub(crate) const HS_WAIT_COUNT: usize = 46;
    pub(crate) const HS_WAIT_NO_MARK: usize = 47;
    pub(crate) const HS_WAIT_MARKED: usize = 48;

    // Hot D3D12 COM & OS syscalls
    pub(crate) const COM_CREATE_CBV: usize = 49;
    pub(crate) const COM_CREATE_SRV: usize = 50;
    pub(crate) const COM_COPY_DESC: usize = 51;
    pub(crate) const COM_COPY_DESC_SIMPLE: usize = 52;
    pub(crate) const COM_COPY_BUF_REGION: usize = 53;
    pub(crate) const COM_BARRIER: usize = 54;
    pub(crate) const COM_DISPATCH: usize = 55;
    pub(crate) const COM_DRAW: usize = 56;
    pub(crate) const COM_EXEC_LISTS: usize = 57;
    pub(crate) const OS_VIRTUAL_QUERY: usize = 58;
    pub(crate) const OS_GET_WRITE_WATCH: usize = 59;
    pub(crate) const OS_VIRTUAL_PROTECT: usize = 60;
    pub(crate) const OS_READ_MEM: usize = 61;
    pub(crate) const OS_WRITE_MEM: usize = 62;
    pub(crate) const HT_SLOTS_SCANNED: usize = 63;
    pub(crate) const HT_UPLOADED: usize = 64;
    pub(crate) const D3D_ALLOC_BYTES: usize = 65;
    pub(crate) const PSO_CACHE_HITS: usize = 66;
    pub(crate) const PSO_LIVE_COMPILES: usize = 67;
    pub(crate) const SUBMIT_LISTS: usize = 68;

    // Redundant copy & wasted-loop auditors
    pub(crate) const UPD_CALLS: usize = 69;
    pub(crate) const UPD_ZERO_BYTES: usize = 70;
    pub(crate) const UPD_COPIED_BYTES: usize = 71;
    pub(crate) const CMAP_CALLS: usize = 72;
    pub(crate) const CMAP_FALLBACK: usize = 73;
    pub(crate) const CMAP_BYTES: usize = 74;
    pub(crate) const BADD_CALLS: usize = 75;
    pub(crate) const BADD_HIT_LAST: usize = 76;
    pub(crate) const BADD_HIT_AHEAD: usize = 77;
    pub(crate) const BADD_PUSHED: usize = 78;
    pub(crate) const VIEWS_TLS_HIT: usize = 79;
    pub(crate) const VIEWS_TLS_MISS: usize = 80;
    pub(crate) const VIEWS_STALE_FAST: usize = 81;
    pub(crate) const SWEEP_SET_CALLS: usize = 82;
    pub(crate) const SWEEP_SET_NOOP: usize = 83;
    pub(crate) const SWEEP_DIRTY_RECOMP: usize = 84;
    pub(crate) const PREL_CALLS: usize = 85;
    pub(crate) const PREL_EMPTY_RET: usize = 86;
    pub(crate) const PREL_RECORDED: usize = 87;
    pub(crate) const DEF_STEPS_TOTAL: usize = 88;
    pub(crate) const DEF_STEPS_TAKEN_SKIP: usize = 89;
    pub(crate) const WATCH_ALONE_ASKED: usize = 90;
    pub(crate) const WATCH_ALONE_EMPTY: usize = 91;

    pub(crate) const COUNT: usize = 92;
}

pub(crate) mod lock {
    pub(crate) const L_SUBMITTED: usize = 0;
    pub(crate) const L_LINE: usize = 1;
    pub(crate) const L_CARD_WORK: usize = 2;
    pub(crate) const L_QUEUE_ORDER: usize = 3;
    pub(crate) const L_BUFFERS_R: usize = 4;
    pub(crate) const L_BUFFERS_W: usize = 5;
    pub(crate) const L_LOOSE_R: usize = 6;
    pub(crate) const L_LOOSE_W: usize = 7;
    pub(crate) const L_EXACT_R: usize = 8;
    pub(crate) const L_EXACT_W: usize = 9;
    pub(crate) const L_VIEWS_R: usize = 10;
    pub(crate) const L_VIEWS_W: usize = 11;
    pub(crate) const L_HEAPS_R: usize = 12;
    pub(crate) const L_HEAPS_W: usize = 13;
    pub(crate) const L_LAND_POOL: usize = 14;
    pub(crate) const L_LIST_IDLE: usize = 15;
    pub(crate) const L_WATCH_JR: usize = 16;
    pub(crate) const L_WATCH_JW: usize = 17;
    pub(crate) const L_SHADOW_CARDED: usize = 18;
    pub(crate) const L_SHADOW_POOL: usize = 19;
    pub(crate) const L_HOST_COPIED: usize = 20;
    pub(crate) const L_MADE_LOCK: usize = 21;
    pub(crate) const L_DESC_LIST_VIEWS: usize = 22;
    pub(crate) const COUNT: usize = 23;

    pub(crate) const NAMES: [&str; COUNT] = [
        "submitted",
        "LINE",
        "CARD_WORK",
        "QUEUE_ORDER",
        "BUFFERS_R",
        "BUFFERS_W",
        "LOOSE_R",
        "LOOSE_W",
        "EXACT_R",
        "EXACT_W",
        "VIEWS_R",
        "VIEWS_W",
        "HEAPS_R",
        "HEAPS_W",
        "LAND_POOL",
        "LIST_IDLE",
        "WATCH_JR",
        "WATCH_JW",
        "CARDED",
        "SNAP_POOL",
        "HOST_COPIED",
        "MADE_LOCK",
        "LIST_VIEWS",
    ];
}

static TIMER_US: [AtomicU64; t::COUNT] = [const { AtomicU64::new(0) }; t::COUNT];
static TIMER_CALLS: [AtomicU64; t::COUNT] = [const { AtomicU64::new(0) }; t::COUNT];
static HIT_COUNTERS: [AtomicU64; c::COUNT] = [const { AtomicU64::new(0) }; c::COUNT];

static LOCK_ACQ: [AtomicU64; lock::COUNT] = [const { AtomicU64::new(0) }; lock::COUNT];
static LOCK_CONT: [AtomicU64; lock::COUNT] = [const { AtomicU64::new(0) }; lock::COUNT];
static LOCK_WAIT_US: [AtomicU64; lock::COUNT] = [const { AtomicU64::new(0) }; lock::COUNT];

const TLS_BATCH_FLUSH: u64 = 512;

struct TlsState {
    hits: [Cell<u64>; c::COUNT],
    lock_acq: [Cell<u64>; lock::COUNT],
    pending: Cell<u64>,
}

impl TlsState {
    const fn new() -> Self {
        Self {
            hits: [const { Cell::new(0) }; c::COUNT],
            lock_acq: [const { Cell::new(0) }; lock::COUNT],
            pending: Cell::new(0),
        }
    }

    fn flush(&self) {
        if self.pending.get() == 0 {
            return;
        }
        self.pending.set(0);
        for (i, cell) in self.hits.iter().enumerate() {
            let v = cell.replace(0);
            if v != 0 {
                HIT_COUNTERS[i].fetch_add(v, Relaxed);
            }
        }
        for (i, cell) in self.lock_acq.iter().enumerate() {
            let v = cell.replace(0);
            if v != 0 {
                LOCK_ACQ[i].fetch_add(v, Relaxed);
            }
        }
    }
}

thread_local! {
    static TLS: TlsState = const { TlsState::new() };
}

pub(crate) struct Span {
    slot: Option<(usize, Instant)>,
}

#[inline(always)]
pub(crate) fn span(slot: usize) -> Span {
    if on() {
        Span { slot: Some((slot, Instant::now())) }
    } else {
        Span { slot: None }
    }
}

impl Drop for Span {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some((slot, started)) = self.slot {
            TIMER_US[slot].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
            TIMER_CALLS[slot].fetch_add(1, Relaxed);
        }
    }
}

#[inline(always)]
pub(crate) fn record_time(slot: usize, elapsed_us: u64) {
    if on() {
        TIMER_US[slot].fetch_add(elapsed_us, Relaxed);
        TIMER_CALLS[slot].fetch_add(1, Relaxed);
    }
}

#[inline(always)]
pub(crate) fn add_calls(slot: usize, calls: u64) {
    if on() {
        TIMER_CALLS[slot].fetch_add(calls, Relaxed);
    }
}

#[inline(always)]
pub(crate) fn inc(counter: usize) {
    inc_by(counter, 1);
}

#[inline(always)]
pub(crate) fn inc_by(counter: usize, delta: u64) {
    if !on() || delta == 0 {
        return;
    }
    let _ = TLS.try_with(|t| {
        let cell = &t.hits[counter];
        cell.set(cell.get().saturating_add(delta));
        let p = t.pending.get() + 1;
        if p >= TLS_BATCH_FLUSH {
            t.flush();
        } else {
            t.pending.set(p);
        }
    });
}

#[inline(always)]
pub(crate) fn flush_tls() {
    if !on() {
        return;
    }
    let _ = TLS.try_with(|t| t.flush());
}

#[inline(always)]
fn note_lock_acq(id: usize) {
    let _ = TLS.try_with(|t| {
        let cell = &t.lock_acq[id];
        cell.set(cell.get() + 1);
        let p = t.pending.get() + 1;
        if p >= TLS_BATCH_FLUSH {
            t.flush();
        } else {
            t.pending.set(p);
        }
    });
}

#[inline(always)]
pub(crate) fn lock_mutex<'a, T>(id: usize, m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    if !on() {
        return m.lock().unwrap_or_else(|e| e.into_inner());
    }
    note_lock_acq(id);
    match m.try_lock() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let started = Instant::now();
            let g = m.lock().unwrap_or_else(|e| e.into_inner());
            LOCK_CONT[id].fetch_add(1, Relaxed);
            LOCK_WAIT_US[id].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
            g
        }
    }
}

#[inline(always)]
pub(crate) fn read_rw<'a, T>(id: usize, rw: &'a RwLock<T>) -> RwLockReadGuard<'a, T> {
    if !on() {
        return rw.read().unwrap_or_else(|e| e.into_inner());
    }
    note_lock_acq(id);
    match rw.try_read() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let started = Instant::now();
            let g = rw.read().unwrap_or_else(|e| e.into_inner());
            LOCK_CONT[id].fetch_add(1, Relaxed);
            LOCK_WAIT_US[id].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
            g
        }
    }
}

#[inline(always)]
pub(crate) fn write_rw<'a, T>(id: usize, rw: &'a RwLock<T>) -> RwLockWriteGuard<'a, T> {
    if !on() {
        return rw.write().unwrap_or_else(|e| e.into_inner());
    }
    note_lock_acq(id);
    match rw.try_write() {
        Ok(g) => g,
        Err(TryLockError::Poisoned(p)) => p.into_inner(),
        Err(TryLockError::WouldBlock) => {
            let started = Instant::now();
            let g = rw.write().unwrap_or_else(|e| e.into_inner());
            LOCK_CONT[id].fetch_add(1, Relaxed);
            LOCK_WAIT_US[id].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
            g
        }
    }
}

/// Helper to time waiting on an already-known `Instant` when a lock couldn't be
/// cleanly wrapped via `lock_mutex`.
#[inline(always)]
pub(crate) fn note_lock_contended(id: usize, wait_us: u64) {
    if !on() {
        return;
    }
    note_lock_acq(id);
    if wait_us > 0 {
        LOCK_CONT[id].fetch_add(1, Relaxed);
        LOCK_WAIT_US[id].fetch_add(wait_us, Relaxed);
    }
}

#[derive(Clone)]
struct Snapshot {
    at: Instant,
    presented: u64,
    timer_us: [u64; t::COUNT],
    timer_calls: [u64; t::COUNT],
    hits: [u64; c::COUNT],
    lock_acq: [u64; lock::COUNT],
    lock_cont: [u64; lock::COUNT],
    lock_wait_us: [u64; lock::COUNT],
}

impl Snapshot {
    fn take(presented: u64) -> Self {
        Self {
            at: Instant::now(),
            presented,
            timer_us: std::array::from_fn(|i| TIMER_US[i].load(Relaxed)),
            timer_calls: std::array::from_fn(|i| TIMER_CALLS[i].load(Relaxed)),
            hits: std::array::from_fn(|i| HIT_COUNTERS[i].load(Relaxed)),
            lock_acq: std::array::from_fn(|i| LOCK_ACQ[i].load(Relaxed)),
            lock_cont: std::array::from_fn(|i| LOCK_CONT[i].load(Relaxed)),
            lock_wait_us: std::array::from_fn(|i| LOCK_WAIT_US[i].load(Relaxed)),
        }
    }
}

/// Emits multi-line structured Phase 2 breakdown aligned to `trace_present()`.
pub(crate) fn emit_breakdown(presented_total: u64) {
    if !on() {
        return;
    }
    flush_tls();
    static PREV: Mutex<Option<Snapshot>> = Mutex::new(None);
    let mut guard = PREV.lock().unwrap_or_else(|e| e.into_inner());
    let cur = Snapshot::take(presented_total);
    let Some(prev) = guard.replace(cur.clone()) else {
        return;
    };
    let frames = cur.presented.saturating_sub(prev.presented);
    if frames == 0 {
        return;
    }
    let f = frames as f64;
    let ms = |idx: usize| cur.timer_us[idx].saturating_sub(prev.timer_us[idx]) as f64 / 1000.0 / f;
    let calls = |idx: usize| cur.timer_calls[idx].saturating_sub(prev.timer_calls[idx]) as f64 / f;
    let cnt = |idx: usize| cur.hits[idx].saturating_sub(prev.hits[idx]);
    let cntf = |idx: usize| cnt(idx) as f64 / f;

    let sub_parts_sum = ms(t::SUB_LOCK_WAIT)
        + ms(t::SUB_OWN_CARD_STEPS)
        + ms(t::SUB_LATE_BLOCKED)
        + ms(t::SUB_SWEEP)
        + ms(t::SUB_WATCH_ONCE)
        + ms(t::SUB_WATCH_ASK)
        + ms(t::SUB_FILTER_STALE)
        + ms(t::SUB_SETTLE_STALE)
        + ms(t::SUB_REFRESH_FRONT)
        + ms(t::SUB_RUN_DEFERRED)
        + ms(t::SUB_PRELUDE)
        + ms(t::SUB_BATCH_FLUSH)
        + ms(t::SUB_SNAPSHOTS)
        + ms(t::SUB_CARD_STEPS_TAIL)
        + ms(t::SUB_DECLARE)
        + ms(t::SUB_CARD_BEFORE)
        + ms(t::SUB_HEAPS_WAIT)
        + ms(t::SUB_MAKE_WAITING)
        + ms(t::SUB_EXEC_LISTS)
        + ms(t::SUB_LATE_END)
        + ms(t::SUB_LAND_RECYCLE);
    let sub_other = (ms(t::SUB_TOTAL) - sub_parts_sum).max(0.0);

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_SUB ({frames}f, {:.2}s): sub_total={:.2}ms({:.1}subs) | lock={:.2} own_cs={:.2} late_blk={:.2} sweep={:.2} w_once={:.2} w_ask={:.2} filt_stale={:.2} set_stale={:.2} ref_front={:.2} run_def={:.2}[batch={:.2} copy={:.2} fill={:.2} read={:.2}] prel={:.2} bflush={:.2} snap={:.2} cs_tail={:.2} decl={:.2} card_bef={:.2} heaps_wt={:.2} mk_wait={:.2} exec={:.2} late_end={:.2} land={:.2} other={:.2} | pres={:.2} thin_settle={:.2} idle={:.2}",
        (cur.at - prev.at).as_secs_f64(),
        ms(t::SUB_TOTAL),
        calls(t::SUB_TOTAL),
        ms(t::SUB_LOCK_WAIT),
        ms(t::SUB_OWN_CARD_STEPS),
        ms(t::SUB_LATE_BLOCKED),
        ms(t::SUB_SWEEP),
        ms(t::SUB_WATCH_ONCE),
        ms(t::SUB_WATCH_ASK),
        ms(t::SUB_FILTER_STALE),
        ms(t::SUB_SETTLE_STALE),
        ms(t::SUB_REFRESH_FRONT),
        ms(t::SUB_RUN_DEFERRED),
        ms(t::DEF_BATCH),
        ms(t::DEF_COPIES),
        ms(t::DEF_FILLS),
        ms(t::DEF_HOST_READS),
        ms(t::SUB_PRELUDE),
        ms(t::SUB_BATCH_FLUSH),
        ms(t::SUB_SNAPSHOTS),
        ms(t::SUB_CARD_STEPS_TAIL),
        ms(t::SUB_DECLARE),
        ms(t::SUB_CARD_BEFORE),
        ms(t::SUB_HEAPS_WAIT),
        ms(t::SUB_MAKE_WAITING),
        ms(t::SUB_EXEC_LISTS),
        ms(t::SUB_LATE_END),
        ms(t::SUB_LAND_RECYCLE),
        sub_other,
        ms(t::SUB_PRESENT),
        ms(t::BEHIND_THIN_SETTLE),
        ms(t::BEHIND_IDLE_WAIT),
    );

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_CARD: cs_total={:.2}ms({:.1}c) [line={:.2} blk={:.2} copies={:.2} does={:.2}({:.0}f/{:.0}c/{:.0}b) plan={:.2} holder={:.2} vq={:.2}({:.0}c,hit={:.0},miss={:.0}) elsew={:.2} rec={:.2} cslots={:.2}] | cb_total={:.2}ms [setup={:.2} send_take={:.2}(cpy_all={:.2}) upq={:.2}(draw_wt={:.2}) reset={:.2} hash={:.2}({:.0}p,{:.1}MB) cmp={:.2}({:.0}p,{:.1}MB) exec={:.2}]",
        ms(t::CS_TOTAL),
        calls(t::CS_TOTAL),
        ms(t::CS_LOCK_LINE),
        ms(t::CS_BLOCKED_BY),
        ms(t::CS_COPIES),
        ms(t::CS_DOES_OF),
        cntf(c::CS_FILLS),
        cntf(c::CS_COPIES),
        cntf(c::CS_BATCHES),
        ms(t::CS_PLAN),
        ms(t::CS_HOLDER),
        ms(t::CS_MAPPED_VQ),
        cntf(c::CS_VQ_CALLS),
        cntf(c::CS_MAPPED_HITS),
        cntf(c::CS_MAPPED_MISSES),
        ms(t::CS_ELSEWHERE),
        ms(t::CS_RECORD),
        ms(t::CS_CONST_SLOTS),
        ms(t::SUB_CARD_BEFORE),
        ms(t::CB_SETUP),
        ms(t::CB_SEND_TAKE),
        ms(t::CB_SEND_COPY_ALL),
        ms(t::CB_UPQ_SEND),
        ms(t::CB_UPQ_DRAW_WAIT),
        ms(t::CB_RESET),
        ms(t::CB_HASH),
        cntf(c::CB_HASH_PASSES),
        cntf(c::CB_HASH_BYTES) / (1024.0 * 1024.0),
        ms(t::CB_COMPARE),
        cntf(c::CB_COMPARE_PASSES),
        cntf(c::CB_COMPARE_BYTES) / (1024.0 * 1024.0),
        ms(t::CB_CLOSE_EXEC),
    );

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_HEAP: hb_state={:.2}ms({:.1}c,early={:.0}) [aside_beg={:.2} tex={:.2}(bk={:.2},up={:.2},scan={:.0},did={:.0}) aside_wt={:.2}] | hbp_work={:.2}ms({:.1}all/{:.1}spr/{:.1}chg) [chg={:.2} slots={:.2}(scan={:.0},skip={:.0}) mir={:.2}({:.0}) bcpy={:.2}({:.0}) spans={:.2}({:.0}) holes={:.2}({:.0}) bref={:.2}] | draw_bind={:.2}ms tables={:.2}ms(fresh_hit={:.0}/miss={:.0})",
        ms(t::HB_STATE_TOTAL),
        calls(t::HB_STATE_TOTAL),
        cntf(c::HB_EARLY_RET),
        ms(t::HB_ASIDE_BEGIN),
        ms(t::HB_TEXTURES),
        ms(t::HT_BOOKKEEP),
        ms(t::HT_LOOP_UPLOAD),
        cntf(c::HT_SLOTS_SCANNED),
        cntf(c::HT_UPLOADED),
        ms(t::HB_ASIDE_WAIT),
        ms(t::HBP_TOTAL),
        cntf(c::HBP_PASS_ALL),
        cntf(c::HBP_PASS_SPREAD),
        cntf(c::HBP_PASS_CHANGED),
        ms(t::HBP_CHANGES),
        ms(t::HBP_SLOTS),
        cntf(c::HBP_SLOTS_SCANNED),
        cntf(c::HBP_SLOTS_SKIP_TABLED),
        ms(t::HBP_MIRROR),
        cntf(c::HBP_SLOTS_MIRRORED),
        ms(t::HBP_BUF_COPY),
        cntf(c::HBP_SLOTS_COPIED),
        ms(t::HBP_SPANS),
        cntf(c::HBP_SPANS_BUILT),
        ms(t::HBP_HOLES),
        cntf(c::HBP_HOLES_SCANNED),
        ms(t::HBP_BATCH_REFRESH),
        ms(t::DRAW_BIND_TOTAL),
        ms(t::DRAW_TABLES_TOTAL),
        cntf(c::CBV_FRESH_HIT),
        cntf(c::CBV_FRESH_MISS),
    );

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_CBV: listed[place={:.0} shad={:.0} copy={:.0}] fallback={:.0} | ok[place={:.0} shad={:.0}({:.1}MB) copy={:.0}({:.1}MB)] | fail[port_off={:.0} place_off={:.0} no_twins={:.0} no_shadow={:.0} sm0_not_inplace={:.0} not_carded={:.0} gpu_written={:.0} writes_pend={:.0} unaligned={:.0} listed_rechk={:.0} views_full={:.0}]",
        cntf(c::CBV_SEEN_IN_PLACE),
        cntf(c::CBV_SEEN_SHADOW_DIRECT),
        cntf(c::CBV_SEEN_LISTED_COPY),
        cntf(c::CBV_SEEN_FALLBACK),
        cntf(c::CBV_OK_PLACE),
        cntf(c::CBV_OK_SHADOW),
        cntf(c::CBV_OK_SHADOW_BYTES) / (1024.0 * 1024.0),
        cntf(c::CBV_OK_COPY),
        cntf(c::CBV_OK_COPY_BYTES) / (1024.0 * 1024.0),
        cntf(c::CBV_FAIL_PORT_OFF),
        cntf(c::CBV_FAIL_PLACE_OFF),
        cntf(c::CBV_FAIL_NO_TWINS),
        cntf(c::CBV_FAIL_FIND_NONE),
        cntf(c::CBV_FAIL_NOT_INPLACE_SM0),
        cntf(c::CBV_FAIL_NOT_CARDED),
        cntf(c::CBV_FAIL_GPU_WRITTEN),
        cntf(c::CBV_FAIL_WRITES_PENDING),
        cntf(c::CBV_FAIL_UNALIGNED),
        cntf(c::CBV_FAIL_LISTED_RECHECK),
        cntf(c::CBV_FAIL_LISTVIEWS_FULL),
    );

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_SYNC_RELAY: heap_sync={:.2}ms(sub={:.1}/early={:.1} pages={:.0} mir={:.0}) [take={:.2} ask={:.2} scan={:.2} mir={:.2} idle={:.2}] heaps_wait[calls={:.1} no_mark={:.1} marked={:.1}] | relay_wait={:.2}ms land={:.2}ms [gather={:.2} fetch={:.2} card={:.2} find_all={:.2} back={:.2} rb={:.2}]",
        ms(t::HS_TOTAL),
        cntf(c::HS_SUB_ROUNDS),
        cntf(c::HS_EARLY_ROUNDS),
        cntf(c::HS_PAGES_SCANNED),
        cntf(c::HS_SLOTS_MIRRORED),
        ms(t::HS_DESC_TAKE),
        ms(t::HS_FALLBACK_ASK),
        ms(t::HS_SCAN),
        ms(t::HS_MIRROR),
        ms(t::HS_IDLE_WAIT),
        cntf(c::HS_WAIT_COUNT),
        cntf(c::HS_WAIT_NO_MARK),
        cntf(c::HS_WAIT_MARKED),
        ms(t::RL_WAIT_CARD),
        ms(t::RL_LAND_TOTAL),
        ms(t::RL_GATHER),
        ms(t::RL_FETCH_WAIT),
        ms(t::RL_LAND_CARD),
        ms(t::RL_FIND_ALL),
        ms(t::RL_LAND_BACK),
        ms(t::RL_READ_BACKS),
    );

    // Top contended locks sorted by wait time descending
    let mut locks: Vec<(usize, u64, u64, f64)> = (0..lock::COUNT)
        .filter_map(|i| {
            let acq = cur.lock_acq[i].saturating_sub(prev.lock_acq[i]);
            let cont = cur.lock_cont[i].saturating_sub(prev.lock_cont[i]);
            let wait = cur.lock_wait_us[i].saturating_sub(prev.lock_wait_us[i]) as f64 / 1000.0 / f;
            (cont > 0 || wait >= 0.005 || acq >= frames * 50).then_some((i, acq / frames.max(1), cont, wait))
        })
        .collect();
    locks.sort_by(|a, b| b.3.total_cmp(&a.3).then_with(|| b.2.cmp(&a.2)));
    let mut lock_str = String::new();
    for (i, acq_f, cont, wait) in locks.iter().take(8) {
        if !lock_str.is_empty() {
            lock_str.push(' ');
        }
        let _ = write!(lock_str, "{}={:.3}ms({}c/{}a)", lock::NAMES[*i], wait, cont, acq_f);
    }

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_LOCKS_COM: locks=[{}] | com/f[cbv={:.0} srv={:.0} cpDesc={:.0}/{:.0}s cpBuf={:.0} bar={:.0} disp={:.0} draw={:.0} exec={:.0}] os/f[vq={:.0} gww={:.0} vp={:.0} rmem={:.0} wmem={:.0}]",
        lock_str,
        cntf(c::COM_CREATE_CBV),
        cntf(c::COM_CREATE_SRV),
        cntf(c::COM_COPY_DESC),
        cntf(c::COM_COPY_DESC_SIMPLE),
        cntf(c::COM_COPY_BUF_REGION),
        cntf(c::COM_BARRIER),
        cntf(c::COM_DISPATCH),
        cntf(c::COM_DRAW),
        cntf(c::COM_EXEC_LISTS),
        cntf(c::OS_VIRTUAL_QUERY),
        cntf(c::OS_GET_WRITE_WATCH),
        cntf(c::OS_VIRTUAL_PROTECT),
        cntf(c::OS_READ_MEM),
        cntf(c::OS_WRITE_MEM),
    );

    tracing::info!(
        target: "phase2_prof",
        "PHASE2_DEEP: send[mem={:.2} pairs={:.2} dir={:.2} cpy={:.2}] rf[plan={:.2} ask={:.2} tex={:.2} buf={:.2}] prel[mk={:.2} rst={:.2} up={:.2} pref={:.2} cls={:.2}] def[cpy={:.2} fill={:.2} gwr={:.2} tex={:.2}] | bind[tot={:.2} early_t={:.2} tbl={:.2} listed={:.2} cbv={:.2} read={:.2}] tblx[dir={:.2} hash={:.2} mk={:.2} fl={:.2} dyn={:.2} hit={:.2} mir={:.2}] | d3d[alloc={:.2}ms({:.1}c,{:.1}MB) list_al={:.2}ms({:.1}c) res={:.2}ms({:.1}c) pso_gfx={:.2}ms({:.1}c) pso_cs={:.2}ms({:.1}c) pso_hit={:.1}({:.2}ms)]",
        ms(t::SEND_MEM),
        ms(t::SEND_PAIRS),
        ms(t::SEND_DIRECT),
        ms(t::CB_SEND_COPY_ALL),
        ms(t::RF_PLAN),
        ms(t::RF_ASK_AHEAD),
        ms(t::RF_TEX_REF),
        ms(t::RF_BUF_UPD),
        ms(t::PREL_MAKE),
        ms(t::PREL_RESET),
        ms(t::PREL_UPLOADS),
        ms(t::PREL_PREFILL),
        ms(t::PREL_CLOSE),
        ms(t::DEF_STEP_COPY),
        ms(t::DEF_STEP_FILL),
        ms(t::DEF_STEP_GPU_WR),
        ms(t::DEF_STEP_TEX_FIN),
        ms(t::DRAW_BIND_TOTAL),
        ms(t::DRAW_EARLY_TEX),
        ms(t::DRAW_TABLES_TOTAL),
        ms(t::DRAW_LISTED),
        ms(t::DRAW_CBV_DEFER),
        ms(t::DRAW_READABLE),
        ms(t::TBL_DIRECT),
        ms(t::TBL_HASH),
        ms(t::TBL_MAKE_AT),
        ms(t::TBL_FLUSH),
        ms(t::TBL_DYN),
        ms(t::TBL_FRESH_HIT),
        ms(t::TBL_MIR_AFTER),
        ms(t::D3D_ALLOC_COMMIT),
        calls(t::D3D_ALLOC_COMMIT),
        cntf(c::D3D_ALLOC_BYTES) / (1024.0 * 1024.0),
        ms(t::D3D_ALLOC_LIST),
        calls(t::D3D_ALLOC_LIST),
        ms(t::D3D_MAKE_RESIDENT),
        calls(t::D3D_MAKE_RESIDENT),
        ms(t::PSO_CREATE_GFX),
        calls(t::PSO_CREATE_GFX),
        ms(t::PSO_CREATE_CS),
        calls(t::PSO_CREATE_CS),
        cntf(c::PSO_CACHE_HITS),
        ms(t::PSO_LOAD_CACHE),
    );

    let (j_avg, j_p95, j_max, p999_ms, hitches100) = take_pacing_window();
    let (gpu_work_ms, gpu_cadence_ms, gpu_n) = take_gpu_window();
    let (started_sub, finished_sub) = crate::host::submissions();
    let inflight = started_sub.saturating_sub(finished_sub);
    let stream = take_stream_deltas(frames);
    tracing::info!(
        target: "phase2_prof",
        "PHASE2_PACING_STREAM: jitter[avg={:.2} p95={:.2} max={:.2}] p999={:.2}ms hitches100={} | gpu[work={:.2}ms cadence={:.2}ms n={}] inflight={} lists/f={:.1} | stream[drop/f={:.1} tex_wait={} tex_due={} tex_up={:.1}({:.1}MB) tex_full={} unmap_null={} neutral={} page_wr={}/{}]",
        j_avg,
        j_p95,
        j_max,
        p999_ms,
        hitches100,
        gpu_work_ms,
        gpu_cadence_ms,
        gpu_n,
        inflight,
        cntf(c::SUBMIT_LISTS),
        stream.dropped_per_frame,
        stream.tex_waiting,
        stream.tex_due,
        stream.tex_up_per_frame,
        stream.tex_up_mb,
        stream.tex_full_delta,
        stream.unmap_null_delta,
        stream.neutral_delta,
        stream.page_wr_kept_delta,
        stream.page_wr_requeued_delta,
    );

    let leaks = take_leak_stats();
    tracing::info!(
        target: "phase2_prof",
        "PHASE2_LEAKS: live[shadows={}({}MB) snap_kept={}MB tex={}({}MB) lists={} scratch={}({}MB,spare={}/{}MB) up_held={}({}MB)] | churn5s[up=+{}({}MB) sh=+{}({}MB) card=+{}({}MB) lists=+{} prel=+{}]",
        leaks.shadows_n,
        leaks.shadows_mb,
        leaks.snap_kept_mb,
        leaks.tex_n,
        leaks.tex_mb,
        leaks.lists_live,
        leaks.scratch_n,
        leaks.scratch_mb,
        leaks.spare_n,
        leaks.spare_mb,
        leaks.up_held_n,
        leaks.up_held_mb,
        leaks.d_up_n,
        leaks.d_up_mb,
        leaks.d_sh_n,
        leaks.d_sh_mb,
        leaks.d_card_n,
        leaks.d_card_mb,
        leaks.d_lists,
        leaks.d_prels,
    );

    let upd_c = cntf(c::UPD_CALLS);
    let upd_z = cntf(c::UPD_ZERO_BYTES);
    let upd_z_pct = if upd_c > 0.0 { upd_z * 100.0 / upd_c } else { 0.0 };
    let vt_h = cntf(c::VIEWS_TLS_HIT);
    let vt_m = cntf(c::VIEWS_TLS_MISS);
    let vt_pct = if vt_h + vt_m > 0.0 { vt_h * 100.0 / (vt_h + vt_m) } else { 0.0 };
    tracing::info!(
        target: "phase2_prof",
        "PHASE2_COPIES_WASTE: upd/f[c={:.0} zero={:.0}({:.0}%) mb={:.1}] cmap/f[c={:.0} fb={:.0} mb={:.1}] badd/f[c={:.0} last={:.0} ahead={:.0} push={:.0}] | views_tls[hit={:.0}({:.1}%) miss={:.0} stale={:.0}] sweep[set={:.0} noop={:.0} recomp={:.0}] prel[c={:.0} empty={:.0} rec={:.0}] def_steps[tot={:.0} skip={:.0}] watch_alone[ask={:.0} empty={:.0}]",
        upd_c,
        upd_z,
        upd_z_pct,
        cntf(c::UPD_COPIED_BYTES) / (1024.0 * 1024.0),
        cntf(c::CMAP_CALLS),
        cntf(c::CMAP_FALLBACK),
        cntf(c::CMAP_BYTES) / (1024.0 * 1024.0),
        cntf(c::BADD_CALLS),
        cntf(c::BADD_HIT_LAST),
        cntf(c::BADD_HIT_AHEAD),
        cntf(c::BADD_PUSHED),
        vt_h,
        vt_pct,
        vt_m,
        cntf(c::VIEWS_STALE_FAST),
        cntf(c::SWEEP_SET_CALLS),
        cntf(c::SWEEP_SET_NOOP),
        cntf(c::SWEEP_DIRTY_RECOMP),
        cntf(c::PREL_CALLS),
        cntf(c::PREL_EMPTY_RET),
        cntf(c::PREL_RECORDED),
        cntf(c::DEF_STEPS_TOTAL),
        cntf(c::DEF_STEPS_TAKEN_SKIP),
        cntf(c::WATCH_ALONE_ASKED),
        cntf(c::WATCH_ALONE_EMPTY),
    );
}

struct LeakPrev {
    made: [u64; 6],
    lists: [u64; 2],
}

struct LeakStats {
    shadows_n: u64,
    shadows_mb: u64,
    snap_kept_mb: u64,
    tex_n: u64,
    tex_mb: u64,
    lists_live: u64,
    scratch_n: u64,
    scratch_mb: u64,
    spare_n: u64,
    spare_mb: u64,
    up_held_n: u64,
    up_held_mb: u64,
    d_up_n: u64,
    d_up_mb: u64,
    d_sh_n: u64,
    d_sh_mb: u64,
    d_card_n: u64,
    d_card_mb: u64,
    d_lists: u64,
    d_prels: u64,
}

fn take_leak_stats() -> LeakStats {
    static PREV: Mutex<Option<LeakPrev>> = Mutex::new(None);
    let (sh_n, sh_b, snap_b) = crate::shadow::live_stats();
    let tex_n = crate::resource::HOST_TEXTURES[0].load(Relaxed);
    let tex_b = crate::resource::HOST_TEXTURES[1].load(Relaxed);
    let alive: [u64; 8] = std::array::from_fn(|i| crate::host::ALIVE[i].load(Relaxed));
    let made: [u64; 6] = std::array::from_fn(|i| crate::host::MADE[i].load(Relaxed));
    let lists: [u64; 2] = [crate::host::LISTS[0].load(Relaxed), crate::host::LISTS[1].load(Relaxed)];
    let mut g = PREV.lock().unwrap_or_else(|e| e.into_inner());
    let prev = g.replace(LeakPrev { made, lists });
    let (d_up_n, d_up_mb, d_sh_n, d_sh_mb, d_card_n, d_card_mb, d_lists, d_prels) = match prev {
        Some(p) => (
            made[0].saturating_sub(p.made[0]),
            made[1].saturating_sub(p.made[1]) >> 20,
            made[2].saturating_sub(p.made[2]),
            made[3].saturating_sub(p.made[3]) >> 20,
            made[4].saturating_sub(p.made[4]),
            made[5].saturating_sub(p.made[5]) >> 20,
            lists[0].saturating_sub(p.lists[0]),
            lists[1].saturating_sub(p.lists[1]),
        ),
        None => (0, 0, 0, 0, 0, 0, 0, 0),
    };
    LeakStats {
        shadows_n: sh_n,
        shadows_mb: sh_b >> 20,
        snap_kept_mb: snap_b >> 20,
        tex_n,
        tex_mb: tex_b >> 20,
        lists_live: alive[0],
        scratch_n: alive[1],
        scratch_mb: alive[2] >> 20,
        spare_n: alive[3],
        spare_mb: alive[4] >> 20,
        up_held_n: alive[6],
        up_held_mb: alive[7] >> 20,
        d_up_n,
        d_up_mb,
        d_sh_n,
        d_sh_mb,
        d_card_n,
        d_card_mb,
        d_lists,
        d_prels,
    }
}

struct PacingWindow {
    prev_dt: Option<f64>,
    dts: Vec<f64>,
    deltas: Vec<f64>,
    hitches_100: u64,
    last_frame_snap: Option<FrameSnap>,
    frame_no: u64,
}

#[derive(Clone, Copy)]
struct FrameSnap {
    sub_total: u64,
    own_cs: u64,
    ref_front: u64,
    run_def: u64,
    prel: u64,
    card_bef: u64,
    send_take: u64,
    watch_ask: u64,
    settle_stale: u64,
    late_blk: u64,
    heaps_wt: u64,
    pres: u64,
    pso_us: u64,
    alloc_us: u64,
    res_us: u64,
    views_r_wt: u64,
    line_wt: u64,
    pso_live: u64,
}

impl FrameSnap {
    fn now() -> Self {
        Self {
            sub_total: TIMER_US[t::SUB_TOTAL].load(Relaxed),
            own_cs: TIMER_US[t::SUB_OWN_CARD_STEPS].load(Relaxed),
            ref_front: TIMER_US[t::SUB_REFRESH_FRONT].load(Relaxed),
            run_def: TIMER_US[t::SUB_RUN_DEFERRED].load(Relaxed),
            prel: TIMER_US[t::SUB_PRELUDE].load(Relaxed),
            card_bef: TIMER_US[t::SUB_CARD_BEFORE].load(Relaxed),
            send_take: TIMER_US[t::CB_SEND_TAKE].load(Relaxed),
            watch_ask: TIMER_US[t::SUB_WATCH_ASK].load(Relaxed),
            settle_stale: TIMER_US[t::SUB_SETTLE_STALE].load(Relaxed),
            late_blk: TIMER_US[t::SUB_LATE_BLOCKED].load(Relaxed),
            heaps_wt: TIMER_US[t::SUB_HEAPS_WAIT].load(Relaxed),
            pres: TIMER_US[t::SUB_PRESENT].load(Relaxed),
            pso_us: TIMER_US[t::PSO_CREATE_GFX].load(Relaxed) + TIMER_US[t::PSO_CREATE_CS].load(Relaxed),
            alloc_us: TIMER_US[t::D3D_ALLOC_COMMIT].load(Relaxed),
            res_us: TIMER_US[t::D3D_MAKE_RESIDENT].load(Relaxed),
            views_r_wt: LOCK_WAIT_US[lock::L_VIEWS_R].load(Relaxed),
            line_wt: LOCK_WAIT_US[lock::L_LINE].load(Relaxed),
            pso_live: HIT_COUNTERS[c::PSO_LIVE_COMPILES].load(Relaxed),
        }
    }
}

static PACING_WIN: Mutex<PacingWindow> = Mutex::new(PacingWindow {
    prev_dt: None,
    dts: Vec::new(),
    deltas: Vec::new(),
    hitches_100: 0,
    last_frame_snap: None,
    frame_no: 0,
});

/// Called on every presented frame from `FramePacing::record`: tracks frame-to-frame
/// jitter (`MSDelta`), tail percentiles, and emits an immediate `PHASE2_HITCH`
/// root-cause snapshot whenever a single frame exceeds 100 ms.
pub(crate) fn on_present_frame(dt_ms: f64, draws: u64) {
    if !on() {
        return;
    }
    let cur = FrameSnap::now();
    let mut w = PACING_WIN.lock().unwrap_or_else(|e| e.into_inner());
    w.frame_no += 1;
    w.dts.push(dt_ms);
    if let Some(prev) = w.prev_dt.replace(dt_ms) {
        w.deltas.push((dt_ms - prev).abs());
    }
    let prev_snap = w.last_frame_snap.replace(cur);
    let is_hitch = dt_ms >= 100.0;
    if is_hitch {
        w.hitches_100 += 1;
    }
    let fno = w.frame_no;
    drop(w);

    if is_hitch
        && let Some(p) = prev_snap
    {
        let d = |a: u64, b: u64| a.saturating_sub(b) as f64 / 1000.0;
        let (s_sub, f_sub) = crate::host::submissions();
        tracing::info!(
            target: "phase2_prof",
            "PHASE2_HITCH (f=#{fno} dt={dt_ms:.1}ms draws={draws} inflight={}): sub={:.1} [cs={:.1} rf={:.1} def={:.1} prel={:.1} cb={:.1}(send={:.1}) ask={:.1} stale={:.1} lblk={:.1} hwt={:.1} pres={:.1}] d3d[pso={:.1}({}c) alloc={:.1} res={:.1}] locks[views_r={:.1} line={:.1}]",
            s_sub.saturating_sub(f_sub),
            d(cur.sub_total, p.sub_total),
            d(cur.own_cs, p.own_cs),
            d(cur.ref_front, p.ref_front),
            d(cur.run_def, p.run_def),
            d(cur.prel, p.prel),
            d(cur.card_bef, p.card_bef),
            d(cur.send_take, p.send_take),
            d(cur.watch_ask, p.watch_ask),
            d(cur.settle_stale, p.settle_stale),
            d(cur.late_blk, p.late_blk),
            d(cur.heaps_wt, p.heaps_wt),
            d(cur.pres, p.pres),
            d(cur.pso_us, p.pso_us),
            cur.pso_live.saturating_sub(p.pso_live),
            d(cur.alloc_us, p.alloc_us),
            d(cur.res_us, p.res_us),
            d(cur.views_r_wt, p.views_r_wt),
            d(cur.line_wt, p.line_wt),
        );
    }
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn take_pacing_window() -> (f64, f64, f64, f64, u64) {
    let (mut dts, mut deltas, hitches) = {
        let mut w = PACING_WIN.lock().unwrap_or_else(|e| e.into_inner());
        let dts = std::mem::take(&mut w.dts);
        let deltas = std::mem::take(&mut w.deltas);
        let hitches = std::mem::take(&mut w.hitches_100);
        (dts, deltas, hitches)
    };
    dts.sort_by(|a, b| a.total_cmp(b));
    deltas.sort_by(|a, b| a.total_cmp(b));
    let j_avg = if deltas.is_empty() {
        0.0
    } else {
        deltas.iter().sum::<f64>() / deltas.len() as f64
    };
    let j_p95 = percentile_sorted(&deltas, 0.95);
    let j_max = deltas.last().copied().unwrap_or(0.0);
    let p999 = percentile_sorted(&dts, 0.999);
    (j_avg, j_p95, j_max, p999, hitches)
}

struct StreamPrev {
    dropped: u64,
    tex_up: u64,
    tex_bytes: u64,
    tex_full: u64,
    unmap_null: u64,
    neutral: u64,
    pw_kept: u64,
    pw_requeued: u64,
}

struct StreamDelta {
    dropped_per_frame: f64,
    tex_waiting: u64,
    tex_due: u64,
    tex_up_per_frame: f64,
    tex_up_mb: f64,
    tex_full_delta: u64,
    unmap_null_delta: u64,
    neutral_delta: u64,
    page_wr_kept_delta: u64,
    page_wr_requeued_delta: u64,
}

fn take_stream_deltas(frames: u64) -> StreamDelta {
    static PREV: Mutex<Option<StreamPrev>> = Mutex::new(None);
    let cur = StreamPrev {
        dropped: crate::draw::dropped_now(),
        tex_up: crate::draw::HEAP_TEXTURES.load(Relaxed),
        tex_bytes: crate::draw::HEAP_TEXTURE_BYTES.load(Relaxed),
        tex_full: crate::draw::HEAP_TEXTURES_FULL.load(Relaxed),
        unmap_null: crate::descriptor::host::STALE[4].load(Relaxed),
        neutral: crate::descriptor::host::neutral_hits(),
        pw_kept: crate::descriptor::host::PAGE_WRITES[0].load(Relaxed),
        pw_requeued: crate::descriptor::host::PAGE_WRITES[3].load(Relaxed),
    };
    let mut g = PREV.lock().unwrap_or_else(|e| e.into_inner());
    let prev = g.replace(StreamPrev { ..cur });
    let f = frames.max(1) as f64;
    let tex_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Relaxed);
    let tex_due = crate::draw::HEAP_TEXTURES_QUEUE[1].load(Relaxed);
    match prev {
        Some(p) => StreamDelta {
            dropped_per_frame: cur.dropped.saturating_sub(p.dropped) as f64 / f,
            tex_waiting,
            tex_due,
            tex_up_per_frame: cur.tex_up.saturating_sub(p.tex_up) as f64 / f,
            tex_up_mb: cur.tex_bytes.saturating_sub(p.tex_bytes) as f64 / (1024.0 * 1024.0),
            tex_full_delta: cur.tex_full.saturating_sub(p.tex_full),
            unmap_null_delta: cur.unmap_null.saturating_sub(p.unmap_null),
            neutral_delta: cur.neutral.saturating_sub(p.neutral),
            page_wr_kept_delta: cur.pw_kept.saturating_sub(p.pw_kept),
            page_wr_requeued_delta: cur.pw_requeued.saturating_sub(p.pw_requeued),
        },
        None => StreamDelta {
            dropped_per_frame: 0.0,
            tex_waiting,
            tex_due,
            tex_up_per_frame: 0.0,
            tex_up_mb: 0.0,
            tex_full_delta: 0,
            unmap_null_delta: 0,
            neutral_delta: 0,
            page_wr_kept_delta: 0,
            page_wr_requeued_delta: 0,
        },
    }
}

/// Lightweight 16-frame ring of hardware D3D12 GPU timestamp pairs
/// (`[frame_begin, frame_present]`), measuring real GPU execution span and
/// GPU present-to-present cadence with zero CPU-GPU sync stalls.
const GPU_RING: usize = 16;

struct GpuTimer {
    heap: windows::Win32::Graphics::Direct3D12::ID3D12QueryHeap,
    readback: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    ptr: *const u64,
    freq: u64,
    next_slot: usize,
    cur_has_begin: bool,
    pending: [Option<(u64, bool)>; GPU_RING],
    prev_end_ts: Option<u64>,
    work_us_sum: u64,
    work_n: u64,
    cadence_us_sum: u64,
    cadence_n: u64,
}

// SAFETY: guarded exclusively by `GPU_TIMER` mutex; D3D12 objects are free-threaded
// and `ptr` is a persistently mapped readback buffer owned by `readback`.
unsafe impl Send for GpuTimer {}

static GPU_TIMER: Mutex<Option<GpuTimer>> = Mutex::new(None);
static GPU_TIMER_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static FRAME_GPU_BEGAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn init_gpu_timer(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    queue: &windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
) -> Option<GpuTimer> {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    let freq = unsafe { queue.GetTimestampFrequency() }.ok()?;
    if freq == 0 {
        return None;
    }
    let qdesc = D3D12_QUERY_HEAP_DESC {
        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
        Count: (GPU_RING * 2) as u32,
        NodeMask: 0,
    };
    let mut heap: Option<ID3D12QueryHeap> = None;
    unsafe { device.CreateQueryHeap(&qdesc, &mut heap) }.ok()?;
    let heap = heap?;
    let hprops = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let rdesc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: (GPU_RING * 2 * 8) as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut readback: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &hprops,
            D3D12_HEAP_FLAG_NONE,
            &rdesc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut readback,
        )
    }
    .ok()?;
    let readback = readback?;
    let mut mapped = std::ptr::null_mut();
    unsafe { readback.Map(0, None, Some(&mut mapped)) }.ok()?;
    Some(GpuTimer {
        heap,
        readback,
        ptr: mapped.cast::<u64>(),
        freq,
        next_slot: 0,
        cur_has_begin: false,
        pending: [None; GPU_RING],
        prev_end_ts: None,
        work_us_sum: 0,
        work_n: 0,
        cadence_us_sum: 0,
        cadence_n: 0,
    })
}

/// Stamps the GPU start timestamp on the first command list of each frame.
pub(crate) fn gpu_stamp_frame_begin(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    queue: &windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
    list: &windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList,
) {
    if !on() || GPU_TIMER_FAILED.load(Relaxed) || FRAME_GPU_BEGAN.load(Relaxed) {
        return;
    }
    if FRAME_GPU_BEGAN.swap(true, Relaxed) {
        return;
    }
    let mut g = GPU_TIMER.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        match init_gpu_timer(device, queue) {
            Some(t) => *g = Some(t),
            None => {
                GPU_TIMER_FAILED.store(true, Relaxed);
                return;
            }
        }
    }
    let Some(t) = g.as_mut() else { return };
    let q = (t.next_slot * 2) as u32;
    // SAFETY: `list` is open, `heap` and `readback` live for process lifetime.
    unsafe {
        list.EndQuery(&t.heap, windows::Win32::Graphics::Direct3D12::D3D12_QUERY_TYPE_TIMESTAMP, q);
        list.ResolveQueryData(
            &t.heap,
            windows::Win32::Graphics::Direct3D12::D3D12_QUERY_TYPE_TIMESTAMP,
            q,
            1,
            &t.readback,
            u64::from(q) * 8,
        );
    }
    t.cur_has_begin = true;
}

/// Stamps the GPU end-of-frame timestamp inside `present_planes` before `copy.close()`.
/// Returns the ring slot index to pass to [`gpu_stamp_committed`].
pub(crate) fn gpu_stamp_present(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    queue: &windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
    list: &windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList,
) -> Option<usize> {
    if !on() || GPU_TIMER_FAILED.load(Relaxed) {
        return None;
    }
    let mut g = GPU_TIMER.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_none() {
        match init_gpu_timer(device, queue) {
            Some(t) => *g = Some(t),
            None => {
                GPU_TIMER_FAILED.store(true, Relaxed);
                return None;
            }
        }
    }
    let t = g.as_mut()?;
    let slot = t.next_slot;
    let q = (slot * 2 + 1) as u32;
    // SAFETY: `list` is open, `heap` and `readback` live for process lifetime.
    unsafe {
        list.EndQuery(&t.heap, windows::Win32::Graphics::Direct3D12::D3D12_QUERY_TYPE_TIMESTAMP, q);
        list.ResolveQueryData(
            &t.heap,
            windows::Win32::Graphics::Direct3D12::D3D12_QUERY_TYPE_TIMESTAMP,
            q,
            1,
            &t.readback,
            u64::from(q) * 8,
        );
    }
    t.next_slot = (slot + 1) % GPU_RING;
    let has_begin = std::mem::take(&mut t.cur_has_begin);
    t.pending[slot] = Some((u64::MAX, has_begin));
    FRAME_GPU_BEGAN.store(false, Relaxed);
    Some(slot)
}

/// Tags `slot` with its submitted fence number `flown` and harvests completed GPU
/// timestamps whose submission number is `<= finished`.
pub(crate) fn gpu_stamp_committed(slot: Option<usize>, flown: u64, finished: u64) {
    let Some(slot) = slot else { return };
    let mut g = GPU_TIMER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(t) = g.as_mut() else { return };
    if let Some((_, has_begin)) = t.pending[slot] {
        // When `flown == 0`, synchronous `execute()` already finished on the GPU.
        t.pending[slot] = Some((flown, has_begin));
    }
    for offset in 0..GPU_RING {
        let idx = (t.next_slot + offset) % GPU_RING;
        let Some((sub, has_begin)) = t.pending[idx] else { continue };
        if sub > finished {
            continue;
        }
        t.pending[idx] = None;
        // SAFETY: GPU finished `sub`; `ptr` points to 2*GPU_RING `u64`s in readback buffer.
        let (ts_begin, ts_end) = unsafe { (*t.ptr.add(idx * 2), *t.ptr.add(idx * 2 + 1)) };
        if ts_end == 0 {
            continue;
        }
        if has_begin && ts_begin != 0 && ts_end >= ts_begin {
            let work_us = ((ts_end - ts_begin) as u128 * 1_000_000 / t.freq as u128) as u64;
            if work_us < 10_000_000 {
                t.work_us_sum += work_us;
                t.work_n += 1;
            }
        }
        if let Some(prev_end) = t.prev_end_ts.replace(ts_end)
            && ts_end >= prev_end
        {
            let cad_us = ((ts_end - prev_end) as u128 * 1_000_000 / t.freq as u128) as u64;
            if cad_us < 10_000_000 {
                t.cadence_us_sum += cad_us;
                t.cadence_n += 1;
            }
        }
    }
}

fn take_gpu_window() -> (f64, f64, u64) {
    let mut g = GPU_TIMER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(t) = g.as_mut() else {
        return (0.0, 0.0, 0);
    };
    let wn = std::mem::take(&mut t.work_n);
    let wus = std::mem::take(&mut t.work_us_sum);
    let cn = std::mem::take(&mut t.cadence_n);
    let cus = std::mem::take(&mut t.cadence_us_sum);
    let work_ms = if wn > 0 { wus as f64 / 1000.0 / wn as f64 } else { 0.0 };
    let cad_ms = if cn > 0 { cus as f64 / 1000.0 / cn as f64 } else { 0.0 };
    (work_ms, cad_ms, cn.max(wn))
}

#[derive(Clone)]
struct ThreadSample {
    tid: u32,
    kernel_100ns: u64,
    user_100ns: u64,
    name: String,
}

fn sample_threads() -> std::collections::HashMap<u32, ThreadSample> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE, LocalFree};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcessId, GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    fn ft_to_u64(ft: FILETIME) -> u64 {
        u64::from(ft.dwLowDateTime) | (u64::from(ft.dwHighDateTime) << 32)
    }
    let mut out = std::collections::HashMap::new();
    // SAFETY: Toolhelp snapshot of current process's threads and read-only thread time queries.
    unsafe {
        let pid = GetCurrentProcessId();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut ok = Thread32First(snap, &mut entry) != 0;
        while ok {
            if entry.th32OwnerProcessID == pid {
                let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, entry.th32ThreadID);
                if !h.is_null() {
                    let mut created = FILETIME::default();
                    let mut exited = FILETIME::default();
                    let mut kernel = FILETIME::default();
                    let mut user = FILETIME::default();
                    let got_times = GetThreadTimes(h, &mut created, &mut exited, &mut kernel, &mut user) != 0;
                    let mut name_ptr = std::ptr::null_mut();
                    let name = if GetThreadDescription(h, &mut name_ptr) >= 0 && !name_ptr.is_null() {
                        let len = (0..).take_while(|&i| *name_ptr.add(i) != 0).count();
                        let s = String::from_utf16_lossy(std::slice::from_raw_parts(name_ptr, len));
                        LocalFree(name_ptr.cast());
                        s
                    } else {
                        String::new()
                    };
                    CloseHandle(h);
                    if got_times {
                        let k = ft_to_u64(kernel);
                        let u = ft_to_u64(user);
                        out.insert(
                            entry.th32ThreadID,
                            ThreadSample {
                                tid: entry.th32ThreadID,
                                kernel_100ns: k,
                                user_100ns: u,
                                name,
                            },
                        );
                    }
                }
            }
            ok = Thread32Next(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
    }
    out
}

fn process_mem_and_faults() -> Option<(u64, u64, u64, u64, u64)> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut pmc = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    let mut mem = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: sized Win32 info structs.
    unsafe {
        if K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut pmc).cast(), pmc.cb) == 0 {
            return None;
        }
        if GlobalMemoryStatusEx(&mut mem) == 0 {
            return None;
        }
    }
    Some((
        u64::from(pmc.PageFaultCount),
        pmc.WorkingSetSize as u64,
        pmc.PrivateUsage as u64,
        mem.ullAvailPhys,
        mem.ullAvailPageFile,
    ))
}

fn process_io_counters() -> Option<(u64, u64, u64)> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessIoCounters, IO_COUNTERS};
    let mut io = IO_COUNTERS::default();
    // SAFETY: valid process pseudo-handle and initialized output struct.
    if unsafe { GetProcessIoCounters(GetCurrentProcess(), &mut io) } == 0 {
        return None;
    }
    Some((io.ReadTransferCount, io.ReadOperationCount, io.WriteTransferCount))
}

fn start_bg_sampler() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("victory-prof".into()).spawn(|| {
            let mut prev_t = Instant::now();
            let mut prev_threads = sample_threads();
            let mut prev_faults = process_mem_and_faults().map_or(0, |v| v.0);
            let mut prev_priv_mb = process_mem_and_faults().map_or(0i64, |v| (v.2 >> 20) as i64);
            let mut prev_vram_mb = crate::dxgi::host_adapter_vram().map_or(0i64, |v| (v.local_used_bytes >> 20) as i64);
            let mut prev_io = process_io_counters().unwrap_or((0, 0, 0));
            let mut prev_frames = crate::frame::current();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let now = Instant::now();
                let dt_s = (now - prev_t).as_secs_f64().max(0.001);
                let wall_100ns = dt_s * 10_000_000.0;
                prev_t = now;

                let cur_threads = sample_threads();
                let mut rows: Vec<(String, u32, f64, f64, f64)> = Vec::with_capacity(cur_threads.len());
                let (mut tot_u, mut tot_k) = (0.0f64, 0.0f64);
                for (tid, cur) in &cur_threads {
                    let (dk, du) = match prev_threads.get(tid) {
                        Some(p) => (
                            cur.kernel_100ns.saturating_sub(p.kernel_100ns),
                            cur.user_100ns.saturating_sub(p.user_100ns),
                        ),
                        None => (cur.kernel_100ns, cur.user_100ns),
                    };
                    let kp = (dk as f64 / wall_100ns) * 100.0;
                    let up = (du as f64 / wall_100ns) * 100.0;
                    let tp = kp + up;
                    tot_u += up;
                    tot_k += kp;
                    if tp >= 0.5 {
                        let label = if cur.name.is_empty() {
                            format!("tid:{tid}")
                        } else {
                            cur.name.clone()
                        };
                        rows.push((label, *tid, tp, up, kp));
                    }
                }
                prev_threads = cur_threads;
                rows.sort_by(|a, b| b.2.total_cmp(&a.2));

                let mut top = String::new();
                for (label, tid, tp, up, kp) in rows.iter().take(16) {
                    if !top.is_empty() {
                        top.push_str(", ");
                    }
                    let _ = write!(top, "{label}#{tid}:{tp:.1}%(u{up:.1}/k{kp:.1})");
                }

                let cur_frames = crate::frame::current();
                let df = cur_frames.saturating_sub(prev_frames);
                prev_frames = cur_frames;

                let (faults_s, ws_mb, priv_mb, avail_phys_mb, avail_commit_mb) = match process_mem_and_faults() {
                    Some((f, ws, priv_b, ap, ac)) => {
                        let dfaults = f.saturating_sub(prev_faults) as f64 / dt_s;
                        prev_faults = f;
                        (dfaults, ws >> 20, priv_b >> 20, ap >> 20, ac >> 20)
                    }
                    None => (0.0, 0, 0, 0, 0),
                };

                let (io_rd_mbs, io_rd_ops, io_wr_mbs) = match process_io_counters() {
                    Some((rb, ro, wb)) => {
                        let rmb = rb.saturating_sub(prev_io.0) as f64 / (1024.0 * 1024.0) / dt_s;
                        let rops = ro.saturating_sub(prev_io.1) as f64 / dt_s;
                        let wmb = wb.saturating_sub(prev_io.2) as f64 / (1024.0 * 1024.0) / dt_s;
                        prev_io = (rb, ro, wb);
                        (rmb, rops, wmb)
                    }
                    None => (0.0, 0.0, 0.0),
                };

                let (vram_u, vram_b, sh_u, sh_b) = match crate::dxgi::host_adapter_vram() {
                    Some(v) => (
                        v.local_used_bytes >> 20,
                        v.local_budget_bytes >> 20,
                        v.shared_used_bytes >> 20,
                        v.shared_budget_bytes >> 20,
                    ),
                    None => (0, 0, 0, 0),
                };

                let priv_delta = priv_mb as i64 - prev_priv_mb;
                prev_priv_mb = priv_mb as i64;
                let vram_delta = vram_u as i64 - prev_vram_mb;
                prev_vram_mb = vram_u as i64;

                tracing::info!(
                    target: "phase2_prof",
                    "PHASE2_SYS ({dt_s:.2}s, +{df}f_begun): proc_cpu={:.1}%(u{tot_u:.1}/k{tot_k:.1}) faults/s={faults_s:.0} io_rd={io_rd_mbs:.1}MB/s({io_rd_ops:.0}ops/s) io_wr={io_wr_mbs:.1}MB/s ws={ws_mb}MB priv={priv_mb}MB({priv_delta:+}MB) free_ram={avail_phys_mb}MB free_commit={avail_commit_mb}MB vram={vram_u}/{vram_b}MB({vram_delta:+}MB) shared={sh_u}/{sh_b}MB | top_threads=[{top}]",
                    tot_u + tot_k,
                );
            }
        });
    });
}
