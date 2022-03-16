// Copyright © 2020, Oracle and/or its affiliates.
//
// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause
//

#[cfg(any(target_arch = "aarch64", feature = "acpi"))]
use crate::config::NumaConfig;
use crate::config::{
    DeviceConfig, DiskConfig, FsConfig, HotplugMethod, NetConfig, PmemConfig, UserDeviceConfig,
    ValidationError, VmConfig, VsockConfig,
};
use crate::cpu;
use crate::device_manager::{self, Console, DeviceManager, DeviceManagerError, PtyPair};
use crate::device_tree::DeviceTree;
use crate::memory_manager::{
    Error as MemoryManagerError, MemoryManager, MemoryManagerSnapshotData,
};
use crate::migration::{get_vm_snapshot, url_to_path, VM_SNAPSHOT_FILE};
use crate::seccomp_filters::{get_seccomp_filter, Thread};
use crate::GuestMemoryMmap;
use crate::{
    PciDeviceInfo, CPU_MANAGER_SNAPSHOT_ID, DEVICE_MANAGER_SNAPSHOT_ID, MEMORY_MANAGER_SNAPSHOT_ID,
};
use anyhow::anyhow;
use arch::PAGE_SIZE;
use arch::get_host_cpu_phys_bits;
#[cfg(target_arch = "x86_64")]
use arch::layout::{KVM_IDENTITY_MAP_START, KVM_TSS_START};
#[cfg(all(feature = "tdx", feature = "acpi"))]
use arch::x86_64::tdx::TdVmmDataRegionType;
#[cfg(feature = "tdx")]
use arch::x86_64::tdx::{TdVmmDataRegion, TdvfSection};
use arch::EntryPoint;
#[cfg(any(target_arch = "aarch64", feature = "acpi"))]
use arch::{NumaNode, NumaNodes};
use devices::AcpiNotificationFlags;
use hypervisor::vm::{HypervisorVmError, VmmOps};
use linux_loader::cmdline::Cmdline;
#[cfg(target_arch = "x86_64")]
use linux_loader::loader::elf::PvhBootCapability::PvhEntryPresent;
#[cfg(target_arch = "aarch64")]
use linux_loader::loader::pe::Error::InvalidImageMagicNumber;
use linux_loader::loader::KernelLoader;
use seccompiler::{apply_filter, SeccompAction};
use signal_hook::{
    consts::{SIGINT, SIGTERM, SIGWINCH},
    iterator::backend::Handle,
    iterator::Signals,
};
use std::cmp;
#[cfg(any(target_arch = "aarch64", feature = "acpi"))]
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::convert::TryInto;
#[cfg(target_arch = "x86_64")]
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::io::{Seek, SeekFrom};
use std::num::Wrapping;
use std::ops::Deref;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, RwLock};
use std::{result, str, thread};
use vm_device::Bus;
#[cfg(all(target_arch = "x86_64", feature = "pci_support"))]
use vm_device::BusDevice;
use vm_memory::{Address, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic};
#[cfg(feature = "tdx")]
use vm_memory::{GuestMemory, GuestMemoryRegion};
use vm_migration::{
    protocol::MemoryRangeTable, Migratable, MigratableError, Pausable, Snapshot,
    SnapshotDataSection, Snapshottable, Transportable,
};
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::signal::unblock_signal;
use vmm_sys_util::terminal::Terminal;

#[cfg(target_arch = "aarch64")]
use arch::aarch64::gic::gicv3_its::kvm::{KvmGicV3Its, GIC_V3_ITS_SNAPSHOT_ID};
#[cfg(target_arch = "aarch64")]
use arch::aarch64::gic::kvm::create_gic;
#[cfg(target_arch = "aarch64")]
use devices::interrupt_controller::{self, InterruptController};

/// Errors associated with VM management
#[derive(Debug)]
pub enum Error {
    /// Cannot open the kernel image
    KernelFile(io::Error),

    /// Cannot open the initramfs image
    InitramfsFile(io::Error),

    /// Cannot load the kernel in memory
    KernelLoad(linux_loader::loader::Error),

    #[cfg(target_arch = "aarch64")]
    /// Cannot load the UEFI binary in memory
    UefiLoad(arch::aarch64::uefi::Error),

    /// Cannot load the initramfs in memory
    InitramfsLoad,

    /// Cannot load the command line in memory
    LoadCmdLine(linux_loader::loader::Error),

    /// Cannot modify the command line
    CmdLineInsertStr(linux_loader::cmdline::Error),

    /// Cannot configure system
    ConfigureSystem(arch::Error),

    /// Cannot enable interrupt controller
    #[cfg(target_arch = "aarch64")]
    EnableInterruptController(interrupt_controller::Error),

    PoisonedState,

    /// Cannot create a device manager.
    DeviceManager(DeviceManagerError),

    /// Write to the console failed.
    Console(vmm_sys_util::errno::Error),

    /// Write to the pty console failed.
    PtyConsole(io::Error),

    /// Cannot setup terminal in raw mode.
    SetTerminalRaw(vmm_sys_util::errno::Error),

    /// Cannot setup terminal in canonical mode.
    SetTerminalCanon(vmm_sys_util::errno::Error),

    /// Memory is overflow
    MemOverflow,

    /// Cannot spawn a signal handler thread
    SignalHandlerSpawn(io::Error),

    /// Failed to join on vCPU threads
    ThreadCleanup(std::boxed::Box<dyn std::any::Any + std::marker::Send>),

    /// VM config is missing.
    VmMissingConfig,

    /// VM is not created
    VmNotCreated,

    /// VM is already created
    VmAlreadyCreated,

    /// VM is not running
    VmNotRunning,

    /// Cannot clone EventFd.
    EventFdClone(io::Error),

    /// Invalid VM state transition
    InvalidStateTransition(VmState, VmState),

    /// Error from CPU handling
    CpuManager(cpu::Error),

    /// Cannot pause devices
    PauseDevices(MigratableError),

    /// Cannot resume devices
    ResumeDevices(MigratableError),

    /// Cannot pause CPUs
    PauseCpus(MigratableError),

    /// Cannot resume cpus
    ResumeCpus(MigratableError),

    /// Cannot pause VM
    Pause(MigratableError),

    /// Cannot resume VM
    Resume(MigratableError),

    /// Memory manager error
    MemoryManager(MemoryManagerError),

    /// Eventfd write error
    EventfdError(std::io::Error),

    /// Cannot snapshot VM
    Snapshot(MigratableError),

    /// Cannot restore VM
    Restore(MigratableError),

    /// Cannot send VM snapshot
    SnapshotSend(MigratableError),

    /// Cannot convert source URL from Path into &str
    RestoreSourceUrlPathToStr,

    /// Failed to validate config
    ConfigValidation(ValidationError),

    /// No more that one virtio-vsock device
    TooManyVsockDevices,

    /// Failed serializing into JSON
    SerializeJson(serde_json::Error),

    /// Invalid configuration for NUMA.
    InvalidNumaConfig,

    /// Cannot create seccomp filter
    CreateSeccompFilter(seccompiler::Error),

    /// Cannot apply seccomp filter
    ApplySeccompFilter(seccompiler::Error),

    /// Failed resizing a memory zone.
    ResizeZone,

    /// Cannot activate virtio devices
    ActivateVirtioDevices(device_manager::DeviceManagerError),

    /// Power button not supported
    PowerButtonNotSupported,

    /// Error triggering power button
    PowerButton(device_manager::DeviceManagerError),

    /// Kernel lacks PVH header
    KernelMissingPvhHeader,

    /// Failed to allocate firmware RAM
    AllocateFirmwareMemory(MemoryManagerError),

    /// Error manipulating firmware file
    FirmwareFile(std::io::Error),

    /// Firmware too big
    FirmwareTooLarge,

    // Failed to copy to memory
    FirmwareLoad(vm_memory::GuestMemoryError),

    /// Error doing I/O on TDX firmware file
    #[cfg(feature = "tdx")]
    LoadTdvf(std::io::Error),

    /// Error parsing TDVF
    #[cfg(feature = "tdx")]
    ParseTdvf(arch::x86_64::tdx::TdvfError),

    /// Error populating HOB
    #[cfg(feature = "tdx")]
    PopulateHob(arch::x86_64::tdx::TdvfError),

    /// Error allocating TDVF memory
    #[cfg(feature = "tdx")]
    AllocatingTdvfMemory(crate::memory_manager::Error),

    /// Error enabling TDX VM
    #[cfg(feature = "tdx")]
    InitializeTdxVm(hypervisor::HypervisorVmError),

    /// Error enabling TDX memory region
    #[cfg(feature = "tdx")]
    InitializeTdxMemoryRegion(hypervisor::HypervisorVmError),

    /// Error finalizing TDX setup
    #[cfg(feature = "tdx")]
    FinalizeTdx(hypervisor::HypervisorVmError),

    /// No PCI support
    NoPciSupport,
}
pub type Result<T> = result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
pub enum VmState {
    Created,
    Running,
    Shutdown,
    Paused,
}

impl VmState {
    fn valid_transition(self, new_state: VmState) -> Result<()> {
        match self {
            VmState::Created => match new_state {
                VmState::Created | VmState::Shutdown => {
                    Err(Error::InvalidStateTransition(self, new_state))
                }
                VmState::Running | VmState::Paused => Ok(()),
            },

            VmState::Running => match new_state {
                VmState::Created | VmState::Running => {
                    Err(Error::InvalidStateTransition(self, new_state))
                }
                VmState::Paused | VmState::Shutdown => Ok(()),
            },

            VmState::Shutdown => match new_state {
                VmState::Paused | VmState::Created | VmState::Shutdown => {
                    Err(Error::InvalidStateTransition(self, new_state))
                }
                VmState::Running => Ok(()),
            },

            VmState::Paused => match new_state {
                VmState::Created | VmState::Paused => {
                    Err(Error::InvalidStateTransition(self, new_state))
                }
                VmState::Running | VmState::Shutdown => Ok(()),
            },
        }
    }
}

// Debug I/O port
#[cfg(target_arch = "x86_64")]
const DEBUG_IOPORT: u16 = 0x80;
#[cfg(target_arch = "x86_64")]
const DEBUG_IOPORT_PREFIX: &str = "Debug I/O port";

#[cfg(target_arch = "x86_64")]
/// Debug I/O port, see:
/// https://www.intel.com/content/www/us/en/support/articles/000005500/boards-and-kits.html
///
/// Since we're not a physical platform, we can freely assign code ranges for
/// debugging specific parts of our virtual platform.
pub enum DebugIoPortRange {
    Firmware,
    Bootloader,
    Kernel,
    Userspace,
    Custom,
}
#[cfg(target_arch = "x86_64")]
impl DebugIoPortRange {
    fn from_u8(value: u8) -> DebugIoPortRange {
        match value {
            0x00..=0x1f => DebugIoPortRange::Firmware,
            0x20..=0x3f => DebugIoPortRange::Bootloader,
            0x40..=0x5f => DebugIoPortRange::Kernel,
            0x60..=0x7f => DebugIoPortRange::Userspace,
            _ => DebugIoPortRange::Custom,
        }
    }
}

