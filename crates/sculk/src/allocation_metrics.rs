#[derive(Clone, Copy, Debug, Default)]
pub struct AllocationSnapshot {
    pub calls: u64,
    pub bytes: u64,
    /// Bytes currently allocated (not a delta).
    pub live: u64,
    /// Highest `live` value seen since start (not a delta).
    pub peak: u64,
}

impl AllocationSnapshot {
    pub fn delta(self, earlier: Self) -> Self {
        Self {
            calls: self.calls.saturating_sub(earlier.calls),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            live: self.live,
            peak: self.peak,
        }
    }
}

#[cfg(feature = "allocation-metrics")]
mod enabled {
    use super::AllocationSnapshot;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CALLS: AtomicU64 = AtomicU64::new(0);
    static BYTES: AtomicU64 = AtomicU64::new(0);
    static LIVE: AtomicU64 = AtomicU64::new(0);
    static PEAK: AtomicU64 = AtomicU64::new(0);

    fn grow(bytes: usize) {
        let live = LIVE.fetch_add(bytes as u64, Ordering::Relaxed) + bytes as u64;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    fn shrink(bytes: usize) {
        LIVE.fetch_sub(bytes as u64, Ordering::Relaxed);
    }

    pub struct CountingAllocator;

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            grow(layout.size());
            // SAFETY: The system allocator receives the unchanged valid layout.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
            grow(layout.size());
            // SAFETY: The system allocator receives the unchanged valid layout.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            shrink(layout.size());
            // SAFETY: The caller provides the pointer/layout pair from this allocator.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            CALLS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
            grow(new_size);
            shrink(layout.size());
            // SAFETY: The caller provides the original allocation and requested new size.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    pub fn snapshot() -> AllocationSnapshot {
        AllocationSnapshot {
            calls: CALLS.load(Ordering::Relaxed),
            bytes: BYTES.load(Ordering::Relaxed),
            live: LIVE.load(Ordering::Relaxed),
            peak: PEAK.load(Ordering::Relaxed),
        }
    }
}

pub const ENABLED: bool = cfg!(feature = "allocation-metrics");

pub fn snapshot() -> AllocationSnapshot {
    #[cfg(feature = "allocation-metrics")]
    {
        enabled::snapshot()
    }
    #[cfg(not(feature = "allocation-metrics"))]
    {
        AllocationSnapshot::default()
    }
}
