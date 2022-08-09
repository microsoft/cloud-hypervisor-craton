// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::transport::{Error, QueueState, VirtioTransport};
use crate::{ActivateResult, GuestMemoryMmap};
use crate::{
    VirtioDevice, VirtioInterrupt, VirtioInterruptType, DEVICE_ACKNOWLEDGE, DEVICE_DRIVER,
    DEVICE_DRIVER_OK, DEVICE_FAILED, DEVICE_FEATURES_OK, DEVICE_INIT,
};
use anyhow::anyhow;
use byteorder::{ByteOrder, LittleEndian};
use libc::EFD_NONBLOCK;
use std::ops::Deref;
use std::result;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use versionize::{VersionMap, Versionize, VersionizeResult};
use versionize_derive::Versionize;
use virtio_queue::{Queue, QueueT};
use vm_device::interrupt::InterruptSourceGroup;
use vm_device::BusDevice;
use vm_memory::{GuestAddress, GuestAddressSpace, GuestMemoryAtomic};
use vm_migration::{
    Migratable, MigratableError, Pausable, Snapshot, Snapshottable, Transportable, VersionMapped,
};
use vm_virtio::AccessPlatform;
use vmm_sys_util::{errno::Result, eventfd::EventFd};

const VENDOR_ID: u32 = 0;

const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;
const MMIO_VERSION: u32 = 2;

const NOTIFY_REG_OFFSET: u32 = 0x50;
const INTERRUPT_STATUS_USED_RING: u32 = 0x1;
const INTERRUPT_STATUS_CONFIG_CHANGED: u32 = 0x2;

pub struct VirtioInterruptIntx {
    interrupt_status: Arc<AtomicUsize>,
    interrupt: Arc<dyn InterruptSourceGroup>,
}

impl VirtioInterruptIntx {
    pub fn new(
        interrupt_status: Arc<AtomicUsize>,
        interrupt: Arc<dyn InterruptSourceGroup>,
    ) -> Self {
        VirtioInterruptIntx {
            interrupt_status,
            interrupt,
        }
    }
}

impl VirtioInterrupt for VirtioInterruptIntx {
    fn trigger(&self, int_type: VirtioInterruptType) -> std::result::Result<(), std::io::Error> {
        let status = match int_type {
            VirtioInterruptType::Config => INTERRUPT_STATUS_CONFIG_CHANGED,
            VirtioInterruptType::Queue(_queue_index) => INTERRUPT_STATUS_USED_RING,
        };
        self.interrupt_status
            .fetch_or(status as usize, Ordering::SeqCst);

        self.interrupt.trigger(0)
    }
}

#[derive(Versionize)]
struct VirtioMmioDeviceState {
    device_activated: bool,
    features_select: u32,
    acked_features_select: u32,
    queue_select: u32,
    interrupt_status: usize,
    driver_status: u32,
    queues: Vec<QueueState>,
    shm_region_select: u32,
}

impl VersionMapped for VirtioMmioDeviceState {}

pub struct VirtioMmioDeviceActivator {
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    memory: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    device: Arc<Mutex<dyn VirtioDevice>>,
    device_activated: Arc<AtomicBool>,
    queues: Option<Vec<(usize, Queue, EventFd)>>,
    barrier: Option<Arc<Barrier>>,
    id: String,
}

impl VirtioMmioDeviceActivator {
    pub fn activate(&mut self) -> ActivateResult {
        self.device.lock().unwrap().activate(
            self.memory.take().unwrap(),
            self.interrupt.take().unwrap(),
            self.queues.take().unwrap(),
        )?;
        self.device_activated.store(true, Ordering::SeqCst);

        if let Some(barrier) = self.barrier.take() {
            info!("{}: Waiting for barrier", self.id);
            barrier.wait();
            info!("{}: Barrier released", self.id);
        }

        Ok(())
    }
}

