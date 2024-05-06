use anyhow::{Context, Result};
use std::fs::File;
use std::{io::Write, path::Path};

use super::{
    spec::{CdiSpec, ContainerEdits, Device, DeviceNode, Mount},
    CURRENT_CDI_VERSION, NVIDIA_GPU_KIND,
};

impl Mount {
    pub fn new(host_path: &str, container_path: &str) -> Self {
        Self {
            host_path: host_path.to_owned(),
            container_path: container_path.to_owned(),
            r#type: "bind".to_owned(),
            options: vec![
                "ro".to_owned(),
                "nosuid".to_owned(),
                "nodev".to_owned(),
                "bind".to_owned(),
            ],
        }
    }
}

impl ContainerEdits {
    pub fn new() -> Self {
        ContainerEdits {
            env: vec!["NVIDIA_VISIBLE_DEVICES=void".to_string()],
            device_nodes: vec![],
            hooks: vec![],
            mounts: Vec::new(),
            ..Default::default()
        }
    }
}

impl Device {
    pub fn new(name: &str, dev_paths: Vec<&str>) -> Self {
        let mut device_nodes: Vec<DeviceNode> = Vec::new();
        for dev_path in dev_paths {
            let dev_node = DeviceNode {
                path: dev_path.to_owned(),
                ..Default::default()
            };
            device_nodes.push(dev_node);
        }

        Device {
            name: name.to_owned(),
            container_edits: ContainerEdits {
                device_nodes,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

impl CdiSpec {
    pub fn new() -> Self {
        CdiSpec {
            kind: NVIDIA_GPU_KIND.to_string(),
            version: CURRENT_CDI_VERSION.to_string(),
            devices: vec![
                Device::new("0", vec!["/dev/vfio/75"]),
                Device::new("1", vec!["/dev/vfio/76"]),
                Device::new("all", vec!["/dev/vfio/75", "/dev/vfio/76"]),
            ],
            container_edits: Some(ContainerEdits::new()),
            ..Default::default()
        }
    }

    pub fn save(&self, cdi_path: &str, config_name: &str) -> Result<()> {
        let yaml = serde_yaml::to_string(self).context("do serde yaml failed")?;

        let cdi_file_path = Path::new(cdi_path).join(config_name);
        let mut file = File::create(cdi_file_path).context("unable to create file")?;

        file.write_all(yaml.as_bytes())
            .context("unable to write to file")?;

        Ok(())
    }
}
