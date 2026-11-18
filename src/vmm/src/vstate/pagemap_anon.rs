// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Pagemap-anon incremental snapshot support.
//!
//! Ported from CubeSandbox (Cloud Hypervisor fork) `pagemap_anon.rs` and
//! adapted to Firecracker's memory types.
//!
//! Selects private anonymous guest pages using `/proc/self/pagemap`.
//!
//! A present page whose bit 61 is clear is private anonymous memory; a
//! file-backed or shared-anonymous page has that bit set. In a MAP_PRIVATE
//! restore, guest writes create private pages while read-only pages continue
//! to refer to the base file. Swapped pages are included conservatively.
//!
//! Do not resolve a pagemap PFN through `/proc/kpageflags`: NUMA migration can
//! replace that PFN between the two reads, so flags may describe a freed or
//! reused page and silently omit live guest data. This classification also
//! works when CAP_SYS_ADMIN restrictions hide PFNs.
//!
//! Faulted shared zero pages are included conservatively. They cannot be
//! distinguished from private pages using these bits alone. The result is
//! a superset of the former KPF_ANON selection, not a physical-page census.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::OnceLock;

use vm_memory::{GuestAddress, GuestMemoryBackend};

use crate::logger::{debug, trace};
use crate::vstate::memory::GuestMemoryMmap;

/// A guest-physical address range selected for incremental snapshotting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryRange {
    /// Guest-physical address of the first byte of the range.
    pub gpa: u64,
    /// Length of the range in bytes.
    pub length: u64,
}

/// Host page size in bytes, probed once from `sysconf(_SC_PAGESIZE)`.
///
/// This is 4 KiB on x86_64 but 64 KiB on ARM64 hosts configured with 64 KiB
/// base pages. `/proc/self/pagemap` and `/proc/kpageflags` are indexed in
/// units of the kernel's real page size, so every page-index, seek-offset and
/// range-length computation must use this value — hardcoding 4096 would
/// mis-index the pagemap and silently corrupt snapshots on 64 KiB kernels.
///
/// The value is fixed for the process lifetime, so probe it once and cache it.
static HOST_PAGE_SIZE: OnceLock<u64> = OnceLock::new();

/// Returns the host page size in bytes (cached; probed once via sysconf).
pub fn host_page_size() -> u64 {
    *HOST_PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf(_SC_PAGESIZE) takes no pointer argument, so it
        // cannot dereference or alias anything.
        let ret = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        // POSIX returns -1 on failure; a non-positive page size would corrupt
        // all pagemap offset math. The checked conversion rejects negatives.
        u64::try_from(ret).expect("sysconf(_SC_PAGESIZE) returned non-positive value")
    })
}

/// Coalesce a per-page boolean bitmap into contiguous guest-physical ranges.
///
/// `gpa` is the base guest-physical address the bitmap covers and `page_size`
/// is the byte granularity each bit represents. Returns the merged ranges plus
/// the number of set pages (for stats/accounting).
///
/// Kept as a pure function (no `/proc` access) with an explicit `page_size`
/// argument so it can be exercised with an injected page size — in particular
/// 65536 to prove the ARM64 64 KiB path — without a matching-page-size kernel.
pub(crate) fn coalesce_pages_to_ranges(
    gpa: u64,
    bitmap: &[bool],
    page_size: u64,
) -> (Vec<MemoryRange>, u64) {
    let mut ranges = Vec::new();
    let mut set_pages: u64 = 0;
    let mut current_range_start: Option<u64> = None;
    let mut current_range_length: u64 = 0;

    for (page_idx, &set) in bitmap.iter().enumerate() {
        let page_gpa = gpa + (page_idx as u64 * page_size);

        if set {
            set_pages += 1;
            if current_range_start.is_none() {
                current_range_start = Some(page_gpa);
                current_range_length = page_size;
            } else {
                current_range_length += page_size;
            }
        } else if let Some(start) = current_range_start.take() {
            ranges.push(MemoryRange {
                gpa: start,
                length: current_range_length,
            });
            current_range_length = 0;
        }
    }

    if let Some(start) = current_range_start {
        ranges.push(MemoryRange {
            gpa: start,
            length: current_range_length,
        });
    }

    (ranges, set_pages)
}

/// Size of a pagemap entry in bytes
const PAGEMAP_ENTRY_SIZE: usize = 8;

/// Bit 63: page is present in RAM
const PAGEMAP_PRESENT_BIT: u64 = 1 << 63;

