// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

//! Backend-neutral writable-rootfs snapshot data operations.
//!
//! This module deliberately knows nothing about Sandbox checkpoint manifests,
//! runtime requests, or container configuration. Callers resolve and verify
//! those protocol objects before passing file descriptors and destinations
//! into these helpers.

mod artifact;
mod external_block;
mod host_overlay;

use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    time::Instant,
};

use anyhow::{anyhow, Result};
use kata_types::mount::{Mount, KATA_BLOCK_VOLUME_CREATE_FS};

use super::OverlayRootfsLayout;
pub use artifact::{VerifiedArtifact, VerifiedArtifactIdentity};

pub const EXTERNAL_BLOCK_FORMAT: &str = "raw";
pub const HOST_OVERLAY_FORMAT: &str = "pax-tar-with-overlay-xattrs-v2";
const WRITABLE_ROOTFS_RESTORE_OPTION: &str = "X-kata.writable-rootfs-restore=true";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritableCheckpointPlan {
    ExternalBlock { device_id: String, fs_type: String },
    HostOverlay { layout: OverlayRootfsLayout },
}

#[derive(Clone, Debug)]
pub enum PreparedWritableRestore {
    ExternalBlock {
        artifact: VerifiedArtifact,
        required_capacity: u64,
        fs_type: String,
    },
    OverlayUpper {
        artifact: VerifiedArtifact,
    },
}

pub enum WritableRestoreTarget {
    ExternalBlock {
        file: File,
        device: u64,
        inode: u64,
        capacity: u64,
    },
    HostOverlay {
        layout: OverlayRootfsLayout,
        upper_device: u64,
        upper_inode: u64,
        work_device: u64,
        work_inode: u64,
    },
}

pub use external_block::import_external_block;
pub use host_overlay::{archive_overlay_upper, restore_overlay_upper};

pub fn ensure_snapshot_deadline(deadline: Option<Instant>, operation: &str) -> Result<()> {
    if deadline
        .map(|deadline| Instant::now() >= deadline)
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "{operation} exceeded the writable-rootfs snapshot deadline"
        ));
    }
    Ok(())
}

pub fn validate_overlay_destination(layout: &OverlayRootfsLayout) -> Result<()> {
    for (name, path) in [("upperdir", &layout.upperdir), ("workdir", &layout.workdir)] {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|err| anyhow!("inspect restored OverlayFS {name}: {err}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!("restored OverlayFS {name} is invalid"));
        }
    }
    let upper_metadata = std::fs::metadata(&layout.upperdir)?;
    let work_metadata = std::fs::metadata(&layout.workdir)?;
    if upper_metadata.dev() != work_metadata.dev()
        || std::fs::read_dir(&layout.workdir)?.next().is_some()
    {
        return Err(anyhow!(
            "restored OverlayFS upperdir/workdir must share a filesystem and workdir must be empty"
        ));
    }
    if std::fs::read_dir(&layout.upperdir)?.next().is_some() {
        return Err(anyhow!(
            "snapshotter-provided OverlayFS upperdir must be empty before restore"
        ));
    }
    Ok(())
}

pub fn resolve_restore_target(
    rootfs_mounts: &[Mount],
    prepared: &PreparedWritableRestore,
) -> Result<WritableRestoreTarget> {
    match prepared {
        PreparedWritableRestore::ExternalBlock {
            artifact,
            required_capacity,
            fs_type,
        } => {
            if fs_type != "ext4" {
                return Err(anyhow!(
                    "writable-rootfs restore requires ext4, got {fs_type}"
                ));
            }
            validate_verified_artifact(artifact)?;
            let candidates = rootfs_mounts
                .iter()
                .filter(|mount| mount.fs_type == *fs_type)
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                return Err(anyhow!(
                    "writable-rootfs restore expected one {fs_type} mount, found {}",
                    candidates.len()
                ));
            }
            let path = Path::new(&candidates[0].source);
            if !path.is_absolute() {
                return Err(anyhow!("writable-rootfs destination is not absolute"));
            }
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_file()
                || metadata.file_type().is_symlink()
                || metadata.len() < *required_capacity
                || artifact.size != *required_capacity
            {
                return Err(anyhow!(
                    "writable-rootfs restore destination has insufficient capacity"
                ));
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)?;
            let opened = file.metadata()?;
            if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
                return Err(anyhow!("writable-rootfs restore destination changed"));
            }
            Ok(WritableRestoreTarget::ExternalBlock {
                file,
                device: opened.dev(),
                inode: opened.ino(),
                capacity: opened.len(),
            })
        }
        PreparedWritableRestore::OverlayUpper { artifact } => {
            validate_verified_artifact(artifact)?;
            let candidates = rootfs_mounts
                .iter()
                .filter(|mount| mount.fs_type == "overlay")
                .collect::<Vec<_>>();
            if candidates.len() != 1 {
                return Err(anyhow!(
                    "writable-rootfs restore expected one overlay mount, found {}",
                    candidates.len()
                ));
            }
            let layout = OverlayRootfsLayout::parse(candidates[0])?;
            validate_overlay_destination(&layout)?;
            let upper = std::fs::metadata(&layout.upperdir)?;
            let work = std::fs::metadata(&layout.workdir)?;
            Ok(WritableRestoreTarget::HostOverlay {
                layout,
                upper_device: upper.dev(),
                upper_inode: upper.ino(),
                work_device: work.dev(),
                work_inode: work.ino(),
            })
        }
    }
}

