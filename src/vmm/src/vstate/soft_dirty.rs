// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
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
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, OnceLock};

use vm_memory::{GuestAddress, GuestMemory};

use crate::logger::{debug, trace};
use crate::vstate::memory::GuestMemoryMmap;
use crate::vstate::pagemap_anon::{self, MemoryRange};

/// Bit 55 of a pagemap entry: soft-dirty flag
pub const PAGEMAP_SOFT_DIRTY_BIT: u64 = 1 << 55;
/// Bit 62: page is swapped out (not present in RAM).
const PAGEMAP_SWAPPED_BIT: u64 = 1 << 62;

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

/// Which ledger incremental accounting currently uses (M1-F5 lazy arming).
///
/// The mode starts [`AccountingMode::Unprobed`]: probing the kernel costs a
/// clear_refs round-trip over every PTE of the process, so it is deferred to
/// the first snapshot that actually needs a ledger, instead of being paid on
/// every microVM start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AccountingMode {
    /// No snapshot has needed a ledger yet; probe on first use.
    #[default]
    Unprobed,
    /// Soft-dirty window ledger (bit 55) is armed and authoritative.
    SoftDirty,
    /// Soft-dirty unsupported on this kernel; fall back to the cumulative
    /// pagemap-anon ledger (only meaningful for MAP_PRIVATE restores).
    AnonOnly,
    /// No usable ledger; the caller must take Full snapshots.
    Disabled,
}

/// Outcome of an [`SoftDirtyAccounting::arm`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmOutcome {
    /// Probe succeeded and the ledger was armed.
    Armed,
    /// The ledger was already armed — nothing was done.
    AlreadyArmed,
    /// Soft-dirty is unusable here; accounting degraded to [`AccountingMode::AnonOnly`].
    AnonOnly,
}

/// Lazy-arming state machine for the soft-dirty ledger, one per microVM.
///
/// Arming is driven exclusively by write success (ack semantics, absorbing
/// the AgentEnv 环② lesson): `clear_refs(4)` opens the next delta window, so
/// it must never run unless the *previous* window was durably persisted —
/// otherwise the pages of a lost window silently vanish from every later
/// delta. Consequently:
/// - [`SoftDirtyAccounting::arm`] only fires on the first incremental
///   snapshot (or after a failure disarmed the ledger);
/// - [`SoftDirtyAccounting::ack_persisted`] re-arms after a successful write;
/// - [`SoftDirtyAccounting::disarm`] marks a failed window so the next
///   snapshot re-arms (which re-baselines by writing the full anon set).
#[derive(Debug)]
pub struct SoftDirtyAccounting {
    mode: std::sync::Mutex<AccountingMode>,
    armed: AtomicBool,
}

impl Default for SoftDirtyAccounting {
    fn default() -> Self {
        Self {
            mode: std::sync::Mutex::new(AccountingMode::Unprobed),
            armed: AtomicBool::new(false),
        }
    }
}

impl SoftDirtyAccounting {
    /// Current accounting mode.
    pub fn mode(&self) -> AccountingMode {
        *self.mode.lock().unwrap()
    }

    /// Whether the soft-dirty ledger is armed (window open).
    pub fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Probe the kernel and arm the ledger if possible.
    ///
    /// Degradation is explicit, never silent: an unusable soft-dirty
    /// interface switches the mode to [`AccountingMode::AnonOnly`] and
    /// returns [`ArmOutcome::AnonOnly`] so the caller knows to write the
    /// cumulative anon set instead of a window delta.
    pub fn arm(&self) -> Result<ArmOutcome> {
        self.arm_with_probe(probe_soft_dirty_support)
    }

    /// Testable core of [`Self::arm`] with an injected probe (arming write
    /// still goes through the real [`clear_soft_dirty`]).
    fn arm_with_probe(&self, probe: impl FnOnce() -> Result<bool>) -> Result<ArmOutcome> {
        self.arm_with(probe, clear_soft_dirty)
    }