/// Implements the
/// [MMIO](http://docs.oasis-open.org/virtio/virtio/v1.0/cs04/virtio-v1.0-cs04.html#x1-1090002)
/// transport for virtio devices.
///
/// This requires 3 points of installation to work with a VM:
///
/// 1. Mmio reads and writes must be sent to this device at what is referred to here as MMIO base.
/// 1. `Mmio::queue_evts` must be installed at `virtio::NOTIFY_REG_OFFSET` offset from the MMIO
/// base. Each event in the array must be signaled if the index is written at that offset.
/// 1. `Mmio::interrupt_evt` must signal an interrupt that the guest driver is listening to when it
/// is written to.
///
/// Typically one page (4096 bytes) of MMIO address space is sufficient to handle this transport
/// and inner virtio device.
pub struct VirtioMmioDevice {
    id: String,
    device: Arc<Mutex<dyn VirtioDevice>>,
    device_activated: Arc<AtomicBool>,

    features_select: u32,
    acked_features_select: u32,
    queue_select: u32,
    interrupt_status: Arc<AtomicUsize>,
    virtio_interrupt: Option<Arc<dyn VirtioInterrupt>>,
    driver_status: u32,
    config_generation: u32,
    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,
    memory: GuestMemoryAtomic<GuestMemoryMmap>,
    shm_region_select: u32,

    activate_evt: EventFd,
    activate_barrier: Arc<Barrier>,
    // Pending activations
    pending_activations: Arc<Mutex<Vec<VirtioMmioDeviceActivator>>>,
}

impl VirtioMmioDevice {
    /// Constructs a new MMIO transport for the given virtio device.
    pub fn new(
        id: String,
        memory: GuestMemoryAtomic<GuestMemoryMmap>,
        device: Arc<Mutex<dyn VirtioDevice>>,
        _access_platform: Option<Arc<dyn AccessPlatform>>,
        interrupt: Arc<dyn InterruptSourceGroup>,
        activate_evt: EventFd,
        pending_activations: Arc<Mutex<Vec<VirtioMmioDeviceActivator>>>,
    ) -> Result<VirtioMmioDevice> {
        let device_clone = device.clone();
        let locked_device = device_clone.lock().unwrap();
        let mut queue_evts = Vec::new();
        for _ in locked_device.queue_max_sizes().iter() {
            queue_evts.push(EventFd::new(EFD_NONBLOCK)?)
        }
        let queues = locked_device
            .queue_max_sizes()
            .iter()
            .map(|&s| Queue::new(s).unwrap())
            .collect();

        let interrupt_status = Arc::new(AtomicUsize::new(0));
        let virtio_interrupt: Option<Arc<dyn VirtioInterrupt>> = Some(Arc::new(
            VirtioInterruptIntx::new(interrupt_status.clone(), interrupt),
        ));

        Ok(VirtioMmioDevice {
            id,
            device,
            device_activated: Arc::new(AtomicBool::new(false)),
            features_select: 0,
            acked_features_select: 0,
            queue_select: 0,
            interrupt_status,
            virtio_interrupt,
            driver_status: DEVICE_INIT,
            config_generation: 0,
            queues,
            queue_evts,
            memory,
            shm_region_select: 0,
            activate_evt,
            activate_barrier: Arc::new(Barrier::new(2)),
            pending_activations,
        })
    }

    fn state(&self) -> VirtioMmioDeviceState {
        VirtioMmioDeviceState {
            device_activated: self.device_activated.load(Ordering::Acquire),
            features_select: self.features_select,
            acked_features_select: self.acked_features_select,
            queue_select: self.queue_select,
            interrupt_status: self.interrupt_status.load(Ordering::SeqCst),
            driver_status: self.driver_status,
            shm_region_select: self.shm_region_select,
            queues: self
                .queues
                .iter()
                .map(|q| QueueState {
                    max_size: q.max_size(),
                    size: q.size(),
                    ready: q.ready(),
                    desc_table: q.desc_table(),
                    avail_ring: q.avail_ring(),
                    used_ring: q.used_ring(),
                })
                .collect(),
        }
    }

