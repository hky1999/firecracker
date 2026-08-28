// Copyright 2020 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

/// Module with the implementation of a Bus that can hold devices.
pub mod bus;
/// VM interrupts implementation.
pub mod interrupts;
/// Module with Kvm implementation.
pub mod kvm;
/// Module with GuestMemory implementation.
pub mod memory;
/// Pagemap-anon (KPF_ANON) incremental snapshot ledger.
pub mod pagemap_anon;
/// Resource manager for devices.
pub mod resources;
/// Soft-dirty (pagemap bit 55) incremental snapshot ledger.
pub mod soft_dirty;
/// Module with Vcpu implementation.
pub mod vcpu;
/// Module with Vm implementation.
pub mod vm;