#[cfg(target_arch = "x86_64")]
impl fmt::Display for DebugIoPortRange {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            DebugIoPortRange::Firmware => write!(f, "{}: Firmware", DEBUG_IOPORT_PREFIX),
            DebugIoPortRange::Bootloader => write!(f, "{}: Bootloader", DEBUG_IOPORT_PREFIX),
            DebugIoPortRange::Kernel => write!(f, "{}: Kernel", DEBUG_IOPORT_PREFIX),
            DebugIoPortRange::Userspace => write!(f, "{}: Userspace", DEBUG_IOPORT_PREFIX),
            DebugIoPortRange::Custom => write!(f, "{}: Custom", DEBUG_IOPORT_PREFIX),
        }
    }
}

struct VmOps {
    memory: GuestMemoryAtomic<GuestMemoryMmap>,
    #[cfg(target_arch = "x86_64")]
    io_bus: Arc<Bus>,
    mmio_bus: Arc<Bus>,
    #[cfg(target_arch = "x86_64")]
    timestamp: std::time::Instant,
    #[cfg(all(target_arch = "x86_64", feature = "pci_support"))]
    pci_config_io: Arc<Mutex<dyn BusDevice>>,
}

impl VmOps {
    #[cfg(target_arch = "x86_64")]
    // Log debug io port codes.
    fn log_debug_ioport(&self, code: u8) {
        let elapsed = self.timestamp.elapsed();

        info!(
            "[{} code 0x{:x}] {}.{:>06} seconds",
            DebugIoPortRange::from_u8(code),
            code,
            elapsed.as_secs(),
            elapsed.as_micros()
        );
    }
}

impl VmmOps for VmOps {
    fn guest_mem_write(&self, gpa: u64, buf: &[u8]) -> hypervisor::vm::Result<usize> {
        self.memory
            .memory()
            .write(buf, GuestAddress(gpa))
            .map_err(|e| HypervisorVmError::GuestMemWrite(e.into()))
    }

    fn guest_mem_read(&self, gpa: u64, buf: &mut [u8]) -> hypervisor::vm::Result<usize> {
        self.memory
            .memory()
            .read(buf, GuestAddress(gpa))
            .map_err(|e| HypervisorVmError::GuestMemRead(e.into()))
    }

    fn mmio_read(&self, gpa: u64, data: &mut [u8]) -> hypervisor::vm::Result<()> {
        if let Err(vm_device::BusError::MissingAddressRange) = self.mmio_bus.read(gpa, data) {
            warn!("Guest MMIO read to unregistered address 0x{:x}", gpa);
        }
        Ok(())
    }