    fn set_state(&mut self, state: &VirtioMmioDeviceState) -> std::result::Result<(), Error> {
        self.device_activated
            .store(state.device_activated, Ordering::Release);
        self.features_select = state.features_select;
        self.acked_features_select = state.acked_features_select;
        self.queue_select = state.queue_select;
        self.interrupt_status
            .store(state.interrupt_status, Ordering::SeqCst);
        info!("MMIO: set_state set status  {:?}", state.driver_status);
        self.driver_status = state.driver_status;

        // Update virtqueues indexes for both available and used rings.
        for (i, queue) in self.queues.iter_mut().enumerate() {
            //queue.max_size = state.queues[i].max_size;
            queue.set_size(state.queues[i].size);
            queue.set_ready(state.queues[i].ready);
            queue
                .try_set_desc_table_address(GuestAddress(state.queues[i].desc_table))
                .unwrap();
            queue
                .try_set_avail_ring_address(GuestAddress(state.queues[i].avail_ring))
                .unwrap();
            queue
                .try_set_used_ring_address(GuestAddress(state.queues[i].used_ring))
                .unwrap();
            queue.set_next_avail(
                queue
                    .used_idx(self.memory.memory().deref(), Ordering::Acquire)
                    .map_err(Error::QueueRingIndex)?
                    .0,
            );
            queue.set_next_used(
                queue
                    .used_idx(self.memory.memory().deref(), Ordering::Acquire)
                    .map_err(Error::QueueRingIndex)?
                    .0,
            );
        }

        self.shm_region_select = state.shm_region_select;

        Ok(())
    }

    /// Gets the list of queue events that must be triggered whenever the VM writes to
    /// `virtio::NOTIFY_REG_OFFSET` past the MMIO base. Each event must be triggered when the
    /// value being written equals the index of the event in this list.
    fn queue_evts(&self) -> &[EventFd] {
        self.queue_evts.as_slice()
    }

    fn is_driver_ready(&self) -> bool {
        let ready_bits = DEVICE_ACKNOWLEDGE | DEVICE_DRIVER | DEVICE_DRIVER_OK | DEVICE_FEATURES_OK;
        self.driver_status == ready_bits && self.driver_status & DEVICE_FAILED == 0
    }

    /// Determines if the driver has requested the device (re)init / reset itself
    fn is_driver_init(&self) -> bool {
        self.driver_status == DEVICE_INIT
    }

    fn with_queue<U, F>(&self, queues: &[Queue], f: F) -> Option<U>
    where
        F: FnOnce(&Queue) -> U,
    {
        queues.get(self.queue_select as usize).map(f)
    }

    fn with_queue_mut<F: FnOnce(&mut Queue)>(&mut self, f: F) {
        if let Some(queue) = self.queues.get_mut(self.queue_select as usize) {
            f(queue);
        }
    }

    pub fn assign_interrupt(&mut self, interrupt: Arc<dyn InterruptSourceGroup>) {
        self.virtio_interrupt = Some(Arc::new(VirtioInterruptIntx::new(
            self.interrupt_status.clone(),
            interrupt,
        )));
    }
    fn prepare_activator(&mut self, barrier: Option<Arc<Barrier>>) -> VirtioMmioDeviceActivator {
        let mut queues = Vec::new();

        for (queue_index, queue) in self.queues.iter().enumerate() {
            if !queue.ready() {
                continue;
            }

            if !queue.is_valid(self.memory.memory().deref()) {
                error!("Queue {} is not valid", queue_index);
            }

            queues.push((
                queue_index,
                vm_virtio::clone_queue(queue),
                self.queue_evts[queue_index].try_clone().unwrap(),
            ));
        }

        VirtioMmioDeviceActivator {
            interrupt: self.virtio_interrupt.take(),
            memory: Some(self.memory.clone()),
            device: self.device.clone(),
            queues: Some(queues),
            device_activated: self.device_activated.clone(),
            barrier,
            id: self.id.clone(),
        }
    }

    fn activate(&mut self) -> ActivateResult {
        self.prepare_activator(None).activate()
    }

    pub fn maybe_activate(&mut self) {
        if self.needs_activation() {
            self.activate().expect("Failed to activate device");
            self.device_activated.store(true, Ordering::SeqCst);
            info!("{}: Waiting for barrier", self.id);
            self.activate_barrier.wait();
            info!("{}: Barrier released", self.id);
        } else {
            info!("{}: Device does not need activation", self.id)
        }
    }

