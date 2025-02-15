// Copyright 2018 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE-BSD-3-Clause file.
//
// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0 AND BSD-3-Clause

extern crate devices;
#[cfg(feature = "pci_support")]
extern crate pci;
extern crate vm_allocator;
extern crate vm_memory;
extern crate vmm_sys_util;

use libc::EFD_NONBLOCK;
use std::any::Any;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use devices::BusDevice;
use pci::{
    BarReprogrammingParams, InterruptDelivery, InterruptParameters, MsixCap, MsixConfig,
    PciBarConfiguration, PciBarRegionType, PciCapability, PciCapabilityID, PciClassCode,
    PciConfiguration, PciDevice, PciDeviceError, PciHeaderType, PciInterruptPin,
    PciMassStorageSubclass, PciNetworkControllerSubclass, PciSubclass,
};
use vm_allocator::SystemAllocator;
use vm_memory::{Address, ByteValued, GuestAddress, GuestMemoryMmap, GuestUsize, Le32};
use vmm_sys_util::{errno::Result, eventfd::EventFd};

use super::VirtioPciCommonConfig;
use crate::transport::VirtioTransport;
use crate::{
    Queue, VirtioDevice, VirtioDeviceType, VirtioInterrupt, VirtioInterruptType,
    VirtioIommuRemapping, DEVICE_ACKNOWLEDGE, DEVICE_DRIVER, DEVICE_DRIVER_OK, DEVICE_FAILED,
    DEVICE_FEATURES_OK, DEVICE_INIT, INTERRUPT_STATUS_CONFIG_CHANGED, INTERRUPT_STATUS_USED_RING,
};

#[allow(clippy::enum_variant_names)]
enum PciCapabilityType {
    CommonConfig = 1,
    NotifyConfig = 2,
    IsrConfig = 3,
    DeviceConfig = 4,
    PciConfig = 5,
    SharedMemoryConfig = 8,
}

#[allow(dead_code)]
#[repr(packed)]
#[derive(Clone, Copy, Default)]
struct VirtioPciCap {
    cap_len: u8,      // Generic PCI field: capability length
    cfg_type: u8,     // Identifies the structure.
    pci_bar: u8,      // Where to find it.
    id: u8,           // Multiple capabilities of the same type
    padding: [u8; 2], // Pad to full dword.
    offset: Le32,     // Offset within bar.
    length: Le32,     // Length of the structure, in bytes.
}
// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for VirtioPciCap {}

impl PciCapability for VirtioPciCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

const VIRTIO_PCI_CAP_LEN_OFFSET: u8 = 2;

impl VirtioPciCap {
    pub fn new(cfg_type: PciCapabilityType, pci_bar: u8, offset: u32, length: u32) -> Self {
        VirtioPciCap {
            cap_len: (std::mem::size_of::<VirtioPciCap>() as u8) + VIRTIO_PCI_CAP_LEN_OFFSET,
            cfg_type: cfg_type as u8,
            pci_bar,
            id: 0,
            padding: [0; 2],
            offset: Le32::from(offset),
            length: Le32::from(length),
        }
    }
}

#[allow(dead_code)]
#[repr(packed)]
#[derive(Clone, Copy, Default)]
struct VirtioPciNotifyCap {
    cap: VirtioPciCap,
    notify_off_multiplier: Le32,
}
// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for VirtioPciNotifyCap {}

impl PciCapability for VirtioPciNotifyCap {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciNotifyCap {
    pub fn new(
        cfg_type: PciCapabilityType,
        pci_bar: u8,
        offset: u32,
        length: u32,
        multiplier: Le32,
    ) -> Self {
        VirtioPciNotifyCap {
            cap: VirtioPciCap {
                cap_len: (std::mem::size_of::<VirtioPciNotifyCap>() as u8)
                    + VIRTIO_PCI_CAP_LEN_OFFSET,
                cfg_type: cfg_type as u8,
                pci_bar,
                id: 0,
                padding: [0; 2],
                offset: Le32::from(offset),
                length: Le32::from(length),
            },
            notify_off_multiplier: multiplier,
        }
    }
}

#[allow(dead_code)]
#[repr(packed)]
#[derive(Clone, Copy, Default)]
struct VirtioPciCap64 {
    cap: VirtioPciCap,
    offset_hi: Le32,
    length_hi: Le32,
}
// It is safe to implement ByteValued. All members are simple numbers and any value is valid.
unsafe impl ByteValued for VirtioPciCap64 {}

impl PciCapability for VirtioPciCap64 {
    fn bytes(&self) -> &[u8] {
        self.as_slice()
    }

