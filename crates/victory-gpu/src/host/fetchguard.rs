//! With the relay's fetch on the copy queue (`relay_copy`): the fetch never
//! runs beside drawing-queue work that writes the copies it reads.
//!
//! A copy-queue fetch of a landed flight's pieces used to run beside the
//! submissions sent after that flight, which may write the same card copies
//! (their shaders' UAV writes, the sync list's sends of title bytes). D3D12
//! allows one queue to write a buffer while another reads it only where the
//! two do not touch the same bytes; here they did, and the test
//! `a_result_caught_in_between_is_put_right` caught the fetch in the middle
//! of the next flight 12 times in 12 without `VICTORY_FETCH_CHECK`. On RDNA 2
//! cards the drawing queue and the high-priority copy queue at the same bytes
//! is the one cross-queue access the relay adds (issue #376: device resets
//! with the relay on, none without).
//!
//! Two rules make the access never overlap:
//!
//! - a copy is counted from its submission's `Host::card_before` (before
//!   anything of the submission goes to the drawing queue) until the
//!   submission's plan is dropped (`card_done`, after it ran); a fetch of a
//!   copy another submission counted is in the air for goes to the drawing
//!   queue, behind that submission, as without `VICTORY_RELAY_COPY`;
//! - a fetch that goes to the copy queue names its copies here before it is
//!   sent, and a submission counted while it is on its way makes the drawing
//!   queue wait for it (a wait on the card, not on the CPU) before its first
//!   list.
//!
//! Both take the one lock, so a submission is counted before a fetch looks,
//! or waits for the fetch.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Fence};

#[derive(Default)]
struct Guard {
    /// By copy ([`crate::shadow::Shadow::id`]): the submissions not done
    /// with that write it on the card.
    writers: HashMap<u64, u32>,
    /// The copies of the fetch on the copy queue, sorted, the fence it
    /// signals and the value.
    fetching: Vec<u64>,
    fence: Option<ID3D12Fence>,
    value: u64,
}

// SAFETY: free-threaded D3D12 objects, under the lock.
unsafe impl Send for Guard {}

static GUARD: Mutex<Option<Guard>> = Mutex::new(None);

/// Since the last frame-rate line: fetches on the copy queue, fetches sent
/// to the drawing queue instead, waits of the drawing queue for a fetch.
pub(super) static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The copies one submission writes on the card, counted until it is
/// dropped.
pub(super) struct Writes(Vec<u64>);

impl Writes {
    pub(super) fn copies(&self) -> &[u64] {
        &self.0
    }
}

impl Drop for Writes {
    fn drop(&mut self) {
        let mut guard = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let Some(guard) = guard.as_mut() else {
            return;
        };
        for id in &self.0 {
            if let Some(n) = guard.writers.get_mut(id) {
                *n -= 1;
                if *n == 0 {
                    guard.writers.remove(id);
                }
            }
        }
    }
}

/// A submission is about to write `copies` on the card: counted, and if a
/// fetch on the copy queue is reading one of them, `drawing` waits for it
/// before anything sent to it after this call.
pub(super) fn writing(drawing: &ID3D12CommandQueue, mut copies: Vec<u64>) -> Writes {
    copies.sort_unstable();
    copies.dedup();
    let mut guard = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let guard = guard.get_or_insert_with(Guard::default);
    for id in &copies {
        *guard.writers.entry(*id).or_insert(0) += 1;
    }
    if let Some(fence) = &guard.fence
        && copies.iter().any(|id| guard.fetching.binary_search(id).is_ok())
    {
        // SAFETY: a live queue and fence; the value is signalled behind the
        // fetch, which waits for nothing.
        if unsafe { drawing.Wait(fence, guard.value) }.is_ok() {
            STATS[2].fetch_add(1, Relaxed);
        } else {
            super::fail(format_args!("the drawing queue could not wait for a fetch on the copy queue"));
        }
    }
    Writes(copies)
}

/// A fetch of `copies` (each may come several times) for the flights whose
/// plans' counts are `landing`: whether it may go to the copy queue. If so it
/// is named here, with the value it will signal on `fence`; else a
/// submission still in the air writes one of them, and it goes to the
/// drawing queue.
pub(super) fn may_copy<'a>(
    copies: impl Iterator<Item = u64>,
    landing: impl Iterator<Item = &'a Writes>,
    fence: &ID3D12Fence,
    value: u64,
) -> bool {
    let mut copies: Vec<u64> = copies.collect();
    copies.sort_unstable();
    copies.dedup();
    let mut ours: HashMap<u64, u32> = HashMap::new();
    for writes in landing {
        for id in writes.copies() {
            *ours.entry(*id).or_insert(0) += 1;
        }
    }
    let mut guard = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let guard = guard.get_or_insert_with(Guard::default);
    let others = copies
        .iter()
        .any(|id| guard.writers.get(id).copied().unwrap_or(0) > ours.get(id).copied().unwrap_or(0));
    if others {
        STATS[1].fetch_add(1, Relaxed);
        return false;
    }
    guard.fetching = copies;
    guard.fence = Some(fence.clone());
    guard.value = value;
    STATS[0].fetch_add(1, Relaxed);
    true
}

/// The fetch that signals `value` has run.
pub(super) fn fetched(value: u64) {
    let mut guard = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(guard) = guard.as_mut()
        && guard.value == value
    {
        guard.fetching.clear();
        guard.fence = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::D3D12_FENCE_FLAG_NONE;

    /// Counts come and go with the submissions; a fetch of a copy a
    /// submission in the air writes is refused, of one only landing
    /// flights write is taken.
    #[test]
    fn a_fetch_goes_to_the_copy_queue_only_when_nothing_in_the_air_writes_it() {
        let Some(host) = super::super::host() else {
            return;
        };
        // SAFETY: plain object creation.
        let fence: ID3D12Fence = unsafe { host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
        // (Ids no copy has: the counts are the test's own.)
        let (a, b) = (u64::MAX - 7, u64::MAX - 8);
        let landing = writing(&host.queue, vec![a]);
        let later = writing(&host.queue, vec![a, a]);
        assert!(!may_copy([a].into_iter(), [&landing].into_iter(), &fence, 1), "a later submission writes it");
        drop(later);
        assert!(may_copy([a, a].into_iter(), [&landing].into_iter(), &fence, 2), "only the landing flight writes it");
        // While it is on its way a submission that writes it makes the
        // drawing queue wait (the value is signalled first: other tests
        // share the queue, it must not stand).
        // SAFETY: a live fence.
        unsafe { fence.Signal(2) }.unwrap();
        let waits = STATS[2].load(Relaxed);
        let during = writing(&host.queue, vec![a, b]);
        assert_eq!(during.copies(), [b, a]);
        assert!(STATS[2].load(Relaxed) > waits, "the drawing queue waits for the fetch");
        fetched(2);
        assert!(GUARD.lock().unwrap().as_ref().unwrap().fence.is_none());
        drop((landing, during));
        assert!(!GUARD.lock().unwrap().as_ref().unwrap().writers.contains_key(&a));
    }
}
