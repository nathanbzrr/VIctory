//! Renderer port #5: the title's constant buffers read by the card where the
//! title has them (`VICTORY_PORT_CBV=1`, off by default; read in [`on`]).
//!
//! What the console allows: a constant buffer anywhere on 16 bytes. What
//! D3D12 asks: a constant buffer view on 256. So the layer made every
//! constant buffer a draw's tables name afresh at each submission: a copy of
//! its bytes (out of title memory, on the CPU) into upload memory of its own
//! on 256, and a view of that ([`crate::draw`]'s tables, `defer_constants`):
//! ~23,000 a frame of the city, ~9 MB, and as many steps of the submitting
//! thread. With renderer port 1 (`VICTORY_PORT_CONSTANTS=1`) the memory they
//! are copied out of is the card's to read in place already.
//!
//! What is done here instead: shaders are translated to read constant buffers
//! as raw bytes (`victory_shader::emit::constants_raw`): a table's through
//! the raw twin of its slot, a root one as a root SRV. A raw view starts on 16
//! bytes, a root SRV on 4. So a constant buffer of a table in title memory in
//! place is a raw view of the title's bytes, made in the twin of its slot,
//! and nothing is copied or done at submission; a root one is bound at its
//! address as root SRVs are. Constant buffers anywhere else are copied as
//! before (the twin of their slot is a raw view of the copy, as it was).
//! The twin of a title slot that holds a constant buffer view is a raw view
//! of its bytes too (it was a constant buffer view: a raw read through one
//! on AMD has no end).
//!
//! What it assumes: what the card reads of a constant buffer at a draw is
//! what the title's CPU wrote before it submitted, as for every other title
//! buffer in place (renderer port 1): before, the copy at submission was
//! taken then whatever the title wrote after. The title writes its constants
//! into a ring it waits on its fences for.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether renderer port #5 is on: the shaders' side
/// (`victory_shader::emit::constants_raw`) and the host's views follow the
/// same switch.
pub(crate) fn on() -> bool {
    victory_shader::emit::constants_raw()
}

/// Whether a table's constant buffer in title memory in place is given to the
/// card where it is (on with the port; `VICTORY_PORT_CBV_PLACE=0`: copied as
/// before, and read through the raw twin all the same). With
/// `VICTORY_AB=<s>,VICTORY_PORT_CBV_PLACE` (diagnostics builds) on and off
/// while the title runs: both sides measured in one run.
pub(crate) fn in_place_on() -> bool {
    on() && crate::toggled!("VICTORY_PORT_CBV_PLACE")
}

/// Shadow-direct CBV mode (`VICTORY_PORT_CBV_SHADOW`):
/// - `0`: restrict direct raw-twin CBVs to `Shadow::in_place()` only (default if off).
/// - `1`: allow all card-backed shadow buffers (`Shadow::carded()`).
/// - `2`: allow card-backed shadow buffers that currently have no pending GPU writes
///   (`!shadow.writes_pending()`), avoiding `merge_under_writes` / `settle_stale`
///   stalls on write-contested heaps while still bypassing copies for read-only rings.
pub(crate) fn shadow_mode() -> u8 {
    if !in_place_on() {
        return 0;
    }
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_PORT_CBV_SHADOW").ok().as_deref() {
        Some("0") => 0,
        Some("2") => 2,
        Some("1") => 1,
        // Default: safe mode (0 unless explicitly opted into shadow-direct, or 2 when enabled).
        _ => 0,
    })
}

/// Non-destructive snapshot of the 7 counters (for `VICTORY_SUBMIT_TRACE=1`).
pub(crate) fn trace_snapshot() -> [u64; 7] {
    std::array::from_fn(|i| STATS[i].load(Relaxed))
}