/// Bit 62: page is in swap
const PAGEMAP_SWAPPED_BIT: u64 = 1 << 62;

/// Bit 61: file-backed page or shared-anonymous page.
const PAGEMAP_FILE_OR_SHARED_ANON_BIT: u64 = 1 << 61;

/// Classify the page from a single pagemap entry, without a racy PFN lookup.
fn pagemap_entry_reports_private(entry: u64) -> bool {
    entry & PAGEMAP_SWAPPED_BIT != 0
        || (entry & PAGEMAP_PRESENT_BIT != 0 && entry & PAGEMAP_FILE_OR_SHARED_ANON_BIT == 0)
}

/// Errors related to pagemap_anon operations
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum PagemapAnonError {
    /// Failed to open {path}: {source}
    OpenFailed {
        /// Path that failed to open.
        path: String,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Failed to read {path}: {source}
    ReadFailed {
        /// Path that failed to read.
        path: String,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Failed to seek in {path}: {source}
    SeekFailed {
        /// Path that failed to seek.
        path: String,
        /// Underlying IO error.
        source: io::Error,
    },

    /// Failed to get host address for guest memory region
    GetHostAddressFailed,

    /// Memory region not aligned to page boundary
    NotPageAligned,

    /// Legacy PFN-based classifier could not read kpageflags without CAP_SYS_ADMIN
    NoCapSysAdmin,
}

/// Result type for pagemap_anon operations
pub type Result<T> = std::result::Result<T, PagemapAnonError>;

/// Statistics about pagemap_anon filtering results
#[derive(Debug, Default, Clone)]
pub struct PagemapAnonStats {
    /// Total number of pages in the memory regions
    pub total_pages: u64,
    /// Private pages selected, conservatively including faulted zero pages
    pub anon_pages: u64,
    /// Number of pages that are swapped out (also counted as anon)
    pub swapped_pages: u64,
    /// Total bytes in all memory regions
    pub total_bytes: u64,
    /// Bytes that will be saved (anonymous pages)
    pub saved_bytes: u64,
}

impl PagemapAnonStats {
    /// Calculate the percentage of memory saved (not needing to be snapshotted)
    pub fn savings_percentage(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        ((self.total_bytes - self.saved_bytes) as f64 / self.total_bytes as f64) * 100.0
    }
}

/// Get a conservative private-page bitmap using only `/proc/self/pagemap`.
///
/// # Arguments
/// * `host_addr` - Host virtual address of the memory region (must be page-aligned)
/// * `length` - Length of the memory region in bytes
///
/// # Returns
/// A vector of bools where each bool indicates if the corresponding page
/// is private anonymous memory (including a faulted shared zero page).
/// Returns one bool per host page (true = private, including a faulted
/// zero page, or swapped out) plus the number of swapped-out anonymous
/// pages seen while scanning; those pages are `true` in the bitmap and
/// counted separately for the stats logging.
pub fn get_anon_pages(host_addr: u64, length: u64) -> Result<(Vec<bool>, u64)> {
    let page_size = host_page_size();
    if !host_addr.is_multiple_of(page_size) {
        return Err(PagemapAnonError::NotPageAligned);
    }

    let num_pages =
        usize::try_from(length.div_ceil(page_size)).expect("page count must fit in usize");
    let start_page = host_addr / page_size;

    // Read classification and residency from the same page-table snapshot.
    let mut pagemap_file =
        File::open("/proc/self/pagemap").map_err(|e| PagemapAnonError::OpenFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    // Batch read all pagemap entries for this region
    let pagemap_offset = start_page * PAGEMAP_ENTRY_SIZE as u64;
    pagemap_file
        .seek(SeekFrom::Start(pagemap_offset))
        .map_err(|e| PagemapAnonError::SeekFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    let buf_size = num_pages * PAGEMAP_ENTRY_SIZE;
    let mut pagemap_buf = vec![0u8; buf_size];
    pagemap_file
        .read_exact(&mut pagemap_buf)
        .map_err(|e| PagemapAnonError::ReadFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    let mut result = vec![false; num_pages];
    let mut swapped_pages = 0u64;
    for (item, bytes) in result.iter_mut().zip(pagemap_buf.chunks_exact(8)) {
        let entry = u64::from_ne_bytes(bytes.try_into().unwrap());
        *item = pagemap_entry_reports_private(entry);
        swapped_pages += u64::from(entry & PAGEMAP_SWAPPED_BIT != 0);
    }

    Ok((result, swapped_pages))
}

/// Filter memory ranges by pagemap_anon, returning only ranges with anonymous
/// (CoW) pages.
///
/// This function takes a list of memory ranges and returns a new list
/// containing only the pages that are anonymous (written by Guest via CoW).
///
/// # Arguments
/// * `guest_memory` - The guest memory object
/// * `ranges` - The original memory ranges
///
/// # Returns
/// A tuple containing:
/// - The filtered memory ranges (only anonymous pages)
/// - Statistics about the filtering
pub fn filter_memory_ranges_by_pagemap_anon(
    guest_memory: &GuestMemoryMmap,
    ranges: &[MemoryRange],
) -> Result<(Vec<MemoryRange>, PagemapAnonStats)> {
    let mut filtered_ranges = Vec::new();
    let mut stats = PagemapAnonStats::default();
    let page_size = host_page_size();

    debug!(
        "Starting pagemap_anon filtering for {} memory regions",
        ranges.len()
    );

    for range in ranges {
        let gpa = range.gpa;
        let length = range.length;

        stats.total_bytes += length;
        stats.total_pages += length.div_ceil(page_size);

        trace!("Processing memory region: GPA=0x{gpa:x}, length={length}");

        // Get host virtual address for this guest physical address
        let host_addr = guest_memory
            .get_host_address(GuestAddress(gpa))
            .map_err(|_| PagemapAnonError::GetHostAddressFailed)?;

        // Get a conservative private-page bitmap directly from pagemap.
        let (anon_pages, swapped) = get_anon_pages(host_addr as u64, length)?;
        stats.swapped_pages += swapped;

        // Convert bitmap to memory ranges (merge consecutive anonymous pages)
        let (region_ranges, anon_count) = coalesce_pages_to_ranges(gpa, &anon_pages, page_size);
        stats.anon_pages += anon_count;
        stats.saved_bytes += anon_count * page_size;
        filtered_ranges.extend(region_ranges);
    }

    debug!(
        "PagemapAnon filtering complete: {} anon ranges, {} total pages, {} anon pages ({} swapped)",
        filtered_ranges.len(),
        stats.total_pages,
        stats.anon_pages,
        stats.swapped_pages
    );

    if stats.total_pages > 0 {
        let anon_pct = (stats.anon_pages as f64 / stats.total_pages as f64) * 100.0;
        debug!(
            "PagemapAnon stats: {anon_pct:.1}% anonymous pages, {:.1}% savings vs full snapshot",
            stats.savings_percentage()
        );
    }

    Ok((filtered_ranges, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pagemap_anon_stats_savings_percentage() {
        let stats = PagemapAnonStats {
            total_bytes: 1000,
            saved_bytes: 250,
            ..Default::default()
        };

        assert!((stats.savings_percentage() - 75.0).abs() < 0.01);
    }

    #[test]
    fn test_pagemap_anon_stats_zero_total() {
        let stats = PagemapAnonStats::default();
        assert_eq!(stats.savings_percentage(), 0.0);
    }

    #[test]
    fn test_pagemap_anon_stats_full_anon() {
        let stats = PagemapAnonStats {
            total_bytes: 4096,
            saved_bytes: 4096,
            ..Default::default()
        };

        assert!((stats.savings_percentage() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_get_anon_pages_not_page_aligned() {
        // An address one byte past a page boundary must never be page-aligned,
        // regardless of whether the host uses 4 KiB or 64 KiB pages.
        let unaligned = host_page_size() + 1;
        let result = get_anon_pages(unaligned, host_page_size()).map(|_| ());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            PagemapAnonError::NotPageAligned
        ));
    }

    #[test]
    fn test_host_page_size_is_power_of_two() {
        let ps = host_page_size();
        assert!(ps >= 4096, "page size unexpectedly small: {ps}");
        assert!(ps.is_power_of_two(), "page size not a power of two: {ps}");
    }

    #[test]
    fn test_pagemap_constants() {
        // Verify bit positions are correct
        assert_eq!(PAGEMAP_PRESENT_BIT, 1u64 << 63);
        assert_eq!(PAGEMAP_SWAPPED_BIT, 1u64 << 62);
        assert_eq!(PAGEMAP_FILE_OR_SHARED_ANON_BIT, 1u64 << 61);
    }

    #[test]
    fn test_private_classification_does_not_depend_on_pfn() {
        // Migration may free/reuse the PFN captured by a previous read.
        // Hidden PFNs and either old/new physical frame must classify alike.
        for pfn in [0, 1, 0x12345, (1u64 << 55) - 1] {
            assert!(pagemap_entry_reports_private(PAGEMAP_PRESENT_BIT | pfn));
            assert!(!pagemap_entry_reports_private(
                PAGEMAP_PRESENT_BIT | PAGEMAP_FILE_OR_SHARED_ANON_BIT | pfn
            ));
        }
        assert!(!pagemap_entry_reports_private(0));
        assert!(!pagemap_entry_reports_private(1 << 55));
        assert!(pagemap_entry_reports_private(PAGEMAP_SWAPPED_BIT));
    }

    #[test]
    fn test_faulted_zero_page_is_included_conservatively() {
        let page = usize::try_from(host_page_size()).unwrap();
        // SAFETY: anonymous private mapping, used only until munmap below.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                2 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        // SAFETY: this read is within the freshly allocated mapping.
        assert_eq!(unsafe { std::ptr::read_volatile(address.cast::<u8>()) }, 0);
        let classified = get_anon_pages(address as u64, (2 * page) as u64);
        // SAFETY: the mapping is no longer used after this call.
        assert_eq!(unsafe { libc::munmap(address, 2 * page) }, 0);
        assert_eq!(classified.unwrap(), (vec![true, false], 0));
    }

    #[test]
    fn test_private_file_mapping_selects_cow_and_retains_readonly_base() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::FileExt;
        use vmm_sys_util::tempfile::TempFile;

        let page = usize::try_from(host_page_size()).unwrap();
        let file = TempFile::new().unwrap();
        file.as_file().set_len((4 * page) as u64).unwrap();
        file.as_file()
            .write_all_at(&vec![0x31; 4 * page], 0)
            .unwrap();
        // SAFETY: valid file range; the mapping is unmapped below after use.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4 * page,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_file().as_raw_fd(),
                0,
            )
        };
        assert_ne!(address, libc::MAP_FAILED);
        let ptr = address.cast::<u8>();
        // SAFETY: all offsets are within this writable private mapping.
        unsafe {
            assert_eq!(std::ptr::read_volatile(ptr), 0x31);
            std::ptr::write_volatile(ptr.add(page), 0x72);
            assert_eq!(std::ptr::read_volatile(ptr.add(3 * page)), 0x31);
        }
        let classified = get_anon_pages(address as u64, (4 * page) as u64);
        // SAFETY: the original mapping is no longer used after this call.
        assert_eq!(unsafe { libc::munmap(address, 4 * page) }, 0);
        let (bitmap, swapped) = classified.unwrap();
        assert_eq!(bitmap, [false, true, false, false]);
        assert_eq!(swapped, 0);
        let mut original = [0u8; 1];
        file.as_file()
            .read_exact_at(&mut original, page as u64)
            .unwrap();
        assert_eq!(original, [0x31], "CoW must not change the input file");
    }

    /// Coalescing must produce byte offsets/lengths scaled by the *injected*
    /// page size. Running the identical bitmap through 4 KiB and 64 KiB proves
    /// the ARM64 64 KiB path: a hardcoded 4096 would fail the 65536 case.
    #[test]
    fn test_coalesce_pages_to_ranges_page_size_matrix() {
        // Bitmap: pages 1,2 dirty; page 3 clean; page 5 dirty.
        // Expect two ranges: [1..=2] (2 pages) and [5] (1 page).
        let bitmap = [false, true, true, false, false, true];

        for &page_size in &[4096u64, 65536u64] {
            let gpa_base = 0x1000_0000;
            let (ranges, set_pages) = coalesce_pages_to_ranges(gpa_base, &bitmap, page_size);

            assert_eq!(set_pages, 3, "page_size={page_size}: wrong set-page count");

            let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
            assert_eq!(
                got,
                vec![
                    (gpa_base + page_size, 2 * page_size),
                    (gpa_base + 5 * page_size, page_size),
                ],
                "page_size={page_size}: ranges must scale with page size"
            );
        }
    }

    #[test]
    fn test_coalesce_pages_to_ranges_all_and_none() {
        for &page_size in &[4096u64, 65536u64] {
            // All clean -> no ranges.
            let (ranges, set) = coalesce_pages_to_ranges(0, &[false; 4], page_size);
            assert!(ranges.is_empty());
            assert_eq!(set, 0);

            // All dirty -> single coalesced range spanning every page.
            let (ranges, set) = coalesce_pages_to_ranges(0, &[true; 4], page_size);
            assert_eq!(set, 4);
            let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
            assert_eq!(got, vec![(0, 4 * page_size)]);
        }
    }
}
