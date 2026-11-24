// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

//! Portable artifacts for a host OverlayFS upper directory.
//!
//! This deliberately serializes filesystem data only.  A virtio-fs backend,
//! its socket and its FUSE state are not part of this format.

use std::{
    collections::HashMap,
    ffi::{CString, OsStr},
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Seek, SeekFrom},
    os::fd::AsRawFd,
    os::unix::ffi::OsStrExt,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{anyhow, Context, Result};
use tar::{Archive, Builder, EntryType, Header};

use super::ensure_snapshot_deadline;

const PAX_XATTR_PREFIX: &str = "SCHILY.xattr.";
const PAX_HEX_XATTR_PREFIX: &str = "KATA.xattr.hex.";

fn relative_name(root: &Path, path: &Path) -> Result<PathBuf> {
    let relative = path
        .strip_prefix(root)
        .context("overlay upper entry is outside source root")?;
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(anyhow!("invalid overlay upper archive entry"));
    }
    Ok(relative.to_path_buf())
}

fn append_xattrs(builder: &mut Builder<BufWriter<File>>, path: &Path, symlink: bool) -> Result<()> {
    let names = if symlink {
        xattr::list_deref(path)
    } else {
        xattr::list(path)
    }
    .with_context(|| format!("list xattrs for {}", path.display()))?;

    let mut xattrs = Vec::new();
    for name in names {
        let value = if symlink {
            xattr::get_deref(path, &name)
        } else {
            xattr::get(path, &name)
        }
        .with_context(|| format!("read xattr {:?} from {}", name, path.display()))?
        .ok_or_else(|| anyhow!("xattr {:?} disappeared from {}", name, path.display()))?;
        // PAX records are newline-delimited; OverlayFS xattrs contain arbitrary bytes.
        let key = format!("{PAX_HEX_XATTR_PREFIX}{}", hex::encode(name.as_bytes()));
        xattrs.push((key, hex::encode(value).into_bytes()));
    }
    xattrs.sort_by(|left, right| left.0.cmp(&right.0));
    builder
        .append_pax_extensions(
            xattrs
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_slice())),
        )
        .context("append overlay upper xattrs")?;
    Ok(())
}

