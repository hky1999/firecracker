# AKernel Firecracker runtime bundle

This directory contains the AKernel Firecracker release inputs: the fork built
VMM, the customized guest kernel, and the packaging script that assembles them
into a checksum-pinned runtime bundle.

The VMM is built from this repository (`build-runtime-bundle.sh` runs
`cargo build --release --target x86_64-unknown-linux-musl -p firecracker`),
because the AKernel snapshot APIs (`snapshot_type`, `deferred_sync`,
`mem_backend: Uffd`) only exist in this fork. The bundle manifest records
`vmm.source: fork-source-build`, the exact source commit, and the binary digest.
Only the guest kernel is built separately from the Amazon Linux sources.

`runtime-versions.env` is the single source of truth for the upstream VMM
version and the Amazon Linux kernel inputs. `kernel/akernel.config` is appended
to Firecracker's upstream 6.1 guest configuration before `make olddefconfig`.

Candidate and promotion workflows deliberately form a two-step release:

1. `AKernel Firecracker candidate` builds and uploads an expiring artifact.
1. The candidate is tested with sandboxd and AKernel.
1. `Promote AKernel Firecracker candidate` publishes those exact bytes without
   rebuilding them.

Release tags use `vX.Y.Z-akernel.N`. A release archive contains the fork built
VMM, the AKernel guest kernel, the resolved kernel configuration, licenses,
checksums, and a provenance manifest. The sandboxd-coupled guest agent and
initrd are intentionally excluded.
