// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use anyhow::{bail, Result};
use kata_types::mount::Mount;

/// Host-side OverlayFS paths that make up one writable rootfs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayRootfsLayout {
    pub upperdir: PathBuf,
    pub workdir: PathBuf,
    pub lowerdirs: Vec<PathBuf>,
}

impl OverlayRootfsLayout {
    /// Parse the single OverlayFS mount-option contract used by both rootfs
    /// discovery and cold-restore destination validation.
    pub fn parse(rootfs: &Mount) -> Result<Self> {
        if rootfs.fs_type != "overlay" {
            bail!("rootfs mount type is {}, expected overlay", rootfs.fs_type);
        }

        let upperdir = unique_option(rootfs, "upperdir")?
            .ok_or_else(|| anyhow::anyhow!("writable overlay rootfs is missing upperdir"))?;
        let workdir = unique_option(rootfs, "workdir")?
            .ok_or_else(|| anyhow::anyhow!("writable overlay rootfs is missing workdir"))?;
        let lowerdir = unique_option(rootfs, "lowerdir")?
            .ok_or_else(|| anyhow::anyhow!("writable overlay rootfs is missing lowerdir"))?;

        if upperdir == workdir {
            bail!("overlay upperdir and workdir must be different");
        }

        let lowerdirs = lowerdir
            .as_os_str()
            .to_string_lossy()
            .split(':')
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        if lowerdirs.is_empty() || lowerdirs.iter().any(|path| !path.is_absolute()) {
            bail!("overlay lowerdir entries must be absolute");
        }

        Ok(Self {
            upperdir,
            workdir,
            lowerdirs,
        })
    }
}

fn unique_option(rootfs: &Mount, key: &str) -> Result<Option<PathBuf>> {
    let prefix = format!("{key}=");
    let values = rootfs
        .options
        .iter()
        .filter_map(|option| option.strip_prefix(&prefix))
        .collect::<Vec<_>>();
    if values.len() > 1 {
        bail!("duplicate {key} option in overlay rootfs mount");
    }
    values
        .first()
        .map(|value| {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                bail!("overlay rootfs {key} must be absolute: {value}");
            }
            Ok(path)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_overlay_layout() {
        let mount = Mount {
            fs_type: "overlay".to_string(),
            options: vec![
                "lowerdir=/lower/a:/lower/b".to_string(),
                "upperdir=/upper".to_string(),
                "workdir=/work".to_string(),
            ],
            ..Default::default()
        };
        let layout = OverlayRootfsLayout::parse(&mount).unwrap();
        assert_eq!(layout.upperdir, PathBuf::from("/upper"));
        assert_eq!(layout.workdir, PathBuf::from("/work"));
        assert_eq!(layout.lowerdirs.len(), 2);
    }

    #[test]
    fn rejects_duplicate_or_relative_paths() {
        let duplicate = Mount {
            fs_type: "overlay".to_string(),
            options: vec![
                "lowerdir=/lower".to_string(),
                "upperdir=/upper".to_string(),
                "upperdir=/other".to_string(),
                "workdir=/work".to_string(),
            ],
            ..Default::default()
        };
        assert!(OverlayRootfsLayout::parse(&duplicate).is_err());
    }
}
