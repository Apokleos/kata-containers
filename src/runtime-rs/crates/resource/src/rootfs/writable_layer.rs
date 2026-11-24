// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

/// Location and kind of a live container rootfs writable layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritableLayer {
    ReadOnly,
    HostDirectory {
        upperdir: PathBuf,
        workdir: PathBuf,
        lowerdirs: Vec<PathBuf>,
    },
    ExternalBlock {
        device_id: String,
        fs_type: Option<String>,
    },
    GuestTmpfs {
        upperdir: PathBuf,
        workdir: PathBuf,
    },
    Unknown {
        reason: String,
    },
}
