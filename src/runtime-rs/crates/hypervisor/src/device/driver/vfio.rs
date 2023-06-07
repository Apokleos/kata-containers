// Copyright (c) 2023 Alibaba Cloud
// Copyright (c) 2023 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, RwLock,
    },
};

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use lazy_static::lazy_static;
use path_clean::PathClean;

use crate::device::{hypervisor, Device, DeviceType};
use kata_sys_util::fs::get_base_name;

pub const SYS_BUS_PCI_DRIVER_PROBE: &str = "/sys/bus/pci/drivers_probe";
pub const SYS_BUS_PCI_DEVICES: &str = "/sys/bus/pci/devices";
pub const SYS_KERN_IOMMU_GROUPS: &str = "/sys/kernel/iommu_groups";
pub const VFIO_PCI_DEV_PATH: &str = "/dev/vfio";
pub const VFIO_PCI_DRIVER: &str = "vfio-pci";
pub const DRIVER_MMIO_BLK_TYPE: &str = "mmioblk";
pub const DRIVER_VFIO_PCI_TYPE: &str = "vfio-pci";
pub const MAX_DEV_ID_SIZE: usize = 31;

const VFIO_PCI_DRIVER_NEW_ID: &str = "/sys/bus/pci/drivers/vfio-pci/new_id";
const VFIO_PCI_DRIVER_UNBIND: &str = "/sys/bus/pci/drivers/vfio-pci/unbind";
const SYS_CLASS_IOMMU: &str = "/sys/class/iommu";
const INTEL_IOMMU_PREFIX: &str = "dmar";
const AMD_IOMMU_PREFIX: &str = "ivhd";