    fn id(&self) -> PciCapabilityID {
        PciCapabilityID::VendorSpecific
    }
}

impl VirtioPciCap64 {
    pub fn new(cfg_type: PciCapabilityType, pci_bar: u8, id: u8, offset: u64, length: u64) -> Self {
        VirtioPciCap64 {
            cap: VirtioPciCap {
                cap_len: (std::mem::size_of::<VirtioPciCap64>() as u8) + VIRTIO_PCI_CAP_LEN_OFFSET,
                cfg_type: cfg_type as u8,
                pci_bar,
                id,
                padding: [0; 2],
                offset: Le32::from(offset as u32),
                length: Le32::from(length as u32),
            },
            offset_hi: Le32::from((offset >> 32) as u32),
            length_hi: Le32::from((length >> 32) as u32),
        }
    }
}

#[allow(dead_code)]
#[derive(Copy, Clone)]
pub enum PciVirtioSubclass {
    NonTransitionalBase = 0xff,
}

impl PciSubclass for PciVirtioSubclass {
    fn get_register_value(&self) -> u8 {
        *self as u8
    }
}

// Allocate one bar for the structs pointed to by the capability structures.
// As per the PCI specification, because the same BAR shares MSI-X and non
// MSI-X structures, it is recommended to use 8KiB alignment for all those
// structures.
const COMMON_CONFIG_BAR_OFFSET: u64 = 0x0000;
const COMMON_CONFIG_SIZE: u64 = 56;
const ISR_CONFIG_BAR_OFFSET: u64 = 0x2000;
const ISR_CONFIG_SIZE: u64 = 1;
const DEVICE_CONFIG_BAR_OFFSET: u64 = 0x4000;
const DEVICE_CONFIG_SIZE: u64 = 0x1000;
const NOTIFICATION_BAR_OFFSET: u64 = 0x6000;
const NOTIFICATION_SIZE: u64 = 0x1000;
const MSIX_TABLE_BAR_OFFSET: u64 = 0x8000;
// The size is 256KiB because the table can hold up to 2048 entries, with each
// entry being 128 bits (4 DWORDS).
const MSIX_TABLE_SIZE: u64 = 0x40000;
const MSIX_PBA_BAR_OFFSET: u64 = 0x48000;
// The size is 2KiB because the Pending Bit Array has one bit per vector and it
// can support up to 2048 vectors.
const MSIX_PBA_SIZE: u64 = 0x800;
// The BAR size must be a power of 2.
const CAPABILITY_BAR_SIZE: u64 = 0x80000;

const NOTIFY_OFF_MULTIPLIER: u32 = 4; // A dword per notification address.

const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_ID_BASE: u16 = 0x1040; // Add to device type to get device ID.

pub struct VirtioPciDevice {
    // PCI configuration registers.
    configuration: PciConfiguration,

    // virtio PCI common configuration
    common_config: VirtioPciCommonConfig,

    // MSI-X config
    msix_config: Option<Arc<Mutex<MsixConfig>>>,

    // Number of MSI-X vectors
    msix_num: u16,

    // Virtio device reference and status
    device: Box<dyn VirtioDevice>,
    device_activated: bool,

    // PCI interrupts.
    interrupt_status: Arc<AtomicUsize>,
    interrupt_cb: Option<Arc<VirtioInterrupt>>,

    // virtio queues
    queues: Vec<Queue>,
    queue_evts: Vec<EventFd>,

    // Guest memory
    memory: Option<Arc<RwLock<GuestMemoryMmap>>>,

    // Setting PCI BAR
    settings_bar: u8,