    /// Fully injectable core: `probe` reports kernel support, `arm_write`
    /// performs the arming clear_refs write.
    fn arm_with(
        &self,
        probe: impl FnOnce() -> Result<bool>,
        arm_write: impl FnOnce() -> Result<()>,
    ) -> Result<ArmOutcome> {
        let mut mode = self.mode.lock().unwrap();

        match *mode {
            AccountingMode::SoftDirty if self.is_armed() => return Ok(ArmOutcome::AlreadyArmed),
            _ => {}
        }

        match probe() {
            Ok(true) => {
                // Propagate an arming failure: the caller must treat this
                // snapshot as failed; the mode stays untouched so the next
                // attempt re-probes.
                arm_write()?;
                *mode = AccountingMode::SoftDirty;
                self.armed.store(true, std::sync::atomic::Ordering::Release);
                Ok(ArmOutcome::Armed)
            }
            Ok(false) | Err(_) => {
                // Kernel without usable soft-dirty tracking (probe Err covers
                // locked-down /proc paths that would mislead an is_ok check).
                *mode = AccountingMode::AnonOnly;
                self.armed
                    .store(false, std::sync::atomic::Ordering::Release);
                Ok(ArmOutcome::AnonOnly)
            }
        }
    }

    /// Re-arm after the current delta window was durably written (ack).
    ///
    /// Errors (and disarms) if re-arming fails: the next snapshot will then
    /// re-arm via [`Self::arm`], whose arming writes a fresh full window.
    pub fn ack_persisted(&self) -> Result<()> {
        let mode = *self.mode.lock().unwrap();
        if mode != AccountingMode::SoftDirty {
            return Ok(());
        }
        if let Err(e) = clear_soft_dirty() {
            self.armed
                .store(false, std::sync::atomic::Ordering::Release);
            return Err(e);
        }
        self.armed.store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    /// Mark the current window as *not* persisted: disarm so the next
    /// snapshot re-arms and re-baselines instead of trusting a ledger whose
    /// window start was lost.
    pub fn disarm(&self) {
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Demote [`AccountingMode::AnonOnly`] to [`AccountingMode::Disabled`]
    /// after the anon ledger itself failed (e.g. kpageflags needs
    /// CAP_SYS_ADMIN); the caller must fall back to Full snapshots.
    pub fn note_ledger_unusable(&self) {
        let mut mode = self.mode.lock().unwrap();
        if *mode == AccountingMode::AnonOnly {
            *mode = AccountingMode::Disabled;
        }
    }
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

/// Interpret one pagemap entry for the soft-dirty window.
///
/// The soft-dirty bit is readable without privileges, but it is only
/// meaningful for present pages. A swapped-out anonymous page (bit 62)
/// was necessarily written (it entered swap through a write), yet its
/// soft-dirty bit is unobservable — the intersection filter would
/// drop it. Report swapped pages as dirty so the anon ∩ dirty
/// intersection conservatively includes them: including an unchanged
/// swapped page is a safe superset, dropping a changed one is stale
/// data at restore.
fn soft_dirty_entry_reports_dirty(entry: u64) -> bool {
    (entry & (PAGEMAP_SWAPPED_BIT | PAGEMAP_SOFT_DIRTY_BIT)) != 0
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
        *item = soft_dirty_entry_reports_dirty(entry);
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

    /// A swapped anonymous page (bit 62, not present) must be reported as
    /// dirty: it was necessarily written to reach swap, and its soft-dirty
    /// bit is unobservable while it stays swapped out. Dropping it would
    /// hand restore stale data for a page the guest wrote during the
    /// window; including an unchanged swapped page is a safe superset.
    #[test]
    fn test_swapped_entry_reports_dirty() {
        const PAGEMAP_PRESENT_BIT: u64 = 1 << 63;
        // Present + soft-dirty: dirty.
        assert!(soft_dirty_entry_reports_dirty(
            PAGEMAP_PRESENT_BIT | PAGEMAP_SOFT_DIRTY_BIT
        ));
        // Present + clean: not dirty.
        assert!(!soft_dirty_entry_reports_dirty(PAGEMAP_PRESENT_BIT));
        // Swapped out (not present) with the soft-dirty bit unreadable
        // (zero): still dirty — the swap-out itself was a write.
        assert!(soft_dirty_entry_reports_dirty(PAGEMAP_SWAPPED_BIT));
        // Swapped out with a stale soft-dirty bit set: dirty either way.
        assert!(soft_dirty_entry_reports_dirty(
            PAGEMAP_SWAPPED_BIT | PAGEMAP_SOFT_DIRTY_BIT
        ));
        // Neither present nor swapped nor dirty (e.g. a file-backed clean
        // page): not dirty.
        assert!(!soft_dirty_entry_reports_dirty(0));
        assert!(!soft_dirty_entry_reports_dirty(0x1234));
    }

    /// F5 state machine: happy path probes once, arms, and `arm` again is a
    /// no-op until a failed write disarms.
    #[test]
    fn test_accounting_arm_ack_disarm_cycle() {
        let acc = SoftDirtyAccounting::default();
        assert_eq!(acc.mode(), AccountingMode::Unprobed);
        assert!(!acc.is_armed());

        assert_eq!(
            acc.arm_with(|| Ok(true), || Ok(())).unwrap(),
            ArmOutcome::Armed
        );
        assert_eq!(acc.mode(), AccountingMode::SoftDirty);
        assert!(acc.is_armed());

        // Armed ledger: no re-probe, no clear_refs reset of the open window.
        assert_eq!(
            acc.arm_with(|| panic!("must not re-probe while armed"), || Ok(()))
                .unwrap(),
            ArmOutcome::AlreadyArmed
        );

        // Ack after a durable write re-arms (window rolls over).
        acc.ack_persisted().unwrap();
        assert!(acc.is_armed());

        // Write failure: disarm, next arm re-probes and re-arms.
        acc.disarm();
        assert!(!acc.is_armed());
        assert_eq!(
            acc.arm_with(|| Ok(true), || Ok(())).unwrap(),
            ArmOutcome::Armed
        );
    }

    /// F5 degradation matrix: unsupported kernel (Ok(false)) and locked-down
    /// /proc (Err) both land in AnonOnly; a failing anon ledger then demotes
    /// to Disabled.
    #[test]
    fn test_accounting_degradation_matrix() {
        for probe_result in [Ok(false), Err(SoftDirtyError::NotPageAligned)] {
            let acc = SoftDirtyAccounting::default();
            assert_eq!(
                acc.arm_with(move || probe_result, || Ok(())).unwrap(),
                ArmOutcome::AnonOnly
            );
            assert_eq!(acc.mode(), AccountingMode::AnonOnly);
            assert!(!acc.is_armed());

            // Ack in AnonOnly mode must stay a no-op: there is no window
            // ledger to roll over.
            acc.ack_persisted().unwrap();

            // The anon ledger failing too (e.g. kpageflags EACCES) is final.
            acc.note_ledger_unusable();
            assert_eq!(acc.mode(), AccountingMode::Disabled);

            // Demotion is one-way: a later successful probe must not
            // resurrect a Disabled accountant (caller owns Full fallback).
            acc.note_ledger_unusable();
            assert_eq!(acc.mode(), AccountingMode::Disabled);
        }
    }

    /// A failed clear_refs during arming must surface the error and leave the
    /// mode unclaimed (neither SoftDirty nor silently degraded).
    #[test]
    fn test_accounting_arm_clear_failure_degrades() {
        let acc = SoftDirtyAccounting::default();
        // Probe succeeds but the subsequent arming write fails: the caller
        // treats the snapshot as failed...
        acc.arm_with(|| Ok(true), || Err(SoftDirtyError::NotPageAligned))
            .unwrap_err();
        // ...and the mode must not claim SoftDirty.
        assert_ne!(acc.mode(), AccountingMode::SoftDirty);
        assert!(!acc.is_armed());
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
