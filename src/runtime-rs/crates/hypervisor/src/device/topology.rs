//
// Copyright (c) 2019-2023 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

/*
The design origins from https://github.com/qemu/qemu/blob/master/docs/pcie.txt

In order to better support the PCIe topologies of different VMMs, we adopt a layered approach.
The first layer is the base layer(the flatten PCIe topology), which mainly consists of the root bus,
which is mainly used by VMMs that only support devices being directly attached to the root bus.
However, not all VMMs have such simple PCIe topologies. For example, Qemu, which can fully simulate
the PCIe topology of the host, has a complex PCIe topology. In this case, we need to add PCIe RootPort,
PCIe Switch, and PCIe-PCI Bridge or pxb-pcie on top of the base layer, which is The Complex PCIe Topology.

The design graghs as below:

(1) The flatten PCIe Topology
pcie.0 bus (Root Complex)
----------------------------------------------------------------------------
|    |    |    |    |    |    |    |    |    |    |    |    |    |    | .. |
--|--------------------|------------------|-------------------------|-------
  |                    |                  |                         |
  V                    V                  V                         V
-----------       -----------         -----------                -----------
| PCI Dev |       | PCI Dev |         | PCI Dev |                | PCI Dev |
-----------       -----------         -----------                -----------

(2) The Complex PCIe Topology(It'll be implemented when Qemu is ready in runtime-rs)
pcie.0 bus (Root Complex)
----------------------------------------------------------------------------
|    |    |    |    |    |    |    |    |    |    |    |    |    |    | .. |
------|----------------|--------------------------------------|-------------
      |                |                                      |
      V                V                                      V
 -------------     -------------                        -------------
 | Root Port |     | Root Port |                        | Root Port |
 -------------     -------------                        -------------
     |                                                       |
     |                              -------------------------|-----------------------
------------                        |                -----------------              |
| PCIe Dev |                        |   PCI Express  | Upstream Port |              |
------------                        |     Switch     -----------------              |
                                    |                 |            |                |
                                    |   -------------------    -------------------  |
                                    |   | Downstream Port |    | Downstream Port |  |
                                    |   -------------------    -------------------  |
                                    -------------|-----------------------|-----------
                                            ------------
                                            | PCIe Dev |
                                            ------------
*/

use std::collections::{hash_map::Entry, HashMap};

use anyhow::{anyhow, Result};

use crate::device::pci_path::PciSlot;
use kata_types::config::hypervisor::TopologyConfigInfo;

use super::pci_path::PciPath;

pub const DEFAULT_PCIE_ROOT_BUS: &str = "pcie.0";
// Currently, CLH and Dragonball support device attachment solely on the root bus.
const DEFAULT_PCIE_ROOT_BUS_ADDRESS: &str = "0000:00";
pub const PCIE_ROOT_BUS_SLOTS_CAPACITY: u32 = 32;

// register_pcie_device: do pre register device into PCIe Topology which
// be called in device driver's attach before device real attached into
// VM. It'll allocate one available PCI path for the device.
// register_pcie_device can be expanded as below:
// register_pcie_device {
//     match pcie_topology {
//         Some(topology) => self.register(topology).await,
//         None => Ok(())
//     }
// }
#[macro_export]
macro_rules! register_pcie_device {
    ($self:ident, $opt:expr) => {
        match $opt {
            Some(topology) => $self.register(topology).await,
            None => Ok(()),
        }
    };
}

// update_pcie_device: do update device info, as some VMMs will be able to
// return the device info containing guest PCI path which differs the one allocated
// in runtime. So we need to compair the two PCI path, and finally update it or not
// based on the difference between them.
// update_pcie_device can be expanded as below:
// update_pcie_device {
//     match pcie_topology {
//         Some(topology) => self.register(topology).await,
//         None => Ok(())
//     }
// }
#[macro_export]
macro_rules! update_pcie_device {
    ($self:ident, $opt:expr) => {
        match $opt {
            Some(topology) => $self.register(topology).await,
            None => Ok(()),
        }
    };
}

