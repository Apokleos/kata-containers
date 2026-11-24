// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result};
use common::{
    error::Error as CommonError,
    types::{
        BaseImageIdentity, CheckpointSandboxRequest, ReconnectOnlyHostDirectory,
        RestoreSandboxRequest, RestoredSandboxTask, RestoredTaskMode,
    },
};
use hypervisor::{BlockDeviceModern, BlockSourceFormat};
use resource::rootfs::{
    writable_snapshot::{
        archive_overlay_upper, PreparedWritableRestore, WritableCheckpointPlan,
        EXTERNAL_BLOCK_FORMAT, HOST_OVERLAY_FORMAT,
    },
    OverlayRootfsLayout, WritableLayer,
};

use super::{
    checkpoint_files, ensure_artifact_deadline,
    manifest::{
        CheckpointManifest, CheckpointMode, CheckpointOverlayUpper,
        CheckpointReconnectOnlyHostDirectory, CheckpointRootfs, CheckpointTask,
        WORKLOAD_CHECKPOINT_STORAGE, WRITABLE_ROOTFS_CHECKPOINT_FORMAT_VERSION,
    },
};
use crate::sandbox::VirtSandbox;

pub(super) const CHECKPOINT_FIDELITY_OPTION: &str = "io.katacontainers.checkpoint.fidelity";

pub(super) fn checkpoint_is_cold(options: &HashMap<String, String>) -> Result<bool> {
    match options.get(CHECKPOINT_FIDELITY_OPTION).map(String::as_str) {
        None | Some("full-vm") => Ok(false),
        Some("writable-rootfs") => Ok(true),
        Some(other) => Err(CommonError::InvalidSandboxOperation(format!(
            "unsupported checkpoint fidelity {other}"
        ))
        .into()),
    }
}

fn base_image_digest(image: &BaseImageIdentity) -> &str {
    match image {
        BaseImageIdentity::OciImage { digest } => digest,
    }
}

fn manifest_directories(
    directories: &[common::types::ReconnectOnlyHostDirectory],
) -> Vec<CheckpointReconnectOnlyHostDirectory> {
    directories
        .iter()
        .map(|directory| CheckpointReconnectOnlyHostDirectory {
            destination: directory.destination.clone(),
            readonly: directory.readonly,
            propagation: directory.propagation,
            selinux_relabel: directory.selinux_relabel,
            recursive_read_only: directory.recursive_read_only,
            mount_options: directory.mount_options.clone(),
        })
        .collect()
}

