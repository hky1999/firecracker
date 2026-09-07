# AKernel Full and SoftDirty checkpoint windows

The AKernel fork's successful `Full` memory snapshot establishes a new soft-dirty window when tracking is available. A previously armed window is reopened at this Full snapshot, so subsequent `SoftDirty` requests can write changes since that image instead of another cumulative anonymous-memory baseline. The guest must remain paused while the image is written and the window is reset, as required by the snapshot operation.

The caller must prepare each incremental target from the matching successful baseline. After Full, use that Full image as the baseline; an older image is no longer the base for the new window. AKernel sandboxd adopts a sealed Full image for a continuing sandbox. If sealing fails after the VMM advanced its window, the orchestrator must invalidate lineage and request another Full rather than patching an older base.

Window setup follows the memory write, flush, and required synchronization. The existing `deferred_sync` contract still delegates durability to the caller. A failed Full write or required synchronization does not reset the window. If tracking cannot be established, Full remains valid, the failed window is disarmed, and a later incremental request must use the existing cumulative baseline or safe failure/fallback behavior. This does not make an empty or unverified incremental base valid.

No memory image format changes are introduced by window setup. It does not make snapshot data durable earlier or replace the orchestrator's artifact validation and commit requirements.

### Experimental incremental content filtering

Firecracker `skip_unchanged=true` is an opt-in for Incremental and SoftDirty memory snapshots only. The caller still supplies a complete, correctly sized base from the latest valid generation. The VMM must be paused, and the caller must keep the base immutable except for this snapshot operation. The target must be readable as well as writable. A fixed 256KiB comparison buffer compares current guest bytes with the target; equal 4KiB granules are skipped, and adjacent changed granules are written together. All bytes in the resulting image retain the ordinary incremental semantics, including true zero holes. Read failures are snapshot failures, never permission to skip data.

This option does not change flush/fsync, acknowledgement, or failed-generation lineage invalidation requirements. Full, Diff and state-only requests with the option are rejected. It is disabled by default because reading a cold base can add I/O and regress workloads where most bytes change. Diagnostic output records compared, written and skipped bytes. In sandboxd, `plugin.runtime.firecracker.skip_unchanged` enables the option only when the selected tier is Incremental or SoftDirty, and requires a matching experimental FC binary; Full fallback remains unchanged.

The release VMM seccomp policy must permit `pread64` for this option. Debug builds with an empty policy do not validate this requirement. Policy coverage includes compiling the real JSON and executing a pread under its installed BPF; do not disable seccomp to enable incremental content filtering.