    fn needs_activation(&self) -> bool {
        !self.device_activated.load(Ordering::SeqCst) && self.is_driver_ready()
    }
}

impl VirtioTransport for VirtioMmioDevice {
    fn ioeventfds(&self, base_addr: u64) -> Vec<(&EventFd, u64)> {
        let notify_base = base_addr + u64::from(NOTIFY_REG_OFFSET);
        self.queue_evts()
            .iter()
            .map(|event| (event, notify_base))
            .collect()
    }
}

impl BusDevice for VirtioMmioDevice {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        match offset {
            0x00..=0xff if data.len() == 4 => {
                let v = match offset {
                    0x0 => MMIO_MAGIC_VALUE,
                    0x04 => MMIO_VERSION,
                    0x08 => self.device.lock().unwrap().device_type(),
                    0x0c => VENDOR_ID, // vendor id
                    0x10 => {
                        if self.features_select < 2 {
                            (self.device.lock().unwrap().features() >> (self.features_select * 32))
                                as u32
                        } else {
                            0
                        }
                    }
                    0x34 => self
                        .with_queue(&self.queues, |q| u32::from(q.max_size()))
                        .unwrap_or(0u32),
                    0x44 => self
                        .with_queue(&self.queues, |q| q.ready() as u32)
                        .unwrap_or(0u32),
                    0x60 => self.interrupt_status.load(Ordering::SeqCst) as u32,
                    0x70 => self.driver_status,
                    0xfc => self.config_generation,
                    0xb0..=0xbc => {
                        // For no SHM region or invalid region the kernel looks for length of -1
                        let (shm_offset, shm_len) = if let Some(shm_regions) =
                            self.device.lock().unwrap().get_shm_regions()
                        {
                            if self.shm_region_select as usize > shm_regions.region_list.len() {
                                (0, !0 as u64)
                            } else {
                                (
                                    shm_regions.region_list[self.shm_region_select as usize].offset
                                        + shm_regions.addr.0,
                                    shm_regions.region_list[self.shm_region_select as usize].len,
                                )
                            }
                        } else {
                            (0, !0 as u64)
                        };
                        match offset {
                            0xb0 => shm_len as u32,
                            0xb4 => (shm_len >> 32) as u32,
                            0xb8 => shm_offset as u32,
                            0xbc => (shm_offset >> 32) as u32,
                            _ => {
                                error!("invalid shm region offset");
                                0
                            }
                        }
                    }
                    _ => {
                        warn!("unknown virtio mmio register read: 0x{:x}", offset);
                        return;
                    }
                };
                LittleEndian::write_u32(data, v);
            }
            0x100..=0xfff => self
                .device
                .lock()
                .unwrap()
                .read_config(offset - 0x100, data),
            _ => {
                warn!(
                    "invalid virtio mmio read: 0x{:x}:0x{:x}",
                    offset,
                    data.len()
                );
            }
        };
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        match offset {
            0x00..=0xff if data.len() == 4 => {
                let v = LittleEndian::read_u32(data);
                match offset {
                    0x14 => self.features_select = v,
                    0x20 => {
                        if self.acked_features_select < 2 {
                            self.device
                                .lock()
                                .unwrap()
                                .ack_features(u64::from(v) << (self.acked_features_select * 32));
                        } else {
                            warn!(
                                "invalid ack_features (page {}, value 0x{:x})",
                                self.acked_features_select, v
                            );
                        }
                    }
                    0x24 => self.acked_features_select = v,
                    0x30 => self.queue_select = v,
                    0x38 => self.with_queue_mut(|q| q.set_size(v as u16)),
                    0x44 => self.with_queue_mut(|q| q.set_ready(v == 1)),
                    0x64 => {
                        self.interrupt_status
                            .fetch_and(!(v as usize), Ordering::SeqCst);
                    }
                    0x70 => self.driver_status = v,
                    0x80 => self.with_queue_mut(|q| q.set_desc_table_address(Some(v), None)),
                    0x84 => self.with_queue_mut(|q| q.set_desc_table_address(None, Some(v))),
                    0x90 => self.with_queue_mut(|q| q.set_avail_ring_address(Some(v), None)),
                    0x94 => self.with_queue_mut(|q| q.set_avail_ring_address(None, Some(v))),
                    0xa0 => self.with_queue_mut(|q| q.set_used_ring_address(Some(v), None)),
                    0xa4 => self.with_queue_mut(|q| q.set_used_ring_address(None, Some(v))),
                    0xac => self.shm_region_select = v,
                    _ => {
                        warn!("unknown virtio mmio register write: 0x{:x}", offset);
                    }
                }
            }
            0x100..=0xfff => {
                self.device
                    .lock()
                    .unwrap()
                    .write_config(offset - 0x100, data);
            }
            _ => {
                warn!(
                    "invalid virtio mmio write: 0x{:x}:0x{:x}",
                    offset,
                    data.len()
                );
            }
        }