    fn mmio_write(&self, gpa: u64, data: &[u8]) -> hypervisor::vm::Result<()> {
        match self.mmio_bus.write(gpa, data) {
            Err(vm_device::BusError::MissingAddressRange) => {
                warn!("Guest MMIO write to unregistered address 0x{:x}", gpa);
            }
            Ok(Some(barrier)) => {
                info!("Waiting for barrier");
                barrier.wait();
                info!("Barrier released");
            }
            _ => {}
        };
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn pio_read(&self, port: u64, data: &mut [u8]) -> hypervisor::vm::Result<()> {
        #[cfg(feature = "pci_support")]
        {
            use pci::{PCI_CONFIG_IO_PORT, PCI_CONFIG_IO_PORT_SIZE};

            if (PCI_CONFIG_IO_PORT..(PCI_CONFIG_IO_PORT + PCI_CONFIG_IO_PORT_SIZE)).contains(&port)
            {
                self.pci_config_io.lock().unwrap().read(
                    PCI_CONFIG_IO_PORT,
                    port - PCI_CONFIG_IO_PORT,
                    data,
                );
                return Ok(());
            }
        }

        if let Err(vm_device::BusError::MissingAddressRange) = self.io_bus.read(port, data) {
            warn!("Guest PIO read to unregistered address 0x{:x}", port);
        }
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn pio_write(&self, port: u64, data: &[u8]) -> hypervisor::vm::Result<()> {
        if port == DEBUG_IOPORT as u64 && data.len() == 1 {
            self.log_debug_ioport(data[0]);
            return Ok(());
        }

        #[cfg(feature = "pci_support")]
        {
            use pci::{PCI_CONFIG_IO_PORT, PCI_CONFIG_IO_PORT_SIZE};
            if (PCI_CONFIG_IO_PORT..(PCI_CONFIG_IO_PORT + PCI_CONFIG_IO_PORT_SIZE)).contains(&port)
            {
                self.pci_config_io.lock().unwrap().write(
                    PCI_CONFIG_IO_PORT,
                    port - PCI_CONFIG_IO_PORT,
                    data,
                );
                return Ok(());
            }
        }

        match self.io_bus.write(port, data) {
            Err(vm_device::BusError::MissingAddressRange) => {
                warn!("Guest PIO write to unregistered address 0x{:x}", port);
            }
            Ok(Some(barrier)) => {
                info!("Waiting for barrier");
                barrier.wait();
                info!("Barrier released");
            }
            _ => {}
        };
        Ok(())
    }
}

pub fn physical_bits(max_phys_bits: u8) -> u8 {
    let host_phys_bits = get_host_cpu_phys_bits();

    cmp::min(host_phys_bits, max_phys_bits)
}

pub const HANDLED_SIGNALS: [i32; 3] = [SIGWINCH, SIGTERM, SIGINT];

pub struct Vm {
    kernel: Option<File>,
    initramfs: Option<File>,
    threads: Vec<thread::JoinHandle<()>>,
    device_manager: Arc<Mutex<DeviceManager>>,
    config: Arc<Mutex<VmConfig>>,
    on_tty: bool,
    signals: Option<Handle>,
    state: RwLock<VmState>,
    cpu_manager: Arc<Mutex<cpu::CpuManager>>,
    memory_manager: Arc<Mutex<MemoryManager>>,
    #[cfg_attr(not(feature = "kvm"), allow(dead_code))]
    // The hypervisor abstracted virtual machine.
    vm: Arc<dyn hypervisor::Vm>,
    #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
    saved_clock: Option<hypervisor::ClockData>,
    #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
    numa_nodes: NumaNodes,
    seccomp_action: SeccompAction,
    exit_evt: EventFd,
    #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
    hypervisor: Arc<dyn hypervisor::Hypervisor>,
}

impl Vm {
    #[allow(clippy::too_many_arguments)]
    fn new_from_memory_manager(
        config: Arc<Mutex<VmConfig>>,
        memory_manager: Arc<Mutex<MemoryManager>>,
        vm: Arc<dyn hypervisor::Vm>,
        exit_evt: EventFd,
        reset_evt: EventFd,
        seccomp_action: &SeccompAction,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
        #[cfg(all(feature = "kvm", target_arch = "x86_64"))] _saved_clock: Option<
            hypervisor::ClockData,
        >,
        activate_evt: EventFd,
        restoring: bool,
    ) -> Result<Self> {
        config
            .lock()
            .unwrap()
            .validate()
            .map_err(Error::ConfigValidation)?;

        info!("Booting VM from config: {:?}", &config);

        // Create NUMA nodes based on NumaConfig.
        #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
        let numa_nodes =
            Self::create_numa_nodes(config.lock().unwrap().numa.clone(), &memory_manager)?;

        #[cfg(feature = "tdx")]
        let force_iommu = config.lock().unwrap().tdx.is_some();
        #[cfg(not(feature = "tdx"))]
        let force_iommu = false;

        let device_manager = DeviceManager::new(
            vm.clone(),
            config.clone(),
            memory_manager.clone(),
            &exit_evt,
            &reset_evt,
            seccomp_action.clone(),
            #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
            numa_nodes.clone(),
            &activate_evt,
            force_iommu,
            restoring,
        )
        .map_err(Error::DeviceManager)?;

        let memory = memory_manager.lock().unwrap().guest_memory();
        #[cfg(target_arch = "x86_64")]
        let io_bus = Arc::clone(device_manager.lock().unwrap().io_bus());
        let mmio_bus = Arc::clone(device_manager.lock().unwrap().mmio_bus());
        // Create the VmOps structure, which implements the VmmOps trait.
        // And send it to the hypervisor.

        #[cfg(all(target_arch = "x86_64", feature = "pci_support"))]
        let pci_config_io =
            device_manager.lock().unwrap().pci_config_io() as Arc<Mutex<dyn BusDevice>>;
        let vm_ops: Arc<dyn VmmOps> = Arc::new(VmOps {
            memory,
            #[cfg(target_arch = "x86_64")]
            io_bus,
            mmio_bus,
            #[cfg(target_arch = "x86_64")]
            timestamp: std::time::Instant::now(),
            #[cfg(all(target_arch = "x86_64", feature = "pci_support"))]
            pci_config_io,
        });

        let exit_evt_clone = exit_evt.try_clone().map_err(Error::EventFdClone)?;
        #[cfg(feature = "tdx")]
        let tdx_enabled = config.lock().unwrap().tdx.is_some();
        let cpu_manager = cpu::CpuManager::new(
            &config.lock().unwrap().cpus.clone(),
            &device_manager,
            &memory_manager,
            vm.clone(),
            exit_evt_clone,
            reset_evt,
            hypervisor.clone(),
            seccomp_action.clone(),
            vm_ops,
            #[cfg(feature = "tdx")]
            tdx_enabled,
            #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
            &numa_nodes,
        )
        .map_err(Error::CpuManager)?;

        let on_tty = unsafe { libc::isatty(libc::STDIN_FILENO as i32) } != 0;
        let kernel = config
            .lock()
            .unwrap()
            .kernel
            .as_ref()
            .map(|k| File::open(&k.path))
            .transpose()
            .map_err(Error::KernelFile)?;

        let initramfs = config
            .lock()
            .unwrap()
            .initramfs
            .as_ref()
            .map(|i| File::open(&i.path))
            .transpose()
            .map_err(Error::InitramfsFile)?;

        Ok(Vm {
            kernel,
            initramfs,
            device_manager,
            config,
            on_tty,
            threads: Vec::with_capacity(1),
            signals: None,
            state: RwLock::new(VmState::Created),
            cpu_manager,
            memory_manager,
            vm,
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            saved_clock: _saved_clock,
            #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
            numa_nodes,
            seccomp_action: seccomp_action.clone(),
            exit_evt,
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            hypervisor,
        })
    }

    #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
    fn create_numa_nodes(
        configs: Option<Vec<NumaConfig>>,
        memory_manager: &Arc<Mutex<MemoryManager>>,
    ) -> Result<NumaNodes> {
        let mm = memory_manager.lock().unwrap();
        let mm_zones = mm.memory_zones();
        let mut numa_nodes = BTreeMap::new();

        if let Some(configs) = &configs {
            for config in configs.iter() {
                if numa_nodes.contains_key(&config.guest_numa_id) {
                    error!("Can't define twice the same NUMA node");
                    return Err(Error::InvalidNumaConfig);
                }

                let mut node = NumaNode::default();

                if let Some(memory_zones) = &config.memory_zones {
                    for memory_zone in memory_zones.iter() {
                        if let Some(mm_zone) = mm_zones.get(memory_zone) {
                            node.memory_regions.extend(mm_zone.regions().clone());
                            if let Some(virtiomem_zone) = mm_zone.virtio_mem_zone() {
                                node.hotplug_regions.push(virtiomem_zone.region().clone());
                            }
                            node.memory_zones.push(memory_zone.clone());
                        } else {
                            error!("Unknown memory zone '{}'", memory_zone);
                            return Err(Error::InvalidNumaConfig);
                        }
                    }
                }

                if let Some(cpus) = &config.cpus {
                    node.cpus.extend(cpus);
                }

                if let Some(distances) = &config.distances {
                    for distance in distances.iter() {
                        let dest = distance.destination;
                        let dist = distance.distance;

                        if !configs.iter().any(|cfg| cfg.guest_numa_id == dest) {
                            error!("Unknown destination NUMA node {}", dest);
                            return Err(Error::InvalidNumaConfig);
                        }

                        if node.distances.contains_key(&dest) {
                            error!("Destination NUMA node {} has been already set", dest);
                            return Err(Error::InvalidNumaConfig);
                        }

                        node.distances.insert(dest, dist);
                    }
                }

                #[cfg(target_arch = "x86_64")]
                if let Some(sgx_epc_sections) = &config.sgx_epc_sections {
                    if let Some(sgx_epc_region) = mm.sgx_epc_region() {
                        let mm_sections = sgx_epc_region.epc_sections();
                        for sgx_epc_section in sgx_epc_sections.iter() {
                            if let Some(mm_section) = mm_sections.get(sgx_epc_section) {
                                node.sgx_epc_sections.push(mm_section.clone());
                            } else {
                                error!("Unknown SGX EPC section '{}'", sgx_epc_section);
                                return Err(Error::InvalidNumaConfig);
                            }
                        }
                    } else {
                        error!("Missing SGX EPC region");
                        return Err(Error::InvalidNumaConfig);
                    }
                }

                numa_nodes.insert(config.guest_numa_id, node);
            }
        }

        Ok(numa_nodes)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Mutex<VmConfig>>,
        exit_evt: EventFd,
        reset_evt: EventFd,
        seccomp_action: &SeccompAction,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
        activate_evt: EventFd,
        serial_pty: Option<PtyPair>,
        console_pty: Option<PtyPair>,
        console_resize_pipe: Option<File>,
    ) -> Result<Self> {
        #[cfg(all(feature = "kvm", target_arch = "aarch64"))]
        let craton_enabled = config.lock().unwrap().craton;
        if craton_enabled {
            return Vm::new_craton(
                    config,
                    exit_evt,
                    reset_evt,
                    seccomp_action,
                    hypervisor,
                    activate_evt,
                    serial_pty,
                    console_pty,
                    console_resize_pipe);
        }
        #[cfg(feature = "tdx")]
        let tdx_enabled = config.lock().unwrap().tdx.is_some();
        hypervisor.check_required_extensions().unwrap();
        #[cfg(feature = "tdx")]
        let vm = hypervisor
            .create_vm_with_type(if tdx_enabled {
                2 // KVM_X86_TDX_VM
            } else {
                0 // KVM_X86_LEGACY_VM
            })
            .unwrap();
        #[cfg(not(feature = "tdx"))]
        let vm = hypervisor.create_vm().unwrap();

        #[cfg(target_arch = "x86_64")]
        {
            vm.set_identity_map_address(KVM_IDENTITY_MAP_START.0)
                .unwrap();
            vm.set_tss_address(KVM_TSS_START.0 as usize).unwrap();
            vm.enable_split_irq().unwrap();
        }

        let phys_bits = physical_bits(config.lock().unwrap().cpus.max_phys_bits);

        #[cfg(target_arch = "x86_64")]
        let sgx_epc_config = config.lock().unwrap().sgx_epc.clone();

        let memory_manager = MemoryManager::new(
            vm.clone(),
            &config.lock().unwrap().memory.clone(),
            None,
            phys_bits,
            #[cfg(feature = "tdx")]
            tdx_enabled,
            None,
            #[cfg(target_arch = "x86_64")]
            sgx_epc_config,
        )
        .map_err(Error::MemoryManager)?;

        let new_vm = Vm::new_from_memory_manager(
            config,
            memory_manager,
            vm,
            exit_evt,
            reset_evt,
            seccomp_action,
            hypervisor,
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            None,
            activate_evt,
            false,
        )?;

        // The device manager must create the devices from here as it is part
        // of the regular code path creating everything from scratch.
        new_vm
            .device_manager
            .lock()
            .unwrap()
            .create_devices(serial_pty, console_pty, console_resize_pipe)
            .map_err(Error::DeviceManager)?;
        Ok(new_vm)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_from_snapshot(
        snapshot: &Snapshot,
        exit_evt: EventFd,
        reset_evt: EventFd,
        source_url: Option<&str>,
        prefault: bool,
        seccomp_action: &SeccompAction,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
        activate_evt: EventFd,
    ) -> Result<Self> {
        hypervisor.check_required_extensions().unwrap();
        let vm = hypervisor.create_vm().unwrap();

        #[cfg(target_arch = "x86_64")]
        {
            vm.set_identity_map_address(KVM_IDENTITY_MAP_START.0)
                .unwrap();
            vm.set_tss_address(KVM_TSS_START.0 as usize).unwrap();
            vm.enable_split_irq().unwrap();
        }

        let vm_snapshot = get_vm_snapshot(snapshot).map_err(Error::Restore)?;
        let config = vm_snapshot.config;
        if let Some(state) = vm_snapshot.state {
            vm.set_state(state)
                .map_err(|e| Error::Restore(MigratableError::Restore(e.into())))?;
        }

        let memory_manager = if let Some(memory_manager_snapshot) =
            snapshot.snapshots.get(MEMORY_MANAGER_SNAPSHOT_ID)
        {
            let phys_bits = physical_bits(config.lock().unwrap().cpus.max_phys_bits);
            MemoryManager::new_from_snapshot(
                memory_manager_snapshot,
                vm.clone(),
                &config.lock().unwrap().memory.clone(),
                source_url,
                prefault,
                phys_bits,
            )
            .map_err(Error::MemoryManager)?
        } else {
            return Err(Error::Restore(MigratableError::Restore(anyhow!(
                "Missing memory manager snapshot"
            ))));
        };

        Vm::new_from_memory_manager(
            config,
            memory_manager,
            vm,
            exit_evt,
            reset_evt,
            seccomp_action,
            hypervisor,
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            vm_snapshot.clock,
            activate_evt,
            true,
        )
    }

    pub fn new_from_migration(
        config: Arc<Mutex<VmConfig>>,
        exit_evt: EventFd,
        reset_evt: EventFd,
        seccomp_action: &SeccompAction,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
        activate_evt: EventFd,
        memory_manager_data: &MemoryManagerSnapshotData,
    ) -> Result<Self> {
        hypervisor.check_required_extensions().unwrap();
        let vm = hypervisor.create_vm().unwrap();

        #[cfg(target_arch = "x86_64")]
        {
            vm.set_identity_map_address(KVM_IDENTITY_MAP_START.0)
                .unwrap();
            vm.set_tss_address(KVM_TSS_START.0 as usize).unwrap();
            vm.enable_split_irq().unwrap();
        }

        let phys_bits = physical_bits(config.lock().unwrap().cpus.max_phys_bits);

        let memory_manager = MemoryManager::new(
            vm.clone(),
            &config.lock().unwrap().memory.clone(),
            None,
            phys_bits,
            #[cfg(feature = "tdx")]
            false,
            Some(memory_manager_data),
            #[cfg(target_arch = "x86_64")]
            None,
        )
        .map_err(Error::MemoryManager)?;

        Vm::new_from_memory_manager(
            config,
            memory_manager,
            vm,
            exit_evt,
            reset_evt,
            seccomp_action,
            hypervisor,
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            None,
            activate_evt,
            true,
        )
    }

    #[cfg(all(feature = "kvm", target_arch = "aarch64"))]
    #[allow(clippy::too_many_arguments)]
    pub fn new_craton(
        config: Arc<Mutex<VmConfig>>,
        exit_evt: EventFd,
        reset_evt: EventFd,
        seccomp_action: &SeccompAction,
        hypervisor: Arc<dyn hypervisor::Hypervisor>,
        activate_evt: EventFd,
        serial_pty: Option<PtyPair>,
        console_pty: Option<PtyPair>,
        console_resize_pipe: Option<File>,
    ) -> Result<Self> {

        let mut ram_dev = 0;
        let mut ram_file = String::new();
        let mut dev_num = 0;
        println!("UIO devices:");
        'uio_devices: loop {
            let path = format!("/dev/uio{}", dev_num);
            match OpenOptions::new().read(true).write(true).open(path.clone()) {
                Ok(_) => (), /* but we don't actually need the file here */
                Err(error) => match error.kind() {
                    std::io::ErrorKind::NotFound => break 'uio_devices,
                    _ => continue 'uio_devices,
                },
            };
            let name_path = format!("/sys/class/uio/uio{}/name", dev_num);
            let mut name_file = File::open(name_path).unwrap();
            let mut name = String::new();
            name_file.read_to_string(&mut name).unwrap();
            if name.trim().eq("ram") {
                println!("Found ram device. Path: {}", path);
                ram_file = path.clone();
                ram_dev = dev_num;
            }
            println!(" {}", name.trim());
            dev_num += 1;
        }
        println!("Found ram device at: {}", ram_file);
        if ram_file.is_empty() {
            eprintln!("Couldn't find uio ram device!");
            return Err(Error::Console(vmm_sys_util::errno::Error::new(1)));
        }
        fn open_and_parse_hex(path: String) -> u64 {
            let mut file = File::open(path).unwrap();
            let mut num = String::new();
            file.read_to_string(&mut num).unwrap();
            let just_num = num.trim().trim_start_matches("0x");
            u64::from_str_radix(just_num, 16).unwrap()
        }
        let ram_start = open_and_parse_hex(
                                format!("/sys/class/uio/uio{}/maps/map0/addr", ram_dev)
                            );
        println!(" ram start: {:#x}", ram_start);
        let ram_size = open_and_parse_hex(
                                format!("/sys/class/uio/uio{}/maps/map0/size", ram_dev)
                            );
        println!(" ram size: {:#x}", ram_size);
        let ram_offset = open_and_parse_hex(
                                format!("/sys/class/uio/uio{}/maps/map0/offset", ram_dev)
                            );
        println!(" ram offset: {:#x}", ram_offset);

        /* Nuno: this checks for SignalMsi and OneReg */
        hypervisor.check_required_extensions().unwrap();

        let vm = hypervisor.create_vm_with_type(0).unwrap(); // type 0 = KVM_X86_LEGACY_VM
        println!("created vm");

        let phys_bits = physical_bits(config.lock().unwrap().cpus.max_phys_bits);

        let memory_manager = MemoryManager::new_craton(
            vm.clone(),
            GuestAddress(ram_start),
            ram_size.try_into().unwrap(),
            ram_offset * (PAGE_SIZE as u64),
            std::path::PathBuf::from(ram_file),
            phys_bits,
        )
        .map_err(Error::MemoryManager)?;

        println!("created MemoryManager");

        /* Nuno: rest of this code is from new_from_memory_manager */

        /* Nuno: no iommu please */
        let force_iommu = false;
        /* Nuno: numa_nodes will just be an empty BTree */
        let numa_nodes =
            Self::create_numa_nodes(config.lock().unwrap().numa.clone(), &memory_manager)?;

        let device_manager = DeviceManager::new(
            vm.clone(),
            config.clone(),
            memory_manager.clone(),
            &exit_evt,
            &reset_evt,
            seccomp_action.clone(),
            numa_nodes.clone(),
            &activate_evt,
            force_iommu,
            false,
        )
        .map_err(Error::DeviceManager)?;

        println!("created DeviceManager");

        let memory = memory_manager.lock().unwrap().guest_memory();
        let mmio_bus = Arc::clone(device_manager.lock().unwrap().mmio_bus());
        // Create the VmOps structure, which implements the VmmOps trait.
        // And send it to the hypervisor.

        let vm_ops: Arc<dyn VmmOps> = Arc::new(VmOps {
            memory,
            mmio_bus,
        });

        let exit_evt_clone = exit_evt.try_clone().map_err(Error::EventFdClone)?;
        let cpu_manager = cpu::CpuManager::new(
            &config.lock().unwrap().cpus.clone(),
            &device_manager,
            &memory_manager,
            vm.clone(),
            exit_evt_clone,
            reset_evt,
            hypervisor.clone(),
            seccomp_action.clone(),
            vm_ops,
            &numa_nodes,
        )
        .map_err(Error::CpuManager)?;

        println!("created CpuManager");

        let on_tty = unsafe { libc::isatty(libc::STDIN_FILENO as i32) } != 0;
        let kernel = config
            .lock()
            .unwrap()
            .kernel
            .as_ref()
            .map(|k| File::open(&k.path))
            .transpose()
            .map_err(Error::KernelFile)?;

        let initramfs = config
            .lock()
            .unwrap()
            .initramfs
            .as_ref()
            .map(|i| File::open(&i.path))
            .transpose()
            .map_err(Error::InitramfsFile)?;

        let new_vm = Vm {
            kernel,
            initramfs,
            device_manager,
            config,
            on_tty,
            threads: Vec::with_capacity(1),
            signals: None,
            state: RwLock::new(VmState::Created),
            cpu_manager,
            memory_manager,
            vm,
            #[cfg(any(target_arch = "aarch64", feature = "acpi"))]
            numa_nodes,
            seccomp_action: seccomp_action.clone(),
            exit_evt,
        };

        // The device manager must create the devices from here as it is part
        // of the regular code path creating everything from scratch.
        new_vm
            .device_manager
            .lock()
            .unwrap()
            .create_devices(serial_pty, console_pty, console_resize_pipe)
            .map_err(Error::DeviceManager)?;

        Ok(new_vm)
    }

    fn load_initramfs(&mut self, guest_mem: &GuestMemoryMmap) -> Result<arch::InitramfsConfig> {
        let mut initramfs = self.initramfs.as_ref().unwrap();
        let size: usize = initramfs
            .seek(SeekFrom::End(0))
            .map_err(|_| Error::InitramfsLoad)?
            .try_into()
            .unwrap();
        initramfs
            .seek(SeekFrom::Start(0))
            .map_err(|_| Error::InitramfsLoad)?;

        let address =
            arch::initramfs_load_addr(guest_mem, size).map_err(|_| Error::InitramfsLoad)?;
        let address = GuestAddress(address);

        guest_mem
            .read_from(address, &mut initramfs, size)
            .map_err(|_| Error::InitramfsLoad)?;

        info!("Initramfs loaded: address = 0x{:x}", address.0);
        Ok(arch::InitramfsConfig { address, size })
    }

    fn get_cmdline(&mut self) -> Result<Cmdline> {
        let mut cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE);
        cmdline
            .insert_str(self.config.lock().unwrap().cmdline.args.clone())
            .map_err(Error::CmdLineInsertStr)?;
        for entry in self.device_manager.lock().unwrap().cmdline_additions() {
            cmdline.insert_str(entry).map_err(Error::CmdLineInsertStr)?;
        }
        Ok(cmdline)
    }

    #[cfg(target_arch = "aarch64")]
    fn load_kernel(&mut self) -> Result<EntryPoint> {
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();
        let mut kernel = self.kernel.as_ref().unwrap();
        let entry_addr = match linux_loader::loader::pe::PE::load(
            mem.deref(),
            Some(GuestAddress(arch::get_kernel_start())),
            &mut kernel,
            None,
        ) {
            Ok(entry_addr) => entry_addr,
            // Try to load the binary as kernel PE file at first.
            // If failed, retry to load it as UEFI binary.
            // As the UEFI binary is formatless, it must be the last option to try.
            Err(linux_loader::loader::Error::Pe(InvalidImageMagicNumber)) => {
                arch::aarch64::uefi::load_uefi(
                    mem.deref(),
                    GuestAddress(arch::get_uefi_start()),
                    &mut kernel,
                )
                .map_err(Error::UefiLoad)?;
                // The entry point offset in UEFI image is always 0.
                return Ok(EntryPoint {
                    entry_addr: GuestAddress(arch::get_uefi_start()),
                });
            }
            Err(e) => {
                return Err(Error::KernelLoad(e));
            }
        };

        let entry_point_addr: GuestAddress = entry_addr.kernel_load;

        Ok(EntryPoint {
            entry_addr: entry_point_addr,
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn load_kernel(&mut self) -> Result<EntryPoint> {
        use linux_loader::loader::{elf::Error::InvalidElfMagicNumber, Error::Elf};
        info!("Loading kernel");
        let cmdline = self.get_cmdline()?;
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();
        let mut kernel = self.kernel.as_ref().unwrap();
        let entry_addr = match linux_loader::loader::elf::Elf::load(
            mem.deref(),
            None,
            &mut kernel,
            Some(arch::layout::HIGH_RAM_START),
        ) {
            Ok(entry_addr) => entry_addr,
            Err(e) => match e {
                Elf(InvalidElfMagicNumber) => {
                    // Not an ELF header - assume raw binary data / firmware
                    let size = kernel.seek(SeekFrom::End(0)).map_err(Error::FirmwareFile)?;

                    // The OVMF firmware is as big as you might expect and it's 4MiB so limit to that
                    if size > 4 << 20 {
                        return Err(Error::FirmwareTooLarge);
                    }

                    // Loaded at the end of the 4GiB
                    let load_address = GuestAddress(4 << 30)
                        .checked_sub(size)
                        .ok_or(Error::FirmwareTooLarge)?;

                    self.memory_manager
                        .lock()
                        .unwrap()
                        .add_ram_region(load_address, size as usize)
                        .map_err(Error::AllocateFirmwareMemory)?;

                    kernel
                        .seek(SeekFrom::Start(0))
                        .map_err(Error::FirmwareFile)?;
                    guest_memory
                        .memory()
                        .read_exact_from(load_address, &mut kernel, size as usize)
                        .map_err(Error::FirmwareLoad)?;

                    return Ok(EntryPoint { entry_addr: None });
                }
                _ => {
                    return Err(Error::KernelLoad(e));
                }
            },
        };

        linux_loader::loader::load_cmdline(mem.deref(), arch::layout::CMDLINE_START, &cmdline)
            .map_err(Error::LoadCmdLine)?;

        if let PvhEntryPresent(entry_addr) = entry_addr.pvh_boot_cap {
            // Use the PVH kernel entry point to boot the guest
            info!("Kernel loaded: entry_addr = 0x{:x}", entry_addr.0);
            Ok(EntryPoint {
                entry_addr: Some(entry_addr),
            })
        } else {
            Err(Error::KernelMissingPvhHeader)
        }
    }

    #[cfg(target_arch = "x86_64")]
    fn configure_system(&mut self, #[cfg(feature = "acpi")] rsdp_addr: GuestAddress) -> Result<()> {
        info!("Configuring system");
        let mem = self.memory_manager.lock().unwrap().boot_guest_memory();

        let initramfs_config = match self.initramfs {
            Some(_) => Some(self.load_initramfs(&mem)?),
            None => None,
        };

        let boot_vcpus = self.cpu_manager.lock().unwrap().boot_vcpus();

        #[cfg(feature = "acpi")]
        let rsdp_addr = Some(rsdp_addr);
        #[cfg(not(feature = "acpi"))]
        let rsdp_addr = None;

        let sgx_epc_region = self
            .memory_manager
            .lock()
            .unwrap()
            .sgx_epc_region()
            .as_ref()
            .cloned();

        arch::configure_system(
            &mem,
            arch::layout::CMDLINE_START,
            &initramfs_config,
            boot_vcpus,
            rsdp_addr,
            sgx_epc_region,
        )
        .map_err(Error::ConfigureSystem)?;
        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    fn configure_system(
        &mut self,
        #[cfg(feature = "acpi")] _rsdp_addr: GuestAddress,
    ) -> Result<()> {
        let cmdline = self.get_cmdline()?;
        let vcpu_mpidrs = self.cpu_manager.lock().unwrap().get_mpidrs();
        let vcpu_topology = self.cpu_manager.lock().unwrap().get_vcpu_topology();
        let mem = self.memory_manager.lock().unwrap().boot_guest_memory();
        let initramfs_config = match self.initramfs {
            Some(_) => Some(self.load_initramfs(&mem)?),
            None => None,
        };

        let device_info = &self
            .device_manager
            .lock()
            .unwrap()
            .get_device_info()
            .clone();

        let pci_space: Option<(u64, u64)> = if cfg!(feature = "pci_support") {
            let pci_space_start: GuestAddress = self
                .memory_manager
                .lock()
                .as_ref()
                .unwrap()
                .start_of_device_area();

            let pci_space_end: GuestAddress = self
                .memory_manager
                .lock()
                .as_ref()
                .unwrap()
                .end_of_device_area();

            let pci_space_size = pci_space_end
                .checked_offset_from(pci_space_start)
                .ok_or(Error::MemOverflow)?
                + 1;

            Some((pci_space_start.0, pci_space_size))
        } else {
            None
        };

        let virtio_iommu_bdf = if cfg!(feature = "pci_support") {
            self.device_manager
                .lock()
                .unwrap()
                .iommu_attached_devices()
                .as_ref()
                .map(|(v, _)| *v)
        } else {
            None
        };

        let gic_device = create_gic(
            &self.memory_manager.lock().as_ref().unwrap().vm,
            self.cpu_manager.lock().unwrap().boot_vcpus() as u64,
        )
        .map_err(|e| {
            Error::ConfigureSystem(arch::Error::AArch64Setup(arch::aarch64::Error::SetupGic(e)))
        })?;

        arch::configure_system(
            &mem,
            cmdline.as_str(),
            vcpu_mpidrs,
            vcpu_topology,
            device_info,
            &initramfs_config,
            &pci_space,
            virtio_iommu_bdf.map(|bdf| bdf.into()),
            &*gic_device,
            &self.numa_nodes,
        )
        .map_err(Error::ConfigureSystem)?;

        // Update the GIC entity in device manager
        self.device_manager
            .lock()
            .unwrap()
            .get_interrupt_controller()
            .unwrap()
            .lock()
            .unwrap()
            .set_gic_device(Arc::new(Mutex::new(gic_device)));

        // Activate gic device
        self.device_manager
            .lock()
            .unwrap()
            .get_interrupt_controller()
            .unwrap()
            .lock()
            .unwrap()
            .enable()
            .map_err(Error::EnableInterruptController)?;

        Ok(())
    }

    pub fn serial_pty(&self) -> Option<PtyPair> {
        self.device_manager.lock().unwrap().serial_pty()
    }

    pub fn console_pty(&self) -> Option<PtyPair> {
        self.device_manager.lock().unwrap().console_pty()
    }

    pub fn console_resize_pipe(&self) -> Option<Arc<File>> {
        self.device_manager.lock().unwrap().console_resize_pipe()
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let mut state = self.state.try_write().map_err(|_| Error::PoisonedState)?;
        let new_state = VmState::Shutdown;

        state.valid_transition(new_state)?;

        if self.on_tty {
            // Don't forget to set the terminal in canonical mode
            // before to exit.
            io::stdin()
                .lock()
                .set_canon_mode()
                .map_err(Error::SetTerminalCanon)?;
        }

        // Trigger the termination of the signal_handler thread
        if let Some(signals) = self.signals.take() {
            signals.close();
        }

        // Wake up the DeviceManager threads so they will get terminated cleanly
        self.device_manager
            .lock()
            .unwrap()
            .resume()
            .map_err(Error::Resume)?;

        self.cpu_manager
            .lock()
            .unwrap()
            .shutdown()
            .map_err(Error::CpuManager)?;

        // Wait for all the threads to finish
        for thread in self.threads.drain(..) {
            thread.join().map_err(Error::ThreadCleanup)?
        }
        *state = new_state;

        event!("vm", "shutdown");

        Ok(())
    }

    pub fn resize(
        &mut self,
        desired_vcpus: Option<u8>,
        desired_memory: Option<u64>,
        desired_balloon: Option<u64>,
    ) -> Result<()> {
        event!("vm", "resizing");

        if let Some(desired_vcpus) = desired_vcpus {
            if self
                .cpu_manager
                .lock()
                .unwrap()
                .resize(desired_vcpus)
                .map_err(Error::CpuManager)?
            {
                self.device_manager
                    .lock()
                    .unwrap()
                    .notify_hotplug(AcpiNotificationFlags::CPU_DEVICES_CHANGED)
                    .map_err(Error::DeviceManager)?;
            }
            self.config.lock().unwrap().cpus.boot_vcpus = desired_vcpus;
        }

        if let Some(desired_memory) = desired_memory {
            let new_region = self
                .memory_manager
                .lock()
                .unwrap()
                .resize(desired_memory)
                .map_err(Error::MemoryManager)?;

            let mut memory_config = &mut self.config.lock().unwrap().memory;

            if let Some(new_region) = &new_region {
                self.device_manager
                    .lock()
                    .unwrap()
                    .update_memory(new_region)
                    .map_err(Error::DeviceManager)?;

                match memory_config.hotplug_method {
                    HotplugMethod::Acpi => {
                        self.device_manager
                            .lock()
                            .unwrap()
                            .notify_hotplug(AcpiNotificationFlags::MEMORY_DEVICES_CHANGED)
                            .map_err(Error::DeviceManager)?;
                    }
                    HotplugMethod::VirtioMem => {}
                }
            }

            // We update the VM config regardless of the actual guest resize
            // operation result (happened or not), so that if the VM reboots
            // it will be running with the last configure memory size.
            match memory_config.hotplug_method {
                HotplugMethod::Acpi => memory_config.size = desired_memory,
                HotplugMethod::VirtioMem => {
                    if desired_memory > memory_config.size {
                        memory_config.hotplugged_size = Some(desired_memory - memory_config.size);
                    } else {
                        memory_config.hotplugged_size = None;
                    }
                }
            }
        }

        if let Some(desired_balloon) = desired_balloon {
            self.device_manager
                .lock()
                .unwrap()
                .resize_balloon(desired_balloon)
                .map_err(Error::DeviceManager)?;

            // Update the configuration value for the balloon size to ensure
            // a reboot would use the right value.
            if let Some(balloon_config) = &mut self.config.lock().unwrap().balloon {
                balloon_config.size = desired_balloon;
            }
        }

        event!("vm", "resized");

        Ok(())
    }

    pub fn resize_zone(&mut self, id: String, desired_memory: u64) -> Result<()> {
        let memory_config = &mut self.config.lock().unwrap().memory;

        if let Some(zones) = &mut memory_config.zones {
            for zone in zones.iter_mut() {
                if zone.id == id {
                    if desired_memory >= zone.size {
                        let hotplugged_size = desired_memory - zone.size;
                        self.memory_manager
                            .lock()
                            .unwrap()
                            .resize_zone(&id, desired_memory - zone.size)
                            .map_err(Error::MemoryManager)?;
                        // We update the memory zone config regardless of the
                        // actual 'resize-zone' operation result (happened or
                        // not), so that if the VM reboots it will be running
                        // with the last configured memory zone size.
                        zone.hotplugged_size = Some(hotplugged_size);

                        return Ok(());
                    } else {
                        error!(
                            "Invalid to ask less ({}) than boot RAM ({}) for \
                            this memory zone",
                            desired_memory, zone.size,
                        );
                        return Err(Error::ResizeZone);
                    }
                }
            }
        }

        error!("Could not find the memory zone {} for the resize", id);
        Err(Error::ResizeZone)
    }

    #[cfg(feature = "pci_support")]
    fn add_to_config<T>(devices: &mut Option<Vec<T>>, device: T) {
        if let Some(devices) = devices {
            devices.push(device);
        } else {
            *devices = Some(vec![device]);
        }
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_device(&mut self, mut _device_cfg: DeviceConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_device(&mut self, mut device_cfg: DeviceConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.devices, device_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_device(&mut device_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.devices, device_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_user_device(&mut self, mut _device_cfg: UserDeviceConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_user_device(&mut self, mut device_cfg: UserDeviceConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.user_devices, device_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_user_device(&mut device_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.user_devices, device_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }
    #[cfg(not(feature = "pci_support"))]
    pub fn remove_device(&mut self, _id: String) -> Result<()> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn remove_device(&mut self, id: String) -> Result<()> {
        self.device_manager
            .lock()
            .unwrap()
            .remove_device(id.clone())
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by removing the device. This is important to
        // ensure the device would not be created in case of a reboot.
        let mut config = self.config.lock().unwrap();

        // Remove if VFIO device
        if let Some(devices) = config.devices.as_mut() {
            devices.retain(|dev| dev.id.as_ref() != Some(&id));
        }

        // Remove if VFIO user device
        if let Some(user_devices) = config.user_devices.as_mut() {
            user_devices.retain(|dev| dev.id.as_ref() != Some(&id));
        }

        // Remove if disk device
        if let Some(disks) = config.disks.as_mut() {
            disks.retain(|dev| dev.id.as_ref() != Some(&id));
        }

        // Remove if net device
        if let Some(net) = config.net.as_mut() {
            net.retain(|dev| dev.id.as_ref() != Some(&id));
        }

        // Remove if pmem device
        if let Some(pmem) = config.pmem.as_mut() {
            pmem.retain(|dev| dev.id.as_ref() != Some(&id));
        }

        // Remove if vsock device
        if let Some(vsock) = config.vsock.as_ref() {
            if vsock.id.as_ref() == Some(&id) {
                config.vsock = None;
            }
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;
        Ok(())
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_disk(&mut self, mut _disk_cfg: DiskConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_disk(&mut self, mut disk_cfg: DiskConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.disks, disk_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_disk(&mut disk_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.disks, disk_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_fs(&mut self, mut _fs_cfg: FsConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_fs(&mut self, mut fs_cfg: FsConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.fs, fs_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_fs(&mut fs_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.fs, fs_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_pmem(&mut self, mut _pmem_cfg: PmemConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_pmem(&mut self, mut pmem_cfg: PmemConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.pmem, pmem_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_pmem(&mut pmem_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.pmem, pmem_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_net(&mut self, mut _net_cfg: NetConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_net(&mut self, mut net_cfg: NetConfig) -> Result<PciDeviceInfo> {
        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            Self::add_to_config(&mut config.net, net_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_net(&mut net_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            Self::add_to_config(&mut config.net, net_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    #[cfg(not(feature = "pci_support"))]
    pub fn add_vsock(&mut self, mut _vsock_cfg: VsockConfig) -> Result<PciDeviceInfo> {
        Err(Error::NoPciSupport)
    }

    #[cfg(feature = "pci_support")]
    pub fn add_vsock(&mut self, mut vsock_cfg: VsockConfig) -> Result<PciDeviceInfo> {
        if self.config.lock().unwrap().vsock.is_some() {
            return Err(Error::TooManyVsockDevices);
        }

        {
            // Validate on a clone of the config
            let mut config = self.config.lock().unwrap().clone();
            config.vsock = Some(vsock_cfg.clone());
            config.validate().map_err(Error::ConfigValidation)?;
        }

        let pci_device_info = self
            .device_manager
            .lock()
            .unwrap()
            .add_vsock(&mut vsock_cfg)
            .map_err(Error::DeviceManager)?;

        // Update VmConfig by adding the new device. This is important to
        // ensure the device would be created in case of a reboot.
        {
            let mut config = self.config.lock().unwrap();
            config.vsock = Some(vsock_cfg);
        }

        self.device_manager
            .lock()
            .unwrap()
            .notify_hotplug(AcpiNotificationFlags::PCI_DEVICES_CHANGED)
            .map_err(Error::DeviceManager)?;

        Ok(pci_device_info)
    }

    pub fn counters(&self) -> Result<HashMap<String, HashMap<&'static str, Wrapping<u64>>>> {
        Ok(self.device_manager.lock().unwrap().counters())
    }

    fn os_signal_handler(
        mut signals: Signals,
        console_input_clone: Arc<Console>,
        on_tty: bool,
        exit_evt: &EventFd,
    ) {
        for sig in HANDLED_SIGNALS {
            unblock_signal(sig).unwrap();
        }

        for signal in signals.forever() {
            match signal {
                SIGWINCH => {
                    console_input_clone.update_console_size();
                }
                SIGTERM | SIGINT => {
                    if on_tty {
                        io::stdin()
                            .lock()
                            .set_canon_mode()
                            .expect("failed to restore terminal mode");
                    }
                    if exit_evt.write(1).is_err() {
                        std::process::exit(1);
                    }
                }
                _ => (),
            }
        }
    }

    #[cfg(feature = "tdx")]
    fn init_tdx(&mut self) -> Result<()> {
        let cpuid = self.cpu_manager.lock().unwrap().common_cpuid();
        let max_vcpus = self.cpu_manager.lock().unwrap().max_vcpus() as u32;
        self.vm
            .tdx_init(&cpuid, max_vcpus)
            .map_err(Error::InitializeTdxVm)?;
        Ok(())
    }

    #[cfg(feature = "tdx")]
    fn extract_tdvf_sections(&mut self) -> Result<Vec<TdvfSection>> {
        use arch::x86_64::tdx::*;
        // The TDVF file contains a table of section as well as code
        let mut firmware_file =
            File::open(&self.config.lock().unwrap().tdx.as_ref().unwrap().firmware)
                .map_err(Error::LoadTdvf)?;

        // For all the sections allocate some RAM backing them
        parse_tdvf_sections(&mut firmware_file).map_err(Error::ParseTdvf)
    }

    #[cfg(feature = "tdx")]
    fn populate_tdx_sections(
        &mut self,
        sections: &[TdvfSection],
        vmm_data_regions: &[TdVmmDataRegion],
    ) -> Result<Option<u64>> {
        use arch::x86_64::tdx::*;
        // Get the memory end *before* we start adding TDVF ram regions
        let boot_guest_memory = self
            .memory_manager
            .lock()
            .as_ref()
            .unwrap()
            .boot_guest_memory();
        for section in sections {
            // No need to allocate if the section falls within guest RAM ranges
            if boot_guest_memory.address_in_range(GuestAddress(section.address)) {
                info!(
                    "Not allocating TDVF Section: {:x?} since it is already part of guest RAM",
                    section
                );
                continue;
            }

            info!("Allocating TDVF Section: {:x?}", section);
            self.memory_manager
                .lock()
                .unwrap()
                .add_ram_region(GuestAddress(section.address), section.size as usize)
                .map_err(Error::AllocatingTdvfMemory)?;
        }

        // The TDVF file contains a table of section as well as code
        let mut firmware_file =
            File::open(&self.config.lock().unwrap().tdx.as_ref().unwrap().firmware)
                .map_err(Error::LoadTdvf)?;

        // The guest memory at this point now has all the required regions so it
        // is safe to copy from the TDVF file into it.
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();
        let mut hob_offset = None;
        for section in sections {
            info!("Populating TDVF Section: {:x?}", section);
            match section.r#type {
                TdvfSectionType::Bfv | TdvfSectionType::Cfv => {
                    info!("Copying section to guest memory");
                    firmware_file
                        .seek(SeekFrom::Start(section.data_offset as u64))
                        .map_err(Error::LoadTdvf)?;
                    mem.read_from(
                        GuestAddress(section.address),
                        &mut firmware_file,
                        section.data_size as usize,
                    )
                    .unwrap();
                }
                TdvfSectionType::TdHob => {
                    hob_offset = Some(section.address);
                }
                _ => {}
            }
        }

        // Generate HOB
        let mut hob = TdHob::start(hob_offset.unwrap());

        let mut sorted_sections = sections.to_vec();
        sorted_sections.retain(|section| {
            !matches!(section.r#type, TdvfSectionType::Bfv | TdvfSectionType::Cfv)
        });

        // Add VMM specific data memory region to TdvfSections as TdHob type
        // to ensure the firmware won't ignore/reject the ranges.
        for region in vmm_data_regions {
            sorted_sections.push(TdvfSection {
                data_offset: 0,
                data_size: 0,
                address: region.start_address,
                size: region.length,
                r#type: TdvfSectionType::TdHob,
                attributes: 0,
            });
        }

        sorted_sections.sort_by_key(|section| section.address);
        sorted_sections.reverse();
        let mut current_section = sorted_sections.pop();

        // RAM regions interleaved with TDVF sections
        let mut next_start_addr = 0;
        for region in boot_guest_memory.iter() {
            let region_start = region.start_addr().0;
            let region_end = region.last_addr().0;
            if region_start > next_start_addr {
                next_start_addr = region_start;
            }

            loop {
                let (start, size, ram) = if let Some(section) = &current_section {
                    if section.address <= next_start_addr {
                        (section.address, section.size, false)
                    } else {
                        let last_addr = std::cmp::min(section.address - 1, region_end);
                        (next_start_addr, last_addr - next_start_addr + 1, true)
                    }
                } else {
                    (next_start_addr, region_end - next_start_addr + 1, true)
                };

                hob.add_memory_resource(&mem, start, size, ram)
                    .map_err(Error::PopulateHob)?;

                if !ram {
                    current_section = sorted_sections.pop();
                }

                next_start_addr = start + size;

                if next_start_addr > region_end {
                    break;
                }
            }
        }

        // MMIO regions
        hob.add_mmio_resource(
            &mem,
            arch::layout::MEM_32BIT_DEVICES_START.raw_value(),
            arch::layout::APIC_START.raw_value()
                - arch::layout::MEM_32BIT_DEVICES_START.raw_value(),
        )
        .map_err(Error::PopulateHob)?;
        let start_of_device_area = self
            .memory_manager
            .lock()
            .unwrap()
            .start_of_device_area()
            .raw_value();
        let end_of_device_area = self
            .memory_manager
            .lock()
            .unwrap()
            .end_of_device_area()
            .raw_value();
        hob.add_mmio_resource(
            &mem,
            start_of_device_area,
            end_of_device_area - start_of_device_area,
        )
        .map_err(Error::PopulateHob)?;

        // Add VMM specific data to the TdHob. The content of the data is
        // is written as part of the HOB, which will be retrieved from the
        // firmware, and processed accordingly to the type.
        for region in vmm_data_regions {
            hob.add_td_vmm_data(&mem, *region)
                .map_err(Error::PopulateHob)?;
        }

        hob.finish(&mem).map_err(Error::PopulateHob)?;

        Ok(hob_offset)
    }

    #[cfg(feature = "tdx")]
    fn init_tdx_memory(
        &mut self,
        sections: &[TdvfSection],
        regions: &[TdVmmDataRegion],
    ) -> Result<()> {
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();

        for section in sections {
            self.vm
                .tdx_init_memory_region(
                    mem.get_host_address(GuestAddress(section.address)).unwrap() as u64,
                    section.address,
                    section.size,
                    /* TDVF_SECTION_ATTRIBUTES_EXTENDMR */
                    section.attributes == 1,
                )
                .map_err(Error::InitializeTdxMemoryRegion)?;
        }

        // The same way we let the hypervisor know about the TDVF sections, we
        // must declare the VMM specific regions shared with the guest so that
        // they won't be discarded.
        for region in regions {
            self.vm
                .tdx_init_memory_region(
                    mem.get_host_address(GuestAddress(region.start_address))
                        .unwrap() as u64,
                    region.start_address,
                    region.length,
                    false,
                )
                .map_err(Error::InitializeTdxMemoryRegion)?;
        }

        Ok(())
    }

    fn setup_signal_handler(&mut self) -> Result<()> {
        let console = self.device_manager.lock().unwrap().console().clone();
        let signals = Signals::new(&HANDLED_SIGNALS);
        match signals {
            Ok(signals) => {
                self.signals = Some(signals.handle());
                let exit_evt = self.exit_evt.try_clone().map_err(Error::EventFdClone)?;
                let on_tty = self.on_tty;
                let signal_handler_seccomp_filter =
                    get_seccomp_filter(&self.seccomp_action, Thread::SignalHandler)
                        .map_err(Error::CreateSeccompFilter)?;
                self.threads.push(
                    thread::Builder::new()
                        .name("signal_handler".to_string())
                        .spawn(move || {
                            if !signal_handler_seccomp_filter.is_empty() {
                                if let Err(e) = apply_filter(&signal_handler_seccomp_filter)
                                    .map_err(Error::ApplySeccompFilter)
                                {
                                    error!("Error applying seccomp filter: {:?}", e);
                                    exit_evt.write(1).ok();
                                    return;
                                }
                            }
                            std::panic::catch_unwind(AssertUnwindSafe(|| {
                                Vm::os_signal_handler(signals, console, on_tty, &exit_evt);
                            }))
                            .map_err(|_| {
                                error!("signal_handler thead panicked");
                                exit_evt.write(1).ok()
                            })
                            .ok();
                        })
                        .map_err(Error::SignalHandlerSpawn)?,
                );
            }
            Err(e) => error!("Signal not found {}", e),
        }
        Ok(())
    }

    fn setup_tty(&self) -> Result<()> {
        if self.on_tty {
            io::stdin()
                .lock()
                .set_raw_mode()
                .map_err(Error::SetTerminalRaw)?;
        }

        Ok(())
    }

    pub fn boot(&mut self) -> Result<()> {
        info!("Booting VM");
        event!("vm", "booting");
        let current_state = self.get_state()?;
        if current_state == VmState::Paused {
            return self.resume().map_err(Error::Resume);
        }

        let new_state = VmState::Running;
        current_state.valid_transition(new_state)?;

        // Load kernel if configured
        let entry_point = if self.kernel.as_ref().is_some() {
            Some(self.load_kernel()?)
        } else {
            None
        };

        // The initial TDX configuration must be done before the vCPUs are
        // created
        #[cfg(feature = "tdx")]
        if self.config.lock().unwrap().tdx.is_some() {
            self.init_tdx()?;
        }

        // Create and configure vcpus
        self.cpu_manager
            .lock()
            .unwrap()
            .create_boot_vcpus(entry_point)
            .map_err(Error::CpuManager)?;

        #[cfg(feature = "tdx")]
        let sections = self.extract_tdvf_sections()?;

        #[cfg(feature = "acpi")]
        let rsdp_addr = {
            let mem = self.memory_manager.lock().unwrap().guest_memory().memory();

            let rsdp_addr = crate::acpi::create_acpi_tables(
                &mem,
                &self.device_manager,
                &self.cpu_manager,
                &self.memory_manager,
                &self.numa_nodes,
            );
            info!("Created ACPI tables: rsdp_addr = 0x{:x}", rsdp_addr.0);

            rsdp_addr
        };

        #[cfg(all(feature = "tdx", not(feature = "acpi")))]
        let vmm_data_regions: Vec<TdVmmDataRegion> = Vec::new();

        // Create a VMM specific data region to share the ACPI tables with
        // the guest. Reserving 64kiB to ensure the ACPI tables will fit.
        #[cfg(all(feature = "tdx", feature = "acpi"))]
        let vmm_data_regions = vec![TdVmmDataRegion {
            start_address: rsdp_addr.0,
            length: 0x10000,
            region_type: TdVmmDataRegionType::AcpiTables,
        }];

        // Configuring the TDX regions requires that the vCPUs are created.
        #[cfg(feature = "tdx")]
        let hob_address = if self.config.lock().unwrap().tdx.is_some() {
            // TDX sections are written to memory.
            self.populate_tdx_sections(&sections, &vmm_data_regions)?
        } else {
            None
        };

        // Configure shared state based on loaded kernel
        entry_point
            .map(|_| {
                self.configure_system(
                    #[cfg(feature = "acpi")]
                    rsdp_addr,
                )
            })
            .transpose()?;

        #[cfg(feature = "tdx")]
        if let Some(hob_address) = hob_address {
            // With the HOB address extracted the vCPUs can have
            // their TDX state configured.
            self.cpu_manager
                .lock()
                .unwrap()
                .initialize_tdx(hob_address)
                .map_err(Error::CpuManager)?;
            // Let the hypervisor know which memory ranges are shared with the
            // guest. This prevents the guest from ignoring/discarding memory
            // regions provided by the host.
            self.init_tdx_memory(&sections, &vmm_data_regions)?;
            // With TDX memory and CPU state configured TDX setup is complete
            self.vm.tdx_finalize().map_err(Error::FinalizeTdx)?;
        }

        self.cpu_manager
            .lock()
            .unwrap()
            .start_boot_vcpus()
            .map_err(Error::CpuManager)?;

        self.setup_signal_handler()?;
        self.setup_tty()?;

        let mut state = self.state.try_write().map_err(|_| Error::PoisonedState)?;
        *state = new_state;
        event!("vm", "booted");
        Ok(())
    }

    /// Gets a thread-safe reference counted pointer to the VM configuration.
    pub fn get_config(&self) -> Arc<Mutex<VmConfig>> {
        Arc::clone(&self.config)
    }

    /// Get the VM state. Returns an error if the state is poisoned.
    pub fn get_state(&self) -> Result<VmState> {
        self.state
            .try_read()
            .map_err(|_| Error::PoisonedState)
            .map(|state| *state)
    }

    /// Load saved clock from snapshot
    #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
    pub fn load_clock_from_snapshot(
        &mut self,
        snapshot: &Snapshot,
    ) -> Result<Option<hypervisor::ClockData>> {
        let vm_snapshot = get_vm_snapshot(snapshot).map_err(Error::Restore)?;
        self.saved_clock = vm_snapshot.clock;
        Ok(self.saved_clock)
    }

    #[cfg(target_arch = "aarch64")]
    /// Add the vGIC section to the VM snapshot.
    fn add_vgic_snapshot_section(
        &self,
        vm_snapshot: &mut Snapshot,
    ) -> std::result::Result<(), MigratableError> {
        let saved_vcpu_states = self.cpu_manager.lock().unwrap().get_saved_states();
        let gic_device = Arc::clone(
            self.device_manager
                .lock()
                .unwrap()
                .get_interrupt_controller()
                .unwrap()
                .lock()
                .unwrap()
                .get_gic_device()
                .unwrap(),
        );

        gic_device
            .lock()
            .unwrap()
            .set_gicr_typers(&saved_vcpu_states);

        vm_snapshot.add_snapshot(
            if let Some(gicv3_its) = gic_device
                .lock()
                .unwrap()
                .as_any_concrete_mut()
                .downcast_mut::<KvmGicV3Its>()
            {
                gicv3_its.snapshot()?
            } else {
                return Err(MigratableError::Snapshot(anyhow!(
                    "GicDevice downcast to KvmGicV3Its failed when snapshotting VM!"
                )));
            },
        );

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    /// Restore the vGIC from the VM snapshot and enable the interrupt controller routing.
    fn restore_vgic_and_enable_interrupt(
        &self,
        vm_snapshot: &Snapshot,
    ) -> std::result::Result<(), MigratableError> {
        let saved_vcpu_states = self.cpu_manager.lock().unwrap().get_saved_states();
        // The number of vCPUs is the same as the number of saved vCPU states.
        let vcpu_numbers = saved_vcpu_states.len();

        // Creating a GIC device here, as the GIC will not be created when
        // restoring the device manager. Note that currently only the bare GICv3
        // without ITS is supported.
        let mut gic_device = create_gic(&self.vm, vcpu_numbers.try_into().unwrap())
            .map_err(|e| MigratableError::Restore(anyhow!("Could not create GIC: {:#?}", e)))?;

        // Here we prepare the GICR_TYPER registers from the restored vCPU states.
        gic_device.set_gicr_typers(&saved_vcpu_states);

        let gic_device = Arc::new(Mutex::new(gic_device));
        // Update the GIC entity in device manager
        self.device_manager
            .lock()
            .unwrap()
            .get_interrupt_controller()
            .unwrap()
            .lock()
            .unwrap()
            .set_gic_device(Arc::clone(&gic_device));

        // Restore GIC states.
        if let Some(gicv3_its_snapshot) = vm_snapshot.snapshots.get(GIC_V3_ITS_SNAPSHOT_ID) {
            if let Some(gicv3_its) = gic_device
                .lock()
                .unwrap()
                .as_any_concrete_mut()
                .downcast_mut::<KvmGicV3Its>()
            {
                gicv3_its.restore(*gicv3_its_snapshot.clone())?;
            } else {
                return Err(MigratableError::Restore(anyhow!(
                    "GicDevice downcast to KvmGicV3Its failed when restoring VM!"
                )));
            };
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing GicV3Its snapshot"
            )));
        }

        // Activate gic device
        self.device_manager
            .lock()
            .unwrap()
            .get_interrupt_controller()
            .unwrap()
            .lock()
            .unwrap()
            .enable()
            .map_err(|e| {
                MigratableError::Restore(anyhow!(
                    "Could not enable interrupt controller routing: {:#?}",
                    e
                ))
            })?;

        Ok(())
    }

    /// Gets the actual size of the balloon.
    pub fn balloon_size(&self) -> u64 {
        self.device_manager.lock().unwrap().balloon_size()
    }

    pub fn receive_memory_regions<F>(
        &mut self,
        ranges: &MemoryRangeTable,
        fd: &mut F,
    ) -> std::result::Result<(), MigratableError>
    where
        F: Read,
    {
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();

        for range in ranges.regions() {
            let mut offset: u64 = 0;
            // Here we are manually handling the retry in case we can't the
            // whole region at once because we can't use the implementation
            // from vm-memory::GuestMemory of read_exact_from() as it is not
            // following the correct behavior. For more info about this issue
            // see: https://github.com/rust-vmm/vm-memory/issues/174
            loop {
                let bytes_read = mem
                    .read_from(
                        GuestAddress(range.gpa + offset),
                        fd,
                        (range.length - offset) as usize,
                    )
                    .map_err(|e| {
                        MigratableError::MigrateReceive(anyhow!(
                            "Error receiving memory from socket: {}",
                            e
                        ))
                    })?;
                offset += bytes_read as u64;

                if offset == range.length {
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn send_memory_regions<F>(
        &mut self,
        ranges: &MemoryRangeTable,
        fd: &mut F,
    ) -> std::result::Result<(), MigratableError>
    where
        F: Write,
    {
        let guest_memory = self.memory_manager.lock().as_ref().unwrap().guest_memory();
        let mem = guest_memory.memory();

        for range in ranges.regions() {
            let mut offset: u64 = 0;
            // Here we are manually handling the retry in case we can't the
            // whole region at once because we can't use the implementation
            // from vm-memory::GuestMemory of write_all_to() as it is not
            // following the correct behavior. For more info about this issue
            // see: https://github.com/rust-vmm/vm-memory/issues/174
            loop {
                let bytes_written = mem
                    .write_to(
                        GuestAddress(range.gpa + offset),
                        fd,
                        (range.length - offset) as usize,
                    )
                    .map_err(|e| {
                        MigratableError::MigrateSend(anyhow!(
                            "Error transferring memory to socket: {}",
                            e
                        ))
                    })?;
                offset += bytes_written as u64;

                if offset == range.length {
                    break;
                }
            }
        }

        Ok(())
    }

    pub fn memory_range_table(&self) -> std::result::Result<MemoryRangeTable, MigratableError> {
        self.memory_manager
            .lock()
            .unwrap()
            .memory_range_table(false)
    }

    pub fn device_tree(&self) -> Arc<Mutex<DeviceTree>> {
        self.device_manager.lock().unwrap().device_tree()
    }

    pub fn activate_virtio_devices(&self) -> Result<()> {
        debug!("MMIO: activate_virtio_devices");
        self.device_manager
            .lock()
            .unwrap()
            .activate_virtio_devices()
            .map_err(Error::ActivateVirtioDevices)
    }

    #[cfg(target_arch = "x86_64")]
    pub fn power_button(&self) -> Result<()> {
        #[cfg(feature = "acpi")]
        return self
            .device_manager
            .lock()
            .unwrap()
            .notify_power_button()
            .map_err(Error::PowerButton);
        #[cfg(not(feature = "acpi"))]
        Err(Error::PowerButtonNotSupported)
    }

    #[cfg(target_arch = "aarch64")]
    pub fn power_button(&self) -> Result<()> {
        self.device_manager
            .lock()
            .unwrap()
            .notify_power_button()
            .map_err(Error::PowerButton)
    }

    pub fn memory_manager_data(&self) -> MemoryManagerSnapshotData {
        self.memory_manager.lock().unwrap().snapshot_data()
    }
}

impl Pausable for Vm {
    fn pause(&mut self) -> std::result::Result<(), MigratableError> {
        event!("vm", "pausing");
        let mut state = self
            .state
            .try_write()
            .map_err(|e| MigratableError::Pause(anyhow!("Could not get VM state: {}", e)))?;
        let new_state = VmState::Paused;

        state
            .valid_transition(new_state)
            .map_err(|e| MigratableError::Pause(anyhow!("Invalid transition: {:?}", e)))?;

        #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
        {
            let mut clock = self
                .vm
                .get_clock()
                .map_err(|e| MigratableError::Pause(anyhow!("Could not get VM clock: {}", e)))?;
            // Reset clock flags.
            clock.flags = 0;
            self.saved_clock = Some(clock);
        }
        self.cpu_manager.lock().unwrap().pause()?;
        self.device_manager.lock().unwrap().pause()?;

        *state = new_state;

        event!("vm", "paused");
        Ok(())
    }

    fn resume(&mut self) -> std::result::Result<(), MigratableError> {
        event!("vm", "resuming");
        let mut state = self
            .state
            .try_write()
            .map_err(|e| MigratableError::Resume(anyhow!("Could not get VM state: {}", e)))?;
        let new_state = VmState::Running;

        state
            .valid_transition(new_state)
            .map_err(|e| MigratableError::Resume(anyhow!("Invalid transition: {:?}", e)))?;

        self.cpu_manager.lock().unwrap().resume()?;
        #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
        {
            if let Some(clock) = &self.saved_clock {
                self.vm.set_clock(clock).map_err(|e| {
                    MigratableError::Resume(anyhow!("Could not set VM clock: {}", e))
                })?;
            }
        }
        self.device_manager.lock().unwrap().resume()?;

        // And we're back to the Running state.
        *state = new_state;
        event!("vm", "resumed");
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub struct VmSnapshot {
    pub config: Arc<Mutex<VmConfig>>,
    #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
    pub clock: Option<hypervisor::ClockData>,
    pub state: Option<hypervisor::VmState>,
    #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
    pub common_cpuid: hypervisor::CpuId,
}

pub const VM_SNAPSHOT_ID: &str = "vm";
impl Snapshottable for Vm {
    fn id(&self) -> String {
        VM_SNAPSHOT_ID.to_string()
    }

    fn snapshot(&mut self) -> std::result::Result<Snapshot, MigratableError> {
        event!("vm", "snapshotting");

        #[cfg(feature = "tdx")]
        {
            if self.config.lock().unwrap().tdx.is_some() {
                return Err(MigratableError::Snapshot(anyhow!(
                    "Snapshot not possible with TDX VM"
                )));
            }
        }

        let current_state = self.get_state().unwrap();
        if current_state != VmState::Paused {
            return Err(MigratableError::Snapshot(anyhow!(
                "Trying to snapshot while VM is running"
            )));
        }

        #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
        let common_cpuid = {
            #[cfg(feature = "tdx")]
            let tdx_enabled = self.config.lock().unwrap().tdx.is_some();
            let phys_bits = physical_bits(self.config.lock().unwrap().cpus.max_phys_bits);
            arch::generate_common_cpuid(
                self.hypervisor.clone(),
                None,
                None,
                phys_bits,
                self.config.lock().unwrap().cpus.kvm_hyperv,
                #[cfg(feature = "tdx")]
                tdx_enabled,
            )
            .map_err(|e| {
                MigratableError::MigrateReceive(anyhow!("Error generating common cpuid: {:?}", e))
            })?
        };

        let mut vm_snapshot = Snapshot::new(VM_SNAPSHOT_ID);
        let vm_state = self
            .vm
            .state()
            .map_err(|e| MigratableError::Snapshot(e.into()))?;
        let vm_snapshot_data = serde_json::to_vec(&VmSnapshot {
            config: self.get_config(),
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            clock: self.saved_clock,
            state: Some(vm_state),
            #[cfg(all(feature = "kvm", target_arch = "x86_64"))]
            common_cpuid,
        })
        .map_err(|e| MigratableError::Snapshot(e.into()))?;

        vm_snapshot.add_snapshot(self.cpu_manager.lock().unwrap().snapshot()?);
        vm_snapshot.add_snapshot(self.memory_manager.lock().unwrap().snapshot()?);

        #[cfg(target_arch = "aarch64")]
        self.add_vgic_snapshot_section(&mut vm_snapshot)
            .map_err(|e| MigratableError::Snapshot(e.into()))?;

        vm_snapshot.add_snapshot(self.device_manager.lock().unwrap().snapshot()?);
        vm_snapshot.add_data_section(SnapshotDataSection {
            id: format!("{}-section", VM_SNAPSHOT_ID),
            snapshot: vm_snapshot_data,
        });

        event!("vm", "snapshotted");
        Ok(vm_snapshot)
    }

    fn restore(&mut self, snapshot: Snapshot) -> std::result::Result<(), MigratableError> {
        event!("vm", "restoring");

        let current_state = self
            .get_state()
            .map_err(|e| MigratableError::Restore(anyhow!("Could not get VM state: {:#?}", e)))?;
        let new_state = VmState::Paused;
        current_state.valid_transition(new_state).map_err(|e| {
            MigratableError::Restore(anyhow!("Could not restore VM state: {:#?}", e))
        })?;

        if let Some(memory_manager_snapshot) = snapshot.snapshots.get(MEMORY_MANAGER_SNAPSHOT_ID) {
            self.memory_manager
                .lock()
                .unwrap()
                .restore(*memory_manager_snapshot.clone())?;
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing memory manager snapshot"
            )));
        }

        if let Some(cpu_manager_snapshot) = snapshot.snapshots.get(CPU_MANAGER_SNAPSHOT_ID) {
            self.cpu_manager
                .lock()
                .unwrap()
                .restore(*cpu_manager_snapshot.clone())?;
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing CPU manager snapshot"
            )));
        }

        if let Some(device_manager_snapshot) = snapshot.snapshots.get(DEVICE_MANAGER_SNAPSHOT_ID) {
            self.device_manager
                .lock()
                .unwrap()
                .restore(*device_manager_snapshot.clone())?;
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing device manager snapshot"
            )));
        }

        #[cfg(target_arch = "aarch64")]
        self.restore_vgic_and_enable_interrupt(&snapshot)?;

        if let Some(device_manager_snapshot) = snapshot.snapshots.get(DEVICE_MANAGER_SNAPSHOT_ID) {
            self.device_manager
                .lock()
                .unwrap()
                .restore_devices(*device_manager_snapshot.clone())?;
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing device manager snapshot"
            )));
        }

        // Now we can start all vCPUs from here.
        self.cpu_manager
            .lock()
            .unwrap()
            .start_restored_vcpus()
            .map_err(|e| {
                MigratableError::Restore(anyhow!("Cannot start restored vCPUs: {:#?}", e))
            })?;

        self.setup_signal_handler().map_err(|e| {
            MigratableError::Restore(anyhow!("Could not setup signal handler: {:#?}", e))
        })?;
        self.setup_tty()
            .map_err(|e| MigratableError::Restore(anyhow!("Could not setup tty: {:#?}", e)))?;

        let mut state = self
            .state
            .try_write()
            .map_err(|e| MigratableError::Restore(anyhow!("Could not set VM state: {:#?}", e)))?;
        *state = new_state;

        event!("vm", "restored");
        Ok(())
    }
}

impl Transportable for Vm {
    fn send(
        &self,
        snapshot: &Snapshot,
        destination_url: &str,
    ) -> std::result::Result<(), MigratableError> {
        let mut vm_snapshot_path = url_to_path(destination_url)?;
        vm_snapshot_path.push(VM_SNAPSHOT_FILE);

        // Create the snapshot file
        let mut vm_snapshot_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(vm_snapshot_path)
            .map_err(|e| MigratableError::MigrateSend(e.into()))?;

        // Serialize and write the snapshot
        let vm_snapshot =
            serde_json::to_vec(snapshot).map_err(|e| MigratableError::MigrateSend(e.into()))?;

        vm_snapshot_file
            .write(&vm_snapshot)
            .map_err(|e| MigratableError::MigrateSend(e.into()))?;

        // Tell the memory manager to also send/write its own snapshot.
        if let Some(memory_manager_snapshot) = snapshot.snapshots.get(MEMORY_MANAGER_SNAPSHOT_ID) {
            self.memory_manager
                .lock()
                .unwrap()
                .send(&*memory_manager_snapshot.clone(), destination_url)?;
        } else {
            return Err(MigratableError::Restore(anyhow!(
                "Missing memory manager snapshot"
            )));
        }

        Ok(())
    }
}

impl Migratable for Vm {
    fn start_dirty_log(&mut self) -> std::result::Result<(), MigratableError> {
        self.memory_manager.lock().unwrap().start_dirty_log()?;
        self.device_manager.lock().unwrap().start_dirty_log()
    }

    fn stop_dirty_log(&mut self) -> std::result::Result<(), MigratableError> {
        self.memory_manager.lock().unwrap().stop_dirty_log()?;
        self.device_manager.lock().unwrap().stop_dirty_log()
    }

    fn dirty_log(&mut self) -> std::result::Result<MemoryRangeTable, MigratableError> {
        Ok(MemoryRangeTable::new_from_tables(vec![
            self.memory_manager.lock().unwrap().dirty_log()?,
            self.device_manager.lock().unwrap().dirty_log()?,
        ]))
    }

    fn complete_migration(&mut self) -> std::result::Result<(), MigratableError> {
        self.memory_manager.lock().unwrap().complete_migration()?;
        self.device_manager.lock().unwrap().complete_migration()
    }
}

#[cfg(all(feature = "kvm", target_arch = "x86_64"))]
#[cfg(test)]
mod tests {
    use super::*;

    fn test_vm_state_transitions(state: VmState) {
        match state {
            VmState::Created => {
                // Check the transitions from Created
                assert!(state.valid_transition(VmState::Created).is_err());
                assert!(state.valid_transition(VmState::Running).is_ok());
                assert!(state.valid_transition(VmState::Shutdown).is_err());
                assert!(state.valid_transition(VmState::Paused).is_ok());
            }
            VmState::Running => {
                // Check the transitions from Running
                assert!(state.valid_transition(VmState::Created).is_err());
                assert!(state.valid_transition(VmState::Running).is_err());
                assert!(state.valid_transition(VmState::Shutdown).is_ok());
                assert!(state.valid_transition(VmState::Paused).is_ok());
            }
            VmState::Shutdown => {
                // Check the transitions from Shutdown
                assert!(state.valid_transition(VmState::Created).is_err());
                assert!(state.valid_transition(VmState::Running).is_ok());
                assert!(state.valid_transition(VmState::Shutdown).is_err());
                assert!(state.valid_transition(VmState::Paused).is_err());
            }
            VmState::Paused => {
                // Check the transitions from Paused
                assert!(state.valid_transition(VmState::Created).is_err());
                assert!(state.valid_transition(VmState::Running).is_ok());
                assert!(state.valid_transition(VmState::Shutdown).is_ok());
                assert!(state.valid_transition(VmState::Paused).is_err());
            }
        }
    }

    #[test]
    fn test_vm_created_transitions() {
        test_vm_state_transitions(VmState::Created);
    }

    #[test]
    fn test_vm_running_transitions() {
        test_vm_state_transitions(VmState::Running);
    }

    #[test]
    fn test_vm_shutdown_transitions() {
        test_vm_state_transitions(VmState::Shutdown);
    }

    #[test]
    fn test_vm_paused_transitions() {
        test_vm_state_transitions(VmState::Paused);
    }
}

#[cfg(target_arch = "aarch64")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::GuestMemoryMmap;
    use arch::aarch64::fdt::create_fdt;
    use arch::aarch64::gic::kvm::create_gic;
    use arch::aarch64::layout;
    use arch::{DeviceType, MmioDeviceInfo};
    use vm_memory::GuestAddress;

    const LEN: u64 = 4096;

    #[test]
    fn test_create_fdt_with_devices() {
        let regions = vec![(
            GuestAddress(layout::RAM_64BIT_START),
            (layout::FDT_MAX_SIZE + 0x1000) as usize,
        )];
        let mem = GuestMemoryMmap::from_ranges(&regions).expect("Cannot initialize memory");

        let dev_info: HashMap<(DeviceType, std::string::String), MmioDeviceInfo> = [
            (
                (DeviceType::Serial, DeviceType::Serial.to_string()),
                MmioDeviceInfo {
                    addr: 0x00,
                    len: LEN,
                    irq: 33,
                },
            ),
            (
                (DeviceType::Virtio(1), "virtio".to_string()),
                MmioDeviceInfo {
                    addr: LEN,
                    len: LEN,
                    irq: 34,
                },
            ),
            (
                (DeviceType::Rtc, "rtc".to_string()),
                MmioDeviceInfo {
                    addr: 2 * LEN,
                    len: LEN,
                    irq: 35,
                },
            ),
        ]
        .iter()
        .cloned()
        .collect();

        let hv = hypervisor::new().unwrap();
        let vm = hv.create_vm().unwrap();
        let gic = create_gic(&vm, 1).unwrap();
        assert!(create_fdt(
            &mem,
            "console=tty0",
            vec![0],
            Some((0, 0, 0)),
            &dev_info,
            &*gic,
            &None,
            &(0x1_0000_0000, 0x1_0000),
            &BTreeMap::new(),
            None,
        )
        .is_ok())
    }
}

#[cfg(all(feature = "kvm", target_arch = "x86_64"))]
#[test]
pub fn test_vm() {
    use hypervisor::VmExit;
    use vm_memory::{Address, GuestMemory, GuestMemoryRegion};
    // This example based on https://lwn.net/Articles/658511/
    let code = [
        0xba, 0xf8, 0x03, /* mov $0x3f8, %dx */
        0x00, 0xd8, /* add %bl, %al */
        0x04, b'0', /* add $'0', %al */
        0xee, /* out %al, (%dx) */
        0xb0, b'\n', /* mov $'\n', %al */
        0xee,  /* out %al, (%dx) */
        0xf4,  /* hlt */
    ];

    let mem_size = 0x1000;
    let load_addr = GuestAddress(0x1000);
    let mem = GuestMemoryMmap::from_ranges(&[(load_addr, mem_size)]).unwrap();

    let hv = hypervisor::new().unwrap();
    let vm = hv.create_vm().expect("new VM creation failed");

    for (index, region) in mem.iter().enumerate() {
        let mem_region = vm.make_user_memory_region(
            index as u32,
            region.start_addr().raw_value(),
            region.len() as u64,
            region.as_ptr() as u64,
            false,
            false,
        );

        vm.create_user_memory_region(mem_region)
            .expect("Cannot configure guest memory");
    }
    mem.write_slice(&code, load_addr)
        .expect("Writing code to memory failed");

    let vcpu = vm.create_vcpu(0, None).expect("new Vcpu failed");

    let mut vcpu_sregs = vcpu.get_sregs().expect("get sregs failed");
    vcpu_sregs.cs.base = 0;
    vcpu_sregs.cs.selector = 0;
    vcpu.set_sregs(&vcpu_sregs).expect("set sregs failed");

    let mut vcpu_regs = vcpu.get_regs().expect("get regs failed");
    vcpu_regs.rip = 0x1000;
    vcpu_regs.rax = 2;
    vcpu_regs.rbx = 3;
    vcpu_regs.rflags = 2;
    vcpu.set_regs(&vcpu_regs).expect("set regs failed");

    loop {
        match vcpu.run().expect("run failed") {
            VmExit::IoOut(addr, data) => {
                println!(
                    "IO out -- addr: {:#x} data [{:?}]",
                    addr,
                    str::from_utf8(data).unwrap()
                );
            }
            VmExit::Reset => {
                println!("HLT");
                break;
            }
            r => panic!("unexpected exit reason: {:?}", r),
        }
    }
}