fn append_entry(
    builder: &mut Builder<BufWriter<File>>,
    root: &Path,
    path: &Path,
    hardlinks: &mut HashMap<(u64, u64), PathBuf>,
    deadline: Option<Instant>,
) -> Result<()> {
    ensure_snapshot_deadline(deadline, "archive OverlayFS upper")?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect overlay upper entry {}", path.display()))?;
    let file_type = metadata.file_type();
    let relative = relative_name(root, path)?;
    let is_symlink = file_type.is_symlink();
    if file_type.is_socket() {
        return Ok(());
    }
    append_xattrs(builder, path, is_symlink)?;

    if file_type.is_file() && metadata.nlink() > 1 {
        let key = (metadata.dev(), metadata.ino());
        if let Some(first) = hardlinks.get(&key) {
            let mut header = Header::new_gnu();
            header.set_metadata(&metadata);
            header.set_entry_type(EntryType::Link);
            header.set_size(0);
            builder
                .append_link(&mut header, &relative, first)
                .with_context(|| format!("append overlay upper hardlink {}", path.display()))?;
        } else {
            builder
                .append_path_with_name(path, &relative)
                .with_context(|| format!("append overlay upper entry {}", path.display()))?;
            hardlinks.insert(key, relative.clone());
        }
    } else if file_type.is_file() || file_type.is_dir() || is_symlink {
        builder
            .append_path_with_name(path, &relative)
            .with_context(|| format!("append overlay upper entry {}", path.display()))?;
    } else if file_type.is_char_device() && metadata.rdev() == 0 {
        // OverlayFS whiteouts are char devices with major/minor 0:0.
        let mut header = Header::new_gnu();
        header.set_metadata(&metadata);
        header.set_entry_type(EntryType::Char);
        header.set_size(0);
        header
            .set_device_major(0)
            .context("set overlay whiteout device major")?;
        header
            .set_device_minor(0)
            .context("set overlay whiteout device minor")?;
        header.set_cksum();
        builder
            .append_data(&mut header, &relative, std::io::empty())
            .with_context(|| format!("append overlay upper whiteout {}", path.display()))?;
    } else {
        return Err(anyhow!(
            "unsupported overlay upper entry {} (only regular files, directories, symlinks, and OverlayFS whiteouts are allowed)",
            path.display()
        ));
    }

    if file_type.is_dir() {
        let mut children = fs::read_dir(path)
            .with_context(|| format!("read overlay upper directory {}", path.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            append_entry(builder, root, &child.path(), hardlinks, deadline)?;
        }
    }
    Ok(())
}

pub fn archive_overlay_upper(
    source: &Path,
    destination: &Path,
    deadline: Option<Instant>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("inspect overlay upper source {}", source.display()))?;
    if !source.is_absolute() || metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(anyhow!(
            "overlay upper source must be an absolute non-symlink directory"
        ));
    }
    if destination.starts_with(source) {
        return Err(anyhow!(
            "overlay upper artifact destination must be outside the source"
        ));
    }

    let source_dir = File::open(source)
        .with_context(|| format!("open overlay upper source {}", source.display()))?;
    if unsafe { libc::syncfs(source_dir.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("sync overlay upper source {}", source.display()));
    }

    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(destination)
        .with_context(|| format!("create overlay upper artifact {}", destination.display()))?;
    let mut builder = Builder::new(BufWriter::new(output));
    builder.follow_symlinks(false);
    append_xattrs(&mut builder, source, false)?;
    builder
        .append_path_with_name(source, ".")
        .context("append overlay upper root metadata")?;
    let mut hardlinks = HashMap::new();
    let mut children = fs::read_dir(source)
        .with_context(|| format!("read overlay upper source {}", source.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        append_entry(
            &mut builder,
            source,
            &child.path(),
            &mut hardlinks,
            deadline,
        )?;
    }
    let output = builder
        .into_inner()
        .context("finish overlay upper artifact")?
        .into_inner()
        .context("flush overlay upper artifact")?;
    output.sync_all().context("sync overlay upper artifact")?;
    Ok(())
}