// unregister_pcie_device: do unregister device from pcie topology.
// unregister_pcie_device can be expanded as below:
// unregister_pcie_device {
//     match pcie_topology {
//         Some(topology) => self.unregister(topology).await,
//         None => Ok(())
//     }
// }
#[macro_export]
macro_rules! unregister_pcie_device {
    ($self:ident, $opt:expr) => {
        match $opt {
            Some(topology) => $self.unregister(topology).await,
            None => Ok(()),
        }
    };
}

pub trait PCIeDevice: Send + Sync {
    fn device_id(&self) -> &str;
}

#[derive(Clone, Debug, Default)]
pub struct PCIeEndpoint {
    // device_id for device in device manager
    pub device_id: String,
    // device's PCI Path in Guest
    pub pci_path: PciPath,
    // root_port for PCIe Device
    pub root_port: Option<PCIeRootPort>,

    // device_type is for device virtio-pci/PCI or PCIe
    pub device_type: String,
}

impl PCIeDevice for PCIeEndpoint {
    fn device_id(&self) -> &str {
        self.device_id.as_str()
    }
}

// reserved resource
#[derive(Clone, Debug, Default)]
pub struct ResourceReserved {
    // This to work needs patches to QEMU
    // The PCIE-PCI bridge can be hot-plugged only into pcie-root-port that has 'bus-reserve'
    // property value to provide secondary bus for the hot-plugged bridge.
    pub bus_reserve: String,

    // reserve prefetched MMIO aperture, 64-bit
    pub pref64_reserve: String,
    // reserve prefetched MMIO aperture, 32-bit
    pub pref32_reserve: String,
    // reserve non-prefetched MMIO aperture, 32-bit *only*
    pub memory_reserve: String,

    // IO reservation
    pub io_reserve: String,
}

// PCIe Root Port
#[derive(Clone, Debug, Default)]
pub struct PCIeRootPort {
    // format: rp{n}, n>=0
    pub id: String,

    // default is pcie.0
    pub bus: String,
    // >=0, default is 0x00
    pub address: String,

    // (slot, chassis) pair is mandatory and must be unique for each pcie-root-port,
    // chassis >=0, default is 0x00
    pub chassis: u8,
    // slot >=0, default is 0x00
    pub slot: u8,

    // multi_function is for PCIe Device passthrough
    // true => "on", false => "off", default is off
    pub multi_function: bool,

    // reserved resource for some VMM, such as Qemu.
    pub resource_reserved: ResourceReserved,

    // romfile specifies the ROM file being used for this device.
    pub romfile: String,
}

// PCIe Root Complex
#[derive(Clone, Debug, Default)]
pub struct PCIeRootComplex {
    pub root_bus: String,
    pub root_bus_address: String,
    pub root_bus_devices: HashMap<String, PCIeEndpoint>,
}

/// PCIePortBusPrefix defines the naming scheme for PCIe ports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PCIePortBusPrefix {
    RootPort,
    SwitchPort,
    SwitchUpstreamPort,
    SwitchDownstreamPort,
}

impl std::fmt::Display for PCIePortBusPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let prefix = match self {
            PCIePortBusPrefix::RootPort => "rp",
            PCIePortBusPrefix::SwitchPort => "sw",
            PCIePortBusPrefix::SwitchUpstreamPort => "swup",
            PCIePortBusPrefix::SwitchDownstreamPort => "swdp",
        };
        write!(f, "{}", prefix)
    }
}

/// PCIePort distinguishes between different types of PCIe ports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PCIePort {
    // NoPort is for disabling VFIO hotplug/coldplug
    #[default]
    NoPort,

    /// RootPort attach VFIO devices to a root-port
    RootPort,

    // SwitchPort attach VFIO devices to a switch-port
    SwitchPort,

    // InvalidPort is for invalid port
    InvalidPort,
}

impl std::fmt::Display for PCIePort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let port = match self {
            PCIePort::NoPort => "no-port",
            PCIePort::RootPort => "root-port",
            PCIePort::SwitchPort => "switch-port",
            PCIePort::InvalidPort => "invalid-port",
        };
        write!(f, "{}", port)
    }
}

/// Represents a PCIe port
#[derive(Default, Clone, Debug)]
pub struct PciePort {
    pub id: u32,
    pub bus: String,
    pub port_type: PCIePort,
    pub connected_swdp_devices: Vec<String>, // Names of connected devices
}

