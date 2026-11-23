// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context, Result};
use common::error::Error as CommonError;
use ring::digest::{Context as Sha256Context, SHA256};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::factory::vm::VmConfig;
use crate::sandbox::VirtSandbox;

use super::manifest::CheckpointManifest;

pub(super) const RUNTIME_ASSET_IDENTITY_OPTION: &str =
    "io.katacontainers.checkpoint.runtime-assets";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedRuntimeFileIdentity {
    path: PathBuf,
    size: u64,
    sha256: String,
}

fn runtime_asset_identities(
    options: &HashMap<String, String>,
) -> Result<HashMap<PathBuf, TrustedRuntimeFileIdentity>> {
    let Some(value) = options.get(RUNTIME_ASSET_IDENTITY_OPTION) else {
        return Ok(HashMap::new());
    };

    let entries: Vec<TrustedRuntimeFileIdentity> = serde_json::from_str(value).map_err(|err| {
        CommonError::InvalidSandboxOperation(format!(
            "decode trusted runtime asset identities: {err}"
        ))
    })?;

    let mut result = HashMap::with_capacity(entries.len());
    for entry in entries {
        let path = entry.path.clone();
        if !entry.path.is_absolute()
            || entry.size == 0
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || result.insert(path, entry).is_some()
        {
            return Err(CommonError::InvalidSandboxOperation(
                "invalid or duplicate trusted runtime asset identity".to_string(),
            )
            .into());
        }
    }

    Ok(result)
}

#[derive(Eq, Hash, PartialEq)]
struct RuntimeFileCacheKey {
    path: PathBuf,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

static RUNTIME_FILE_DIGEST_CACHE: OnceLock<Mutex<HashMap<RuntimeFileCacheKey, String>>> =
    OnceLock::new();
pub(super) static RUNTIME_FINGERPRINT_SEMAPHORE: Semaphore = Semaphore::const_new(1);

fn runtime_cache_key(path: &Path, metadata: &fs::Metadata) -> RuntimeFileCacheKey {
    RuntimeFileCacheKey {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

#[derive(Serialize)]
pub(super) struct RuntimeFileIdentity {
    pub(super) size: u64,
    pub(super) sha256: String,
}

#[derive(Serialize)]
pub(super) struct RuntimeIdentity {
    pub(super) architecture: String,
    pub(super) hypervisor: RuntimeFileIdentity,
    pub(super) kernel: RuntimeFileIdentity,
    pub(super) initrd: RuntimeFileIdentity,
    pub(super) image: RuntimeFileIdentity,
    pub(super) machine_type: String,
    pub(super) machine_accelerators: String,
    pub(super) cpu_features: String,
    pub(super) default_vcpus_bits: u32,
    pub(super) default_maxvcpus: u32,
    pub(super) default_memory: u32,
    pub(super) default_maxmemory: u32,
    pub(super) memory_slots: u32,
    pub(super) block_device_driver: String,
    pub(super) shared_fs: Option<String>,
}

impl VirtSandbox {
    pub(super) async fn verify_runtime_fingerprint(
        &self,
        manifest: &CheckpointManifest,
        options: &HashMap<String, String>,
    ) -> Result<()> {
        let actual = self.runtime_fingerprint(options).await?;
        if manifest.runtime_fingerprint != actual {
            return Err(CommonError::SandboxOperationPrecondition(
                "checkpoint runtime fingerprint does not match this runtime".to_string(),
            )
            .into());
        }

        Ok(())
    }
}

pub(super) fn file_integrity(path: &Path) -> Result<(u64, String)> {
    let file =
        fs::File::open(path).context(format!("open checkpoint artifact {}", path.display()))?;
    let size = file.metadata()?.len();

    let mut reader = BufReader::new(file);
    let mut hasher = Sha256Context::new(&SHA256);

    // This helper runs from Tokio worker tasks whose stacks are small.
    // Keep the large I/O buffer on the heap to avoid aborting a deeply
    // nested Restore call with a worker-stack overflow.
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .context(format!("read checkpoint artifact {}", path.display()))?;
        if count == 0 {
            break;
        }

        hasher.update(&buffer[..count]);
    }

    let digest = hasher.finish();
    let sha256 = digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    Ok((size, sha256))
}

impl VirtSandbox {
    fn runtime_file_identity(
        path: &str,
        trusted: Option<&TrustedRuntimeFileIdentity>,
    ) -> Result<RuntimeFileIdentity> {
        if path.is_empty() {
            return Ok(RuntimeFileIdentity {
                size: 0,
                sha256: String::new(),
            });
        }

        let path = Path::new(path);
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(CommonError::InvalidSandboxOperation(format!(
                "runtime asset {} is not a regular file",
                path.display()
            ))
            .into());
        }

        if let Some(trusted) = trusted {
            if metadata.len() != trusted.size {
                return Err(CommonError::InvalidSandboxOperation(format!(
                    "trusted runtime asset size for {} does not match the runtime file",
                    path.display()
                ))
                .into());
            }

            return Ok(RuntimeFileIdentity {
                size: trusted.size,
                sha256: trusted.sha256.clone(),
            });
        }

        let key = runtime_cache_key(path, &metadata);
        let cache = RUNTIME_FILE_DIGEST_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(sha256) = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(RuntimeFileIdentity {
                size: metadata.len(),
                sha256,
            });
        }

        let (size, sha256) = file_integrity(path)?;
        let current_metadata = fs::metadata(path)?;
        if key == runtime_cache_key(path, &current_metadata) {
            cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, sha256.clone());
        }

