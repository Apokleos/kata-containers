// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use agent::Storage;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hypervisor::device::device_manager::DeviceManager;
use oci_spec::runtime as oci;
use tokio::sync::RwLock;

use super::{Rootfs, WritableLayer};

pub(super) struct RestoredBlockRootfs {
    device_ids: Vec<String>,
}

impl RestoredBlockRootfs {
    pub(super) fn new(device_ids: Vec<String>) -> Self {
        Self { device_ids }
    }
}

#[async_trait]
impl Rootfs for RestoredBlockRootfs {
    fn writable_layer(&self) -> WritableLayer {
        self.device_ids.first().map_or_else(
            || WritableLayer::Unknown {
                reason: "restored block rootfs has no device".to_string(),
            },
            |device_id| WritableLayer::ExternalBlock {
                device_id: device_id.clone(),
                fs_type: None,
            },
        )
    }

    fn external_device_ids(&self) -> Vec<String> {
        self.device_ids.clone()
    }

    async fn get_guest_rootfs_path(&self) -> Result<String> {
        Err(anyhow!(
            "full-state restored block rootfs has no provisioning guest path"
        ))
    }

    async fn get_rootfs_mount(&self) -> Result<Vec<oci::Mount>> {
        Err(anyhow!(
            "restored block rootfs does not support provisioning mounts"
        ))
    }

    async fn get_storage(&self) -> Option<Vec<Storage>> {
        None
    }

    async fn get_device_ids(&self) -> Result<Vec<String>> {
        Ok(self.device_ids.clone())
    }

    async fn cleanup(&self, device_manager: &RwLock<DeviceManager>) -> Result<()> {
        let mut manager = device_manager.write().await;
        for device_id in &self.device_ids {
            manager.try_remove_device(device_id).await?;
        }
        Ok(())
    }
}
