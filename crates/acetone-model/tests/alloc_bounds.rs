//! Decoder memory-amplification bound (security LOW-1, acetone-8gp).
//!
//! A hostile CBOR payload whose array head declares a huge element count
//! that *passes* the count-vs-remaining precheck (every element costs at
//! least one byte) used to make the decoder reserve
//! `count * size_of::<Value|String>()` up front — a ~24–32× amplification
//! of the input size. The fix caps the speculative reservation
//! (`MAX_PREALLOC_ITEMS`) and lets vectors grow incrementally.
//!
//! This test measures the decoder's peak allocation directly with a
//! counting global allocator: a ~1 MiB payload claiming ~10⁶ elements
//! must fail cleanly (the first element is invalid) while allocating no
//! more than a small envelope — far below the tens of MiB the uncapped
//! reservation would take.
//!
//! Both probes live in ONE `#[test]` so no parallel test thread can
//! perturb the shared peak counter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use acetone_model::records::NodeRecord;
use acetone_model::values::decode_value;

/// Bytes currently allocated through the global allocator.
static CURRENT: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of `CURRENT` since the last reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct CountingAlloc;

// SAFETY (and the repo's no-unsafe rule): implementing `GlobalAlloc`
// requires `unsafe`; this impl only delegates verbatim to `System` and
// updates atomic counters, adding no memory manipulation of its own. It
// exists solely so this test can observe the decoder's peak allocation —
// the test below exercises it on every run.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
            let now = CURRENT.fetch_add(new_size, Ordering::SeqCst) + new_size;
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        new_ptr
    }
}

#[global_allocator]
static ALLOCATOR: CountingAlloc = CountingAlloc;

/// Run `f`, returning its result and the peak allocation growth (bytes
/// above the level current when `f` started).
fn peak_growth_during<R>(f: impl FnOnce() -> R) -> (R, usize) {
    let baseline = CURRENT.load(Ordering::SeqCst);
    PEAK.store(baseline, Ordering::SeqCst);
    let result = f();
    let peak = PEAK.load(Ordering::SeqCst);
    (result, peak.saturating_sub(baseline))
}

/// The number of elements the hostile heads claim (~10⁶ → a ~1 MiB
/// payload, since each claimed element must be backed by ≥ 1 input byte
/// to pass the precheck).
const CLAIMED: u32 = 1_000_000;

/// Peak-allocation envelope for a failing decode of a ~1 MiB hostile
/// payload. Uncapped, the value-array reservation alone is
/// `CLAIMED * size_of::<Value>()` (tens of MiB) and the label-array
/// reservation `CLAIMED * size_of::<String>()` (~24 MiB); capped, the
/// decoder needs only a few KiB. 4 MiB gives generous headroom for
/// allocator noise while still failing loudly on any regression.
const ENVELOPE: usize = 4 << 20;

/// CBOR array head (additional info 26: 4-byte length) claiming
/// `CLAIMED` elements, under the given major type.
fn array_head(major: u8) -> Vec<u8> {
    let mut out = vec![(major << 5) | 26];
    out.extend_from_slice(&CLAIMED.to_be_bytes());
    out
}

#[test]
fn hostile_claimed_counts_decode_or_error_within_a_small_memory_envelope() {
    // --- property-value array (values.rs read_value, MAJOR_ARRAY) --------
    // array(CLAIMED), first element 0xff (a lone "break" — always an
    // error), then padding so the count-vs-remaining precheck passes.
    let mut hostile_value = array_head(4);
    hostile_value.push(0xff);
    hostile_value.extend(std::iter::repeat_n(0u8, CLAIMED as usize - 1));

    let (result, peak) = peak_growth_during(|| decode_value(&hostile_value));
    assert!(result.is_err(), "hostile value payload must error");
    assert!(
        peak < ENVELOPE,
        "value-array decode peaked at {peak} bytes (envelope {ENVELOPE}); \
         the preallocation cap has regressed"
    );

    // --- node-record secondary-labels array (records.rs decode) ----------
    // [array(2), array(CLAIMED), 0xff, padding...]: the first "label" is
    // not a text head, so decoding errors after the reservation.
    let mut hostile_record = vec![0x82];
    hostile_record.extend(array_head(4));
    hostile_record.push(0xff);
    hostile_record.extend(std::iter::repeat_n(0u8, CLAIMED as usize - 1));

    let (result, peak) = peak_growth_during(|| NodeRecord::decode(&hostile_record));
    assert!(result.is_err(), "hostile record payload must error");
    assert!(
        peak < ENVELOPE,
        "label-array decode peaked at {peak} bytes (envelope {ENVELOPE}); \
         the preallocation cap has regressed"
    );
}
