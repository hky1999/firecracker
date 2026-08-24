// Copyright © 2026 Tencent Corporation
//
// SPDX-License-Identifier: Apache-2.0

//! Soft-dirty incremental snapshot support.
//!
//! Ported from CubeSandbox (Cloud Hypervisor fork) `soft_dirty.rs` and
//! adapted to Firecracker's memory types.
//!
//! This module provides functionality for tracking which guest memory pages
//! have been written since the last snapshot, using the kernel's per-PTE
//! soft-dirty bit (bit 55 in `/proc/self/pagemap`).
//!
//! Lifecycle:
//! 1. Arm: write "4" to `/proc/self/clear_refs` — clears bit 55 on every PTE
//!    of the process (this is why the guest-memory mapping must belong to the
//!    VMM process itself).
//! 2. Guest runs: any host-side write into guest memory (KVM exit handling,
//!    vCPU stores, DMA) faults the PTE and sets bit 55. Reads do not set it —
//!    this is the read-neutral property the AgentEnv AtomicBitmap supplement
//!    lacked.
//! 3. Snapshot: read bit 55 per page = the exact delta window; re-arm only
//!    after the delta is durably written (ack semantics).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Mutex, OnceLock};

use vm_memory::{GuestAddress, GuestMemory};

use crate::logger::{debug, trace};
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::pagemap_anon::{self, MemoryRange};

/// Bit 55 of a pagemap entry: soft-dirty flag
pub const PAGEMAP_SOFT_DIRTY_BIT: u64 = 1 << 55;

/// Value written to /proc/self/clear_refs to clear soft-dirty bits:
/// "4" = clear soft-dirty bits only
const CLEAR_REFS_SOFT_DIRTY: &[u8] = b"4\n";

/// Size of a pagemap entry in bytes
const PAGEMAP_ENTRY_SIZE: usize = 8;

/// Serialize clear_refs operations.
///
/// Writing "4" to clear_refs walks and write-locks every PTE of the process.
/// Concurrent writers are merely wasted work, but the operation is global, so
/// tests (and any future snapshot paths) must not interleave arming with
/// delta collection.
static CLEAR_REFS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn clear_refs_lock() -> &'static Mutex<()> {
    CLEAR_REFS_LOCK.get_or_init(|| Mutex::new(()))
}

