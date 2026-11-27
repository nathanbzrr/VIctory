//! Memory `allocate_upload` (`list.rs`) hands out: the title's CPU writes
//! it and its shaders read it at the same address (the structure builder's
//! instance descriptions lie there).
//!
//! The console's GPU addresses have 40 bits, and a translated shader that
//! reads by address has no more (`victory_shader`'s `Lower::memory`): memory
//! from the process heap lies above 2^40 and was read at another address, as
//! zeros. So this memory is taken in blocks of 64 KiB out of arenas in
//! [4 GiB, 2^37), as upstream VIctory 0f637570 and 6d021fe3 place it (there
//! an allocation of its own each). An arena is a title buffer to the host
//! copies ([`crate::shadow`]) and lives as long as the process: the directory
//! of memory by address ([`crate::memory`]) keeps the copy it found at an
//! address for good, so the same addresses must stay the same copy.
//! `VICTORY_UPLOAD_LOW=0`: from the process heap, as before.

use std::sync::{Mutex, OnceLock};

/// What blocks are cut on: a page of the directory of memory by address.
const BLOCK: usize = 64 << 10;
/// An arena's size, if one request does not need more.
const ARENA: usize = 4 << 20;

struct Arena {
    at: usize,
    free: usize,
    /// Per block whether it is handed out (bitmask words, 64 blocks per word).
    taken: Vec<u64>,
    /// Per block whether it has ever been handed out before (fresh VirtualAlloc2 pages are OS-zeroed).
    used_before: Vec<u64>,
    blocks: usize,
}

impl Arena {
    fn new(at: usize, blocks: usize) -> Self {
        let words = blocks.div_ceil(64);
        Self {
            at,
            free: blocks,
            taken: vec![0; words],
            used_before: vec![0; words],
            blocks,
        }
    }

    #[inline]
    fn is_taken(&self, i: usize) -> bool {
        (self.taken[i / 64] & (1u64 << (i % 64))) != 0
    }

    /// Finds the first run of `n` free blocks using O(1) bit ops for common 1-block or <=64-block arenas.
    fn find_gap(&self, n: usize) -> Option<usize> {
        if n == 1 {
            for (w_idx, &word) in self.taken.iter().enumerate() {
                if word != u64::MAX {
                    let bit = (!word).trailing_zeros() as usize;
                    let idx = w_idx * 64 + bit;
                    return (idx < self.blocks).then_some(idx);
                }
            }
            return None;
        }
        if self.blocks <= 64 && n <= 64 {
            let valid = if self.blocks == 64 { u64::MAX } else { (1u64 << self.blocks) - 1 };
            let mask = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            let used = self.taken[0] | !valid;
            let mut shift = 0;
            while shift + n <= self.blocks {
                let window = (used >> shift) & mask;
                if window == 0 {
                    return Some(shift);
                }
                // Advance past the highest set bit inside `window`.
                let skip = (64 - window.leading_zeros()) as usize;
                shift += skip;
            }
            return None;
        }
        let mut free = 0;
        for i in 0..self.blocks {
            free = if self.is_taken(i) { 0 } else { free + 1 };
            if free == n {
                return Some(i + 1 - n);
            }
        }
        None
    }

    /// Marks `first..first + n` as taken and returns whether any block in that range was used before.
    fn claim(&mut self, first: usize, n: usize) -> bool {
        let mut needs_zero = false;
        let end = first + n;
        let mut i = first;
        while i < end {
            let w = i / 64;
            let bit = i % 64;
            let take = (64 - bit).min(end - i);
            let mask = if take == 64 { u64::MAX } else { ((1u64 << take) - 1) << bit };
            self.taken[w] |= mask;
            if (self.used_before[w] & mask) != 0 {
                needs_zero = true;
            }
            self.used_before[w] |= mask;
            i += take;
        }
        self.free -= n;
        needs_zero
    }

    fn release(&mut self, first: usize, n: usize) {
        let end = (first + n).min(self.blocks);
        let mut i = first;
        while i < end {
            let w = i / 64;
            let bit = i % 64;
            let drop_n = (64 - bit).min(end - i);
            let mask = if drop_n == 64 { u64::MAX } else { ((1u64 << drop_n) - 1) << bit };
            self.taken[w] &= !mask;
            i += drop_n;
        }
        self.free += end.saturating_sub(first);
    }
}

static ARENAS: Mutex<Vec<Arena>> = Mutex::new(Vec::new());

/// Upload memory of one request, zeroed, given back when dropped.
pub(crate) struct Upload(Kind);

enum Kind {
    /// Arena index, address, bytes.
    Low(usize, usize, usize),
    Heap(Vec<u8>),
}

fn low() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPLOAD_LOW").is_none_or(|v| v != "0"))
}

/// The first place of `n` free blocks side by side.
#[cfg(test)]
fn gap(taken: &[bool], n: usize) -> Option<usize> {
    let mut free = 0;
    for (i, &t) in taken.iter().enumerate() {
        free = if t { 0 } else { free + 1 };
        if free == n {
            return Some(i + 1 - n);
        }
    }
    None
}