/// Represents a PCIe switch
#[derive(Debug, Clone)]
pub struct PcieSwitch {
    pub id: u32,
    pub bus: String,
    pub switch_ports: HashMap<u32, PciePort>, // Switch ports
}

/// Represents a root port attached on root bus, TopologyPortDevice represents a PCIe device used for hotplugging.
#[derive(Debug, Clone)]
pub struct TopologyPortDevice {
    pub id: String,
    pub bus: String,
    pub connected_switch: Option<PcieSwitch>, // Connected PCIe switch
}

impl TopologyPortDevice {
    pub fn new(id: &str, bus: &str) -> Self {
        Self {
            id: id.to_string(),
            bus: bus.to_owned(),
            connected_switch: None,
        }
    }

    pub fn get_port_type(&self) -> PCIePort {
        match self.connected_switch {
            Some(_) => PCIePort::SwitchPort,
            None => PCIePort::RootPort,
        }
    }
}

/// Represents strategy selection
#[derive(Debug)]
pub enum Strategy {
    // Strategy 1: Use 1 Root Port to connect 1 PCIe Switch with multiple downstream switch ports
    SingleRootPort,
    // Strategy 2: Use multiple Root Ports to connect multiple separate PCIe Switches (each Switch provides 1 downstream port)
    MultipleRootPorts,
    // Strategy 3: Use multiple Root Ports to connect multiple PCIe Switches (each Switch provides at least 1 downstream port)
    Hybrid,
}

#[derive(Clone, Debug, Default)]
pub struct PCIeTopology {
    pub hypervisor_name: String,
    pub root_complex: PCIeRootComplex,

    pub bridges: u32,
    pub pcie_root_ports: u32,
    pub pcie_switch_ports: u32,
    pub hotplug_vfio_on_root_bus: bool,
    // pcie_port_devices keeps track of the devices attached to different types of PCI ports.
    pub pcie_port_devices: HashMap<u32, TopologyPortDevice>,
}

impl PCIeTopology {
    // As some special case doesn't support PCIe devices, there's no need to build a PCIe Topology.
    pub fn new(config_info: Option<&TopologyConfigInfo>) -> Option<Self> {
        // if config_info is None, it will return None.
        let topo_config = config_info?;

        let root_complex = PCIeRootComplex {
            root_bus: DEFAULT_PCIE_ROOT_BUS.to_owned(),
            root_bus_address: DEFAULT_PCIE_ROOT_BUS_ADDRESS.to_owned(),
            root_bus_devices: HashMap::with_capacity(PCIE_ROOT_BUS_SLOTS_CAPACITY as usize),
        };

        // initialize port devices within PCIe Topology
        let total_rp = topo_config.device_info.pcie_root_port;
        let total_swp = topo_config.device_info.pcie_switch_port;

        Some(Self {
            hypervisor_name: topo_config.hypervisor_name.to_owned(),
            root_complex,
            bridges: topo_config.device_info.default_bridges,
            pcie_root_ports: total_rp,
            pcie_switch_ports: total_swp,
            hotplug_vfio_on_root_bus: topo_config.device_info.hotplug_vfio_on_root_bus,
            pcie_port_devices: HashMap::new(),
        })
    }

    pub fn insert_device(&mut self, ep: &mut PCIeEndpoint) -> Option<PciPath> {
        let to_pcipath = |v: u32| -> PciPath {
            PciPath {
                slots: vec![PciSlot(v as u8)],
            }
        };

        let to_string = |v: u32| -> String { to_pcipath(v).to_string() };

        // find the first available index as the allocated slot.
        let allocated_slot = (0..PCIE_ROOT_BUS_SLOTS_CAPACITY).find(|&i| {
            !self
                .root_complex
                .root_bus_devices
                .contains_key(&to_string(i))
        })?;

        let pcipath = to_string(allocated_slot);

        // update pci_path in Endpoint
        ep.pci_path = to_pcipath(allocated_slot);
        // convert the allocated slot to pci path and then insert it with ep
        self.root_complex
            .root_bus_devices
            .insert(pcipath, ep.clone());

        Some(to_pcipath(allocated_slot))
    }

