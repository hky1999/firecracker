// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines state structures for saving/restoring a Firecracker microVM.

use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::mem::forget;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use semver::Version;
use serde::{Deserialize, Serialize};
use userfaultfd::{FeatureFlags, Uffd, UffdBuilder};
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

#[cfg(target_arch = "aarch64")]
use crate::arch::aarch64::vcpu::get_manufacturer_id_from_host;
use crate::builder::{self, BuildMicrovmFromSnapshotError};
use crate::cpu_config::templates::StaticCpuTemplate;
#[cfg(target_arch = "x86_64")]
use crate::cpu_config::x86_64::cpuid::CpuidTrait;
#[cfg(target_arch = "x86_64")]
use crate::cpu_config::x86_64::cpuid::common::get_vendor_id_from_host;
use crate::device_manager::{DevicePersistError, DevicesState};
use crate::logger::{info, warn};
use crate::resources::VmResources;
use crate::seccomp::BpfThreadMap;
use crate::snapshot::Snapshot;
use crate::utils::u64_to_usize;
use crate::vmm_config::boot_source::BootSourceConfig;
use crate::vmm_config::instance_info::InstanceInfo;
use crate::vmm_config::machine_config::{HugePageConfig, MachineConfigError, MachineConfigUpdate};
use crate::vmm_config::snapshot::{
    CreateSnapshotParams, LoadSnapshotParams, MemBackendType, SnapshotType,
};
use crate::vstate::kvm::KvmState;
use crate::vstate::memory::{
    self, GuestMemoryState, GuestRegionMmap, GuestRegionType, MemoryError,
};
use crate::vstate::vcpu::{VcpuSendEventError, VcpuState};
use crate::vstate::vm::{VmError, VmState};
use crate::{EventManager, Vmm, vstate};

/// Holds information related to the VM that is not part of VmState.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct VmInfo {
    /// Guest memory size.
    pub mem_size_mib: u64,
    /// smt information
    pub smt: bool,
    /// CPU template type
    pub cpu_template: StaticCpuTemplate,
    /// Boot source information.
    pub boot_source: BootSourceConfig,
    /// Huge page configuration
    pub huge_pages: HugePageConfig,
}

impl From<&VmResources> for VmInfo {
    fn from(value: &VmResources) -> Self {
        Self {
            mem_size_mib: value.machine_config.mem_size_mib as u64,
            smt: value.machine_config.smt,
            cpu_template: StaticCpuTemplate::from(&value.machine_config.cpu_template),
            boot_source: value.boot_source.config.clone(),
            huge_pages: value.machine_config.huge_pages,
        }
    }
}

impl From<&Vmm> for VmInfo {
    fn from(value: &Vmm) -> Self {
        let machine_config = &value.machine_config;
        Self {
            mem_size_mib: machine_config.mem_size_mib as u64,
            smt: machine_config.smt,
            cpu_template: StaticCpuTemplate::from(&machine_config.cpu_template),
            boot_source: value.boot_source_config.clone(),
            huge_pages: machine_config.huge_pages,
        }
    }
}

/// Contains the necessary state for saving/restoring a microVM.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MicrovmState {
    /// Miscellaneous VM info.
    pub vm_info: VmInfo,
    /// KVM KVM state.
    pub kvm_state: KvmState,
    /// VM KVM state.
    pub vm_state: VmState,
    /// Vcpu states.
    pub vcpu_states: Vec<VcpuState>,
    /// Device states.
    pub device_states: DevicesState,
}

/// This describes the mapping between Firecracker base virtual address and
/// offset in the buffer or file backend for a guest memory region. It is used
/// to tell an external process/thread where to populate the guest memory data
/// for this range.
///
/// E.g. Guest memory contents for a region of `size` bytes can be found in the
/// backend at `offset` bytes from the beginning, and should be copied/populated
/// into `base_host_address`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuestRegionUffdMapping {
    /// Base host virtual address where the guest memory contents for this
    /// region should be copied/populated.
    pub base_host_virt_addr: u64,
    /// Region size.
    pub size: usize,
    /// Offset in the backend file/buffer where the region contents are.
    pub offset: u64,
    /// The configured page size for this memory region.
    pub page_size: usize,
    /// The configured page size **in bytes** for this memory region. The name is
    /// wrong but cannot be changed due to being API, so this field is deprecated,
    /// to be removed in 2.0.
    #[deprecated]
    pub page_size_kib: usize,
}

/// Errors related to saving and restoring Microvm state.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum MicrovmStateError {
    /// Operation not allowed: {0}
    NotAllowed(String),
    /// Cannot restore devices: {0}
    RestoreDevices(#[from] DevicePersistError),
    /// Cannot save Vcpu state: {0}
    SaveVcpuState(vstate::vcpu::VcpuError),
    /// Cannot save KvmVm state: {0}
    SaveVmState(vstate::vm::KvmVmError),
    /// Cannot signal Vcpu: {0}
    SignalVcpu(VcpuSendEventError),
    /// Vcpu is in unexpected state.
    UnexpectedVcpuResponse,
}