        // Try and activate the device if the driver status has changed
        if self.needs_activation() {
            let barrier = Arc::new(Barrier::new(2));
            let activator = self.prepare_activator(Some(barrier.clone()));
            self.pending_activations.lock().unwrap().push(activator);
            info!(
                "{}: Needs activation; writing to activate event fd",
                self.id
            );
            self.activate_evt.write(1).ok();
            info!("{}: Needs activation; returning barrier", self.id);
            return Some(barrier);
        }

        // Device has been reset by the driver
        if self.device_activated.load(Ordering::SeqCst) && self.is_driver_init() {
            let mut device = self.device.lock().unwrap();
            if let Some(virtio_interrupt) = device.reset() {
                // Upon reset the device returns its interrupt EventFD
                self.virtio_interrupt = Some(virtio_interrupt);
                self.device_activated.store(false, Ordering::SeqCst);

                // Reset queue readiness (changes queue_enable), queue sizes
                // and selected_queue as per spec for reset
                self.queues.iter_mut().for_each(Queue::reset);
                self.queue_select = 0;
            } else {
                error!("Attempt to reset device when not implemented in underlying device");
                self.driver_status = crate::DEVICE_FAILED as u32;
            }
        }

        None
    }
}

impl Pausable for VirtioMmioDevice {
    fn pause(&mut self) -> result::Result<(), MigratableError> {
        Ok(())
    }

    fn resume(&mut self) -> result::Result<(), MigratableError> {
        Ok(())
    }
}

impl Snapshottable for VirtioMmioDevice {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn snapshot(&mut self) -> std::result::Result<Snapshot, MigratableError> {
        let virtio_mmio_dev_snapshot = Snapshot::new_from_versioned_state(&self.id, &self.state())?;

        Ok(virtio_mmio_dev_snapshot)
    }

    fn restore(&mut self, snapshot: Snapshot) -> std::result::Result<(), MigratableError> {
        if let Some(virtio_mmio_dev_section) =
            snapshot.snapshot_data.get(&format!("{}-section", self.id))
        {
            // First restore the status of the virtqueues.
            self.set_state(&virtio_mmio_dev_section.to_versioned_state()?)
                .map_err(|e| {
                    MigratableError::Restore(anyhow!(
                        "Could not restore VIRTIO_MMIO_DEVICE state {:?}",
                        e
                    ))
                })?;

            // Then we can activate the device, as we know at this point that
            // the virtqueues are in the right state and the device is ready
            // to be activated, which will spawn each virtio worker thread.
            if self.device_activated.load(Ordering::SeqCst) && self.is_driver_ready() {
                self.activate().map_err(|e| {
                    MigratableError::Restore(anyhow!("Failed activating the device: {:?}", e))
                })?;
            }

            return Ok(());
        }

        Err(MigratableError::Restore(anyhow!(
            "Could not find VIRTIO_MMIO_DEVICE snapshot section"
        )))
    }
}

impl Transportable for VirtioMmioDevice {}
impl Migratable for VirtioMmioDevice {}