    pub fn remove_device(&mut self, device_id: &str) -> Option<String> {
        let mut target_device: Option<String> = None;
        self.root_complex.root_bus_devices.retain(|k, v| {
            if v.device_id() != device_id {
                true
            } else {
                target_device = Some((*k).to_string());
                false
            }
        });

        target_device
    }

    pub fn update_device(&mut self, ep: &PCIeEndpoint) -> Option<PciPath> {
        let pci_addr = ep.pci_path.clone();

        // First, find the PCIe Endpoint corresponding to the endpoint in the Hash Map based on the PCI path.
        // If found, it means that we do not need to update the device's position in the Hash Map.
        // If not found, it means that the PCI Path corresponding to the device has changed, and the device's
        // position in the Hash Map needs to be updated.
        match self
            .root_complex
            .root_bus_devices
            .entry(pci_addr.to_string())
        {
            Entry::Occupied(_) => None,
            Entry::Vacant(_entry) => {
                self.remove_device(&ep.device_id);
                self.root_complex
                    .root_bus_devices
                    .insert(pci_addr.to_string(), ep.clone());

                Some(pci_addr)
            }
        }
    }

    pub fn find_device(&mut self, device_id: &str) -> bool {
        for v in self.root_complex.root_bus_devices.values() {
            info!(
                sl!(),
                "find_device with: {:?}, {:?}.",
                &device_id,
                v.device_id()
            );
            if v.device_id() == device_id {
                return true;
            }
        }

        false
    }

    pub fn do_insert_or_update(&mut self, pciep: &mut PCIeEndpoint) -> Result<PciPath> {
        // Try to check whether the device is present in the PCIe Topology.
        // If the device dosen't exist, it proceeds to register it within the topology
        let pci_path = if !self.find_device(&pciep.device_id) {
            // Register a device within the PCIe topology, allocating and assigning it an available PCI Path.
            // Upon successful insertion, it updates the pci_path in PCIeEndpoint and returns it.
            // Finally, update both the guest_pci_path and devices_options with the allocated PciPath.
            if let Some(pci_addr) = self.insert_device(pciep) {
                pci_addr
            } else {
                return Err(anyhow!("pci path allocated failed."));
            }
        } else {
            // If the device exists, it proceeds to update its pcipath within
            // the topology and the device's guest_pci_path and device_options.
            if let Some(pci_addr) = self.update_device(pciep) {
                pci_addr
            } else {
                return Ok(pciep.pci_path.clone());
            }
        };

        Ok(pci_path)
    }

    /// get pcie port and its total
    pub fn get_pcie_port(&self) -> Option<(PCIePort, u32)> {
        match (self.pcie_root_ports, self.pcie_switch_ports) {
            (0, 0) | (_, _) if self.pcie_root_ports > 0 && self.pcie_switch_ports > 0 => None,
            (r, _) if r > 0 => Some((PCIePort::RootPort, r)),
            (_, s) if s > 0 => Some((PCIePort::SwitchPort, s)),
            _ => None,
        }
    }

    /// Adds a root port to pcie bus
    fn add_pcie_root_port(&mut self, id: u32) -> Result<()> {
        if self.pcie_port_devices.contains_key(&id) {
            return Err(anyhow!("Root Port {} already exists.", id));
        }

        let root_port_id = format!("{}{}", PCIePortBusPrefix::RootPort, id);
        self.pcie_port_devices.insert(
            id,
            TopologyPortDevice {
                id: root_port_id,
                bus: DEFAULT_PCIE_ROOT_BUS.to_string(),
                connected_switch: None,
            },
        );

        Ok(())
    }

    /// Adds a PCIe switch to a root port
    fn add_switch_to_root_port(&mut self, root_port_id: u32, switch_id: u32) -> Result<()> {
        let root_port = self
            .pcie_port_devices
            .get_mut(&root_port_id)
            .ok_or_else(|| anyhow!("Root Port {} does not exist.", root_port_id))?;

        if root_port.connected_switch.is_some() {
            return Err(anyhow!(
                "Root Port {} already has a connected switch.",
                root_port_id
            ));
        }
        let rp_bus = format!("{}{}", PCIePortBusPrefix::RootPort, root_port_id);

        root_port.connected_switch = Some(PcieSwitch {
            id: switch_id,
            bus: rp_bus,
            switch_ports: HashMap::new(),
        });

        Ok(())
    }