lazy_static! {
    static ref GUEST_DEVICE_ID: Arc<AtomicU8> = Arc::new(AtomicU8::new(0_u8));
    static ref HOST_GUEST_MAP: Arc<RwLock<HashMap<String, String>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

pub fn do_check_iommu_on() -> Result<bool> {
    let element = std::fs::read_dir(SYS_CLASS_IOMMU)?
        .filter_map(|e| e.ok())
        .last();

    if element.is_none() {
        return Err(anyhow!("iommu is not enabled"));
    }

    // safe here, the result of map is always be Some(true) or Some(false).
    Ok(element
        .map(|e| {
            let x = e.file_name().to_string_lossy().into_owned();
            x.starts_with(INTEL_IOMMU_PREFIX) || x.starts_with(AMD_IOMMU_PREFIX)
        })
        .unwrap())
}

fn override_driver(bdf: &str, driver: &str) -> Result<()> {
    let driver_override = format!("/sys/bus/pci/devices/{}/driver_override", bdf);
    fs::write(&driver_override, driver)
        .with_context(|| format!("echo {} > {}", driver, &driver_override))?;
    info!(sl!(), "echo {} > {}", driver, driver_override);
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum VfioBusMode {
    #[default]
    MMIO,
    PCI,
}

impl VfioBusMode {
    pub fn new(mode: &str) -> Self {
        match mode {
            "c" | "pci" => VfioBusMode::PCI,
            _ => VfioBusMode::MMIO,
        }
    }

    pub fn revert(mode: VfioBusMode) -> String {
        match mode {
            VfioBusMode::PCI => "pci".to_owned(),
            VfioBusMode::MMIO => "mmio".to_owned(),
        }
    }

    pub fn driver_type(mode: &str) -> &str {
        match mode {
            "c" | "pci" => DRIVER_VFIO_PCI_TYPE,
            _ => DRIVER_MMIO_BLK_TYPE,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub enum VfioDeviceType {
    // VFIODeviceErrorType is the error type of VFIO device
    VFIODeviceErrorType,

    // VFIODeviceNormalType is a normal VFIO device type
    #[default]
    VFIODeviceNormalType,

    // VFIODeviceMediatedType is a VFIO mediated device type
    VFIODeviceMediatedType,
}

// represents a PCI device's device id and vendor id
// DeVendor: (device, vendor)
#[derive(Clone, Debug)]
pub struct DeVendor(String, String);

impl DeVendor {
    pub fn new(vendor_id: &str, dev_id: &str) -> Self {
        DeVendor(dev_id.to_owned(), vendor_id.to_owned())
    }

    pub fn get_device_vendor_id(&self) -> Result<u32> {
        // default value is 0 when vendor_id or device_id is empty
        if self.0.is_empty() || self.1.is_empty() {
            return Ok(0);
        }

        let do_convert = |id: &String| {
            u32::from_str_radix(
                id.trim_start_matches("0x")
                    .trim_matches(char::is_whitespace),
                16,
            )
            .with_context(|| anyhow!("invalid id {:?}", id))
        };

        let device = do_convert(&self.0).context("convert device failed")?;
        let vendor = do_convert(&self.1).context("convert vendor failed")?;

        Ok(((device & 0xffff) << 16) | (vendor & 0xffff))
    }
}

#[derive(Clone, Debug, Default)]
pub struct PciPath {
    slots: Vec<u8>, // list of PCI slots
}

impl PciPath {
    // constructor
    pub fn new(slots: Vec<u8>) -> PciPath {
        PciPath { slots }
    }

    // method to format the PciPath into a string
    pub fn pci_path_to_string(&self) -> String {
        self.slots
            .iter()
            .map(|slot| format!("{:02x}", slot))
            .collect::<Vec<String>>()
            .join("/")
    }
}

// VfioConfig represents a VFIO drive used for hotplugging
#[derive(Clone, Debug, Default)]
pub struct HostDevice {
    /// Unique identifier of the device
    pub hostdev_id: String,

    /// Sysfs path for mdev bus type device
    pub sysfs_path: String,

    /// PCI device information (BDF): "bus:slot:function"
    pub bus_slot_func: String,

    /// device_vendor: device id and vendor id
    pub device_vendor: Option<DeVendor>,

    /// type of vfio device
    pub vfio_type: VfioDeviceType,

    /// guest PCI path of device
    pub guest_pci_path: Option<PciPath>,

    /// vfio_vendor for vendor's some special cases.
    #[cfg(feature = "enable-vendor")]
    pub vfio_vendor: VfioVendor,
}

// VfioConfig represents a VFIO drive used for hotplugging
#[derive(Clone, Debug, Default)]
pub struct VfioConfig {
    // usually host path will be /dev/vfio/N
    pub host_path: String,

    // device as block or char
    pub dev_type: String,

    /// hostdev_prefix for devices:
    /// (1) phisycial endpoint: "physical_nic_"
    /// (2) vfio mdev: "vfio_mdev_"
    /// (3) vfio pci: "vfio_device_"
    /// (4) vfio volume: "vfio_vol_"
    /// (5) vfio nvme: "vfio_nvme_"
    pub hostdev_prefix: String,

    /// device in guest which the device appears inside the VM,
    /// outside of the container mount namespace
    /// virt_path: Option<(index, virt_path_name)>
    pub virt_path: Option<(u64, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct VfioDevice {
    pub device_id: String,
    pub attach_count: u64,

    /// Bus Mode, PCI or MMIO
    pub bus_mode: VfioBusMode,
    /// driver type
    pub driver_type: String,

    /// vfio config from business
    pub config: VfioConfig,

    // host device with multi-funtions
    pub devices: Vec<HostDevice>,
    // options for vfio pci handler in kata-agent
    pub device_options: Vec<String>,
}

impl VfioDevice {
    // new creates a new VfioConfig
    pub fn new(device_id: String, dev_info: &VfioConfig) -> Self {
        // devices and options are in a 1-1 correspondence. Mainly used in
        // vfio-pci handler for kata-agent.
        let devices: Vec<HostDevice> = Vec::with_capacity(MAX_DEV_ID_SIZE);
        let device_options: Vec<String> = Vec::with_capacity(MAX_DEV_ID_SIZE);

        // get bus mode and driver type based on the device type
        let dev_type = dev_info.dev_type.as_str();
        // in kata community, no mmio bus only pci exists.
        // let bus_mode = VfioBusMode::new(dev_type);
        let bus_mode = VfioBusMode::PCI;
        let driver_type = VfioBusMode::driver_type(dev_type).to_owned();

        Self {
            device_id,
            attach_count: 0,
            bus_mode,
            driver_type,
            config: dev_info.clone(),
            devices,
            device_options,
        }
    }

    fn get_host_path(&self) -> String {
        self.config.host_path.clone()
    }

    fn get_vfio_prefix(&self) -> String {
        self.config.hostdev_prefix.clone()
    }

    // get_vfio_device_bdf returns the BDF of pci device
    // Expected format:[<domain>]:[<bus>][<slot>].[<func>] eg. 0000:02:10.0
    fn get_vfio_device_bdf(&self, dev_sys_str: String) -> Option<String> {
        let dev_sys = dev_sys_str;
        let parts: Vec<&str> = dev_sys.as_str().splitn(2, ':').collect();
        if parts.len() < 2 {
            return None;
        }

        parts.get(1).copied().map(|bdf| bdf.to_owned())
    }

    pub fn generate_guest_pci_path(&self, host_bdf: String) -> u8 {
        let host_bdf = self.get_vfio_device_bdf(host_bdf).unwrap();

        let current_id = GUEST_DEVICE_ID.clone();
        current_id.fetch_add(1, Ordering::SeqCst);
        let pci_path = current_id.load(Ordering::SeqCst);

        // In some Hypervisors, dragonball, cloud-hypervisor or firecracker,
        // the device is directly connected to the bus without intermediary bus.
        // FIXME: Qemu's pci path needs to be implemented;
        let guest_bdf = format!("0000:00:{:02x}.0", pci_path);
        let hg_map = HOST_GUEST_MAP.clone();
        hg_map.write().unwrap().insert(host_bdf, guest_bdf);

        pci_path
    }

    // get_mediated_device_bdf returns the BDF of a VF
    // Expected input string format is /sys/devices/pci0000:d7/BDF0/BDF1/.../MDEVBDF/UUID
    fn get_mediated_device_bdf(&self, dev_sys_str: String) -> Option<String> {
        let dev_sys = dev_sys_str;
        let parts: Vec<&str> = dev_sys.as_str().split('/').collect();
        if parts.len() < 4 {
            return None;
        }

        parts
            .get(parts.len() - 2)
            .copied()
            .map(|bdf| bdf.to_owned())
    }

    fn get_vfio_device_type(&self, device_sys_path: String) -> Result<VfioDeviceType> {
        // Nornaml VFIO BDF: 0000:04:00.0
        // Mediated VFIO BDF: 83b8f4f2-509f-382f-3c1e-e6bfe0fa1001
        let mut tokens: Vec<&str> = device_sys_path.as_str().split(':').collect();
        let vfio_type = match tokens.len() {
            3 => VfioDeviceType::VFIODeviceNormalType,
            _ => {
                tokens = device_sys_path.split('-').collect();
                if tokens.len() == 5 {
                    VfioDeviceType::VFIODeviceMediatedType
                } else {
                    VfioDeviceType::VFIODeviceErrorType
                }
            }
        };

        Ok(vfio_type)
    }

    // get_sysfs_device returns the sysfsdev of mediated device
    // expected input string format is absolute path to the sysfs dev node
    // eg. /sys/kernel/iommu_groups/0/devices/f79944e4-5a3d-11e8-99ce-479cbab002e4
    fn get_sysfs_device(&self, sysfs_dev_path: PathBuf) -> Result<String> {
        // eval_symlinks(sysfs_dev)?
        let mut buf =
            fs::canonicalize(sysfs_dev_path.clone()).context("sysfs device path not exist")?;
        let mut resolved = false;

        // resolve symbolic links until there's no more to resolve
        while buf.symlink_metadata()?.file_type().is_symlink() {
            let link = fs::read_link(&buf)?;
            buf.pop();
            buf.push(link);
            resolved = true;
        }

        // If a symbolic link was resolved, the resulting path may be relative to the original path
        if resolved {
            // If the original path is relative and the resolved path is not, the resolved path
            // should be returned as absolute.
            if sysfs_dev_path.is_relative() && buf.is_absolute() {
                buf = fs::canonicalize(&buf)?;
            }
        }

        Ok(buf.clean().display().to_string())
    }

    fn get_vfio_device_details(
        &self,
        dev_file_name: String,
        iommu_dev_path: PathBuf,
    ) -> Result<(Option<String>, String, VfioDeviceType)> {
        let vfio_type = self.get_vfio_device_type(dev_file_name.clone())?;
        match vfio_type {
            VfioDeviceType::VFIODeviceNormalType => {
                let dev_bdf = self.get_vfio_device_bdf(dev_file_name.clone());
                let dev_sys = [SYS_BUS_PCI_DEVICES, dev_file_name.as_str()].join("/");
                Ok((dev_bdf, dev_sys, vfio_type))
            }
            VfioDeviceType::VFIODeviceMediatedType => {
                // sysfsdev eg. /sys/devices/pci0000:00/0000:00:02.0/f79944e4-5a3d-11e8-99ce-479cbab002e4
                let sysfs_dev = Path::new(&iommu_dev_path).join(dev_file_name);
                let dev_sys = self
                    .get_sysfs_device(sysfs_dev)
                    .context("get sysfs device failed")?;

                let dev_bdf = if let Some(dev_s) = self.get_mediated_device_bdf(dev_sys.clone()) {
                    self.get_vfio_device_bdf(dev_s)
                } else {
                    None
                };

                Ok((dev_bdf, dev_sys, vfio_type))
            }
            _ => Err(anyhow!("unsupported vfio type : {:?}", vfio_type)),
        }
    }

    fn get_device_property(&self, bdf: &str, property: &str) -> Result<String> {
        let device_name = if !bdf.starts_with("0000") {
            format!("0000:{}", bdf)
        } else {
            bdf.to_string()
        };

        // dev_sys_path: /sys/bus/pci/devices/0000:54:00.5
        let dev_sys_path = Path::new(SYS_BUS_PCI_DEVICES).join(device_name);

        // cfg_path: : /sys/bus/pci/devices/0000:54:00.5/vendor[device]
        let cfg_path = fs::read_to_string(dev_sys_path.join(property)).with_context(|| {
            format!(
                "failed to read {}",
                dev_sys_path.join(property).to_str().unwrap()
            )
        })?;

        Ok(cfg_path.as_str().trim_end_matches('\n').to_string())
    }

    // read vendor and deviceor from /sys/bus/pci/devices/BDF/X
    fn get_vfio_device_vendor(&self, bdf: &str) -> Result<DeVendor> {
        let device = self
            .get_device_property(bdf, "device")
            .context("get device from syspath failed")?;
        let vendor = self
            .get_device_property(bdf, "vendor")
            .context("get vendor from syspath failed")?;

        Ok(DeVendor(device, vendor))
    }

    async fn set_vfio_config(
        &mut self,
        iommu_devs_path: PathBuf,
        device_name: &str,
    ) -> Result<HostDevice> {
        // device details: (device BDF, device SysfsDev, vfio Device Type)
        let vfio_dev_details = self
            .get_vfio_device_details(device_name.to_owned(), iommu_devs_path)
            .context("get vfio device details failed")?;

        // It's safe as BDF really exists.
        let dev_bdf = vfio_dev_details.0.unwrap();
        let dev_vendor = self
            .get_vfio_device_vendor(&dev_bdf)
            .context("get property device and vendor failed")?;

        let mut vfio_dev = HostDevice {
            bus_slot_func: dev_bdf.clone(),
            device_vendor: Some(dev_vendor),
            sysfs_path: vfio_dev_details.1,
            vfio_type: vfio_dev_details.2,
            ..Default::default()
        };

        // when vfio pci, kata-agent handles with device_options,
        // and its FORMAT: "dddd:bb:dd.f=<pcipath>"
        // dddd:bb:dd.f is the device's PCI address on host
        // <pcipath> is the device's PCI path in the guest
        if self.bus_mode == VfioBusMode::PCI {
            let pci_path = self.generate_guest_pci_path(dev_bdf.clone());
            vfio_dev.guest_pci_path = Some(PciPath::new(vec![pci_path]));
            self.device_options
                .push(format!("0000:{}={}", dev_bdf, pci_path));
        }

        Ok(vfio_dev)
    }
}

#[async_trait]
impl Device for VfioDevice {
    async fn attach(&mut self, h: &dyn hypervisor) -> Result<()> {
        // host path: /dev/vfio/620
        let host_path = self.get_host_path();
        // vfio group: 620
        let vfio_group = get_base_name(host_path.clone())?
            .into_string()
            .map_err(|e| anyhow!("failed to get base name {:?}", e))?;

        // /sys/kernel/iommu_groups/620/devices
        let iommu_devs_path = Path::new(SYS_KERN_IOMMU_GROUPS)
            .join(vfio_group.as_str())
            .join("devices");

        // iommu_devs_path: /sys/kernel/iommu_groups/620/devices
        // 0000:54:00.1 0000:54:00.2
        let iommu_devices = fs::read_dir(iommu_devs_path.clone())?
            .filter_map(|e| {
                let x = e.ok()?.file_name().to_string_lossy().into_owned();
                Some(x)
            })
            .collect::<Vec<String>>();
        if iommu_devices.len() > 1 {
            warn!(sl!(), "vfio device {} with multi-function", host_path);
        }

        // pass all devices in iommu group, and use index to identify device.
        for (index, device) in iommu_devices.iter().enumerate() {
            // TODO: filter host or PCI bridge

            let mut hostdev: HostDevice = self
                .set_vfio_config(iommu_devs_path.clone(), device)
                .await
                .context("set vfio config failed")?;
            let dev_prefix = self.get_vfio_prefix();
            hostdev.hostdev_id = make_name_id(&dev_prefix, index, MAX_DEV_ID_SIZE);

            self.devices.push(hostdev);
        }

        // increase attach count, skip attach the device if the device is already attached
        if self
            .increase_attach_count()
            .await
            .context("failed to increase attach count")?
        {
            return Err(anyhow!("attach count increased failed as some reason."));
        }

        // finally do add device for vfio deivce
        if let Err(e) = h.add_device(DeviceType::Vfio(self.clone())).await {
            self.decrease_attach_count().await?;

            return Err(e);
        }

        Ok(())
    }

    // device_index is generated in device manager
    async fn detach(&mut self, h: &dyn hypervisor) -> Result<Option<u64>> {
        // get the count of device detached, skip detach once it reaches the 0
        if self
            .decrease_attach_count()
            .await
            .context("failed to decrease attach count")?
        {
            return Ok(None);
        }

        if let Err(e) = h.remove_device(DeviceType::Vfio(self.clone())).await {
            self.increase_attach_count().await?;
            return Err(e);
        }

        // only virt_path is Some, there's a device index
        let device_index = if let Some(virt_path) = self.config.virt_path.clone() {
            Some(virt_path.0)
        } else {
            None
        };

        Ok(device_index)
    }

    async fn increase_attach_count(&mut self) -> Result<bool> {
        match self.attach_count {
            0 => {
                // do real attach
                self.attach_count += 1;
                Ok(false)
            }
            std::u64::MAX => Err(anyhow!("device was attached too many times")),
            _ => {
                self.attach_count += 1;
                Ok(true)
            }
        }
    }

    async fn decrease_attach_count(&mut self) -> Result<bool> {
        match self.attach_count {
            0 => Err(anyhow!("detaching a device that wasn't attached")),
            1 => {
                // do real wrok
                self.attach_count -= 1;
                Ok(false)
            }
            _ => {
                self.attach_count -= 1;
                Ok(true)
            }
        }
    }

    async fn get_device_info(&self) -> DeviceType {
        DeviceType::Vfio(self.clone())
    }
}

/// binds the device to vfio driver after unbinding from host.
/// Will be called by a network interface or a generic pcie device.
pub fn bind_device_to_vfio(bdf: &str, host_driver: &str, _vendor_device_id: &str) -> Result<()> {
    // modprobe vfio-pci
    if !Path::new(VFIO_PCI_DRIVER_NEW_ID).exists() {
        Command::new("modprobe")
            .arg(VFIO_PCI_DRIVER)
            .output()
            .expect("Failed to run modprobe vfio-pci");
    }

    // Arm does not need cmdline to open iommu, just set it through bios.
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        // check intel_iommu=on
        let cmdline = fs::read_to_string("/proc/cmdline").unwrap();
        if cmdline.contains("iommu=off") || !cmdline.contains("iommu=") {
            return Err(anyhow!("iommu isn't set on kernel cmdline"));
        }

        if !do_check_iommu_on().context("check iommu on failed")? {
            return Err(anyhow!("IOMMU dosn't work."));
        }
    }

    // if it's already bound to vfio
    if is_equal_driver(bdf, VFIO_PCI_DRIVER) {
        info!(sl!(), "bdf : {} was already bound to vfio-pci", bdf);
        return Ok(());
    }

    info!(sl!(), "host driver : {}", host_driver);
    override_driver(bdf, VFIO_PCI_DRIVER).context("override driver")?;

    let unbind_path = format!("/sys/bus/pci/devices/{}/driver/unbind", bdf);

    // echo bdf > /sys/bus/pci/drivers/virtio-pci/unbind"
    fs::write(&unbind_path, bdf)
        .with_context(|| format!("Failed to echo {} > {}", bdf, &unbind_path))?;

    info!(sl!(), "{} is unbound from {}", bdf, host_driver);

    // echo bdf > /sys/bus/pci/drivers_probe
    fs::write(SYS_BUS_PCI_DRIVER_PROBE, bdf)
        .with_context(|| format!("Failed to echo {} > {}", bdf, SYS_BUS_PCI_DRIVER_PROBE))?;

    info!(sl!(), "echo {} > /sys/bus/pci/drivers_probe", bdf);
    Ok(())
}

pub fn is_equal_driver(bdf: &str, host_driver: &str) -> bool {
    let sys_pci_devices_path = Path::new(SYS_BUS_PCI_DEVICES);
    let driver_file = sys_pci_devices_path.join(bdf).join("driver");

    if driver_file.exists() {
        let driver_path = fs::read_link(driver_file).unwrap_or_default();
        let driver_name = driver_path
            .file_name()
            .map_or(String::new(), |v| v.to_str().unwrap().to_owned());
        return driver_name.eq(host_driver);
    }

    false
}

/// bind_device_to_host binds the device to the host driver after unbinding from vfio-pci.
pub fn bind_device_to_host(bdf: &str, host_driver: &str, _vendor_device_id: &str) -> Result<()> {
    // Unbind from vfio-pci driver to the original host driver

    info!(sl!(), "bind {} to {}", bdf, host_driver);

    // if it's already bound to host_driver
    if is_equal_driver(bdf, host_driver) {
        info!(
            sl!(),
            "bdf {} was already unbound to host driver {}", bdf, host_driver
        );
        return Ok(());
    }

    override_driver(bdf, host_driver).context("override driver")?;

    // echo bdf > /sys/bus/pci/drivers/vfio-pci/unbind"
    std::fs::write(VFIO_PCI_DRIVER_UNBIND, bdf)
        .with_context(|| format!("echo {}> {}", bdf, VFIO_PCI_DRIVER_UNBIND))?;
    info!(sl!(), "echo {} > {}", bdf, VFIO_PCI_DRIVER_UNBIND);

    // echo bdf > /sys/bus/pci/drivers_probe
    std::fs::write(SYS_BUS_PCI_DRIVER_PROBE, bdf)
        .with_context(|| format!("echo {} > {}", bdf, SYS_BUS_PCI_DRIVER_PROBE))?;
    info!(sl!(), "echo {} > {}", bdf, SYS_BUS_PCI_DRIVER_PROBE);

    Ok(())
}

// make_name_id: generate a ID for the hypervisor commandline
fn make_name_id(name_type: &str, id: usize, max_len: usize) -> String {
    let name_id = format!("{}_{}", name_type, id);

    if name_id.len() > max_len {
        name_id[0..max_len].to_string()
    } else {
        name_id
    }
}

pub fn get_vfio_iommu_group(dev_bdf: String) -> Result<String> {
    // /sys/bus/pci/devices/0000:43:00.2/iommu_group
    let bdf = if dev_bdf.starts_with("0000") {
        dev_bdf
    } else {
        format!("0000:{}", dev_bdf)
    };

    let iommugrp_path = Path::new(SYS_BUS_PCI_DEVICES)
        .join(bdf.as_str())
        .join("iommu_group");
    if !iommugrp_path.exists() {
        warn!(
            sl!(),
            "IOMMU group path: {:?} not found, do bind device to vfio first.", iommugrp_path
        );
        return Err(anyhow!("please do bind device to vfio"));
    }

    // iommu group symlink: ../../../../../../kernel/iommu_groups/110
    let iommugrp_symlink = fs::read_link(&iommugrp_path)
        .map_err(|e| anyhow!("read iommu group symlink failed {:?}", e))?;

    // get base name from iommu group symlink: 110
    let iommu_group = get_base_name(iommugrp_symlink)?
        .into_string()
        .map_err(|e| anyhow!("failed to get iommu group {:?}", e))?;

    // we'd better verify the path to ensure it dose exist.
    if !Path::new(SYS_KERN_IOMMU_GROUPS)
        .join(&iommu_group)
        .join("devices")
        .join(bdf.as_str())
        .exists()
    {
        return Err(anyhow!(
            "device bdf {:?} dosn't exist in {}/{}/devices.",
            bdf.as_str(),
            SYS_KERN_IOMMU_GROUPS,
            iommu_group
        ));
    }

    Ok(format!("/dev/vfio/{}", iommu_group))
}

pub fn get_vfio_device(device: String) -> Result<String> {
    // support both /dev/vfio/xxx and BDF<0000:43:00.2> or BDF<43:00.2>
    let mut vfio_device = device;

    // do this for different cases with one handler method.
    // Domain:Bus:Device.Function -> 0000:00:01.2
    let bdf_vec: Vec<&str> = vfio_device.as_str().split(&[':', '.'][..]).collect();
    if bdf_vec.len() >= 3 && bdf_vec.len() < 5 {
        // 0000:43:00.2 -> /dev/vfio/110
        vfio_device =
            get_vfio_iommu_group(vfio_device.clone()).context("get vfio iommu group failed")?;
    }

    Ok(vfio_device)
}