/// Errors related to soft_dirty operations
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SoftDirtyError {
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

    /// Failed to write {path}: {source}
    WriteFailed {
        /// Path that failed to be written.
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

    /// mmap of probe scratch page failed: {source}
    ProbeMmapFailed {
        /// Underlying IO error.
        source: io::Error,
    },

    /// munmap of probe scratch page failed: {source}
    ProbeMunmapFailed {
        /// Underlying IO error.
        source: io::Error,
    },
}

/// Result type for soft_dirty operations
pub type Result<T> = std::result::Result<T, SoftDirtyError>;

/// Errors of the intersection filter: either ledger can fail.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum IntersectionError {
    /// pagemap-anon ledger failed: {0}
    Anon(#[from] pagemap_anon::PagemapAnonError),
    /// soft-dirty ledger failed: {0}
    SoftDirty(#[from] SoftDirtyError),
}

/// Statistics about soft-dirty filtering results
#[derive(Debug, Default, Clone)]
pub struct SoftDirtyStats {
    /// Total number of pages examined
    pub total_pages: u64,
    /// Pages dirtied since the last clear_refs (the delta window)
    pub dirty_pages: u64,
    /// Total bytes in all input ranges
    pub total_bytes: u64,
    /// Bytes that must be written out (dirtied)
    pub dirty_bytes: u64,
}

impl SoftDirtyStats {
    /// Percentage of the input that stayed clean (savings vs full snapshot)
    pub fn savings_percentage(&self) -> f64 {
        if self.total_bytes == 0 {
            return 0.0;
        }
        ((self.total_bytes - self.dirty_bytes) as f64 / self.total_bytes as f64) * 100.0
    }
}

/// Read the soft-dirty bitmap for a host memory region.
///
/// # Arguments
/// * `host_addr` - Host virtual address (must be page-aligned)
/// * `length` - Region length in bytes
///
/// # Returns
/// One bool per host page: true = written since the last `clear_refs(4)`.
pub fn get_soft_dirty_pages(host_addr: u64, length: u64) -> Result<Vec<bool>> {
    let page_size = pagemap_anon::host_page_size();
    if !host_addr.is_multiple_of(page_size) {
        return Err(SoftDirtyError::NotPageAligned);
    }

    let num_pages =
        usize::try_from(length.div_ceil(page_size)).expect("page count must fit in usize");
    let start_page = host_addr / page_size;

    let mut pagemap_file =
        File::open("/proc/self/pagemap").map_err(|e| SoftDirtyError::OpenFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    let pagemap_offset = start_page * PAGEMAP_ENTRY_SIZE as u64;
    pagemap_file
        .seek(SeekFrom::Start(pagemap_offset))
        .map_err(|e| SoftDirtyError::SeekFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    let buf_size = num_pages * PAGEMAP_ENTRY_SIZE;
    let mut pagemap_buf = vec![0u8; buf_size];
    pagemap_file
        .read_exact(&mut pagemap_buf)
        .map_err(|e| SoftDirtyError::ReadFailed {
            path: "/proc/self/pagemap".to_string(),
            source: e,
        })?;

    let mut result = vec![false; num_pages];
    for (i, item) in result.iter_mut().enumerate().take(num_pages) {
        let entry_offset = i * PAGEMAP_ENTRY_SIZE;
        let entry = u64::from_ne_bytes(
            pagemap_buf[entry_offset..entry_offset + PAGEMAP_ENTRY_SIZE]
                .try_into()
                .unwrap(),
        );

        // The soft-dirty bit is readable without privileges; a swapped-out
        // page (bit 62) is never present, but anonymous swapped pages were
        // necessarily written, so the anon filter (see the intersection
        // filter below) classifies them.
        *item = (entry & PAGEMAP_SOFT_DIRTY_BIT) != 0;
    }

    Ok(result)
}

/// Clear (arm) the soft-dirty bits of the whole process by writing "4" to
/// `/proc/self/clear_refs`.
///
/// This is the *only* arming primitive: after it returns, bit 55 of every PTE
/// is 0, and any subsequent host-side write into guest memory re-sets it.
/// The caller must invoke this again only after the previous delta window
/// has been durably written (ack semantics).
///
/// Note: the kernel walks and write-locks all PTEs of the process; on
/// multi-GiB VMMs this costs on the order of hundreds of milliseconds and is
/// charged to the snapshot pause window.
pub fn clear_soft_dirty() -> Result<()> {
    let _guard = clear_refs_lock().lock().unwrap();
    clear_soft_dirty_locked()
}

/// Arming primitive for callers already holding `CLEAR_REFS_LOCK` — tests
/// must hold the lock across their whole arm → write → collect window, or a
/// concurrently running arming test would wipe the bits under them.
fn clear_soft_dirty_locked() -> Result<()> {
    let start = std::time::Instant::now();

    let mut clear_refs_file = OpenOptions::new()
        .write(true)
        .open("/proc/self/clear_refs")
        .map_err(|e| SoftDirtyError::OpenFailed {
            path: "/proc/self/clear_refs".to_string(),
            source: e,
        })?;

    clear_refs_file
        .write_all(CLEAR_REFS_SOFT_DIRTY)
        .map_err(|e| SoftDirtyError::WriteFailed {
            path: "/proc/self/clear_refs".to_string(),
            source: e,
        })?;

    debug!(
        "Cleared soft-dirty bits (clear_refs=4) in {} ms",
        start.elapsed().as_millis()
    );
    Ok(())
}

/// Probe whether the running kernel actually supports soft-dirty tracking.
///
/// A live round-trip on a private scratch page:
/// mmap → write → bit 55 must be 1 → clear_refs(4) → bit must be 0 →
/// write again → bit must be 1 again → munmap.
///
/// The round-trip is required because a kernel without
/// `CONFIG_MEM_SOFT_DIRTY=y` *silently accepts* writes to clear_refs and
/// reports pagemap entries with bit 55 always 0 — a naive "open+write
/// succeeded" probe would happily arm a ledger that never records anything.
pub fn probe_soft_dirty_support() -> Result<bool> {
    let _guard = clear_refs_lock().lock().unwrap();
    probe_soft_dirty_support_locked()
}

/// Probe body for callers already holding `CLEAR_REFS_LOCK`.
fn probe_soft_dirty_support_locked() -> Result<bool> {
    let page_size = pagemap_anon::host_page_size();
    let page_size_usize = usize::try_from(page_size).expect("host page size must fit in usize");

    // SAFETY: mmap of a fresh anonymous private page with null fd; the
    // returned pointer is page-aligned and we only touch `page_size` bytes.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page_size_usize,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if addr == libc::MAP_FAILED {
        return Err(SoftDirtyError::ProbeMmapFailed {
            source: io::Error::last_os_error(),
        });
    }

    let probe = |write_first: bool| -> Result<bool> {
        if write_first {
            // SAFETY: addr is a valid PROT_WRITE mapping of one page.
            unsafe { addr.cast::<u8>().write_volatile(0x5a) };
        }
        let bits = get_soft_dirty_pages(addr as u64, page_size)?;
        Ok(bits[0])
    };

    let supported = (|| -> Result<bool> {
        // Initial state of a fresh mapping is soft-dirty (kernel marks new
        // mappings dirty so the first window can observe them); either way we
        // only trust the transition, so start by arming.
        clear_soft_dirty_locked()?;
        let clean = probe(false)?;
        if clean {
            // Bit must be 0 right after a clear; a kernel that keeps
            // reporting 1 cannot provide window semantics.
            return Ok(false);
        }
        let dirtied = probe(true)?;
        let recleared = {
            clear_soft_dirty_locked()?;
            probe(false)?
        };
        Ok(dirtied && !recleared)
    })();

    // SAFETY: unmapping the one page we mapped above.
    let munmap_ret = unsafe { libc::munmap(addr, page_size_usize) };
    if munmap_ret != 0 {
        return Err(SoftDirtyError::ProbeMunmapFailed {
            source: io::Error::last_os_error(),
        });
    }

    supported
}

/// Filter memory ranges to pages whose soft-dirty bit is set.
///
/// Returns the coalesced guest-physical dirty ranges plus stats. `ranges` must
/// describe guest memory mapped in the current process.
pub fn filter_memory_ranges_by_soft_dirty(
    guest_memory: &GuestMemoryMmap,
    ranges: &[MemoryRange],
) -> Result<(Vec<MemoryRange>, SoftDirtyStats)> {
    let mut filtered_ranges = Vec::new();
    let mut stats = SoftDirtyStats::default();
    let page_size = pagemap_anon::host_page_size();

    debug!(
        "Starting soft-dirty filtering for {} memory regions",
        ranges.len()
    );

    for range in ranges {
        let gpa = range.gpa;
        let length = range.length;

        stats.total_bytes += length;
        stats.total_pages += length.div_ceil(page_size);

        trace!("Processing memory region: GPA=0x{gpa:x}, length={length}");

        let host_addr = guest_memory
            .get_host_address(GuestAddress(gpa))
            .map_err(|_| SoftDirtyError::GetHostAddressFailed)?;

        let dirty_bitmap = get_soft_dirty_pages(host_addr as u64, length)?;
        let (region_ranges, dirty_count) =
            pagemap_anon::coalesce_pages_to_ranges(gpa, &dirty_bitmap, page_size);
        stats.dirty_pages += dirty_count;
        stats.dirty_bytes += dirty_count * page_size;
        filtered_ranges.extend(region_ranges);
    }

    debug!(
        "Soft-dirty filtering complete: {} dirty ranges, {} of {} pages dirty ({:.1}% savings vs full)",
        filtered_ranges.len(),
        stats.dirty_pages,
        stats.total_pages,
        stats.savings_percentage()
    );

    Ok((filtered_ranges, stats))
}

/// Filter memory ranges by the intersection of pagemap-anon and soft-dirty.
///
/// Why the intersection is the minimal safe delta:
/// - soft-dirty alone is a *window* delta (resets on clear_refs) but can be
///   set by writes to file-backed mappings that never CoW'd (e.g. kernel
///   migration entries, or pages the VMM wrote before the mapping became
///   CoW);
/// - pagemap-anon alone is *cumulative since restore* (KPF_ANON never resets
///   while the mapping lives), so it re-reports every page written since the
///   previous incremental snapshot, not just the current window;
/// - a page is exactly what the current delta must contain iff it is private
///   anonymous (its content diverges from the base file) AND it was written
///   in the current window. Swapped-out anonymous pages are anon=true but
///   present=0 (their soft-dirty bit reads 0), so the anon filter's swapped
///   classification recovers them into the delta.
pub fn filter_memory_ranges_by_anon_and_soft_dirty(
    guest_memory: &GuestMemoryMmap,
    ranges: &[MemoryRange],
) -> std::result::Result<(Vec<MemoryRange>, SoftDirtyStats), IntersectionError> {
    let mut filtered_ranges = Vec::new();
    let mut stats = SoftDirtyStats::default();
    let page_size = pagemap_anon::host_page_size();

    debug!(
        "Starting anon+soft-dirty intersection filtering for {} memory regions",
        ranges.len()
    );

    for range in ranges {
        let gpa = range.gpa;
        let length = range.length;

        stats.total_bytes += length;
        stats.total_pages += length.div_ceil(page_size);

        trace!("Processing memory region: GPA=0x{gpa:x}, length={length}");

        let host_addr = guest_memory
            .get_host_address(GuestAddress(gpa))
            .map_err(|_| {
                IntersectionError::Anon(pagemap_anon::PagemapAnonError::GetHostAddressFailed)
            })?;

        // Both ledgers read from /proc/self/pagemap; anon additionally
        // consults /proc/kpageflags per present page.
        let anon_bitmap = pagemap_anon::get_anon_pages(host_addr as u64, length)
            .map_err(IntersectionError::Anon)?;
        let soft_dirty_bitmap =
            get_soft_dirty_pages(host_addr as u64, length).map_err(IntersectionError::SoftDirty)?;

        // Swapped anonymous pages: anon=true, soft-dirty unobservable.
        // Take them (content diverges from base file).
        let delta_bitmap: Vec<bool> = anon_bitmap
            .iter()
            .zip(soft_dirty_bitmap.iter())
            .map(|(&anon, &dirty)| anon && dirty)
            .collect();

        let (region_ranges, dirty_count) =
            pagemap_anon::coalesce_pages_to_ranges(gpa, &delta_bitmap, page_size);
        stats.dirty_pages += dirty_count;
        stats.dirty_bytes += dirty_count * page_size;
        filtered_ranges.extend(region_ranges);
    }

    debug!(
        "Anon+soft-dirty intersection complete: {} delta ranges, {} of {} pages ({:.1}% savings vs full)",
        filtered_ranges.len(),
        stats.dirty_pages,
        stats.total_pages,
        stats.savings_percentage()
    );

    Ok((filtered_ranges, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vmm_config::machine_config::HugePageConfig;
    use crate::vstate::memory::test_utils::into_region_ext;
    use vm_memory::{Bytes, GuestAddress, GuestMemory};

    /// Build a page-aligned guest memory fixture of `num_pages` host pages
    /// starting at GPA 0, anonymous MAP_PRIVATE (so faulted pages are KPF_ANON).
    fn fixture(num_pages: usize) -> (GuestMemoryMmap, u64, u64) {
        let page_size = pagemap_anon::host_page_size();
        let length = num_pages * usize::try_from(page_size).unwrap();
        let regions = vec![(GuestAddress(0), length)];
        let guest_memory = into_region_ext(
            crate::vstate::memory::anonymous(regions.into_iter(), false, HugePageConfig::None)
                .unwrap(),
        );
        let host_addr = guest_memory.get_host_address(GuestAddress(0)).unwrap() as u64;
        assert_eq!(host_addr % page_size, 0);
        (guest_memory, host_addr, page_size)
    }

    /// Write one byte into page `idx` of the fixture's host mapping.
    fn touch_page(guest_memory: &GuestMemoryMmap, idx: usize) {
        guest_memory
            .write_obj(
                0x5au8,
                GuestAddress(idx as u64 * pagemap_anon::host_page_size()),
            )
            .unwrap();
    }

    #[test]
    fn test_soft_dirty_constants() {
        assert_eq!(PAGEMAP_SOFT_DIRTY_BIT, 1u64 << 55);
        assert_eq!(CLEAR_REFS_SOFT_DIRTY, b"4\n");
    }

    #[test]
    fn test_get_soft_dirty_pages_not_page_aligned() {
        let unaligned = pagemap_anon::host_page_size() + 1;
        let result = get_soft_dirty_pages(unaligned, pagemap_anon::host_page_size());
        assert!(matches!(
            result.unwrap_err(),
            SoftDirtyError::NotPageAligned
        ));
    }

    #[test]
    fn test_get_soft_dirty_pages_empty_region() {
        let (_gm, host_addr, page_size) = fixture(1);
        let bits = get_soft_dirty_pages(host_addr, 0).unwrap();
        assert!(bits.is_empty());
        // Silence unused warnings for the fixture fields.
        let _ = page_size;
    }

    #[test]
    fn test_clear_soft_dirty_opens_and_writes() {
        // Exercise the arming path itself; bit semantics are covered by the
        // probe/round-trip tests below when the kernel supports soft-dirty.
        assert!(matches!(clear_soft_dirty(), Ok(())));
    }

    #[test]
    fn test_probe_soft_dirty_support_round_trip() {
        // On a CONFIG_MEM_SOFT_DIRTY kernel this must be Ok(true). An Ok(false)
        // or specific errors mean the probe *correctly* detected a lack of
        // support (or an inaccessible /proc), which is the degradation path —
        // acceptable outcomes for this test, as long as it doesn't panic.
        match probe_soft_dirty_support() {
            Ok(supported) => {
                if !supported {
                    eprintln!(
                        "soft-dirty not supported on this kernel; probe correctly returned false"
                    );
                }
            }
            Err(e) => {
                // Only the probe mmap itself failing is fatal; clear_refs
                // permission issues degrade via Err on kernels where the
                // interface exists but is locked down.
                eprintln!("soft-dirty probe errored (degradation path): {e}");
            }
        }
    }

    /// End-to-end delta round-trip over four windows, proving the ledger is
    /// a window delta (not cumulative): arm → write → collected set must be
    /// exactly the written set of the current window, and re-arming with no
    /// intervening writes yields an empty set.
    #[test]
    fn test_soft_dirty_delta_round_trip() {
        if !matches!(probe_soft_dirty_support(), Ok(true)) {
            eprintln!("skipping: kernel lacks usable soft-dirty tracking");
            return;
        }

        let num_pages = 64usize;
        let (guest_memory, _host_addr, page_size) = fixture(num_pages);
        let full_range = [MemoryRange {
            gpa: 0,
            length: (num_pages as u64) * page_size,
        }];

        // Hold the lock for the whole test: clear_refs is process-wide, so a
        // concurrent arming test would wipe the bits mid-window.
        let _guard = clear_refs_lock().lock().unwrap();

        // Window 1: pages 3 and 10.
        clear_soft_dirty_locked().unwrap();
        touch_page(&guest_memory, 3);
        touch_page(&guest_memory, 10);
        let (ranges, stats) =
            filter_memory_ranges_by_soft_dirty(&guest_memory, &full_range).unwrap();
        let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
        assert_eq!(
            got,
            vec![(3 * page_size, page_size), (10 * page_size, page_size)]
        );
        assert_eq!(stats.dirty_pages, 2);

        // Window 2: no writes — the armed ledger must report nothing.
        clear_soft_dirty_locked().unwrap();
        let (ranges, stats) =
            filter_memory_ranges_by_soft_dirty(&guest_memory, &full_range).unwrap();
        assert!(ranges.is_empty(), "empty window must produce empty delta");
        assert_eq!(stats.dirty_pages, 0);

        // Window 3: a disjoint page set — old writes must not reappear.
        clear_soft_dirty_locked().unwrap();
        touch_page(&guest_memory, 40);
        touch_page(&guest_memory, 41);
        touch_page(&guest_memory, 50);
        let (ranges, stats) =
            filter_memory_ranges_by_soft_dirty(&guest_memory, &full_range).unwrap();
        let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
        assert_eq!(
            got,
            vec![(40 * page_size, 2 * page_size), (50 * page_size, page_size)]
        );
        assert_eq!(stats.dirty_pages, 3);

        // Window 4: overlapping writes — still exactly the current window.
        clear_soft_dirty_locked().unwrap();
        touch_page(&guest_memory, 3);
        let (ranges, stats) =
            filter_memory_ranges_by_soft_dirty(&guest_memory, &full_range).unwrap();
        let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
        assert_eq!(got, vec![(3 * page_size, page_size)]);
        assert_eq!(stats.dirty_pages, 1);
    }

    /// The intersection filter must degrade gracefully (skip, not panic) when
    /// kpageflags PFNs are hidden (no CAP_SYS_ADMIN) and must otherwise select
    /// exactly the written pages of the current window.
    #[test]
    fn test_intersection_filter_matches_written_pages() {
        if !matches!(probe_soft_dirty_support(), Ok(true)) {
            eprintln!("skipping: kernel lacks usable soft-dirty tracking");
            return;
        }

        let num_pages = 32usize;
        let (guest_memory, _host_addr, page_size) = fixture(num_pages);
        let full_range = [MemoryRange {
            gpa: 0,
            length: (num_pages as u64) * page_size,
        }];

        // Same process-wide-clear_refs serialization as the round-trip test.
        let _guard = clear_refs_lock().lock().unwrap();

        clear_soft_dirty_locked().unwrap();
        touch_page(&guest_memory, 1);
        touch_page(&guest_memory, 5);

        match filter_memory_ranges_by_anon_and_soft_dirty(&guest_memory, &full_range) {
            Ok((ranges, stats)) => {
                // Fixture memory is MAP_PRIVATE|MAP_ANONYMOUS, so every
                // faulted page is KPF_ANON and the intersection reduces to
                // the soft-dirty window.
                let got: Vec<(u64, u64)> = ranges.iter().map(|r| (r.gpa, r.length)).collect();
                assert_eq!(
                    got,
                    vec![(page_size, page_size), (5 * page_size, page_size)]
                );
                assert_eq!(stats.dirty_pages, 2);
            }
            Err(IntersectionError::Anon(pagemap_anon::PagemapAnonError::NoCapSysAdmin)) => {
                // Degradation path: caller falls back to soft-dirty-only or
                // Full snapshots.
                eprintln!("skipping intersection assertion: no CAP_SYS_ADMIN for kpageflags");
            }
            Err(IntersectionError::Anon(pagemap_anon::PagemapAnonError::OpenFailed {
                path,
                ..
            })) if path.contains("kpageflags") => {
                // Same degradation on kernels that gate the *open* itself
                // behind CAP_SYS_ADMIN (observed as EACCES) instead of just
                // zeroing the PFNs.
                eprintln!("skipping intersection assertion: kpageflags not readable: {path}");
            }
            Err(e) => panic!("unexpected intersection error: {e}"),
        }
    }
}