    /// Adds a switch port to a PCIe switch
    fn add_switch_port_to_switch(
        &mut self,
        swup_bus: &str,
        root_port_id: u32,
        switch_port_id: u32,
        swdp_name: String,
    ) -> Result<()> {
        let root_port = self
            .pcie_port_devices
            .get_mut(&root_port_id)
            .ok_or_else(|| anyhow!("Root Port {} does not exist.", root_port_id))?;

        let switch = root_port
            .connected_switch
            .as_mut()
            .ok_or_else(|| anyhow!("Root Port {} has no connected switch.", root_port_id))?;

        if switch.switch_ports.contains_key(&switch_port_id) {
            return Err(anyhow!(
                "Switch Port {} already exists in Switch {}.",
                switch_port_id,
                switch.id
            ));
        }

        switch.switch_ports.insert(
            switch_port_id,
            PciePort {
                id: switch_port_id,
                bus: swup_bus.to_string(),
                port_type: PCIePort::SwitchPort,
                connected_swdp_devices: vec![swdp_name],
            },
        );

        Ok(())
    }

    /// Adds a root port to pcie bus
    pub fn add_root_ports_on_bus(&mut self, num_root_ports: u32) -> Result<()> {
        for index in 0..num_root_ports {
            self.add_pcie_root_port(index)?;
        }

        Ok(())
    }

    /// Strategy selection for adding switch ports
    pub fn add_switch_ports_with_strategy(
        &mut self,
        num_switches: u32,
        num_switch_ports: u32,
        strategy: Strategy,
    ) -> Result<()> {
        match strategy {
            Strategy::SingleRootPort => {
                self.add_switch_ports_single_root_port(num_switches, num_switch_ports)
            }
            Strategy::MultipleRootPorts => {
                self.add_switch_ports_multiple_root_ports(num_switches, num_switch_ports)
            }
            Strategy::Hybrid => self.add_switch_ports_hybrid(num_switches, num_switch_ports),
        }
    }

    /// Strategy 1: Use 1 Root Port to connect 1 PCIe Switch with multiple downstream switch ports
    fn add_switch_ports_single_root_port(
        &mut self,
        _num_switches: u32,
        num_switch_ports: u32,
    ) -> Result<()> {
        let root_port_id = 1;
        if !self.pcie_port_devices.contains_key(&root_port_id) {
            self.add_pcie_root_port(root_port_id)?;
        }

        let switch_id = 101;
        self.add_switch_to_root_port(root_port_id, switch_id)?;

        let swup_bus = format!("{}{}", PCIePortBusPrefix::SwitchUpstreamPort, switch_id);
        for i in 1..=num_switch_ports {
            self.add_switch_port_to_switch(
                &swup_bus,
                root_port_id,
                i,
                format!("{}{}", PCIePortBusPrefix::SwitchDownstreamPort, i),
            )?;
        }

        Ok(())
    }

    /// Strategy 2: Use multiple Root Ports to connect multiple separate PCIe Switches (each Switch provides 1 downstream port)
    fn add_switch_ports_multiple_root_ports(
        &mut self,
        _num_switches: u32,
        num_switch_ports: u32,
    ) -> Result<()> {
        for i in 1..=num_switch_ports {
            let root_port_id = i;
            if !self.pcie_port_devices.contains_key(&root_port_id) {
                self.add_pcie_root_port(root_port_id)?;
            }

            let switch_id = 100 + i;
            self.add_switch_to_root_port(root_port_id, switch_id)?;
            let swup_bus = format!("{}{}", PCIePortBusPrefix::SwitchUpstreamPort, switch_id);

            self.add_switch_port_to_switch(
                &swup_bus,
                root_port_id,
                1,
                format!("{}{}", PCIePortBusPrefix::SwitchDownstreamPort, i),
            )?;
        }

        Ok(())
    }