/// Monotonic diagnostic mirrors for `VICTORY_SUBMIT_TRACE=1` (keeps `host.rs` untouched).
pub(crate) static TRACE_CARD_STEPS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
pub(crate) static TRACE_UPQ: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Microsecond timers for `VICTORY_SUBMIT_TRACE=1`: `[card_steps_us, upq_us, tables_us, bind_us]`.
pub(crate) static TRACE_US: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Lock-free set of shadow IDs that any shader has ever declared GPU UAV writes
/// to (e.g. the 174 MB skinning heap). Used by `shadow_mode() == 2` so direct
/// shadow CBVs never push `Step::Refresh` onto write-contested heaps (avoiding
/// `merge_under_writes` / `settle_stale` GPU stalls).
static GPU_WRITTEN: [AtomicU64; 32] = [const { AtomicU64::new(0) }; 32];

#[inline]
pub(crate) fn mark_gpu_written(shadow_id: u64) {
    if shadow_id == 0 {
        return;
    }
    for slot in &GPU_WRITTEN {
        let cur = slot.load(Relaxed);
        if cur == shadow_id {
            return;
        }
        if cur == 0 {
            if slot.compare_exchange(0, shadow_id, Relaxed, Relaxed).is_ok() {
                return;
            }
            if slot.load(Relaxed) == shadow_id {
                return;
            }
        }
    }
}

#[inline]
pub(crate) fn is_gpu_written(shadow_id: u64) -> bool {
    if shadow_id == 0 {
        return false;
    }
    for slot in &GPU_WRITTEN {
        let cur = slot.load(Relaxed);
        if cur == 0 {
            return false;
        }
        if cur == shadow_id {
            return true;
        }
    }
    // Table saturated: conservatively treat as potentially GPU-written.
    true
}

/// Since the last line: tables' constant buffers given in place, their bytes;
/// copied as before because not in place/carded, because not on 16 bytes;
/// title slots' constant buffers whose twin is a raw view; tables' constant
/// buffers given directly from a card-backed shadow copy, their bytes.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

thread_local! {
    static LOCAL: std::cell::Cell<[u64; 8]> = const { std::cell::Cell::new([0; 8]) };
}

#[inline]
fn bump(i: usize, n: u64, j: Option<(usize, u64)>) {
    LOCAL.with(|c| {
        let mut v = c.get();
        v[i] += n;
        if let Some((k, m)) = j {
            v[k] += m;
        }
        v[7] += 1;
        if v[7] >= 256 {
            for (dst, &src) in STATS.iter().zip(&v[..7]) {
                if src != 0 {
                    dst.fetch_add(src, Relaxed);
                }
            }
            v = [0; 8];
        }
        c.set(v);
    });
}

/// A table's constant buffer of `size` bytes was given in place.
pub(crate) fn in_place(size: u32) {
    bump(0, 1, Some((1, u64::from(size))));
}

/// A table's constant buffer of `size` bytes was read directly from a card-backed shadow copy.
pub(crate) fn shadow_direct(size: u32) {
    bump(5, 1, Some((6, u64::from(size))));
}

/// A table's constant buffer was copied as before: `aligned` false when it
/// is in place/carded but not on 16 bytes.
pub(crate) fn copied(aligned: bool) {
    bump(if aligned { 2 } else { 3 }, 1, None);
}

/// A title slot's constant buffer view got a raw twin.
pub(crate) fn twinned() {
    bump(4, 1, None);
}

/// Whether a constant buffer at host address `gpu` can be a raw view of
/// those bytes: on 16 bytes.
pub(crate) fn raw_ok(gpu: u64) -> bool {
    gpu.is_multiple_of(16)
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames.max(1e-9)).collect();
    tracing::info!(
        "renderer port 5 (VICTORY_PORT_CBV=1) per frame: {:.0} in place ({:.1} KB) + {:.0} direct shadow ({:.1} KB not copied, \
         no fill step); copied as before: {:.0} not in place/carded, {:.0} not on 16 bytes; {:.0} title slots' constant buffers with a raw twin",
        s[0],
        s[1] / 1024.0,
        s[5],
        s[6] / 1024.0,
        s[2],
        s[3],
        s[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_views_need_sixteen_bytes() {
        assert!(raw_ok(0x3500_0010));
        assert!(raw_ok(0x3500_0100));
        assert!(!raw_ok(0x3500_0008));
        assert!(!raw_ok(0x3500_0004));
    }
}
