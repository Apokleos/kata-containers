// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::AsRawFd,
};

use super::ensure_snapshot_deadline;
use anyhow::{anyhow, Context, Result};

pub fn import_external_block(
    source: &File,
    source_size: u64,
    deadline: Option<std::time::Instant>,
    destination: &File,
    required_capacity: u64,
) -> Result<()> {
    let destination_metadata = destination
        .metadata()
        .context("inspect writable-rootfs restore destination")?;
    if !destination_metadata.is_file()
        || destination_metadata.len() < required_capacity
        || source_size != required_capacity
    {
        return Err(anyhow!(
            "writable-rootfs restore destination has insufficient capacity"
        ));
    }
    let destination_capacity = destination_metadata.len();
    let mut source = source
        .try_clone()
        .context("clone verified block artifact descriptor")?;
    source.seek(SeekFrom::Start(0))?;
    let mut destination_file = destination.try_clone()?;

    const FICLONE: u64 = 0x4004_9409;
    let cloned = unsafe {
        libc::ioctl(
            destination_file.as_raw_fd(),
            FICLONE as _,
            source.as_raw_fd(),
        )
    } == 0;
    if !cloned {
        destination_file.set_len(0)?;
        destination_file.seek(SeekFrom::Start(0))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            ensure_snapshot_deadline(deadline, "restore external block")?;
            let count = source.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            destination_file.write_all(&buffer[..count])?;
        }
    }
    destination_file.set_len(destination_capacity)?;
    destination_file.sync_all()?;
    Ok(())
}
