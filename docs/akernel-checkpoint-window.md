# AKernel Full and SoftDirty checkpoint windows

The AKernel fork's successful `Full` memory snapshot establishes a new soft-dirty window when tracking is available. A previously armed window is reopened at this Full snapshot, so subsequent `SoftDirty` requests can write changes since that image instead of another cumulative anonymous-memory baseline. The guest must remain paused while the image is written and the window is reset, as required by the snapshot operation.

The caller must prepare each incremental target from the matching successful baseline. After Full, use that Full image as the baseline; an older image is no longer the base for the new window. AKernel sandboxd adopts a sealed Full image for a continuing sandbox. If sealing fails after the VMM advanced its window, the orchestrator must invalidate lineage and request another Full rather than patching an older base.

Window setup follows the memory write, flush, and required synchronization. The existing `deferred_sync` contract still delegates durability to the caller. A failed Full write or required synchronization does not reset the window. If tracking cannot be established, Full remains valid, the failed window is disarmed, and a later incremental request must use the existing cumulative baseline or safe failure/fallback behavior. This does not make an empty or unverified incremental base valid.

No memory image format changes are introduced by window setup. It does not make snapshot data durable earlier or replace the orchestrator's artifact validation and commit requirements.