    /// Strategy 3: Use multiple Root Ports to connect multiple PCIe Switches (each Switch provides at least 1 downstream port)
    fn add_switch_ports_hybrid(&mut self, num_switches: u32, num_switch_ports: u32) -> Result<()> {
        let ports_per_switch = num_switch_ports / num_switches;

        for i in 1..=num_switches {
            let root_port_id = i;
            if !self.pcie_port_devices.contains_key(&root_port_id) {
                self.add_pcie_root_port(root_port_id)?;
            }

            let switch_id = 100 + i;
            self.add_switch_to_root_port(root_port_id, switch_id)?;
            let swup_bus = format!("{}{}", PCIePortBusPrefix::SwitchUpstreamPort, switch_id);

            for j in 1..=ports_per_switch {
                let switch_port_id = j;
                self.add_switch_port_to_switch(
                    &swup_bus,
                    root_port_id,
                    switch_port_id,
                    format!(
                        "{}{}",
                        PCIePortBusPrefix::SwitchDownstreamPort,
                        (i - 1) * ports_per_switch + j
                    ),
                )?;
            }
        }

        Ok(())
    }
}

// do_add_pcie_endpoint do add a device into PCIe topology with pcie endpoint
// device_id: device's Unique ID in Device Manager.
// allocated_pcipath: allocated pcipath before add_device
// topology: PCIe Topology for devices to build a PCIe Topology in Guest.
pub fn do_add_pcie_endpoint(
    device_id: String,
    allocated_pcipath: Option<PciPath>,
    topology: &mut PCIeTopology,
) -> Result<PciPath> {
    let pcie_endpoint = &mut PCIeEndpoint {
        device_type: "PCIe".to_string(),
        device_id,
        ..Default::default()
    };

    if let Some(pci_path) = allocated_pcipath {
        pcie_endpoint.pci_path = pci_path;
    }

    topology.do_insert_or_update(pcie_endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_pcie_topology(rp_total: u32, sw_total: u32) -> PCIeTopology {
        PCIeTopology {
            pcie_root_ports: rp_total,
            pcie_switch_ports: sw_total,
            pcie_port_devices: HashMap::with_capacity(sw_total as usize),
            ..Default::default()
        }
    }

    fn create_pcie_topo(rps: u32, swps: u32) -> PCIeTopology {
        PCIeTopology {
            pcie_root_ports: rps,
            pcie_switch_ports: swps,
            ..Default::default()
        }
    }

    #[test]
    fn test_no_port() {
        let pcie_topo = create_pcie_topo(0, 0);
        assert_eq!(pcie_topo.get_pcie_port(), None);
    }

    #[test]
    fn test_root_port_only() {
        let pcie_topo = create_pcie_topo(2, 0);
        assert_eq!(pcie_topo.get_pcie_port(), Some((PCIePort::RootPort, 2)));
    }

    #[test]
    fn test_switch_port_only() {
        let pcie_topo = create_pcie_topo(0, 1);
        assert_eq!(pcie_topo.get_pcie_port(), Some((PCIePort::SwitchPort, 1)));
    }

    #[test]
    fn test_both_ports_invalid() {
        let pcie_topo = create_pcie_topo(1, 1);
        assert_eq!(pcie_topo.get_pcie_port(), None);
    }

    #[test]
    fn test_new_pcie_topology() {
        let topology = new_pcie_topology(2, 3);
        assert_eq!(topology.pcie_root_ports, 2);
        assert_eq!(topology.pcie_switch_ports, 3);
        assert!(topology.pcie_port_devices.is_empty());
    }

    #[test]
    fn test_get_pcie_port() {
        let topology = new_pcie_topology(2, 0);
        assert_eq!(topology.get_pcie_port(), Some((PCIePort::RootPort, 2)));

        let topology = new_pcie_topology(0, 3);
        assert_eq!(topology.get_pcie_port(), Some((PCIePort::SwitchPort, 3)));

        let topology = new_pcie_topology(0, 0);
        assert_eq!(topology.get_pcie_port(), None);

        let topology = new_pcie_topology(2, 3);
        assert_eq!(topology.get_pcie_port(), None);
    }

    #[test]
    fn test_add_pcie_root_port() {
        let mut topology = new_pcie_topology(1, 0);
        assert!(topology.add_pcie_root_port(1).is_ok());
        assert!(topology.pcie_port_devices.contains_key(&1));

        // Adding the same root port again should fail
        assert!(topology.add_pcie_root_port(1).is_err());
    }

    #[test]
    fn test_add_switch_to_root_port() {
        let mut topology = new_pcie_topology(1, 0);
        topology.add_pcie_root_port(1).unwrap();
        assert!(topology.add_switch_to_root_port(1, 101).is_ok());

        // Adding a switch to a non-existent root port should fail
        assert!(topology.add_switch_to_root_port(2, 102).is_err());

        // Adding a switch to a root port that already has a switch should fail
        assert!(topology.add_switch_to_root_port(1, 103).is_err());
    }

    #[test]
    fn test_add_switch_port_to_switch() {
        let mut topology = new_pcie_topology(1, 0);
        topology.add_pcie_root_port(1).unwrap();
        topology.add_switch_to_root_port(1, 101).unwrap();

        let swup_bus = format!("{}{}", PCIePortBusPrefix::SwitchUpstreamPort, 101);
        assert!(topology
            .add_switch_port_to_switch(&swup_bus, 1, 1, "swdp1".to_string())
            .is_ok());

        // Adding a switch port to a non-existent root port should fail
        assert!(topology
            .add_switch_port_to_switch(&swup_bus, 2, 1, "swdp2".to_string())
            .is_err());

        // Adding a switch port to a root port without a switch should fail
        let mut topology = new_pcie_topology(1, 0);
        topology.add_pcie_root_port(1).unwrap();
        assert!(topology
            .add_switch_port_to_switch(&swup_bus, 1, 1, "swdp1".to_string())
            .is_err());

        // Adding a switch port with a duplicate ID should fail
        let mut topology = new_pcie_topology(1, 0);
        topology.add_pcie_root_port(1).unwrap();
        topology.add_switch_to_root_port(1, 101).unwrap();
        assert!(topology
            .add_switch_port_to_switch(&swup_bus, 1, 1, "swdp1".to_string())
            .is_ok());
        assert!(topology
            .add_switch_port_to_switch(&swup_bus, 1, 1, "swdp2".to_string())
            .is_err());
    }

    #[test]
    fn test_add_root_ports_on_bus() {
        let mut topology = new_pcie_topology(3, 0);
        assert!(topology.add_root_ports_on_bus(3).is_ok());
        assert_eq!(topology.pcie_port_devices.len(), 3);

        // Adding more root ports than available should fail
        assert!(topology.add_root_ports_on_bus(1).is_err());
    }

    #[test]
    fn test_add_switch_ports_single_root_port() {
        let mut topology = new_pcie_topology(1, 0);
        assert!(topology
            .add_switch_ports_with_strategy(1, 2, Strategy::SingleRootPort)
            .is_ok());

        let root_port = topology.pcie_port_devices.get(&1).unwrap();
        assert!(root_port.connected_switch.is_some());
        let switch = root_port.connected_switch.as_ref().unwrap();
        assert_eq!(switch.switch_ports.len(), 2);
    }

    #[test]
    fn test_add_switch_ports_multiple_root_ports() {
        let mut topology = new_pcie_topology(0, 4);
        assert!(topology
            .add_switch_ports_with_strategy(4, 4, Strategy::MultipleRootPorts)
            .is_ok());

        for i in 1..=3 {
            let root_port = topology.pcie_port_devices.get(&i).unwrap();
            assert!(root_port.connected_switch.is_some());
            let switch = root_port.connected_switch.as_ref().unwrap();
            assert_eq!(switch.switch_ports.len(), 1);
        }
    }

    #[test]
    fn test_add_switch_ports_hybrid() {
        let mut topology = new_pcie_topology(2, 0);
        assert!(topology
            .add_switch_ports_with_strategy(2, 4, Strategy::Hybrid)
            .is_ok());

        for i in 1..=2 {
            let root_port = topology.pcie_port_devices.get(&i).unwrap();
            assert!(root_port.connected_switch.is_some());
            let switch = root_port.connected_switch.as_ref().unwrap();
            assert_eq!(switch.switch_ports.len(), 2);
        }
    }
}
