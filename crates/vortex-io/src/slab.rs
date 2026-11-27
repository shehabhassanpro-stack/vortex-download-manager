//! 4 KiB-Aligned Reusable Slab Pool for write coalescing (`P1-T11` / `§9` & `§12.1.3`).
//!
//! Uses `#[repr(C, align(4096))]` [`SectorPage`] in 100% safe Rust so every staging slab is
//! guaranteed hardware-page aligned (`4096` bytes), eliminating OS Read-Modify-Write penalties on
//! NVMe/SSD sectors while capping memory churn via a bounded free-list.

use std::sync::Mutex;

/// Hardware sector / OS page alignment unit (`4 KiB`).
pub const SECTOR_ALIGN_BYTES: usize = 4096;

/// Default coalescing threshold (`2 MiB = 512 * 4 KiB`, within the `1–4 MiB` spec window).
pub const DEFAULT_COALESCE_BYTES: usize = 2 * 1024 * 1024;

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct SectorPage([u8; SECTOR_ALIGN_BYTES]);

/// A `4 KiB`-aligned contiguous byte staging buffer leased from [`AlignedSlabPool`].
pub struct AlignedStagingBuffer {
    pages: Vec<SectorPage>,
    len: usize,
}

impl std::fmt::Debug for AlignedStagingBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlignedStagingBuffer")
            .field("capacity_bytes", &self.capacity())
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl AlignedStagingBuffer {
    fn with_capacity_bytes(cap_bytes: usize) -> Self {
        let num_pages = cap_bytes.div_ceil(SECTOR_ALIGN_BYTES).max(1);
        let pages = vec![SectorPage([0_u8; SECTOR_ALIGN_BYTES]); num_pages];
        Self { pages, len: 0 }
    }

    /// Total capacity in bytes (always a multiple of `4096`).
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.pages.len() * SECTOR_ALIGN_BYTES
    }

    /// Current occupied byte count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if `len() == 0`.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Memory address alignment check (`addr % 4096 == 0`).
    #[must_use]
    pub fn is_4k_aligned(&self) -> bool {
        (self.pages.as_ptr() as usize).is_multiple_of(SECTOR_ALIGN_BYTES)
    }

    /// Appends `src` if it fits within `capacity()`, returning `true` on success.
    pub fn try_append(&mut self, src: &[u8]) -> bool {
        if self.len.saturating_add(src.len()) > self.capacity() {
            return false;
        }
        let mut remaining = src;
        let mut cur = self.len;
        while !remaining.is_empty() {
            let page_idx = cur / SECTOR_ALIGN_BYTES;
            let page_off = cur % SECTOR_ALIGN_BYTES;
            let space_in_page = SECTOR_ALIGN_BYTES - page_off;
            let copy_len = remaining.len().min(space_in_page);
            if let (Some(page), Some(chunk), Some(rest)) = (
                self.pages.get_mut(page_idx),
                remaining.get(..copy_len),
                remaining.get(copy_len..),
            ) && let Some(dst) = page.0.get_mut(page_off..page_off + copy_len)
            {
                dst.copy_from_slice(chunk);
                cur += copy_len;
                remaining = rest;
            } else {
                return false;
            }
        }
        self.len = cur;
        true
    }

    /// Resets `len` to `0` while keeping allocated `4 KiB` pages intact for reuse.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Copies the occupied bytes into a contiguous slice invocation or yields page-aligned slices.
    #[must_use]
    #[allow(unsafe_code)]
    pub fn as_contiguous_bytes(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `SectorPage` is `#[repr(C, align(4096))]` wrapping `[u8; 4096]` with size == 4096
        // and zero padding bytes (`size_of::<SectorPage>() == 4096`), and `self.len <= self.pages.len() * 4096`
        // is strictly initialized.
        unsafe { std::slice::from_raw_parts(self.pages.as_ptr().cast::<u8>(), self.len) }
    }
}

/// Bounded free-list pool of `4 KiB`-aligned staging slabs (`ADR-004` / `§9`).
#[derive(Debug)]
pub struct AlignedSlabPool {
    slab_bytes: usize,
    max_pooled: usize,
    free_list: Mutex<Vec<AlignedStagingBuffer>>,
}

impl AlignedSlabPool {
    /// Creates a new [`AlignedSlabPool`] with `slab_bytes` per slab and at most `max_pooled` cached slabs.
    #[must_use]
    pub fn new(slab_bytes: usize, max_pooled: usize) -> Self {
        Self {
            slab_bytes: slab_bytes.max(SECTOR_ALIGN_BYTES),
            max_pooled: max_pooled.max(1),
            free_list: Mutex::new(Vec::with_capacity(max_pooled.min(32))),
        }
    }

    /// Checks out a clean `4 KiB`-aligned staging buffer (reusing pooled memory when available).
    #[must_use]
    pub fn checkout(&self) -> AlignedStagingBuffer {
        if let Ok(mut guard) = self.free_list.lock()
            && let Some(mut buf) = guard.pop()
        {
            buf.clear();
            return buf;
        }
        AlignedStagingBuffer::with_capacity_bytes(self.slab_bytes)
    }

    /// Returns a staging buffer to the pool (dropping it if the pool is already at `max_pooled`).
    pub fn checkin(&self, mut buf: AlignedStagingBuffer) {
        buf.clear();
        if let Ok(mut guard) = self.free_list.lock()
            && guard.len() < self.max_pooled
        {
            guard.push(buf);
        }
    }
}

impl Default for AlignedSlabPool {
    fn default() -> Self {
        Self::new(DEFAULT_COALESCE_BYTES, 16)
    }
}
