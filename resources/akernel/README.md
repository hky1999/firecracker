# AKernel Firecracker runtime bundle

This directory contains the AKernel guest-kernel customization and release
inputs. The Firecracker VMM is not modified: releases verify and reuse the
official upstream binary byte-for-byte. Only the guest kernel is built here.

`runtime-versions.env` is the single source of truth for the upstream VMM and
Amazon Linux kernel inputs. `kernel/akernel.config` is appended to
Firecracker's upstream 6.1 guest configuration before `make olddefconfig`.

Candidate and promotion workflows deliberately form a two-step release:

1. `AKernel Firecracker candidate` builds and uploads an expiring artifact.
2. The candidate is tested with sandboxd and AKernel.
3. `Promote AKernel Firecracker candidate` publishes those exact bytes without
   rebuilding them.

Release tags use `vX.Y.Z-akernel.N`. A release archive contains the verified
official VMM, the AKernel guest kernel, the resolved kernel configuration,
licenses, checksums, and a provenance manifest. The sandboxd-coupled guest
agent and initrd are intentionally excluded.