    // Whether to use 64-bit bar location or 32-bit
    use_64bit_bar: bool,
}

impl VirtioPciDevice {
    /// Constructs a new PCI transport for the given virtio device.
    pub fn new(
        memory: Arc<RwLock<GuestMemoryMmap>>,
        device: Box<dyn VirtioDevice>,
        msix_num: u16,
        iommu_mapping_cb: Option<Arc<VirtioIommuRemapping>>,
    ) -> Result<Self> {
        let mut queue_evts = Vec::new();
        for _ in device.queue_max_sizes().iter() {
            queue_evts.push(EventFd::new(EFD_NONBLOCK)?)
        }
        let queues = device
            .queue_max_sizes()
            .iter()
            .map(|&s| {
                let mut queue = Queue::new(s);
                queue.iommu_mapping_cb = iommu_mapping_cb.clone();
                queue
            })
            .collect();

        let pci_device_id = VIRTIO_PCI_DEVICE_ID_BASE + device.device_type() as u16;

        let (msix_config, msix_config_clone) = if msix_num > 0 {
            let msix_config = Arc::new(Mutex::new(MsixConfig::new(msix_num)));
            let msix_config_clone = msix_config.clone();
            (Some(msix_config), Some(msix_config_clone))
        } else {
            (None, None)
        };

        // All device types *except* virtio block devices should be allocated a 64-bit bar
        // The block devices should be given a 32-bit BAR so that they are easily accessible
        // to firmware without requiring excessive identity mapping.
        let mut use_64bit_bar = true;
        let (class, subclass) = match VirtioDeviceType::from(device.device_type()) {
            VirtioDeviceType::TYPE_NET => (
                PciClassCode::NetworkController,
                &PciNetworkControllerSubclass::EthernetController as &dyn PciSubclass,
            ),
            VirtioDeviceType::TYPE_BLOCK => {
                use_64bit_bar = false;
                (
                    PciClassCode::MassStorage,
                    &PciMassStorageSubclass::MassStorage as &dyn PciSubclass,
                )
            }
            _ => (
                PciClassCode::Other,
                &PciVirtioSubclass::NonTransitionalBase as &dyn PciSubclass,
            ),
        };

        let configuration = PciConfiguration::new(
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
            class,
            subclass,
            None,
            PciHeaderType::Device,
            VIRTIO_PCI_VENDOR_ID,
            pci_device_id,
            msix_config_clone,
        );

        Ok(VirtioPciDevice {
            configuration,
            common_config: VirtioPciCommonConfig {
                driver_status: 0,
                config_generation: 0,
                device_feature_select: 0,
                driver_feature_select: 0,
                queue_select: 0,
                msix_config: Arc::new(AtomicU16::new(0)),
            },
            msix_config,
            msix_num,
            device,
            device_activated: false,
            interrupt_status: Arc::new(AtomicUsize::new(0)),
            interrupt_cb: None,
            queues,
            queue_evts,
            memory: Some(memory),
            settings_bar: 0,
            use_64bit_bar,
        })
    }

    /// Gets the list of queue events that must be triggered whenever the VM writes to
    /// `virtio::NOTIFY_REG_OFFSET` past the MMIO base. Each event must be triggered when the
    /// value being written equals the index of the event in this list.
    fn queue_evts(&self) -> &[EventFd] {
        self.queue_evts.as_slice()
    }

    fn is_driver_ready(&self) -> bool {
        let ready_bits =
            (DEVICE_ACKNOWLEDGE | DEVICE_DRIVER | DEVICE_DRIVER_OK | DEVICE_FEATURES_OK) as u8;
        self.common_config.driver_status == ready_bits
            && self.common_config.driver_status & DEVICE_FAILED as u8 == 0
    }

    /// Determines if the driver has requested the device (re)init / reset itself
    fn is_driver_init(&self) -> bool {
        self.common_config.driver_status == DEVICE_INIT as u8
    }

    fn are_queues_valid(&self) -> bool {
        if let Some(mem) = self.memory.as_ref() {
            self.queues.iter().all(|q| q.is_valid(&mem.read().unwrap()))
        } else {
            false
        }
    }

    pub fn config_bar_addr(&self) -> u64 {
        self.configuration.get_bar_addr(self.settings_bar as usize)
    }