pub fn restore_overlay_upper(
    source: &File,
    destination: &Path,
    deadline: Option<Instant>,
) -> Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && fs::read_dir(destination)?.next().is_none() => {}
        Ok(_) => {
            return Err(anyhow!(
                "restored overlay upper destination must be an empty non-symlink directory"
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(destination).with_context(|| {
                format!("create restored overlay upper {}", destination.display())
            })?;
        }
        Err(err) => {
            return Err(err).with_context(|| {
                format!("inspect restored overlay upper {}", destination.display())
            })
        }
    }
    let mut source = source
        .try_clone()
        .context("clone verified overlay upper artifact descriptor")?;
    source.seek(SeekFrom::Start(0))?;
    let mut archive = Archive::new(BufReader::new(source));
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    let mut directories = Vec::new();
    for entry in archive
        .entries()
        .context("read overlay upper archive entries")?
    {
        ensure_snapshot_deadline(deadline, "restore OverlayFS upper")?;
        let mut entry = entry.context("read overlay upper archive entry")?;
        let relative = entry
            .path()
            .context("read overlay upper archive path")?
            .into_owned();
        let target = destination.join(&relative);
        let whiteout = entry.header().entry_type() == EntryType::Char
            && entry.header().device_major()? == Some(0)
            && entry.header().device_minor()? == Some(0);
        let mode = entry.header().mode()?;
        let uid = entry.header().uid()?;
        let gid = entry.header().gid()?;
        let mtime = entry.header().mtime()?;
        let is_directory = entry.header().entry_type() == EntryType::Directory;
        let mut xattrs = Vec::new();
        if let Some(extensions) = entry.pax_extensions()? {
            for extension in extensions {
                let extension = extension.context("read overlay upper PAX extension")?;
                if let Some(name) = extension
                    .key_bytes()
                    .strip_prefix(PAX_HEX_XATTR_PREFIX.as_bytes())
                {
                    xattrs.push((
                        hex::decode(name).context("decode overlay xattr name")?,
                        hex::decode(extension.value_bytes())
                            .context("decode overlay xattr value")?,
                    ));
                } else if let Some(name) = extension
                    .key_bytes()
                    .strip_prefix(PAX_XATTR_PREFIX.as_bytes())
                {
                    xattrs.push((name.to_vec(), extension.value_bytes().to_vec()));
                }
            }
        }
        if relative == Path::new(".") {
            if !is_directory {
                return Err(anyhow!("overlay upper root entry is not a directory"));
            }
        } else if !entry
            .unpack_in(destination)
            .with_context(|| format!("restore overlay upper into {}", destination.display()))?
        {
            return Err(anyhow!("overlay upper archive entry escaped destination"));
        }
        if whiteout {
            fs::remove_file(&target)
                .with_context(|| format!("replace restored whiteout {}", target.display()))?;
            let path = CString::new(target.as_os_str().as_bytes())
                .context("overlay whiteout path contains a NUL byte")?;
            let device = libc::makedev(0, 0);
            if unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | mode, device) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("restore overlay whiteout {}", target.display()));
            }
            if unsafe { libc::lchown(path.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("restore overlay whiteout ownership {}", target.display())
                });
            }
        }
        let is_symlink = fs::symlink_metadata(&target)?.file_type().is_symlink();
        if is_directory {
            directories.push((target, uid, gid, mode, mtime, xattrs));
            continue;
        }
        for (name, value) in xattrs {
            let name = OsStr::from_bytes(&name);
            if is_symlink {
                xattr::set_deref(&target, name, &value)
            } else {
                xattr::set(&target, name, &value)
            }
            .with_context(|| format!("restore xattr {:?} on {}", name, target.display()))?;
        }
    }
    directories.sort_by_key(|(path, ..)| std::cmp::Reverse(path.components().count()));
    for (path, uid, gid, mode, mtime, xattrs) in directories {
        let cpath = CString::new(path.as_os_str().as_bytes())?;
        if unsafe { libc::lchown(cpath.as_ptr(), uid as libc::uid_t, gid as libc::gid_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore directory ownership {}", path.display()));
        }
        if unsafe { libc::chmod(cpath.as_ptr(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore directory mode {}", path.display()));
        }
        for (name, value) in xattrs {
            xattr::set(&path, OsStr::from_bytes(&name), &value)
                .with_context(|| format!("restore directory xattr on {}", path.display()))?;
        }
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: mtime as _,
                tv_nsec: 0,
            },
        ];
        if unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("restore directory mtime {}", path.display()));
        }
    }
    File::open(destination)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn archive_round_trip_preserves_files_and_hardlinks() {
        let source = tempfile::tempdir().unwrap();
        let restored_parent = tempfile::tempdir().unwrap();
        let archive = source.path().join("upper.tar");
        let upper = source.path().join("upper");
        fs::create_dir(&upper).unwrap();
        fs::create_dir(upper.join("dir")).unwrap();
        fs::write(upper.join("dir/file"), b"content").unwrap();
        xattr::set(upper.join("dir/file"), "user.kata.overlay-test", b"value").unwrap();
        fs::hard_link(upper.join("dir/file"), upper.join("dir/link")).unwrap();
        std::os::unix::fs::symlink("file", upper.join("dir/symlink")).unwrap();

        archive_overlay_upper(&upper, &archive, None).unwrap();
        let restored = restored_parent.path().join("upper");
        restore_overlay_upper(&File::open(&archive).unwrap(), &restored, None).unwrap();

        assert_eq!(fs::read(restored.join("dir/file")).unwrap(), b"content");
        assert_eq!(
            xattr::get(restored.join("dir/file"), "user.kata.overlay-test").unwrap(),
            Some(b"value".to_vec())
        );
        assert_eq!(
            fs::read_link(restored.join("dir/symlink")).unwrap(),
            PathBuf::from("file")
        );
        let first = fs::metadata(restored.join("dir/file")).unwrap();
        let second = fs::metadata(restored.join("dir/link")).unwrap();
        assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
    }

    #[test]
    fn archive_round_trip_preserves_directory_metadata_after_children() {
        let source = tempfile::tempdir().unwrap();
        let upper = source.path().join("upper");
        fs::create_dir(&upper).unwrap();
        fs::create_dir(upper.join("nested")).unwrap();
        fs::write(upper.join("nested/file"), b"data").unwrap();
        xattr::set(&upper, "user.kata.root-test", b"root").unwrap();
        for path in [&upper, &upper.join("nested")] {
            let cpath = CString::new(path.as_os_str().as_bytes()).unwrap();
            let times = [
                libc::timespec {
                    tv_sec: 1_700_000_000,
                    tv_nsec: 0,
                },
                libc::timespec {
                    tv_sec: 1_700_000_000,
                    tv_nsec: 0,
                },
            ];
            assert_eq!(
                unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), times.as_ptr(), 0) },
                0
            );
        }
        let archive = source.path().join("upper.tar");
        archive_overlay_upper(&upper, &archive, None).unwrap();
        let restored = source.path().join("restored");
        restore_overlay_upper(&File::open(&archive).unwrap(), &restored, None).unwrap();
        assert_eq!(fs::metadata(&restored).unwrap().mtime(), 1_700_000_000);
        assert_eq!(
            fs::metadata(restored.join("nested")).unwrap().mtime(),
            1_700_000_000
        );
        assert_eq!(
            xattr::get(&restored, "user.kata.root-test").unwrap(),
            Some(b"root".to_vec())
        );
    }

    #[test]
    fn archive_round_trip_preserves_binary_xattr_with_newline() {
        let source = tempfile::tempdir().unwrap();
        let upper = source.path().join("upper");
        fs::create_dir(&upper).unwrap();
        let file = upper.join("file");
        fs::write(&file, b"data").unwrap();
        let value = b"before\nAfter\0\xff";
        xattr::set(&file, "user.kata.binary", value).unwrap();
        let archive = source.path().join("upper.tar");
        archive_overlay_upper(&upper, &archive, None).unwrap();
        let restored = source.path().join("restored");
        restore_overlay_upper(&File::open(&archive).unwrap(), &restored, None).unwrap();
        assert_eq!(
            xattr::get(restored.join("file"), "user.kata.binary").unwrap(),
            Some(value.to_vec())
        );
    }

    #[test]
    fn archive_rejects_special_entries() {
        let source = tempfile::tempdir().unwrap();
        let upper = source.path().join("upper");
        fs::create_dir(&upper).unwrap();
        let fifo = upper.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);

        let error =
            archive_overlay_upper(&upper, &source.path().join("upper.tar"), None).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported overlay upper entry"));
    }

    #[test]
    fn archive_excludes_unix_sockets() {
        let source = tempfile::tempdir().unwrap();
        let upper = source.path().join("upper");
        fs::create_dir(&upper).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(upper.join("service.sock")).unwrap();
        fs::write(upper.join("marker"), b"data").unwrap();

        let archive = source.path().join("upper.tar");
        archive_overlay_upper(&upper, &archive, None).unwrap();
        let destination = source.path().join("restored");
        restore_overlay_upper(&File::open(&archive).unwrap(), &destination, None).unwrap();

        assert_eq!(fs::read(destination.join("marker")).unwrap(), b"data");
        assert!(!destination.join("service.sock").exists());
    }
}