pub(super) fn cold_restore_tasks(
    req: &RestoreSandboxRequest,
    manifest: &CheckpointManifest,
    prepared: &mut HashMap<String, PreparedWritableRestore>,
) -> Result<Vec<RestoredSandboxTask>> {
    if !checkpoint_is_cold(&req.options)? && req.options.contains_key(CHECKPOINT_FIDELITY_OPTION) {
        return Err(CommonError::InvalidSandboxOperation(
            "restore fidelity does not match cold checkpoint".into(),
        )
        .into());
    }
    if manifest.tasks.is_empty() || manifest.tasks.len() != req.tasks.len() {
        return Err(CommonError::InvalidSandboxOperation(
            "restore task inventory does not match cold checkpoint".into(),
        )
        .into());
    }
    let sources = manifest
        .tasks
        .iter()
        .map(|task| (task.checkpoint_key.as_str(), task))
        .collect::<HashMap<_, _>>();
    if sources.len() != manifest.tasks.len() {
        return Err(
            CommonError::InvalidSandboxOperation("duplicate checkpoint task key".into()).into(),
        );
    }
    let mut keys = HashSet::new();
    let mut ids = HashSet::new();
    let mut tasks = Vec::with_capacity(req.tasks.len());
    for task in &req.tasks {
        if task.checkpoint_key.is_empty()
            || !keys.insert(task.checkpoint_key.as_str())
            || task.task_id.is_empty()
            || !ids.insert(task.task_id.as_str())
        {
            return Err(CommonError::InvalidSandboxOperation(
                "invalid or duplicate cold restore task".into(),
            )
            .into());
        }
        let source = sources.get(task.checkpoint_key.as_str()).ok_or_else(|| {
            CommonError::InvalidSandboxOperation(format!(
                "restore task {} is absent from checkpoint",
                task.checkpoint_key
            ))
        })?;
        manifest.writable_artifact(&task.checkpoint_key)?;
        if task.has_undeclared_mounts {
            return Err(CommonError::SandboxOperationPrecondition(format!(
                "restore task {} has undeclared CRI mounts",
                task.checkpoint_key
            ))
            .into());
        }
        let expected = source.base_image_sha256.as_deref().ok_or_else(|| {
            CommonError::InvalidSandboxOperation("cold checkpoint lacks base image digest".into())
        })?;
        let observed = task
            .base_image
            .as_ref()
            .map(base_image_digest)
            .ok_or_else(|| {
                CommonError::SandboxOperationPrecondition(
                    "restore requires a digest-pinned image".into(),
                )
            })?;
        if expected != observed
            || source.reconnect_only_host_directories.len()
                != task.reconnect_only_host_directories.len()
            || !source
                .reconnect_only_host_directories
                .iter()
                .zip(&task.reconnect_only_host_directories)
                .all(|(a, b)| {
                    a.destination == b.destination
                        && a.readonly == b.readonly
                        && a.propagation == b.propagation
                        && a.selinux_relabel == b.selinux_relabel
                        && a.recursive_read_only == b.recursive_read_only
                })
        {
            return Err(CommonError::SandboxOperationPrecondition(format!(
                "restore task {} does not match checkpoint rootfs contract",
                task.checkpoint_key
            ))
            .into());
        }
        let artifact = prepared.remove(&task.checkpoint_key).ok_or_else(|| {
            CommonError::InvalidSandboxOperation("prepared writable artifact is missing".into())
        })?;
        tasks.push(RestoredSandboxTask {
            checkpoint_key: task.checkpoint_key.clone(),
            task_id: task.task_id.clone(),
            mode: RestoredTaskMode::ColdWritableRootfs {
                prepared: artifact,
                reconnect_only_host_directories: source
                    .reconnect_only_host_directories
                    .iter()
                    .map(|directory| ReconnectOnlyHostDirectory {
                        destination: directory.destination.clone(),
                        readonly: directory.readonly,
                        propagation: directory.propagation,
                        selinux_relabel: directory.selinux_relabel,
                        recursive_read_only: directory.recursive_read_only,
                        mount_options: directory.mount_options.clone(),
                    })
                    .collect(),
            },
        });
    }
    if !prepared.is_empty() {
        return Err(CommonError::InvalidSandboxOperation(
            "unknown prepared writable artifacts".into(),
        )
        .into());
    }
    Ok(tasks)
}

fn clone_or_copy(source: &Path, destination: &Path) -> Result<()> {
    let source_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(source)?;
    let destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o400)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)?;
    const FICLONE: u64 = 0x4004_9409;
    let cloned = unsafe {
        libc::ioctl(
            destination_file.as_raw_fd(),
            FICLONE as _,
            source_file.as_raw_fd(),
        )
    };
    if cloned < 0 {
        let error = std::io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY)
        ) {
            return Err(error).context("clone writable rootfs artifact");
        }
        drop(destination_file);
        fs::remove_file(destination)?;
        fs::copy(source, destination)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o400))?;
        OpenOptions::new()
            .read(true)
            .open(destination)?
            .sync_all()?;
    } else {
        destination_file.sync_all()?;
    }
    Ok(())
}

struct ColdArtifacts {
    blocks: Vec<CheckpointRootfs>,
    uppers: Vec<CheckpointOverlayUpper>,
}