/// Errors associated with creating a snapshot.
#[rustfmt::skip]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum CreateSnapshotError {
    /// Cannot get dirty bitmap: {0}
    DirtyBitmap(#[from] VmError),
    /// Cannot write memory file: {0}
    Memory(#[from] MemoryError),
    /// Cannot perform {0} on the memory backing file: {1}
    MemoryBackingFile(&'static str, io::Error),
    /// Cannot save the microVM state: {0}
    MicrovmState(MicrovmStateError),
    /// Cannot serialize the microVM state: {0}
    SerializeMicrovmState(#[from] crate::snapshot::SnapshotError),
    /// Cannot perform {0} on the snapshot backing file: {1}
    SnapshotBackingFile(&'static str, io::Error),
    /// Incremental snapshot requires a pre-existing base memory file: {0}
    BaseMemoryFileMissing(std::path::PathBuf),
    /// Base memory file {path} has size {actual}, expected {expected}
    BaseMemoryFileSizeMismatch {
        /// Path of the offending base file.
        path: std::path::PathBuf,
        /// Actual file size in bytes.
        actual: u64,
        /// Full guest memory size in bytes.
        expected: u64,
    },
    /// Invalid create-snapshot parameter combination: {0}
    InvalidParams(&'static str),
    /// pagemap-anon ledger error: {0}
    PagemapAnon(#[from] crate::vstate::pagemap_anon::PagemapAnonError),
    /// soft-dirty ledger error: {0}
    SoftDirty(#[from] crate::vstate::soft_dirty::SoftDirtyError),
    /// virtio-fs snapshot error: {0}
    VirtioFs(#[from] crate::devices::virtio::virtio_fs::VirtioFsError),
}

/// Snapshot version
// Version 11 adds the optional MMIO virtio-fs frontend state. Runtime bundles
// remain responsible for restoring snapshots with the exact compatible VMM.
pub const SNAPSHOT_VERSION: Version = Version::new(11, 0, 0);

/// Creates a Microvm snapshot.
pub fn create_snapshot(
    vmm: &mut Vmm,
    vm_info: &VmInfo,
    params: &CreateSnapshotParams,
) -> Result<(), CreateSnapshotError> {
    // Validate before any snapshot work: the API layer rejects invalid
    // field combinations early (friendlier errors), but
    // `VmmAction::CreateSnapshot` can also be constructed directly by
    // controller code and tests. A rejected request must not capture VM
    // state or touch (truncate/overwrite) the snapshot_path file first.
    let mem_file_path = match (params.state_only, params.mem_file_path.as_ref()) {
        // Explicit state-only snapshot: memory dumping is fully delegated
        // to the caller (only the state file is written).
        (true, None) if params.snapshot_type == SnapshotType::Full => None,
        (false, Some(path)) => Some(path),
        (true, None) => {
            return Err(CreateSnapshotError::InvalidParams(
                "state_only snapshots require snapshot_type Full: no memory write or ledger transition happens",
            ));
        }
        (true, Some(_)) => {
            return Err(CreateSnapshotError::InvalidParams(
                "mem_file_path must be omitted when state_only is true",
            ));
        }
        (false, None) => {
            return Err(CreateSnapshotError::InvalidParams(
                "mem_file_path is required unless state_only is true",
            ));
        }
    };

    let has_virtio_fs = vmm.has_virtio_fs();
    match (has_virtio_fs, params.fs_state_path.as_deref()) {
        (true, None) => {
            return Err(CreateSnapshotError::InvalidParams(
                "fs_state_path is required when virtio-fs is configured",
            ));
        }
        (false, Some(_)) => {
            return Err(CreateSnapshotError::InvalidParams(
                "fs_state_path was provided but no virtio-fs device is configured",
            ));
        }
        _ => {}
    }

    if let Some(path) = params.fs_state_path.as_deref() {
        vmm.prepare_virtio_fs_snapshot(path, params.deferred_sync)?;
    }
    let virtio_fs_dirty_ranges = vmm.virtio_fs_dirty_ranges();

    let snapshot_result = (|| {
        let microvm_state = vmm
            .save_state(vm_info)
            .map_err(CreateSnapshotError::MicrovmState)?;

        snapshot_state_to_file(&microvm_state, &params.snapshot_path, params.deferred_sync)?;

        let kvm_vm = if let Some(mem_file_path) = mem_file_path {
            let kvm_vm = vmm.vm.as_kvm().ok_or_else(|| {
                CreateSnapshotError::MicrovmState(MicrovmStateError::NotAllowed(
                    "snapshot requires KVM".into(),
                ))
            })?;
            kvm_vm.snapshot_memory_to_file(
                mem_file_path,
                params.snapshot_type,
                params.deferred_sync,
                &virtio_fs_dirty_ranges,
                has_virtio_fs,
            )?;
            Some(kvm_vm)
        } else {
            None
        };

        // Queue memory is modified outside the normal dirty accounting.
        if let Some(kvm_vm) = kvm_vm {
            vmm.device_manager
                .mark_virtio_queue_memory_dirty(kvm_vm.guest_memory());
        }
        Ok(())
    })();

    let finish_result = if has_virtio_fs {
        vmm.finish_virtio_fs_snapshot(snapshot_result.is_ok())
            .map_err(CreateSnapshotError::VirtioFs)
    } else {
        Ok(())
    };

    match (snapshot_result, finish_result) {
        (Err(snapshot_error), _) => Err(snapshot_error),
        (Ok(()), finish) => finish,
    }
}

fn snapshot_state_to_file(
    microvm_state: &MicrovmState,
    snapshot_path: &Path,
    defer_sync: bool,
) -> Result<(), CreateSnapshotError> {
    use self::CreateSnapshotError::*;
    let mut snapshot_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(snapshot_path)
        .map_err(|err| SnapshotBackingFile("open", err))?;

    let snapshot = Snapshot::new(microvm_state);
    snapshot.save(&mut snapshot_file)?;
    snapshot_file
        .flush()
        .map_err(|err| SnapshotBackingFile("flush", err))?;
    if defer_sync {
        // Durability is delegated to the caller (e.g. an orchestrator that
        // fsyncs the file before committing a manifest).
        return Ok(());
    }
    snapshot_file
        .sync_all()
        .map_err(|err| SnapshotBackingFile("sync_all", err))
}

/// Validates that snapshot CPU vendor matches the host CPU vendor.
///
/// # Errors
///
/// When:
/// - Failed to read host vendor.
/// - Failed to read snapshot vendor.
#[cfg(target_arch = "x86_64")]
pub fn validate_cpu_vendor(microvm_state: &MicrovmState) {
    let host_vendor_id = get_vendor_id_from_host();
    let snapshot_vendor_id = microvm_state.vcpu_states[0].cpuid.vendor_id();
    match (host_vendor_id, snapshot_vendor_id) {
        (Ok(host_id), Some(snapshot_id)) => {
            info!("Host CPU vendor ID: {host_id:?}");
            info!("Snapshot CPU vendor ID: {snapshot_id:?}");
            if host_id != snapshot_id {
                warn!("Host CPU vendor ID differs from the snapshotted one",);
            }
        }
        (Ok(host_id), None) => {
            info!("Host CPU vendor ID: {host_id:?}");
            warn!("Snapshot CPU vendor ID: couldn't get from the snapshot");
        }
        (Err(_), Some(snapshot_id)) => {
            warn!("Host CPU vendor ID: couldn't get from the host");
            info!("Snapshot CPU vendor ID: {snapshot_id:?}");
        }
        (Err(_), None) => {
            warn!("Host CPU vendor ID: couldn't get from the host");
            warn!("Snapshot CPU vendor ID: couldn't get from the snapshot");
        }
    }
}

/// Validate that Snapshot Manufacturer ID matches
/// the one from the Host
///
/// The manufacturer ID for the Snapshot is taken from each VCPU state.
/// # Errors
///
/// When:
/// - Failed to read host vendor.
/// - Failed to read snapshot vendor.
#[cfg(target_arch = "aarch64")]
pub fn validate_cpu_manufacturer_id(microvm_state: &MicrovmState) {
    let host_cpu_id = get_manufacturer_id_from_host();
    let snapshot_cpu_id = microvm_state.vcpu_states[0].regs.manifacturer_id();
    match (host_cpu_id, snapshot_cpu_id) {
        (Some(host_id), Some(snapshot_id)) => {
            info!("Host CPU manufacturer ID: {host_id:?}");
            info!("Snapshot CPU manufacturer ID: {snapshot_id:?}");
            if host_id != snapshot_id {
                warn!("Host CPU manufacturer ID differs from the snapshotted one",);
            }
        }
        (Some(host_id), None) => {
            info!("Host CPU manufacturer ID: {host_id:?}");
            warn!("Snapshot CPU manufacturer ID: couldn't get from the snapshot");
        }
        (None, Some(snapshot_id)) => {
            warn!("Host CPU manufacturer ID: couldn't get from the host");
            info!("Snapshot CPU manufacturer ID: {snapshot_id:?}");
        }
        (None, None) => {
            warn!("Host CPU manufacturer ID: couldn't get from the host");
            warn!("Snapshot CPU manufacturer ID: couldn't get from the snapshot");
        }
    }
}
/// Error type for [`snapshot_state_sanity_check`].
#[derive(Debug, thiserror::Error, displaydoc::Display, PartialEq, Eq)]
pub enum SnapShotStateSanityCheckError {
    /// No memory region defined.
    NoMemory,
    /// No DRAM memory region defined.
    NoDramMemory,
    /// DRAM memory has more than a single slot.
    DramMemoryTooManySlots,
    /// DRAM memory is unplugged.
    DramMemoryUnplugged,
}

/// Performs sanity checks against the state file and returns specific errors.
pub fn snapshot_state_sanity_check(
    microvm_state: &MicrovmState,
) -> Result<(), SnapShotStateSanityCheckError> {
    // Check that the snapshot contains at least 1 mem region, that at least one is Dram,
    // and that Dram region contains a single plugged slot.
    // Upper bound check will be done when creating guest memory by comparing against
    // KVM max supported value kvm_context.max_memslots().
    let regions = &microvm_state.vm_state.memory.regions;

    if regions.is_empty() {
        return Err(SnapShotStateSanityCheckError::NoMemory);
    }

    if !regions
        .iter()
        .any(|r| r.region_type == GuestRegionType::Dram)
    {
        return Err(SnapShotStateSanityCheckError::NoDramMemory);
    }

    for dram_region in regions
        .iter()
        .filter(|r| r.region_type == GuestRegionType::Dram)
    {
        if dram_region.plugged.len() != 1 {
            return Err(SnapShotStateSanityCheckError::DramMemoryTooManySlots);
        }

        if !dram_region.plugged[0] {
            return Err(SnapShotStateSanityCheckError::DramMemoryUnplugged);
        }
    }

    #[cfg(target_arch = "x86_64")]
    validate_cpu_vendor(microvm_state);
    #[cfg(target_arch = "aarch64")]
    validate_cpu_manufacturer_id(microvm_state);

    Ok(())
}

/// Error type for [`restore_from_snapshot`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum RestoreFromSnapshotError {
    /// Failed to get snapshot state from file: {0}
    File(#[from] SnapshotStateFromFileError),
    /// Invalid snapshot state: {0}
    Invalid(#[from] SnapShotStateSanityCheckError),
    /// Failed to load guest memory: {0}
    GuestMemory(#[from] RestoreFromSnapshotGuestMemoryError),
    /// Failed to build microVM from snapshot: {0}
    Build(#[from] BuildMicrovmFromSnapshotError),
    /// Invalid guest memory backend configuration: {0}
    InvalidMemoryBackend(&'static str),
}
/// Sub-Error type for [`restore_from_snapshot`] to contain either [`GuestMemoryFromFileError`] or
/// [`GuestMemoryFromUffdError`] within [`RestoreFromSnapshotError`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum RestoreFromSnapshotGuestMemoryError {
    /// Error creating guest memory from file: {0}
    File(#[from] GuestMemoryFromFileError),
    /// Error creating guest memory from uffd: {0}
    Uffd(#[from] GuestMemoryFromUffdError),
}

/// Loads a Microvm snapshot producing a 'paused' Microvm.
pub fn restore_from_snapshot(
    instance_info: &InstanceInfo,
    event_manager: &mut EventManager,
    seccomp_filters: &BpfThreadMap,
    params: &LoadSnapshotParams,
    vm_resources: &mut VmResources,
) -> Result<Arc<Mutex<Vmm>>, RestoreFromSnapshotError> {
    match (
        &params.mem_backend.backend_type,
        params.mem_backend.source_path.as_ref(),
    ) {
        (MemBackendType::SharedFile, None) => {
            return Err(RestoreFromSnapshotError::InvalidMemoryBackend(
                "source_path is required for SharedFile",
            ));
        }
        (MemBackendType::File | MemBackendType::Uffd, Some(_)) => {
            return Err(RestoreFromSnapshotError::InvalidMemoryBackend(
                "source_path is allowed only for SharedFile",
            ));
        }
        _ => {}
    }
    let mut microvm_state = snapshot_state_from_file(&params.snapshot_path)?;
    if microvm_state.device_states.mmio_state.fs_device.is_some()
        && params.mem_backend.backend_type != MemBackendType::SharedFile
    {
        return Err(RestoreFromSnapshotError::InvalidMemoryBackend(
            "virtio-fs snapshots require the SharedFile memory backend",
        ));
    }
    for entry in &params.network_overrides {
        microvm_state
            .device_states
            .mmio_state
            .net_devices
            .iter_mut()
            .map(|device| &mut device.device_state)
            .chain(
                microvm_state
                    .device_states
                    .pci_state
                    .net_devices
                    .iter_mut()
                    .map(|device| &mut device.device_state),
            )
            .find(|x| x.id == entry.iface_id)
            .map(|device_state| device_state.tap_if_name.clone_from(&entry.host_dev_name))
            .ok_or(SnapshotStateFromFileError::UnknownNetworkDevice)?;
    }

    if let Some(vsock_override) = &params.vsock_override {
        // There should only ever be at most one vsock device, therefore this
        // should correctly find it and modify the path if such a device exists.
        let device_state = microvm_state
            .device_states
            .mmio_state
            .vsock_device
            .as_mut()
            .map(|device| &mut device.device_state)
            .or_else(|| {
                microvm_state
                    .device_states
                    .pci_state
                    .vsock_device
                    .as_mut()
                    .map(|device| &mut device.device_state)
            })
            .ok_or(SnapshotStateFromFileError::UnknownVsockDevice)?;

        device_state
            .backend
            .uds_path
            .clone_from(&vsock_override.uds_path);
    }

    match (
        microvm_state.device_states.mmio_state.fs_device.as_mut(),
        params.fs_override.as_ref(),
    ) {
        (Some(device), Some(fs_override)) if device.device_id == fs_override.fs_id => {
            device
                .device_state
                .config
                .socket_path
                .clone_from(&fs_override.socket_path);
            device.device_state.backend_state_path = Some(fs_override.state_path.clone());
        }
        (Some(_), Some(_)) | (None, Some(_)) => {
            return Err(SnapshotStateFromFileError::UnknownFsDevice.into());
        }
        (Some(_), None) => {
            return Err(SnapshotStateFromFileError::MissingFsOverride.into());
        }
        (None, None) => {}
    }

    let track_dirty_pages = params.track_dirty_pages;

    let vcpu_count = microvm_state
        .vcpu_states
        .len()
        .try_into()
        .map_err(|_| MachineConfigError::InvalidVcpuCount)
        .map_err(BuildMicrovmFromSnapshotError::VmUpdateConfig)?;

    vm_resources
        .update_machine_config(&MachineConfigUpdate {
            vcpu_count: Some(vcpu_count),
            mem_size_mib: Some(u64_to_usize(microvm_state.vm_info.mem_size_mib)),
            smt: Some(microvm_state.vm_info.smt),
            cpu_template: Some(microvm_state.vm_info.cpu_template),
            track_dirty_pages: Some(track_dirty_pages),
            huge_pages: Some(microvm_state.vm_info.huge_pages),
            #[cfg(feature = "gdb")]
            gdb_socket_path: None,
        })
        .map_err(BuildMicrovmFromSnapshotError::VmUpdateConfig)?;

    // Some sanity checks before building the microvm.
    snapshot_state_sanity_check(&microvm_state)?;

    let mem_backend_path = &params.mem_backend.backend_path;
    let mem_state = &microvm_state.vm_state.memory;

    let (guest_memory, uffd) = match &params.mem_backend.backend_type {
        MemBackendType::File => {
            if vm_resources.machine_config.huge_pages.is_hugetlbfs() {
                return Err(RestoreFromSnapshotGuestMemoryError::File(
                    GuestMemoryFromFileError::HugetlbfsSnapshot,
                )
                .into());
            }
            (
                guest_memory_from_file(mem_backend_path, mem_state, track_dirty_pages)
                    .map_err(RestoreFromSnapshotGuestMemoryError::File)?,
                None,
            )
        }
        MemBackendType::SharedFile => {
            if vm_resources.machine_config.huge_pages.is_hugetlbfs() {
                return Err(RestoreFromSnapshotGuestMemoryError::File(
                    GuestMemoryFromFileError::HugetlbfsSnapshot,
                )
                .into());
            }
            (
                guest_memory_from_shared_file(
                    params.mem_backend.source_path.as_ref().unwrap(),
                    mem_backend_path,
                    mem_state,
                    track_dirty_pages,
                )
                .map_err(RestoreFromSnapshotGuestMemoryError::File)?,
                None,
            )
        }
        MemBackendType::Uffd => guest_memory_from_uffd(
            mem_backend_path,
            mem_state,
            track_dirty_pages,
            vm_resources.machine_config.huge_pages,
        )
        .map_err(RestoreFromSnapshotGuestMemoryError::Uffd)?,
    };
    builder::build_microvm_from_snapshot(
        instance_info,
        event_manager,
        microvm_state,
        guest_memory,
        uffd,
        seccomp_filters,
        vm_resources,
        params.clock_realtime,
    )
    .map_err(RestoreFromSnapshotError::Build)
}

/// Error type for [`snapshot_state_from_file`]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum SnapshotStateFromFileError {
    /// Failed to open snapshot file: {0}
    Open(#[from] std::io::Error),
    /// Failed to load snapshot state from file: {0}
    Load(#[from] crate::snapshot::SnapshotError),
    /// Unknown Network Device.
    UnknownNetworkDevice,
    /// Unknown Vsock Device.
    UnknownVsockDevice,
    /// Unknown virtio-fs device.
    UnknownFsDevice,
    /// Snapshot contains virtio-fs but no fs_override was supplied.
    MissingFsOverride,
}

fn snapshot_state_from_file(
    snapshot_path: &Path,
) -> Result<MicrovmState, SnapshotStateFromFileError> {
    let mut snapshot_reader = File::open(snapshot_path)?;
    let snapshot = Snapshot::load(&mut snapshot_reader)?;

    Ok(snapshot.data)
}

/// Error type for [`guest_memory_from_file`].
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GuestMemoryFromFileError {
    /// Failed to load guest memory: {0}
    File(#[from] std::io::Error),
    /// Failed to restore guest memory: {0}
    Restore(#[from] MemoryError),
    /// Cannot restore hugetlbfs backed snapshot by mapping the memory file. Please use uffd.
    HugetlbfsSnapshot,
    /// Shared live-memory target already exists: {0}
    LiveMemoryTargetExists(std::path::PathBuf),
    /// Shared-memory source is not a regular file: {0}
    SharedSourceNotRegular(std::path::PathBuf),
}

fn guest_memory_from_file(
    mem_file_path: &Path,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
) -> Result<Vec<GuestRegionMmap>, GuestMemoryFromFileError> {
    let mem_file = File::open(mem_file_path)?;
    let guest_mem = memory::snapshot_file(mem_file, mem_state.regions(), track_dirty_pages)?;
    Ok(guest_mem)
}

fn guest_memory_from_shared_file(
    source_path: &Path,
    live_path: &Path,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
) -> Result<Vec<GuestRegionMmap>, GuestMemoryFromFileError> {
    use std::os::fd::AsRawFd;

    struct RemoveOnError<'a> {
        path: &'a Path,
        keep: bool,
    }

    impl Drop for RemoveOnError<'_> {
        fn drop(&mut self) {
            if !self.keep {
                let _ = std::fs::remove_file(self.path);
            }
        }
    }

    if live_path.exists() {
        return Err(GuestMemoryFromFileError::LiveMemoryTargetExists(
            live_path.to_path_buf(),
        ));
    }

    let mut source = File::open(source_path)?;
    if !source.metadata()?.file_type().is_file() {
        return Err(GuestMemoryFromFileError::SharedSourceNotRegular(
            source_path.to_path_buf(),
        ));
    }
    let mut live = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(live_path)?;
    // create_new() establishes that this invocation owns the target. Do not
    // leave a zero-length or partially initialized live-memory file behind
    // when cloning or mapping fails.
    let mut cleanup = RemoveOnError {
        path: live_path,
        keep: false,
    };

    // Linux uapi: #define FICLONE _IOW(0x94, 9, int)
    const FICLONE: libc::c_ulong = 0x4004_9409;
    // SAFETY: both descriptors are valid regular files and the ioctl only
    // clones extents from the read-only source into the newly created target.
    let result = unsafe { libc::ioctl(live.as_raw_fd(), FICLONE as _, source.as_raw_fd()) };
    if result != 0 {
        // Reflink is the fast path, not a correctness requirement. A new
        // independent live file is still safe on filesystems without
        // FICLONE; copy the immutable source without adding an fsync to the
        // restore path.
        live.set_len(0)?;
        source.seek(SeekFrom::Start(0))?;
        live.seek(SeekFrom::Start(0))?;
        let expected = source.metadata()?.len();
        let copied = std::io::copy(&mut source, &mut live)?;
        if copied != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "checkpoint memory changed while copying",
            )
            .into());
        }
    }

    let guest_mem =
        memory::snapshot_live_file_shared(live, mem_state.regions(), track_dirty_pages)?;
    cleanup.keep = true;
    Ok(guest_mem)
}

/// Error type for [`guest_memory_from_uffd`]
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum GuestMemoryFromUffdError {
    /// Failed to restore guest memory: {0}
    Restore(#[from] MemoryError),
    /// Failed to UFFD object: {0}
    Create(userfaultfd::Error),
    /// Failed to register memory address range with the userfaultfd object: {0}
    Register(userfaultfd::Error),
    /// Failed to connect to UDS Unix stream: {0}
    Connect(#[from] std::io::Error),
    /// Failed to sends file descriptor: {0}
    Send(#[from] vmm_sys_util::errno::Error),
}

fn guest_memory_from_uffd(
    mem_uds_path: &Path,
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
    huge_pages: HugePageConfig,
) -> Result<(Vec<GuestRegionMmap>, Option<Uffd>), GuestMemoryFromUffdError> {
    let (guest_memory, backend_mappings) =
        create_guest_memory(mem_state, track_dirty_pages, huge_pages)?;

    let mut uffd_builder = UffdBuilder::new();

    // We only make use of this if balloon devices are present, but we can enable it unconditionally
    // because the only place the kernel checks this is in a hook from madvise, e.g. it doesn't
    // actively change the behavior of UFFD, only passively. Without balloon devices
    // we never call madvise anyway, so no need to put this into a conditional.
    uffd_builder.require_features(FeatureFlags::EVENT_REMOVE);

    let uffd = uffd_builder
        .close_on_exec(true)
        .non_blocking(true)
        .user_mode_only(false)
        .create()
        .map_err(GuestMemoryFromUffdError::Create)?;

    for mem_region in guest_memory.iter() {
        uffd.register(mem_region.as_ptr().cast(), mem_region.size() as _)
            .map_err(GuestMemoryFromUffdError::Register)?;
    }

    send_uffd_handshake(mem_uds_path, &backend_mappings, &uffd)?;

    Ok((guest_memory, Some(uffd)))
}

fn create_guest_memory(
    mem_state: &GuestMemoryState,
    track_dirty_pages: bool,
    huge_pages: HugePageConfig,
) -> Result<(Vec<GuestRegionMmap>, Vec<GuestRegionUffdMapping>), GuestMemoryFromUffdError> {
    let guest_memory = memory::anonymous(mem_state.regions(), track_dirty_pages, huge_pages)?;
    let mut backend_mappings = Vec::with_capacity(guest_memory.len());
    let mut offset = 0;
    for mem_region in guest_memory.iter() {
        #[allow(deprecated)]
        backend_mappings.push(GuestRegionUffdMapping {
            base_host_virt_addr: mem_region.as_ptr() as u64,
            size: mem_region.size(),
            offset,
            page_size: huge_pages.page_size(),
            page_size_kib: huge_pages.page_size(),
        });
        offset += mem_region.size() as u64;
    }

    Ok((guest_memory, backend_mappings))
}

fn send_uffd_handshake(
    mem_uds_path: &Path,
    backend_mappings: &[GuestRegionUffdMapping],
    uffd: &impl AsRawFd,
) -> Result<(), GuestMemoryFromUffdError> {
    // This is safe to unwrap() because we control the contents of the vector
    // (i.e GuestRegionUffdMapping entries).
    let backend_mappings = serde_json::to_string(backend_mappings).unwrap();

    let socket = UnixStream::connect(mem_uds_path)?;
    socket.send_with_fd(
        backend_mappings.as_bytes(),
        // In the happy case we can close the fd since the other process has it open and is
        // using it to serve us pages.
        //
        // The problem is that if other process crashes/exits, firecracker guest memory
        // will simply revert to anon-mem behavior which would lead to silent errors and
        // undefined behavior.
        //
        // To tackle this scenario, the page fault handler can notify Firecracker of any
        // crashes/exits. There is no need for Firecracker to explicitly send its process ID.
        // The external process can obtain Firecracker's PID by calling `getsockopt` with
        // `libc::SO_PEERCRED` option like so:
        //
        // let mut val = libc::ucred { pid: 0, gid: 0, uid: 0 };
        // let mut ucred_size: u32 = mem::size_of::<libc::ucred>() as u32;
        // libc::getsockopt(
        //      socket.as_raw_fd(),
        //      libc::SOL_SOCKET,
        //      libc::SO_PEERCRED,
        //      &mut val as *mut _ as *mut _,
        //      &mut ucred_size as *mut libc::socklen_t,
        // );
        //
        // Per this linux man page: https://man7.org/linux/man-pages/man7/unix.7.html,
        // `SO_PEERCRED` returns the credentials (PID, UID and GID) of the peer process
        // connected to this socket. The returned credentials are those that were in effect
        // at the time of the `connect` call.
        //
        // Moreover, Firecracker holds a copy of the UFFD fd as well, so that even if the
        // page fault handler process does not tear down Firecracker when necessary, the
        // uffd will still be alive but with no one to serve faults, leading to guest freeze.
        uffd.as_raw_fd(),
    )?;

    // We prevent Rust from closing the socket file descriptor to avoid a potential race condition
    // between the mappings message and the connection shutdown. If the latter arrives at the UFFD
    // handler first, the handler never sees the mappings.
    forget(socket);

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    use vm_memory::{Bytes, MemoryRegionAddress};
    use vmm_sys_util::tempfile::TempFile;

    use super::*;
    use crate::Vmm;
    #[cfg(target_arch = "x86_64")]
    use crate::builder::tests::insert_vmclock_device;
    #[cfg(target_arch = "x86_64")]
    use crate::builder::tests::insert_vmgenid_device;
    use crate::builder::tests::{
        CustomBlockConfig, default_kernel_cmdline, default_vmm, insert_balloon_device,
        insert_block_devices, insert_net_device, insert_vsock_device,
    };
    #[cfg(target_arch = "aarch64")]
    use crate::construct_kvm_mpidrs;
    use crate::devices::virtio::block::CacheType;
    use crate::snapshot::Persist;
    use crate::vmm_config::balloon::BalloonDeviceConfig;
    use crate::vmm_config::net::NetworkInterfaceConfig;
    use crate::vmm_config::vsock::tests::default_config;
    use crate::vstate::memory::{GuestMemoryRegionState, GuestRegionType};

    fn default_vmm_with_devices() -> Vmm {
        let mut event_manager = EventManager::new().expect("Cannot create EventManager");
        let mut vmm = default_vmm();
        let mut cmdline = default_kernel_cmdline();

        // Add a balloon device.
        let balloon_config = BalloonDeviceConfig {
            amount_mib: 0,
            deflate_on_oom: false,
            stats_polling_interval_s: 0,
            free_page_hinting: false,
            free_page_reporting: false,
        };
        insert_balloon_device(&mut vmm, &mut cmdline, &mut event_manager, balloon_config);

        // Add a block device.
        let drive_id = String::from("root");
        let block_configs = vec![CustomBlockConfig::new(
            drive_id,
            true,
            None,
            true,
            CacheType::Unsafe,
        )];
        insert_block_devices(&mut vmm, &mut cmdline, &mut event_manager, block_configs);

        // Add net device.
        let network_interface = NetworkInterfaceConfig {
            iface_id: String::from("netif"),
            host_dev_name: String::from("hostname"),
            guest_mac: None,
            mtu: None,
            rx_rate_limiter: None,
            tx_rate_limiter: None,
        };
        insert_net_device(
            &mut vmm,
            &mut cmdline,
            &mut event_manager,
            network_interface,
        );

        // Add vsock device.
        let mut tmp_sock_file = TempFile::new().unwrap();
        tmp_sock_file.remove().unwrap();
        let vsock_config = default_config(&tmp_sock_file);

        insert_vsock_device(&mut vmm, &mut cmdline, &mut event_manager, vsock_config);

        #[cfg(target_arch = "x86_64")]
        insert_vmgenid_device(&mut vmm);
        #[cfg(target_arch = "x86_64")]
        insert_vmclock_device(&mut vmm);

        vmm
    }

    #[test]
    fn test_microvm_state_snapshot() {
        let vmm = default_vmm_with_devices();
        let states = vmm.device_manager.save();

        // Only checking that all devices are saved, actual device state
        // is tested by that device's tests.
        assert_eq!(states.mmio_state.block_devices.len(), 1);
        assert_eq!(states.mmio_state.net_devices.len(), 1);
        assert!(states.mmio_state.vsock_device.is_some());
        assert!(states.mmio_state.balloon_device.is_some());

        let vcpu_states = vec![VcpuState::default()];
        #[cfg(target_arch = "aarch64")]
        let mpidrs = construct_kvm_mpidrs(&vcpu_states);
        let microvm_state = MicrovmState {
            device_states: states,
            vcpu_states,
            kvm_state: Default::default(),
            vm_info: VmInfo {
                mem_size_mib: 1u64,
                ..Default::default()
            },
            #[cfg(target_arch = "aarch64")]
            vm_state: vmm.vm.as_kvm().unwrap().save_state(&mpidrs).unwrap(),
            #[cfg(target_arch = "x86_64")]
            vm_state: vmm.vm.as_kvm().unwrap().save_state().unwrap(),
        };

        let serialized_data = bitcode::serialize(&microvm_state).unwrap();

        let restored_microvm_state: MicrovmState = bitcode::deserialize(&serialized_data).unwrap();

        assert_eq!(restored_microvm_state.vm_info, microvm_state.vm_info);
        assert_eq!(
            restored_microvm_state.device_states.mmio_state,
            microvm_state.device_states.mmio_state
        )
    }

    #[test]
    fn test_create_guest_memory() {
        let mem_state = GuestMemoryState {
            regions: vec![GuestMemoryRegionState {
                base_address: 0,
                size: 0x20000,
                region_type: GuestRegionType::Dram,
                plugged: vec![true],
            }],
        };

        let (_, uffd_regions) =
            create_guest_memory(&mem_state, false, HugePageConfig::None).unwrap();

        assert_eq!(uffd_regions.len(), 1);
        assert_eq!(uffd_regions[0].size, 0x20000);
        assert_eq!(uffd_regions[0].offset, 0);
        assert_eq!(uffd_regions[0].page_size, HugePageConfig::None.page_size());
    }

    fn single_region_memory_state(size: usize) -> GuestMemoryState {
        GuestMemoryState {
            regions: vec![GuestMemoryRegionState {
                base_address: 0,
                size,
                region_type: GuestRegionType::Dram,
                plugged: vec![true],
            }],
        }
    }

    #[test]
    fn test_guest_memory_from_shared_file_isolates_checkpoint() {
        let source = TempFile::new().unwrap();
        source.as_file().set_len(4096).unwrap();
        let mut source_writer = source.as_file();
        source_writer.write_all(b"checkpoint").unwrap();

        let mut live = TempFile::new().unwrap();
        let live_path = live.as_path().to_path_buf();
        live.remove().unwrap();

        let regions = guest_memory_from_shared_file(
            source.as_path(),
            &live_path,
            &single_region_memory_state(4096),
            true,
        )
        .unwrap();
        regions[0]
            .write_slice(b"live-memory", MemoryRegionAddress(0))
            .unwrap();

        let mut live_bytes = [0_u8; 11];
        File::open(&live_path)
            .unwrap()
            .read_exact(&mut live_bytes)
            .unwrap();
        assert_eq!(&live_bytes, b"live-memory");

        let mut source_bytes = [0_u8; 10];
        File::open(source.as_path())
            .unwrap()
            .read_exact(&mut source_bytes)
            .unwrap();
        assert_eq!(&source_bytes, b"checkpoint");
    }

    #[test]
    fn test_guest_memory_from_shared_file_rejects_existing_target() {
        let source = TempFile::new().unwrap();
        source.as_file().set_len(4096).unwrap();
        let live = TempFile::new().unwrap();

        let err = guest_memory_from_shared_file(
            source.as_path(),
            live.as_path(),
            &single_region_memory_state(4096),
            false,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GuestMemoryFromFileError::LiveMemoryTargetExists(path)
                if path == live.as_path()
        ));
    }

    #[test]
    fn test_guest_memory_from_shared_file_removes_invalid_target() {
        let source = TempFile::new().unwrap();
        source.as_file().set_len(1).unwrap();
        let mut live = TempFile::new().unwrap();
        let live_path = live.as_path().to_path_buf();
        live.remove().unwrap();

        let err = guest_memory_from_shared_file(
            source.as_path(),
            &live_path,
            &single_region_memory_state(4096),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, GuestMemoryFromFileError::Restore(_)));
        assert!(!live_path.exists());
    }

    #[test]
    fn test_guest_memory_from_shared_file_rejects_non_regular_source() {
        let mut live = TempFile::new().unwrap();
        let live_path = live.as_path().to_path_buf();
        live.remove().unwrap();

        let source_path = std::env::temp_dir();
        let err = guest_memory_from_shared_file(
            &source_path,
            &live_path,
            &single_region_memory_state(4096),
            false,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            GuestMemoryFromFileError::SharedSourceNotRegular(path)
                if path == source_path
        ));
        assert!(!live_path.exists());
    }

    #[test]
    fn test_send_uffd_handshake() {
        #[allow(deprecated)]
        let uffd_regions = vec![
            GuestRegionUffdMapping {
                base_host_virt_addr: 0,
                size: 0x100000,
                offset: 0,
                page_size: HugePageConfig::None.page_size(),
                page_size_kib: HugePageConfig::None.page_size(),
            },
            GuestRegionUffdMapping {
                base_host_virt_addr: 0x100000,
                size: 0x200000,
                offset: 0,
                page_size: HugePageConfig::Hugetlbfs2M.page_size(),
                page_size_kib: HugePageConfig::Hugetlbfs2M.page_size(),
            },
        ];

        let uds_path = TempFile::new().unwrap();
        let uds_path = uds_path.as_path();
        std::fs::remove_file(uds_path).unwrap();

        let listener = UnixListener::bind(uds_path).expect("Cannot bind to socket path");

        send_uffd_handshake(uds_path, &uffd_regions, &std::io::stdin()).unwrap();

        let (stream, _) = listener.accept().expect("Cannot listen on UDS socket");

        let mut message_buf = vec![0u8; 1024];
        let (bytes_read, _) = stream
            .recv_with_fd(&mut message_buf[..])
            .expect("Cannot recv_with_fd");
        message_buf.resize(bytes_read, 0);

        let deserialized: Vec<GuestRegionUffdMapping> =
            serde_json::from_slice(&message_buf).unwrap();

        assert_eq!(uffd_regions, deserialized);
    }
}
