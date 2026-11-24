// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::{fs, path::Path};

use anyhow::Result;
use common::error::Error as CommonError;
use resource::rootfs::writable_snapshot::{EXTERNAL_BLOCK_FORMAT, HOST_OVERLAY_FORMAT};
use serde::{Deserialize, Serialize};

use crate::factory::template::TemplateIdentity;
use crate::sandbox::checkpoint_restore::artifacts::vm::VmCheckpointArtifact;

use super::checkpoint_files;

pub(super) const WORKLOAD_CHECKPOINT_FORMAT_VERSION: u32 = 3;
pub(super) const WRITABLE_ROOTFS_CHECKPOINT_FORMAT_VERSION: u32 = 4;
pub(super) const WORKLOAD_CHECKPOINT_MANIFEST: &str = "manifest.json";
pub(super) const WORKLOAD_CHECKPOINT_STATE: &str = "vm";
pub(super) const WORKLOAD_CHECKPOINT_STORAGE: &str = "storage";

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointTask {
    pub(super) checkpoint_key: String,
    pub(super) task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) base_image_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) reconnect_only_host_directories: Vec<CheckpointReconnectOnlyHostDirectory>,
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointReconnectOnlyHostDirectory {
    pub(super) destination: String,
    pub(super) readonly: bool,
    pub(super) propagation: i32,
    pub(super) selinux_relabel: bool,
    pub(super) recursive_read_only: bool,
    pub(super) mount_options: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum CheckpointMode {
    #[default]
    FullState,
    ColdRootfs,
}

impl CheckpointMode {
    fn is_full_state(&self) -> bool {
        *self == Self::FullState
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointRootfs {
    pub(super) checkpoint_key: String,
    pub(super) file: String,
    pub(super) format: String,
    pub(super) device_id: String,
    pub(super) index: u64,
    pub(super) driver_option: String,
    pub(super) virt_path: String,
    pub(super) is_direct: Option<bool>,
    pub(super) discard_unmap: bool,
    pub(super) aio: String,
    pub(super) queue_size: u32,
    pub(super) num_queues: usize,
    pub(super) logical_sector_size: u32,
    pub(super) physical_sector_size: u32,
    pub(super) serial_override: String,
    pub(super) logical_size: u64,
    pub(super) encoded_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) immutable_lower: Option<ImmutableRootfs>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct ImmutableRootfs {
    pub(super) device_id: String,
    pub(super) index: u64,
    pub(super) driver_option: String,
    pub(super) virt_path: String,
    pub(super) is_direct: Option<bool>,
    pub(super) discard_unmap: bool,
    pub(super) aio: String,
    pub(super) queue_size: u32,
    pub(super) num_queues: usize,
    pub(super) logical_sector_size: u32,
    pub(super) physical_sector_size: u32,
    pub(super) serial_override: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointOverlayUpper {
    pub(super) checkpoint_key: String,
    pub(super) file: String,
    pub(super) format: String,
    pub(super) size: u64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct CheckpointManifest {
    pub(super) format_version: u32,
    #[serde(default, skip_serializing_if = "CheckpointMode::is_full_state")]
    pub(super) mode: CheckpointMode,
    pub(super) hypervisor: String,
    pub(super) tasks: Vec<CheckpointTask>,
    pub(super) rootfs: Vec<CheckpointRootfs>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) overlay_rootfs: Vec<CheckpointOverlayUpper>,
    pub(super) network_macs: Vec<[u8; 6]>,
    pub(super) vm_vcpus: u32,
    pub(super) vm_memory_mb: u32,
    pub(super) vm_state: Vec<VmCheckpointArtifact>,
    pub(super) runtime_fingerprint: String,
    #[serde(default)]
    pub(super) template: Option<TemplateIdentity>,
}

pub(super) enum WritableLayerArtifactRef<'a> {
    ExternalBlock(&'a CheckpointRootfs),
    HostOverlay(&'a CheckpointOverlayUpper),
}

impl CheckpointManifest {
    pub(super) fn writable_artifact(
        &self,
        checkpoint_key: &str,
    ) -> Result<WritableLayerArtifactRef<'_>> {
        let position = self
            .tasks
            .iter()
            .position(|task| task.checkpoint_key == checkpoint_key)
            .ok_or_else(|| {
                CommonError::InvalidSandboxOperation(format!(
                    "checkpoint task {checkpoint_key} is absent from the manifest"
                ))
            })?;
        let expected = |single: &str, multi: &str| {
            if self.tasks.len() == 1 {
                format!("{WORKLOAD_CHECKPOINT_STORAGE}/{single}")
            } else {
                format!("{WORKLOAD_CHECKPOINT_STORAGE}/rootfs-{position}-{multi}")
            }
        };
        let blocks = self
            .rootfs
            .iter()
            .filter(|a| a.checkpoint_key == checkpoint_key)
            .collect::<Vec<_>>();
        let overlays = self
            .overlay_rootfs
            .iter()
            .filter(|a| a.checkpoint_key == checkpoint_key)
            .collect::<Vec<_>>();
        match (blocks.as_slice(), overlays.as_slice()) {
            ([artifact], []) if artifact.format == EXTERNAL_BLOCK_FORMAT
                && artifact.file == expected("rootfs-base.raw", "base.raw")
                => Ok(WritableLayerArtifactRef::ExternalBlock(artifact)),
            ([], [artifact]) if matches!(artifact.format.as_str(), HOST_OVERLAY_FORMAT | "pax-tar-with-overlay-xattrs-v1")
                && artifact.file == expected("rootfs-upper.tar", "upper.tar")
                => Ok(WritableLayerArtifactRef::HostOverlay(artifact)),
            _ => Err(CommonError::InvalidSandboxOperation(format!(
                "checkpoint task {checkpoint_key} must have exactly one valid writable rootfs artifact"
            )).into()),
        }
    }

    pub(super) fn load(checkpoint_path: &str) -> Result<Self> {
        let (state_path, manifest_path) = checkpoint_files(checkpoint_path)?;

        let manifest_metadata = fs::symlink_metadata(&manifest_path)?;
        if manifest_metadata.file_type().is_symlink() || !manifest_metadata.is_file() {
            return Err(CommonError::InvalidSandboxOperation(
                "checkpoint manifest is not a regular file".to_string(),
            )
            .into());
        }

        let mut manifest_file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&manifest_path)
            .map_err(|err| {
                CommonError::InvalidSandboxOperation(format!("open checkpoint manifest: {err}"))
            })?;

        let mut manifest_bytes = Vec::new();
        manifest_file
            .read_to_end(&mut manifest_bytes)
            .map_err(|err| {
                CommonError::InvalidSandboxOperation(format!("read checkpoint manifest: {err}"))
            })?;
        let manifest: CheckpointManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|err| {
                CommonError::InvalidSandboxOperation(format!("decode checkpoint manifest: {err}"))
            })?;

        let expected_version = match manifest.mode {
            CheckpointMode::FullState => WORKLOAD_CHECKPOINT_FORMAT_VERSION,
            CheckpointMode::ColdRootfs => WRITABLE_ROOTFS_CHECKPOINT_FORMAT_VERSION,
        };
        if manifest.format_version != expected_version {
            return Err(CommonError::InvalidSandboxOperation(
                "unsupported checkpoint format".into(),
            )
            .into());
        }
        if manifest.mode == CheckpointMode::FullState {
            let state_metadata = fs::symlink_metadata(&state_path)?;
            if state_metadata.file_type().is_symlink() || !state_metadata.is_dir() {
                return Err(CommonError::InvalidSandboxOperation(
                    "VM checkpoint artifact path is not a directory".to_string(),
                )
                .into());
            }
            if !manifest.overlay_rootfs.is_empty() {
                return Err(CommonError::InvalidSandboxOperation(
                    "full-VM checkpoint contains OverlayFS upper artifacts".into(),
                )
                .into());
            }
        } else if !manifest.vm_state.is_empty()
            || !manifest.network_macs.is_empty()
            || manifest.vm_vcpus != 0
            || manifest.vm_memory_mb != 0
            || manifest.template.is_some()
        {
            return Err(CommonError::InvalidSandboxOperation(
                "cold rootfs checkpoint contains VM runtime state".into(),
            )
            .into());
        }
        if manifest.mode == CheckpointMode::ColdRootfs {
            if manifest.tasks.is_empty()
                || manifest
                    .tasks
                    .iter()
                    .any(|task| task.base_image_sha256.is_none())
                || manifest.rootfs.len() + manifest.overlay_rootfs.len() != manifest.tasks.len()
            {
                return Err(CommonError::InvalidSandboxOperation(
                    "cold rootfs checkpoint has an invalid task or artifact inventory".into(),
                )
                .into());
            }
            for task in &manifest.tasks {
                manifest.writable_artifact(&task.checkpoint_key)?;
            }
        }

        Ok(manifest)
    }

    pub(super) fn publish(&self, path: &Path) -> Result<()> {
        let partial = path.with_extension("json.partial");
        let result = (|| -> Result<()> {
            fs::write(&partial, serde_json::to_vec_pretty(self)?)?;
            fs::File::open(&partial)?.sync_all()?;
            fs::rename(&partial, path)?;
            fs::File::open(path.parent().unwrap())?.sync_all()?;

            Ok(())
        })();

        if result.is_err() {
            let _ = fs::remove_file(&partial);
            let _ = fs::remove_file(path);
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(vm_vcpus: u32, vm_memory_mb: u32) -> CheckpointManifest {
        CheckpointManifest {
            format_version: WORKLOAD_CHECKPOINT_FORMAT_VERSION,
            mode: CheckpointMode::FullState,
            hypervisor: hypervisor::HYPERVISOR_QEMU.to_string(),
            tasks: Vec::new(),
            rootfs: Vec::new(),
            overlay_rootfs: Vec::new(),
            network_macs: Vec::new(),
            vm_vcpus,
            vm_memory_mb,
            vm_state: Vec::new(),
            runtime_fingerprint: "test".to_string(),
            template: None,
        }
    }

    #[test]
    fn published_manifest_preserves_instance_data() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join(WORKLOAD_CHECKPOINT_STATE)).unwrap();
        let path = directory.path().join(WORKLOAD_CHECKPOINT_MANIFEST);
        let mut original = manifest(2, 256);
        original.tasks.push(CheckpointTask {
            checkpoint_key: "checkpoint-task".into(),
            task_id: "source-task".into(),
            ..Default::default()
        });

        original.publish(&path).unwrap();
        let loaded = CheckpointManifest::load(directory.path().to_str().unwrap()).unwrap();

        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        assert!(!path.with_extension("json.partial").exists());
    }

    #[test]
    fn manifest_requires_vm_resource_topology_fields() {
        for field in ["vm_vcpus", "vm_memory_mb"] {
            let mut value = serde_json::to_value(manifest(1, 128)).unwrap();
            value.as_object_mut().unwrap().remove(field);
            let error = serde_json::from_value::<CheckpointManifest>(value)
                .err()
                .expect("missing VM topology field must be rejected");
            assert!(error
                .to_string()
                .contains(&format!("missing field `{field}`")));
        }
    }

    #[test]
    fn full_vm_v3_keeps_its_existing_wire_shape() {
        let value = serde_json::to_value(manifest(1, 128)).unwrap();
        assert_eq!(value["format_version"], 3);
        assert!(value.get("mode").is_none());
        assert!(value.get("overlay_rootfs").is_none());
    }

    #[test]
    fn cold_v4_loads_without_vm_state_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(WORKLOAD_CHECKPOINT_MANIFEST);
        let mut cold = manifest(0, 0);
        cold.format_version = WRITABLE_ROOTFS_CHECKPOINT_FORMAT_VERSION;
        cold.mode = CheckpointMode::ColdRootfs;
        cold.tasks.push(CheckpointTask {
            checkpoint_key: "app".into(),
            task_id: "source-app".into(),
            base_image_sha256: Some("a".repeat(64)),
            ..Default::default()
        });
        cold.overlay_rootfs.push(CheckpointOverlayUpper {
            checkpoint_key: "app".into(),
            file: "storage/rootfs-upper.tar".into(),
            format: HOST_OVERLAY_FORMAT.into(),
            size: 1,
        });
        cold.publish(&path).unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(persisted["overlay_rootfs"][0].get("sha256").is_none());

        let loaded = CheckpointManifest::load(directory.path().to_str().unwrap()).unwrap();
        assert_eq!(loaded.mode, CheckpointMode::ColdRootfs);
        assert!(!directory.path().join(WORKLOAD_CHECKPOINT_STATE).exists());
    }
}