fn plan_for_task(task: &common::types::SandboxCheckpointTask) -> Result<WritableCheckpointPlan> {
    match task.writable_layer.as_ref() {
        Some(WritableLayer::ExternalBlock {
            device_id,
            fs_type: Some(fs_type),
        }) if fs_type == "ext4" => Ok(WritableCheckpointPlan::ExternalBlock {
            device_id: device_id.clone(),
            fs_type: fs_type.clone(),
        }),
        Some(WritableLayer::HostDirectory {
            upperdir,
            workdir,
            lowerdirs,
        }) => Ok(WritableCheckpointPlan::HostOverlay {
            layout: OverlayRootfsLayout {
                upperdir: upperdir.clone(),
                workdir: workdir.clone(),
                lowerdirs: lowerdirs.clone(),
            },
        }),
        Some(layer) => Err(CommonError::SandboxOperationUnsupported(format!(
            "checkpoint task {} writable layer {layer:?} is unsupported",
            task.task_id,
        ))
        .into()),
        None => Err(CommonError::SandboxOperationPrecondition(format!(
            "checkpoint task {} has no resolved writable layer",
            task.task_id,
        ))
        .into()),
    }
}

fn write_cold_artifacts(
    req: &CheckpointSandboxRequest,
    plans: Vec<WritableCheckpointPlan>,
    blocks: Vec<BlockDeviceModern>,
    deadline: Option<Instant>,
) -> Result<ColdArtifacts> {
    let mut blocks_by_id = blocks
        .into_iter()
        .map(|block| (block.device_id.clone(), block))
        .collect::<HashMap<_, _>>();
    let storage = Path::new(&req.output_path).join(WORKLOAD_CHECKPOINT_STORAGE);
    fs::create_dir(&storage).context("create cold rootfs storage")?;
    let mut artifacts = ColdArtifacts {
        blocks: Vec::new(),
        uppers: Vec::new(),
    };
    for (position, (task, plan)) in req.tasks.iter().zip(plans).enumerate() {
        let single = req.tasks.len() == 1;
        match plan {
            WritableCheckpointPlan::HostOverlay { layout } => {
                let name = if single {
                    "rootfs-upper.tar".to_owned()
                } else {
                    format!("rootfs-{position}-upper.tar")
                };
                let relative = format!("{WORKLOAD_CHECKPOINT_STORAGE}/{name}");
                let destination = Path::new(&req.output_path).join(&relative);
                let partial = destination.with_extension("tar.partial");
                archive_overlay_upper(&layout.upperdir, &partial, deadline)?;
                fs::set_permissions(&partial, fs::Permissions::from_mode(0o400))?;
                fs::rename(&partial, &destination)?;
                let size = fs::metadata(&destination)?.len();
                artifacts.uppers.push(CheckpointOverlayUpper {
                    checkpoint_key: task.checkpoint_key.clone(),
                    file: relative,
                    format: HOST_OVERLAY_FORMAT.to_string(),
                    size,
                });
            }
            WritableCheckpointPlan::ExternalBlock { device_id, .. } => {
                let block = blocks_by_id.remove(&device_id).ok_or_else(|| {
                    CommonError::SandboxOperationPrecondition(format!(
                        "writable rootfs device {device_id} is absent"
                    ))
                })?;
                let source = Path::new(&block.config.path_on_host);
                let metadata = fs::symlink_metadata(source)?;
                if !source.is_absolute()
                    || metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || !matches!(block.config.source, BlockSourceFormat::Raw)
                {
                    return Err(CommonError::SandboxOperationPrecondition(format!(
                        "writable rootfs block {} is not a regular raw file",
                        source.display()
                    ))
                    .into());
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(source)?
                    .sync_all()?;
                let name = if single {
                    "rootfs-base.raw".to_owned()
                } else {
                    format!("rootfs-{position}-base.raw")
                };
                let relative = format!("{WORKLOAD_CHECKPOINT_STORAGE}/{name}");
                let destination = Path::new(&req.output_path).join(&relative);
                let partial = destination.with_extension("raw.partial");
                ensure_artifact_deadline(deadline, "writable rootfs artifact clone")?;
                clone_or_copy(source, &partial)?;
                fs::rename(&partial, &destination)?;
                let size = fs::metadata(&destination)?.len();
                artifacts.blocks.push(CheckpointRootfs {
                    checkpoint_key: task.checkpoint_key.clone(),
                    file: relative,
                    format: EXTERNAL_BLOCK_FORMAT.to_owned(),
                    device_id: block.device_id,
                    index: block.config.index,
                    driver_option: block.config.driver_option,
                    virt_path: block.config.virt_path,
                    is_direct: block.config.is_direct,
                    discard_unmap: block.config.discard_unmap,
                    aio: block.config.blkdev_aio.to_string(),
                    queue_size: block.config.queue_size,
                    num_queues: block.config.num_queues,
                    logical_sector_size: block.config.logical_sector_size,
                    physical_sector_size: block.config.physical_sector_size,
                    serial_override: block.config.serial_override,
                    logical_size: size,
                    encoded_size: size,
                    immutable_lower: None,
                });
            }
        }
    }
    fs::File::open(&storage)?.sync_all()?;
    Ok(artifacts)
}

impl VirtSandbox {
    pub(super) async fn validate_cold_rootfs_checkpoint(
        &self,
        req: &CheckpointSandboxRequest,
    ) -> Result<()> {
        if req.sandbox_id != self.sid || req.tasks.is_empty() {
            return Err(CommonError::InvalidSandboxOperation(
                "invalid cold rootfs checkpoint sandbox or task inventory".into(),
            )
            .into());
        }
        if req.options.keys().any(|key| {
            key != CHECKPOINT_FIDELITY_OPTION
                && key != super::identity::RUNTIME_ASSET_IDENTITY_OPTION
        }) {
            return Err(CommonError::InvalidSandboxOperation(
                "unsupported cold rootfs checkpoint option".into(),
            )
            .into());
        }
        checkpoint_files(&req.output_path)?;
        if fs::read_dir(&req.output_path)?.next().is_some() {
            return Err(CommonError::InvalidSandboxOperation(
                "checkpoint output directory is not empty".into(),
            )
            .into());
        }
        self.validate_cold_rootfs_inventory(req).await.map(|_| ())
    }

    async fn validate_cold_rootfs_inventory(
        &self,
        req: &CheckpointSandboxRequest,
    ) -> Result<Vec<WritableCheckpointPlan>> {
        let manager = self.resource_manager.get_device_manager().await;
        let blocks = manager
            .read()
            .await
            .checkpoint_block_devices()
            .await
            .into_iter()
            .map(|block| (block.device_id.clone(), block))
            .collect::<HashMap<_, _>>();
        let mut rootfs_ids = HashSet::new();
        let mut writable_ids = HashSet::new();
        let mut plans = Vec::with_capacity(req.tasks.len());
        for task in &req.tasks {
            if task.base_image.is_none() || task.has_undeclared_mounts {
                return Err(CommonError::SandboxOperationPrecondition(format!(
                    "checkpoint task {} lacks a reproducible image or mount contract",
                    task.task_id
                ))
                .into());
            }
            rootfs_ids.extend(task.rootfs_external_device_ids.iter().cloned());
            let plan = plan_for_task(task)?;
            match &plan {
                WritableCheckpointPlan::HostOverlay { layout } => {
                    let upper = fs::symlink_metadata(&layout.upperdir)?;
                    let work = fs::symlink_metadata(&layout.workdir)?;
                    if layout.upperdir == layout.workdir
                        || upper.file_type().is_symlink()
                        || work.file_type().is_symlink()
                        || !upper.is_dir()
                        || !work.is_dir()
                        || layout.lowerdirs.is_empty()
                        || !task.rootfs_external_device_ids.is_empty()
                    {
                        return Err(CommonError::SandboxOperationPrecondition(format!(
                            "checkpoint task {} has an invalid host OverlayFS rootfs contract",
                            task.task_id
                        ))
                        .into());
                    }
                }
                WritableCheckpointPlan::ExternalBlock { device_id, .. } => {
                    if device_id.is_empty()
                        || !writable_ids.insert(device_id.clone())
                        || !rootfs_ids.contains(device_id)
                        || !blocks.contains_key(device_id)
                    {
                        return Err(CommonError::SandboxOperationPrecondition(format!(
                            "checkpoint task {} has an invalid writable rootfs device {device_id}",
                            task.task_id
                        ))
                        .into());
                    }
                }
            }
            plans.push(plan);
        }
        if blocks.keys().any(|id| !rootfs_ids.contains(id)) {
            return Err(CommonError::SandboxOperationUnsupported(
                "external block volumes require a stable storage snapshot identity".into(),
            )
            .into());
        }
        Ok(plans)
    }

    pub(super) async fn checkpoint_cold_rootfs(
        &self,
        req: &CheckpointSandboxRequest,
        owns_output: &mut bool,
    ) -> Result<()> {
        let started = Instant::now();
        let deadline = req
            .operation_timeout
            .and_then(|timeout| started.checked_add(timeout));
        let plans = self.validate_cold_rootfs_inventory(req).await?;
        let (_, manifest_path) = checkpoint_files(&req.output_path)?;
        *owns_output = true;

        self.hypervisor
            .pause_vm()
            .await
            .context("pause VM while copying cold rootfs")?;
        let device_manager = self.resource_manager.get_device_manager().await;
        let blocks = device_manager.read().await.checkpoint_block_devices().await;
        let snapshot_request = req.clone();
        let checkpoint_result = tokio::task::spawn_blocking(move || {
            write_cold_artifacts(&snapshot_request, plans, blocks, deadline)
        })
        .await
        .context("cold rootfs artifact worker failed")
        .and_then(|result| result);
        let resume_result = self.hypervisor.resume_vm().await;
        let artifacts = match checkpoint_result {
            Ok(artifacts) => artifacts,
            Err(error) => {
                if let Err(resume_error) = resume_result {
                    return Err(error).context(format!(
                        "resume VM after cold rootfs checkpoint failure also failed: {resume_error:#}"
                    ));
                }
                return Err(error);
            }
        };
        resume_result.context("resume VM after cold rootfs checkpoint")?;

        let hypervisor_name = self
            .resource_manager
            .config()
            .await
            .runtime
            .hypervisor_name
            .clone();
        let runtime_fingerprint = self.runtime_fingerprint(&req.options).await?;
        let manifest = CheckpointManifest {
            format_version: WRITABLE_ROOTFS_CHECKPOINT_FORMAT_VERSION,
            mode: CheckpointMode::ColdRootfs,
            hypervisor: hypervisor_name,
            tasks: req
                .tasks
                .iter()
                .map(|task| CheckpointTask {
                    checkpoint_key: task.checkpoint_key.clone(),
                    task_id: task.task_id.clone(),
                    base_image_sha256: task
                        .base_image
                        .as_ref()
                        .map(base_image_digest)
                        .map(str::to_owned),
                    reconnect_only_host_directories: manifest_directories(
                        &task.reconnect_only_host_directories,
                    ),
                })
                .collect(),
            rootfs: artifacts.blocks,
            overlay_rootfs: artifacts.uppers,
            network_macs: Vec::new(),
            vm_vcpus: 0,
            vm_memory_mb: 0,
            vm_state: Vec::new(),
            runtime_fingerprint,
            template: None,
        };
        manifest.publish(&manifest_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fidelity_option_is_explicit_and_strict() {
        let mut options = HashMap::new();
        assert!(!checkpoint_is_cold(&options).unwrap());
        options.insert(CHECKPOINT_FIDELITY_OPTION.into(), "writable-rootfs".into());
        assert!(checkpoint_is_cold(&options).unwrap());
        options.insert(CHECKPOINT_FIDELITY_OPTION.into(), "full-vm".into());
        assert!(!checkpoint_is_cold(&options).unwrap());
        options.insert(CHECKPOINT_FIDELITY_OPTION.into(), "unknown".into());
        assert!(checkpoint_is_cold(&options).is_err());
    }
}
