//! Query heaps. Occlusion heaps measure on the host GPU (the draws that
//! mark them run there), so the title's own culling and dynamic resolution
//! read real counts; the other kinds are still answered at resolve time.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use windows::Win32::Graphics::Direct3D12::{
    D3D12_QUERY_HEAP_DESC, D3D12_QUERY_HEAP_TYPE_OCCLUSION, D3D12_QUERY_TYPE,
    D3D12_QUERY_TYPE_BINARY_OCCLUSION, D3D12_QUERY_TYPE_OCCLUSION, ID3D12QueryHeap,
};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, QUERY_HEAP, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

/// Occlusion: samples that passed, or (binary) whether any did.
const OCCLUSION: u32 = 0;
const BINARY: u32 = 1;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x0d9658ae,
        0xed45,
        0x469e,
        [0xa6, 0x1d, 0x97, 0x0e, 0xc5, 0x83, 0xca, 0xb4],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Desc {
    kind: u32,
    count: u32,
    nodes: u32,
}

#[repr(C)]
struct Heap {
    object: Object,
    desc: Desc,
    /// The host's own heap for the kinds that measure there, made with the
    /// same count so every index the title marks exists.
    host: Option<ID3D12QueryHeap>,
}

/// The host heap for a description, for the kinds the host can answer.
fn host_heap(desc: &Desc) -> Option<ID3D12QueryHeap> {
    if !matches!(desc.kind, OCCLUSION | BINARY) || desc.count == 0 {
        return None;
    }
    let host = crate::host::host()?;
    let mut heap: Option<ID3D12QueryHeap> = None;
    // SAFETY: plain object creation.
    unsafe {
        host.device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    // One heap type answers both the count and the
                    // pass/fail forms.
                    Type: D3D12_QUERY_HEAP_TYPE_OCCLUSION,
                    Count: desc.count,
                    NodeMask: 0,
                },
                &mut heap,
            )
            .ok()?;
    }
    heap
}

/// Device method.
pub(crate) extern "system" fn create(
    _: This,
    desc: *const Desc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let host = host_heap(&desc);
    tracing::debug!(?desc, measuring = host.is_some(), "query heap");
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        host,
    });
    object::hand_out(QUERY_HEAP, heap, iid, &HEAP_IIDS, out)
}

/// The host heap behind one of ours, and the query type a mark or a
/// resolve uses with it; `None` for the kinds that do not measure on the
/// host.
pub(crate) fn host(this: This) -> Option<(ID3D12QueryHeap, D3D12_QUERY_TYPE)> {
    if this.is_null() {
        return None;
    }
    // SAFETY: the title passes a heap it created here.
    let heap = unsafe { &*this.cast::<Heap>() };
    let kind = match heap.desc.kind {
        OCCLUSION => D3D12_QUERY_TYPE_OCCLUSION,
        BINARY => D3D12_QUERY_TYPE_BINARY_OCCLUSION,
        _ => return None,
    };
    Some((heap.host.clone()?, kind))
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    object::answer(
        QUERY_HEAP,
        iid,
        object::is(iid, &HEAP_IIDS).then_some(this),
        out,
    )
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}

/// An occlusion heap made the way the device makes one, for the tests:
/// `None` when the host refuses it (no GPU, or no heap of that kind).
#[cfg(test)]
pub(crate) fn occlusion_for_test(kind: u32, count: u32) -> Option<This> {
    let desc = Desc {
        kind,
        count,
        nodes: 0,
    };
    let mut out: This = std::ptr::null_mut();
    let hr = create(std::ptr::null(), &desc, &HEAP_IIDS[0], &mut out);
    (hr == object::S_OK && !out.is_null()).then_some(out)
}

/// IUnknown 3, object 4, device child 1.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<QUERY_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{E_NOINTERFACE, E_POINTER, S_FALSE, S_OK, slot};

    #[test]
    fn null_descriptions_are_rejected() {
        let mut heap = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), std::ptr::null(), &HEAP_IIDS[0], &mut heap),
            E_INVALIDARG
        );
        assert!(heap.is_null());
        // The description is checked first: a probe with no room for the
        // heap fails on it rather than answering S_FALSE.
        assert_eq!(
            create(
                std::ptr::null(),
                std::ptr::null(),
                &HEAP_IIDS[0],
                std::ptr::null_mut()
            ),
            E_INVALIDARG
        );
    }

    #[test]
    fn creation_without_an_out_pointer_only_probes() {
        let desc = Desc {
            kind: 1, // timestamps
            count: 64,
            nodes: 0,
        };
        // A known interface with no out pointer asks whether creation would
        // succeed; the heap is dropped instead of handed out.
        assert_eq!(
            create(std::ptr::null(), &desc, &HEAP_IIDS[0], std::ptr::null_mut()),
            S_FALSE
        );
        // A null interface id is tolerated: no interface, no heap.
        let mut heap = std::ptr::null();
        assert_eq!(
            create(std::ptr::null(), &desc, std::ptr::null(), &mut heap),
            E_NOINTERFACE
        );
        assert!(heap.is_null());
    }

    #[test]
    fn heaps_answer_their_interfaces_and_count_references() {
        let desc = Desc {
            kind: 0, // occlusion
            count: 8,
            nodes: 0,
        };
        let null = std::ptr::null();
        let mut heap = null;
        assert_eq!(create(null, &desc, &HEAP_IIDS[0], &mut heap), S_OK);
        assert!(!heap.is_null());
        let query = slot::<extern "system" fn(This, *const Iid, *mut This) -> HRESULT>(heap, 0x0);
        // Every interface the heap answers to hands out the same object.
        for iid in &HEAP_IIDS {
            let mut out = null;
            assert_eq!(query(heap, iid, &mut out), S_OK);
            assert_eq!(out as usize, heap as usize);
        }
        // An unknown interface writes null behind and reports it missing.
        let unknown = [0u8; 16];
        let mut out = heap;
        assert_eq!(query(heap, &unknown, &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // A null interface id is tolerated rather than matched.
        let mut out = heap;
        assert_eq!(query(heap, std::ptr::null(), &mut out), E_NOINTERFACE);
        assert!(out.is_null());
        // No room for the answer is a pointer error.
        assert_eq!(query(heap, &unknown, std::ptr::null_mut()), E_POINTER);
        // References: one from creation, four from the queries, one added;
        // each release hands back what is left, the last frees the heap.
        assert_eq!(slot::<extern "system" fn(This) -> u32>(heap, 0x8)(heap), 6);
        let release = slot::<extern "system" fn(This) -> u32>(heap, 0x10);
        for left in [5, 4, 3, 2, 1, 0] {
            assert_eq!(release(heap), left);
        }
    }
}
