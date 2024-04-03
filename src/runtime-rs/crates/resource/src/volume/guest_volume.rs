// Copyright (c) 2024 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use hypervisor::device::device_manager::DeviceManager;
use tokio::sync::RwLock;

use anyhow::Result;
use async_trait::async_trait;

use super::Volume;

pub const GUEST_PREFIX: &str = "@guest";
pub const NCCL_XML_CONTAINER_PATH: &str = "/var/run/nvidia-topologyd/";

#[derive(Debug)]
pub(crate) struct GuestVolume {
    mount: oci::Mount,
}

/// GuestVolume: passthrough the mount from guest to container
impl GuestVolume {
    pub fn new(m: &oci::Mount) -> Result<Self> {
            let guest_src = extract_before_guest(&m.source);

            let mnt = oci::Mount {
                destination: m.destination.clone(),
                r#type: m.r#type.to_string(),
                source: guest_src.to_string(),
                options: m.options.clone(),
            };
            warn!(sl!(), "GuestVolume with Mount{:?}.",mnt.clone());
            Ok(Self {
                mount: mnt,
            })
    }
}

#[async_trait]
impl Volume for GuestVolume {
    fn get_volume_mount(&self) -> anyhow::Result<Vec<oci::Mount>> {
        Ok(vec![self.mount.clone()])
    }

    fn get_storage(&self) -> Result<Vec<agent::Storage>> {

        Ok(vec![])
    }

    async fn cleanup(&self, _device_manager: &RwLock<DeviceManager>) -> Result<()> {
        warn!(sl!(), "Cleaning up GuestVolume is still unimplemented.");
        Ok(())
    }

    fn get_device_id(&self) -> Result<Option<String>> {
        Ok(None)
    }
}

pub(crate) fn is_guest_volume(m: &oci::Mount) -> bool {
    //warn!(sl!(), "is_guest_volume: {:?}", &m);
    m.r#type == "bind" && is_guest_mount(&m.source)
}

// Agent will support this kind of bind mount.
fn is_guest_mount(src: &str) -> bool {
    if src.contains(GUEST_PREFIX) {
        return true;
    }

    false
}

fn extract_before_guest(input: &str) -> &str {
    if let Some(index) = input.rfind(GUEST_PREFIX) {
        if index > 0 {
            &input[..index]
        } else {
            ""
        }
    } else {
        input
    }
}