// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0

use vmm_sys_util::eventfd::EventFd;
#[cfg(feature = "pci_support")]
mod pci_common_config;
#[cfg(feature = "pci_support")]
mod pci_device;
#[cfg(feature = "pci_support")]
pub use pci_common_config::VirtioPciCommonConfig;
#[cfg(feature = "pci_support")]
pub use pci_device::{VirtioPciDevice, VirtioPciDeviceActivator};

pub trait VirtioTransport {
    fn ioeventfds(&self, base_addr: u64) -> Vec<(&EventFd, u64)>;
}