        Ok(RuntimeFileIdentity { size, sha256 })
    }

    pub(super) async fn runtime_fingerprint(
        &self,
        options: &HashMap<String, String>,
    ) -> Result<String> {
        let started = Instant::now();

        let config = self.hypervisor.hypervisor_config().await;
        let template = if let Some(identity) = &self.template_identity {
            Some((
                identity.clone(),
                VmConfig::new(self.resource_manager.config().await.as_ref()),
            ))
        } else {
            None
        };

        let trusted = runtime_asset_identities(options)?;
        let permit = RUNTIME_FINGERPRINT_SEMAPHORE.acquire().await?;

        let fingerprint = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let expected: HashSet<PathBuf> = [
                &config.path,
                &config.boot_info.kernel,
                &config.boot_info.initrd,
                &config.boot_info.image,
            ]
            .iter()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .collect();

            if !trusted.is_empty()
                && (trusted.len() != expected.len()
                    || trusted.keys().any(|path| !expected.contains(path)))
            {
                return Err(CommonError::InvalidSandboxOperation(
                    "runtime asset identities do not match the active runtime configuration"
                        .to_string(),
                )
                .into());
            }

            let hypervisor =
                Self::runtime_file_identity(&config.path, trusted.get(Path::new(&config.path)))?;
            let kernel = Self::runtime_file_identity(
                &config.boot_info.kernel,
                trusted.get(Path::new(&config.boot_info.kernel)),
            )?;
            let initrd = Self::runtime_file_identity(
                &config.boot_info.initrd,
                trusted.get(Path::new(&config.boot_info.initrd)),
            )?;
            let image = Self::runtime_file_identity(
                &config.boot_info.image,
                trusted.get(Path::new(&config.boot_info.image)),
            )?;

            if let Some((template, template_config)) = &template {
                let assets = [
                    (config.path.as_str(), &hypervisor),
                    (config.boot_info.kernel.as_str(), &kernel),
                    (config.boot_info.initrd.as_str(), &initrd),
                    (config.boot_info.image.as_str(), &image),
                ];
                template
                    .verify_vm_config(template_config, |path| {
                        assets
                            .iter()
                            .find(|(asset, _)| *asset == path)
                            .map(|(_, identity)| identity.sha256.clone())
                            .ok_or_else(|| anyhow::anyhow!("template boot asset {path} is absent"))
                    })
                    .map_err(|err| {
                        CommonError::SandboxOperationPrecondition(format!(
                            "VM template boot assets are incompatible: {err:#}"
                        ))
                    })?;
            }

            let identity = RuntimeIdentity {
                architecture: std::env::consts::ARCH.to_string(),
                hypervisor,
                kernel,
                initrd,
                image,
                machine_type: config.machine_info.machine_type,
                machine_accelerators: config.machine_info.machine_accelerators,
                cpu_features: config.cpu_info.cpu_features,
                default_vcpus_bits: config.cpu_info.default_vcpus.to_bits(),
                default_maxvcpus: config.cpu_info.default_maxvcpus,
                default_memory: config.memory_info.default_memory,
                default_maxmemory: config.memory_info.default_maxmemory,
                memory_slots: config.memory_info.memory_slots,
                block_device_driver: config.blockdev_info.block_device_driver,
                shared_fs: config.shared_fs.shared_fs,
            };

            let mut hasher = Sha256Context::new(&SHA256);
            hasher.update(&serde_json::to_vec(&identity)?);
            let digest = hasher.finish();

            Ok::<String, anyhow::Error>(
                digest
                    .as_ref()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            )
        })
        .await
        .context("runtime fingerprint worker failed")??;

        debug!(
            sl!(),
            "runtime fingerprint calculated";
            "elapsed_us" => started.elapsed().as_micros(),
        );

        Ok(fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_file_digest_is_standard_sha256_and_invalidates() {
        let path = std::env::temp_dir().join(format!(
            "kata-runtime-fingerprint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"abc").unwrap();
        let first = VirtSandbox::runtime_file_identity(path.to_str().unwrap(), None).unwrap();
        assert_eq!(
            first.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        fs::write(&path, b"different size").unwrap();
        let second = VirtSandbox::runtime_file_identity(path.to_str().unwrap(), None).unwrap();
        assert_ne!(first.sha256, second.sha256);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn trusted_runtime_asset_uses_verified_digest_and_checks_size() {
        let path = std::env::temp_dir().join(format!(
            "kata-trusted-runtime-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"abc").unwrap();
        let trusted = TrustedRuntimeFileIdentity {
            path: path.clone(),
            size: 3,
            sha256: "a".repeat(64),
        };
        let identity =
            VirtSandbox::runtime_file_identity(path.to_str().unwrap(), Some(&trusted)).unwrap();
        assert_eq!(identity.sha256, trusted.sha256);

        fs::write(&path, b"changed-size").unwrap();
        assert!(
            VirtSandbox::runtime_file_identity(path.to_str().unwrap(), Some(&trusted)).is_err()
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn trusted_runtime_asset_option_is_strict() {
        let mut options = HashMap::new();
        options.insert(
            RUNTIME_ASSET_IDENTITY_OPTION.to_string(),
            r#"[{"path":"/runtime/kernel","size":3,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]"#.to_string(),
        );
        let parsed = runtime_asset_identities(&options).unwrap();
        assert_eq!(parsed.len(), 1);

        options.insert(
            RUNTIME_ASSET_IDENTITY_OPTION.to_string(),
            r#"[{"path":"relative","size":3,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]"#.to_string(),
        );
        assert!(runtime_asset_identities(&options).is_err());
    }
}