/// Fresh zeroed memory in [4 GiB, 2^37) (where the directory of memory by
/// address reaches, `MEMORY_PAGES`), write-watched as the title's own
/// pools are (copies then take only written pages).
pub(crate) fn reserve(len: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER, MEM_RESERVE, MemExtendedParameterAddressRequirements,
        PAGE_READWRITE, VirtualAlloc2,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: (1usize << 32) as *mut _,
        HighestEndingAddress: ((1usize << 37) - 1) as *mut _,
        Alignment: 0,
    };
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // SAFETY: a fresh allocation; the parameter points at `requirements`,
    // which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            std::ptr::null_mut(),
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
            PAGE_READWRITE,
            &mut parameter,
            1,
        )
    } as usize;
    (at != 0).then_some(at)
}

impl Upload {
    pub(crate) fn new(len: usize) -> Option<Upload> {
        if !low() {
            return Some(Upload(Kind::Heap(vec![0; len])));
        }
        let n = len.max(1).div_ceil(BLOCK);
        let (a, at, needs_zero) = {
            let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
            let found = arenas
                .iter()
                .enumerate()
                .filter(|(_, arena)| arena.free >= n)
                .find_map(|(a, arena)| Some((a, arena.find_gap(n)?)));
            let (a, first) = match found {
                Some(place) => place,
                None => {
                    let blocks = n.max(ARENA / BLOCK);
                    let at = reserve(blocks * BLOCK)?;
                    crate::shadow::register(at, at as u64, (blocks * BLOCK) as u64);
                    tracing::info!(at = format_args!("{at:#x}"), bytes = blocks * BLOCK, "memory for lists' uploads, where shaders reach it by address");
                    arenas.push(Arena::new(at, blocks));
                    (arenas.len() - 1, 0)
                }
            };
            let arena = &mut arenas[a];
            let needs_zero = arena.claim(first, n);
            (a, arena.at + first * BLOCK, needs_zero)
        };
        if needs_zero {
            // SAFETY: the blocks are exclusively owned by this `Upload` now.
            unsafe { std::ptr::write_bytes(at as *mut u8, 0, len) };
        }
        Some(Upload(Kind::Low(a, at, len)))
    }

    pub(crate) fn start(&mut self) -> *mut u8 {
        match &mut self.0 {
            Kind::Low(_, at, _) => *at as *mut u8,
            Kind::Heap(block) => block.as_mut_ptr(),
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        let Kind::Low(a, at, len) = self.0 else { return };
        let n = len.max(1).div_ceil(BLOCK);
        let mut arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(arena) = arenas.get_mut(a) {
            let first = (at - arena.at) / BLOCK;
            arena.release(first, n);
        }
    }
}

/// The block handed out that holds `address`: its start and bytes (whole
/// [`BLOCK`]s).
pub(crate) fn block(address: usize) -> Option<(usize, usize)> {
    let arenas = ARENAS.lock().unwrap_or_else(|e| e.into_inner());
    let arena = arenas.iter().find(|a| (a.at..a.at + a.blocks * BLOCK).contains(&address))?;
    let first = (address - arena.at) / BLOCK;
    let mut n = 0;
    while first + n < arena.blocks && arena.is_taken(first + n) {
        n += 1;
    }
    (n > 0).then_some((arena.at + first * BLOCK, n * BLOCK))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_lie_below_40_bits_and_are_used_again_zeroed() {
        let mut a = Upload::new(100).unwrap();
        let mut b = Upload::new(BLOCK + 1).unwrap();
        let (pa, pb) = (a.start(), b.start());
        assert!((pa as usize) >= 1 << 32 && (pb as usize) + 2 * BLOCK <= 1 << 40);
        assert!((pa as usize).is_multiple_of(BLOCK) && pa != pb);
        // SAFETY: 100 bytes were handed out.
        unsafe { pa.write_bytes(0xab, 100) };
        drop(a);
        // The lowest free place (another test may have taken it meanwhile;
        // zeroed either way).
        let mut c = Upload::new(100).unwrap();
        // SAFETY: as above.
        assert!(unsafe { std::slice::from_raw_parts(c.start(), 100) }.iter().all(|&x| x == 0));
        // More than an arena.
        let mut d = Upload::new(ARENA + 1).unwrap();
        // SAFETY: the bytes were handed out.
        unsafe { d.start().add(ARENA).write(1) };
        drop(b);
    }

    #[test]
    fn a_gap_is_blocks_side_by_side() {
        assert_eq!(gap(&[true, false, true, false, false, true], 1), Some(1));
        assert_eq!(gap(&[true, false, true, false, false, true], 2), Some(3));
        assert_eq!(gap(&[true, false, true, false, false, true], 3), None);

        let mut arena = Arena::new(0x1000_0000, 64);
        assert!(!arena.claim(0, 1));
        assert!(!arena.claim(2, 1));
        assert!(!arena.claim(5, 59));
        assert_eq!(arena.find_gap(1), Some(1));
        assert_eq!(arena.find_gap(2), Some(3));
        assert_eq!(arena.find_gap(3), None);
        arena.release(2, 1);
        assert_eq!(arena.find_gap(4), Some(1));
        assert!(arena.claim(1, 4));
        assert_eq!(arena.free, 0);
        assert_eq!(arena.find_gap(1), None);
    }
}