    fn add_pci_capabilities(
        &mut self,
        settings_bar: u8,
    ) -> std::result::Result<(), PciDeviceError> {
        // Add pointers to the different configuration structures from the PCI capabilities.
        let common_cap = VirtioPciCap::new(
            PciCapabilityType::CommonConfig,
            settings_bar,
            COMMON_CONFIG_BAR_OFFSET as u32,
            COMMON_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&common_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let isr_cap = VirtioPciCap::new(
            PciCapabilityType::IsrConfig,
            settings_bar,
            ISR_CONFIG_BAR_OFFSET as u32,
            ISR_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&isr_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        // TODO(dgreid) - set based on device's configuration size?
        let device_cap = VirtioPciCap::new(
            PciCapabilityType::DeviceConfig,
            settings_bar,
            DEVICE_CONFIG_BAR_OFFSET as u32,
            DEVICE_CONFIG_SIZE as u32,
        );
        self.configuration
            .add_capability(&device_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        let notify_cap = VirtioPciNotifyCap::new(
            PciCapabilityType::NotifyConfig,
            settings_bar,
            NOTIFICATION_BAR_OFFSET as u32,
            NOTIFICATION_SIZE as u32,
            Le32::from(NOTIFY_OFF_MULTIPLIER),
        );
        self.configuration
            .add_capability(&notify_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        //TODO(dgreid) - How will the configuration_cap work?
        let configuration_cap = VirtioPciCap::new(PciCapabilityType::PciConfig, 0, 0, 0);
        self.configuration
            .add_capability(&configuration_cap)
            .map_err(PciDeviceError::CapabilitiesSetup)?;

        if self.msix_config.is_some() {
            let msix_cap = MsixCap::new(
                settings_bar,
                self.msix_num,
                MSIX_TABLE_BAR_OFFSET as u32,
                settings_bar,
                MSIX_PBA_BAR_OFFSET as u32,
            );
            self.configuration
                .add_capability(&msix_cap)
                .map_err(PciDeviceError::CapabilitiesSetup)?;
        }

        self.settings_bar = settings_bar;
        Ok(())
    }
}

impl VirtioTransport for VirtioPciDevice {
    fn ioeventfds(&self, base_addr: u64) -> Vec<(&EventFd, u64)> {
        let notify_base = base_addr + NOTIFICATION_BAR_OFFSET;
        self.queue_evts()
            .iter()
            .enumerate()
            .map(|(i, event)| {
                (
                    event,
                    notify_base + i as u64 * u64::from(NOTIFY_OFF_MULTIPLIER),
                )
            })
            .collect()
    }
}

impl PciDevice for VirtioPciDevice {
    fn assign_pin_irq(
        &mut self,
        irq_cb: Arc<InterruptDelivery>,
        irq_num: u32,
        irq_pin: PciInterruptPin,
    ) {
        self.configuration.set_irq(irq_num as u8, irq_pin);

        let interrupt_status = self.interrupt_status.clone();
        let cb = Arc::new(Box::new(
            move |int_type: &VirtioInterruptType, _queue: Option<&Queue>| {
                let param = InterruptParameters { msix: None };

                let status = match int_type {
                    VirtioInterruptType::Config => INTERRUPT_STATUS_CONFIG_CHANGED,
                    VirtioInterruptType::Queue => INTERRUPT_STATUS_USED_RING,
                };
                interrupt_status.fetch_or(status as usize, Ordering::SeqCst);

                (irq_cb)(param)
            },
        ) as VirtioInterrupt);

        self.interrupt_cb = Some(cb);
    }

    fn assign_msix(&mut self, msi_cb: Arc<InterruptDelivery>) {
        if let Some(msix_config) = &self.msix_config {
            msix_config
                .lock()
                .unwrap()
                .register_interrupt_cb(msi_cb.clone());

            let msix_config_clone = msix_config.clone();

            let common_config_msi_vector = self.common_config.msix_config.clone();
            let cb = Arc::new(Box::new(
                move |int_type: &VirtioInterruptType, queue: Option<&Queue>| {
                    let vector = match int_type {
                        VirtioInterruptType::Config => {
                            common_config_msi_vector.load(Ordering::SeqCst)
                        }
                        VirtioInterruptType::Queue => {
                            if let Some(q) = queue {
                                q.vector
                            } else {
                                0
                            }
                        }
                    };

                    let config = &mut msix_config_clone.lock().unwrap();
                    let entry = &config.table_entries[vector as usize];

                    // If MSI-X interrupts are not enabled for this device, then simply
                    // ignore the interrupt.
                    if !config.enabled() {
                        return Ok(());
                    }

                    // In case the vector control register associated with the entry
                    // has its first bit set, this means the vector is masked and the
                    // device should not inject the interrupt.
                    // Instead, the Pending Bit Array table is updated to reflect there
                    // is a pending interrupt for this specific vector.
                    if config.masked() || entry.masked() {
                        config.set_pba_bit(vector, false);
                        return Ok(());
                    }

                    (msi_cb)(InterruptParameters { msix: Some(entry) })
                },
            ) as VirtioInterrupt);

            self.interrupt_cb = Some(cb);
        }
    }

    fn write_config_register(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        self.configuration
            .write_config_register(reg_idx, offset, data);
    }

    fn read_config_register(&self, reg_idx: usize) -> u32 {
        self.configuration.read_reg(reg_idx)
    }

    fn detect_bar_reprogramming(
        &mut self,
        reg_idx: usize,
        data: &[u8],
    ) -> Option<BarReprogrammingParams> {
        self.configuration.detect_bar_reprogramming(reg_idx, data)
    }

    fn allocate_bars(
        &mut self,
        allocator: &mut SystemAllocator,
    ) -> std::result::Result<Vec<(GuestAddress, GuestUsize, PciBarRegionType)>, PciDeviceError>
    {
        let mut ranges = Vec::new();

        // Allocate the virtio-pci capability BAR.
        // See http://docs.oasis-open.org/virtio/virtio/v1.0/cs04/virtio-v1.0-cs04.html#x1-740004
        let (virtio_pci_bar_addr, region_type) = if self.use_64bit_bar {
            let region_type = PciBarRegionType::Memory64BitRegion;
            let addr = allocator
                .allocate_mmio_addresses(None, CAPABILITY_BAR_SIZE, None)
                .ok_or(PciDeviceError::IoAllocationFailed(CAPABILITY_BAR_SIZE))?;
            ranges.push((addr, CAPABILITY_BAR_SIZE, region_type));
            (addr, region_type)
        } else {
            let region_type = PciBarRegionType::Memory32BitRegion;
            let addr = allocator
                .allocate_mmio_hole_addresses(None, CAPABILITY_BAR_SIZE, None)
                .ok_or(PciDeviceError::IoAllocationFailed(CAPABILITY_BAR_SIZE))?;
            ranges.push((addr, CAPABILITY_BAR_SIZE, region_type));
            (addr, region_type)
        };

        let config = PciBarConfiguration::default()
            .set_register_index(0)
            .set_address(virtio_pci_bar_addr.raw_value())
            .set_size(CAPABILITY_BAR_SIZE)
            .set_region_type(region_type);
        let virtio_pci_bar =
            self.configuration.add_pci_bar(&config).map_err(|e| {
                PciDeviceError::IoRegistrationFailed(virtio_pci_bar_addr.raw_value(), e)
            })? as u8;

        // Once the BARs are allocated, the capabilities can be added to the PCI configuration.
        self.add_pci_capabilities(virtio_pci_bar)?;

        // Allocate a dedicated BAR if there are some shared memory regions.
        if let Some(shm_list) = self.device.get_shm_regions() {
            let config = PciBarConfiguration::default()
                .set_register_index(2)
                .set_address(shm_list.addr.raw_value())
                .set_size(shm_list.len);
            let virtio_pci_shm_bar =
                self.configuration.add_pci_bar(&config).map_err(|e| {
                    PciDeviceError::IoRegistrationFailed(shm_list.addr.raw_value(), e)
                })? as u8;

            for (idx, shm) in shm_list.region_list.iter().enumerate() {
                let shm_cap = VirtioPciCap64::new(
                    PciCapabilityType::SharedMemoryConfig,
                    virtio_pci_shm_bar,
                    idx as u8,
                    shm.offset,
                    shm.len,
                );
                self.configuration
                    .add_capability(&shm_cap)
                    .map_err(PciDeviceError::CapabilitiesSetup)?;
            }
        }

        Ok(ranges)
    }

    fn read_bar(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        match offset {
            o if o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE => self.common_config.read(
                o - COMMON_CONFIG_BAR_OFFSET,
                data,
                &mut self.queues,
                self.device.as_mut(),
            ),
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get_mut(0) {
                    // Reading this register resets it to 0.
                    *v = self.interrupt_status.swap(0, Ordering::SeqCst) as u8;
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.read_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioeventfds.
            }
            o if MSIX_TABLE_BAR_OFFSET <= o && o < MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE => {
                if let Some(msix_config) = &self.msix_config {
                    msix_config
                        .lock()
                        .unwrap()
                        .read_table(o - MSIX_TABLE_BAR_OFFSET, data);
                }
            }
            o if MSIX_PBA_BAR_OFFSET <= o && o < MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE => {
                if let Some(msix_config) = &self.msix_config {
                    msix_config
                        .lock()
                        .unwrap()
                        .read_pba(o - MSIX_PBA_BAR_OFFSET, data);
                }
            }
            _ => (),
        }
    }

    fn write_bar(&mut self, _base: u64, offset: u64, data: &[u8]) {
        match offset {
            o if o < COMMON_CONFIG_BAR_OFFSET + COMMON_CONFIG_SIZE => self.common_config.write(
                o - COMMON_CONFIG_BAR_OFFSET,
                data,
                &mut self.queues,
                self.device.as_mut(),
            ),
            o if ISR_CONFIG_BAR_OFFSET <= o && o < ISR_CONFIG_BAR_OFFSET + ISR_CONFIG_SIZE => {
                if let Some(v) = data.get(0) {
                    self.interrupt_status
                        .fetch_and(!(*v as usize), Ordering::SeqCst);
                }
            }
            o if DEVICE_CONFIG_BAR_OFFSET <= o
                && o < DEVICE_CONFIG_BAR_OFFSET + DEVICE_CONFIG_SIZE =>
            {
                self.device.write_config(o - DEVICE_CONFIG_BAR_OFFSET, data);
            }
            o if NOTIFICATION_BAR_OFFSET <= o
                && o < NOTIFICATION_BAR_OFFSET + NOTIFICATION_SIZE =>
            {
                // Handled with ioeventfds.
            }
            o if MSIX_TABLE_BAR_OFFSET <= o && o < MSIX_TABLE_BAR_OFFSET + MSIX_TABLE_SIZE => {
                if let Some(msix_config) = &self.msix_config {
                    msix_config
                        .lock()
                        .unwrap()
                        .write_table(o - MSIX_TABLE_BAR_OFFSET, data);
                }
            }
            o if MSIX_PBA_BAR_OFFSET <= o && o < MSIX_PBA_BAR_OFFSET + MSIX_PBA_SIZE => {
                if let Some(msix_config) = &self.msix_config {
                    msix_config
                        .lock()
                        .unwrap()
                        .write_pba(o - MSIX_PBA_BAR_OFFSET, data);
                }
            }
            _ => (),
        };

        if !self.device_activated && self.is_driver_ready() && self.are_queues_valid() {
            if let Some(interrupt_cb) = self.interrupt_cb.take() {
                if self.memory.is_some() {
                    let mem = self.memory.as_ref().unwrap().clone();
                    self.device
                        .activate(
                            mem,
                            interrupt_cb,
                            self.queues.clone(),
                            self.queue_evts.split_off(0),
                        )
                        .expect("Failed to activate device");
                    self.device_activated = true;
                }
            }
        }

        // Device has been reset by the driver
        if self.device_activated && self.is_driver_init() {
            if let Some((interrupt_cb, mut queue_evts)) = self.device.reset() {
                // Upon reset the device returns its interrupt EventFD and it's queue EventFDs
                self.interrupt_cb = Some(interrupt_cb);
                self.queue_evts.append(&mut queue_evts);

                self.device_activated = false;

                // Reset queue readiness (changes queue_enable), queue sizes
                // and selected_queue as per spec for reset
                self.queues.iter_mut().for_each(Queue::reset);
                self.common_config.queue_select = 0;
            } else {
                error!("Attempt to reset device when not implemented in underlying device");
                self.common_config.driver_status = crate::DEVICE_FAILED as u8;
            }
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }
}

impl BusDevice for VirtioPciDevice {
    fn read(&mut self, base: u64, offset: u64, data: &mut [u8]) {
        self.read_bar(base, offset, data)
    }

    fn write(&mut self, base: u64, offset: u64, data: &[u8]) {
        self.write_bar(base, offset, data)
    }
}
