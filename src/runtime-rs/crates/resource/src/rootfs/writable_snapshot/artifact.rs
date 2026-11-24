// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::{fs::File, path::PathBuf, sync::Arc, time::Instant};

/// Opened immutable checkpoint artifact with verified metadata and identity.
#[derive(Clone, Debug)]
pub struct VerifiedArtifact {
    pub path: PathBuf,
    /// Restore consumes the descriptor verified during preparation.
    pub file: Arc<File>,
    pub size: u64,
    pub identity: VerifiedArtifactIdentity,
    pub deadline: Option<Instant>,
}

#[derive(Clone, Debug)]
pub struct VerifiedArtifactIdentity {
    pub device: u64,
    pub inode: u64,
    pub ctime_seconds: i64,
    pub ctime_nanoseconds: i64,
}