pub fn apply_prepared_restore(
    rootfs_mounts: &mut [Mount],
    prepared: &PreparedWritableRestore,
    target: &WritableRestoreTarget,
) -> Result<()> {
    match (prepared, target) {
        (
            PreparedWritableRestore::ExternalBlock {
                artifact,
                required_capacity,
                fs_type,
            },
            WritableRestoreTarget::ExternalBlock {
                file,
                device,
                inode,
                capacity,
            },
        ) => {
            validate_verified_artifact(artifact)?;
            let before = file.metadata()?;
            if before.dev() != *device || before.ino() != *inode || before.len() != *capacity {
                return Err(anyhow!("writable-rootfs restore destination changed"));
            }
            import_external_block(
                &artifact.file,
                artifact.size,
                artifact.deadline,
                file,
                *required_capacity,
            )?;
            validate_verified_artifact(artifact)?;
            let after = file.metadata()?;
            if after.dev() != *device || after.ino() != *inode {
                return Err(anyhow!(
                    "writable-rootfs restore destination changed during copy"
                ));
            }
            let uses_erofs_lower = rootfs_mounts.iter().any(|mount| mount.fs_type == "erofs");
            let mount = rootfs_mounts
                .iter_mut()
                .find(|mount| mount.fs_type == *fs_type)
                .ok_or_else(|| anyhow!("writable-rootfs ext4 mount disappeared"))?;
            mount
                .options
                .retain(|option| option != KATA_BLOCK_VOLUME_CREATE_FS);
            if uses_erofs_lower
                && !mount
                    .options
                    .iter()
                    .any(|option| option == WRITABLE_ROOTFS_RESTORE_OPTION)
            {
                mount
                    .options
                    .push(WRITABLE_ROOTFS_RESTORE_OPTION.to_string());
            }
            Ok(())
        }
        (
            PreparedWritableRestore::OverlayUpper { artifact },
            WritableRestoreTarget::HostOverlay {
                layout,
                upper_device,
                upper_inode,
                work_device,
                work_inode,
            },
        ) => {
            validate_verified_artifact(artifact)?;
            validate_overlay_destination(layout)?;
            let upper = std::fs::metadata(&layout.upperdir)?;
            let work = std::fs::metadata(&layout.workdir)?;
            if upper.dev() != *upper_device
                || upper.ino() != *upper_inode
                || work.dev() != *work_device
                || work.ino() != *work_inode
            {
                return Err(anyhow!("OverlayFS restore target changed"));
            }
            restore_overlay_upper(&artifact.file, &layout.upperdir, artifact.deadline)?;
            validate_verified_artifact(artifact)
        }
        _ => Err(anyhow!(
            "writable-rootfs restore target does not match artifact"
        )),
    }
}

fn validate_verified_artifact(artifact: &VerifiedArtifact) -> Result<()> {
    ensure_snapshot_deadline(artifact.deadline, "writable-rootfs restore")?;
    let metadata = artifact.file.metadata()?;
    if !metadata.is_file()
        || metadata.len() != artifact.size
        || !metadata.permissions().readonly()
        || metadata.dev() != artifact.identity.device
        || metadata.ino() != artifact.identity.inode
        || metadata.ctime() != artifact.identity.ctime_seconds
        || metadata.ctime_nsec() != artifact.identity.ctime_nanoseconds
    {
        return Err(anyhow!(
            "verified writable-rootfs artifact {} changed before restore",
            artifact.path.display()
        ));
    }
    Ok(())
}
