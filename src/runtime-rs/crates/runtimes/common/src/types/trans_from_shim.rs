// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use super::{
    BaseImageIdentity, CheckpointSandboxRequest, ContainerConfig, ContainerID, ContainerProcess,
    CreateSandboxRequest as InternalCreateSandboxRequest, ExecProcessRequest, KillRequest,
    PrepareRestoreOptions, ReconnectOnlyHostDirectory, ResizePTYRequest, RestoreSandboxRequest,
    SandboxCheckpointTask, SandboxConfig, SandboxID, SandboxNetworkEnv, SandboxRequest,
    SandboxRestoreTask, SandboxStatusRequest, ShutdownRequest, StopSandboxRequest, TaskRequest,
    UpdateRequest, DEFAULT_SHM_SIZE,
};

use kata_types::mount::Mount;
use std::{
    collections::{HashMap, HashSet},
    convert::{From, TryFrom},
    fs,
    path::{Component, Path, PathBuf},
};

use protobuf::Message;
use runtime_spec;

use protocols::api as cri_api_v1;

use anyhow::{anyhow, Context, Result};
use containerd_shim_protos::{api, sandbox_api};

pub const SANDBOX_API_V1: &str = "runtime.v1.PodSandboxConfig";
const CONTAINER_CONFIG_V1: &str = "runtime.v1.ContainerConfig";
const CONTAINER_STATUS_V1: &str = "runtime.v1.ContainerStatus";
pub const RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION: &str =
    "io.katacontainers.checkpoint.reconnect-only-host-directory-mounts";

fn canonical_sha256_digest(value: &str) -> Option<String> {
    let marker = "sha256:";
    let offset = value.rfind(marker)?;
    let digest = &value[offset + marker.len()..];
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{marker}{}", digest.to_ascii_lowercase()))
}

fn checkpoint_base_image(
    status: &protobuf::MessageField<protobuf::well_known_types::any::Any>,
    config: &protobuf::MessageField<protobuf::well_known_types::any::Any>,
) -> Option<BaseImageIdentity> {
    let resolved = status.as_ref().and_then(|status| {
        if !status.type_url.ends_with(CONTAINER_STATUS_V1) {
            return None;
        }
        let status = cri_api_v1::ContainerStatus::parse_from_bytes(&status.value).ok()?;
        canonical_sha256_digest(&status.image().image)
            .or_else(|| canonical_sha256_digest(&status.image_ref))
    });
    resolved
        .map(|digest| BaseImageIdentity::OciImage { digest })
        .or_else(|| restore_base_image(config))
}

fn restore_base_image(
    config: &protobuf::MessageField<protobuf::well_known_types::any::Any>,
) -> Option<BaseImageIdentity> {
    let config = config.as_ref()?;
    if !config.type_url.ends_with(CONTAINER_CONFIG_V1) {
        return None;
    }
    let config = cri_api_v1::ContainerConfig::parse_from_bytes(&config.value).ok()?;
    canonical_sha256_digest(&config.image().image)
        .map(|digest| BaseImageIdentity::OciImage { digest })
}

fn canonical_mount_destination(value: &str) -> Result<String> {
    let path = Path::new(value);
    if !path.is_absolute() || path == Path::new("/") {
        return Err(anyhow!(
            "mount destination must be an absolute non-root path: {value}"
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::Prefix(_)
        )
    }) {
        return Err(anyhow!("mount destination is not canonical: {value}"));
    }
    let canonical = path.to_string_lossy().to_string();
    if canonical != value {
        return Err(anyhow!("mount destination is not canonical: {value}"));
    }
    if ["/proc", "/sys", "/dev"]
        .iter()
        .any(|reserved| path.starts_with(reserved))
    {
        return Err(anyhow!(
            "reconnect-only mount destination overlaps a reserved guest path: {value}"
        ));
    }
    Ok(canonical)
}

fn mount_destinations_overlap(left: &str, right: &str) -> bool {
    let left = Path::new(left);
    let right = Path::new(right);
    left != right && (left.starts_with(right) || right.starts_with(left))
}

fn is_regenerated_cri_file(mount: &cri_api_v1::Mount) -> Result<bool> {
    let destination = mount.container_path();
    if !matches!(destination, "/etc/hosts" | "/dev/termination-log") {
        return Ok(false);
    }
    let source = Path::new(mount.host_path());
    let generated_path = match destination {
        "/etc/hosts" => {
            source.file_name().is_some_and(|name| name == "etc-hosts")
                && source
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "pods")
        }
        "/dev/termination-log" => {
            source
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|name| name == "containers")
                && source
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "pods")
        }
        _ => false,
    };
    if !generated_path {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(source)?;
    if !source.is_absolute() || metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!(
            "regenerated CRI mount {} must have an absolute regular-file source",
            mount.container_path()
        ));
    }
    Ok(true)
}

fn reconnect_only_host_directories(
    annotations: &HashMap<String, String>,
    mounts: &[cri_api_v1::Mount],
) -> Result<(Vec<ReconnectOnlyHostDirectory>, bool)> {
    let mounts = mounts
        .iter()
        .filter_map(|mount| match is_regenerated_cri_file(mount) {
            Ok(true) => None,
            Ok(false) => Some(Ok(mount)),
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    let declared = annotations
        .get(RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION)
        .map(String::as_str);
    if declared.is_none() && mounts.is_empty() {
        return Ok((Vec::new(), false));
    }
    if declared.is_none() {
        return Ok((Vec::new(), true));
    }
    let declared = declared.ok_or_else(|| {
        anyhow!(
            "container mounts require {}",
            RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION
        )
    })?;
    if declared.is_empty() {
        return Err(anyhow!(
            "{} must not be empty when container mounts are present",
            RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION
        ));
    }

    let mut mounts_by_destination = HashMap::with_capacity(mounts.len());
    for mount in mounts {
        let destination = canonical_mount_destination(mount.container_path())?;
        if mounts_by_destination
            .insert(destination.clone(), mount)
            .is_some()
        {
            return Err(anyhow!("duplicate CRI mount destination {destination}"));
        }
    }

    let mut destinations = HashSet::new();
    let mut result = Vec::new();
    for destination in declared.split(',') {
        let destination = canonical_mount_destination(destination)?;
        if !destinations.insert(destination.clone()) {
            return Err(anyhow!(
                "duplicate reconnect-only mount destination {destination}"
            ));
        }
        let mount = mounts_by_destination.remove(&destination).ok_or_else(|| {
            anyhow!("reconnect-only mount destination {destination} is absent from CRI mounts")
        })?;
        let source = Path::new(mount.host_path());
        let metadata = fs::symlink_metadata(source).map_err(|err| {
            anyhow!(
                "inspect reconnect-only host directory {}: {err}",
                source.display()
            )
        })?;
        if !source.is_absolute() || metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(anyhow!(
                "reconnect-only source {} must be an absolute non-symlink directory",
                source.display()
            ));
        }
        if mount.has_image() {
            return Err(anyhow!(
                "reconnect-only mount {destination} must not be an image mount"
            ));
        }
        result.push(ReconnectOnlyHostDirectory {
            destination,
            readonly: mount.readonly(),
            propagation: mount.propagation() as i32,
            selinux_relabel: mount.selinux_relabel(),
            recursive_read_only: mount.recursive_read_only(),
            mount_options: Vec::new(),
        });
    }
    if let Some((destination, _)) = mounts_by_destination.into_iter().next() {
        return Err(anyhow!(
            "CRI mount {destination} is not declared reconnect-only"
        ));
    }
    result.sort_by(|left, right| left.destination.cmp(&right.destination));
    for (index, left) in result.iter().enumerate() {
        if result[index + 1..]
            .iter()
            .any(|right| mount_destinations_overlap(&left.destination, &right.destination))
        {
            return Err(anyhow!(
                "reconnect-only mount destinations must not overlap: {}",
                left.destination
            ));
        }
    }
    Ok((result, false))
}

fn checkpoint_reconnect_only_host_directories(
    status: &protobuf::MessageField<protobuf::well_known_types::any::Any>,
) -> Result<(Vec<ReconnectOnlyHostDirectory>, bool)> {
    let Some(status) = status.as_ref() else {
        return Ok((Vec::new(), false));
    };
    if !status.type_url.ends_with(CONTAINER_STATUS_V1) {
        return Ok((Vec::new(), false));
    }
    let status = cri_api_v1::ContainerStatus::parse_from_bytes(&status.value)?;
    reconnect_only_host_directories(&status.annotations, &status.mounts)
}

fn restore_reconnect_only_host_directories(
    config: &protobuf::MessageField<protobuf::well_known_types::any::Any>,
) -> Result<(Vec<ReconnectOnlyHostDirectory>, bool)> {
    let Some(config) = config.as_ref() else {
        return Ok((Vec::new(), false));
    };
    if !config.type_url.ends_with(CONTAINER_CONFIG_V1) {
        return Ok((Vec::new(), false));
    }
    let config = cri_api_v1::ContainerConfig::parse_from_bytes(&config.value)?;
    reconnect_only_host_directories(&config.annotations, &config.mounts)
}

fn trans_from_shim_mount(from: &api::Mount) -> Mount {
    let options = from.options.to_vec();
    let mut read_only = false;
    for o in &options {
        if o == "ro" {
            read_only = true;
            break;
        }
    }

    Mount {
        source: from.source.clone(),
        destination: PathBuf::from(&from.target),
        fs_type: from.type_.clone(),
        options,
        device_id: None,
        host_shared_fs_path: None,
        read_only,
    }
}

fn dns_config_to_resolv_conf(dns_config: cri_api_v1::DNSConfig) -> Vec<String> {
    let mut dns = dns_config
        .servers
        .into_iter()
        .map(|server| format!("nameserver {server}"))
        .collect::<Vec<_>>();

    if !dns_config.searches.is_empty() {
        dns.push(format!("search {}", dns_config.searches.join(" ")));
    }

    if !dns_config.options.is_empty() {
        dns.push(format!("options {}", dns_config.options.join(" ")));
    }

    dns
}

// Build the sandbox from PodSandboxConfig when options are provided. Direct
// Sandbox API callers may omit options and use the request fields instead.
impl TryFrom<sandbox_api::CreateSandboxRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::CreateSandboxRequest) -> Result<Self> {
        let (hostname, dns, annotations) = if from.has_options() {
            let type_url = from.options.type_url.clone();
            if type_url != SANDBOX_API_V1 {
                return Err(anyhow!(format!("unsupported type url: {}", type_url)));
            }

            let config = cri_api_v1::PodSandboxConfig::parse_from_bytes(&from.options.value)?;
            let dns = config
                .dns_config
                .into_option()
                .map(dns_config_to_resolv_conf)
                .unwrap_or_default();
            (config.hostname, dns, config.annotations)
        } else {
            (
                from.sandbox_id.clone(),
                Vec::new(),
                from.annotations.clone(),
            )
        };

        let prepare_restore =
            from.prepare_restore
                .into_option()
                .map(|prepare| PrepareRestoreOptions {
                    checkpoint_path: prepare.checkpoint_path,
                    options: prepare.options,
                });

        let sandbox_config = SandboxConfig {
            sandbox_id: from.sandbox_id.clone(),
            hostname,
            dns,
            network_env: SandboxNetworkEnv {
                netns: Some(from.netns_path),
                network_created: false,
            },
            annotations: annotations.clone(),
            hooks: None,
            state: runtime_spec::State {
                version: Default::default(),
                id: from.sandbox_id,
                status: runtime_spec::ContainerState::Creating,
                pid: 0,
                bundle: from.bundle_path,
                annotations,
            },
            shm_size: DEFAULT_SHM_SIZE,
            config_path: None,
        };

        Ok(SandboxRequest::CreateSandbox(Box::new(
            InternalCreateSandboxRequest {
                sandbox_config,
                prepare_restore,
            },
        )))
    }
}

impl TryFrom<sandbox_api::StartSandboxRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::StartSandboxRequest) -> Result<Self> {
        Ok(SandboxRequest::StartSandbox(SandboxID {
            sandbox_id: from.sandbox_id,
        }))
    }
}

impl TryFrom<sandbox_api::PlatformRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::PlatformRequest) -> Result<Self> {
        Ok(SandboxRequest::Platform(SandboxID {
            sandbox_id: from.sandbox_id,
        }))
    }
}

impl TryFrom<sandbox_api::StopSandboxRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::StopSandboxRequest) -> Result<Self> {
        Ok(SandboxRequest::StopSandbox(StopSandboxRequest {
            sandbox_id: from.sandbox_id,
            timeout_secs: from.timeout_secs,
        }))
    }
}

impl TryFrom<sandbox_api::WaitSandboxRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::WaitSandboxRequest) -> Result<Self> {
        Ok(SandboxRequest::WaitSandbox(SandboxID {
            sandbox_id: from.sandbox_id,
        }))
    }
}

impl TryFrom<sandbox_api::SandboxStatusRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::SandboxStatusRequest) -> Result<Self> {
        Ok(SandboxRequest::SandboxStatus(SandboxStatusRequest {
            sandbox_id: from.sandbox_id,
            verbose: from.verbose,
        }))
    }
}

impl TryFrom<sandbox_api::PingRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::PingRequest) -> Result<Self> {
        Ok(SandboxRequest::Ping(SandboxID {
            sandbox_id: from.sandbox_id,
        }))
    }
}

impl TryFrom<sandbox_api::ShutdownSandboxRequest> for SandboxRequest {
    type Error = anyhow::Error;
    fn try_from(from: sandbox_api::ShutdownSandboxRequest) -> Result<Self> {
        Ok(SandboxRequest::ShutdownSandbox(SandboxID {
            sandbox_id: from.sandbox_id,
        }))
    }
}

impl TryFrom<sandbox_api::CheckpointRequest> for SandboxRequest {
    type Error = anyhow::Error;

    fn try_from(from: sandbox_api::CheckpointRequest) -> Result<Self> {
        Ok(SandboxRequest::CheckpointSandbox(
            CheckpointSandboxRequest {
                sandbox_id: from.sandbox_id,
                output_path: from.output_path,
                tasks: from
                    .containers
                    .into_iter()
                    .map(|c| -> Result<SandboxCheckpointTask> {
                        let base_image = checkpoint_base_image(&c.status, &c.config);
                        let (reconnect_only_host_directories, has_undeclared_mounts) =
                            checkpoint_reconnect_only_host_directories(&c.status)?;
                        Ok(SandboxCheckpointTask {
                            checkpoint_key: c.name.clone(),
                            task_id: c.id,
                            rootfs_device_ids: Vec::new(),
                            rootfs_external_device_ids: Vec::new(),
                            writable_layer: None,
                            base_image,
                            reconnect_only_host_directories,
                            has_undeclared_mounts,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
                options: from.options,
                operation_timeout: None,
            },
        ))
    }
}

impl TryFrom<sandbox_api::RestoreRequest> for SandboxRequest {
    type Error = anyhow::Error;

    fn try_from(from: sandbox_api::RestoreRequest) -> Result<Self> {
        let type_url = from.sandbox_config.type_url.clone();
        if !type_url.is_empty() && type_url != SANDBOX_API_V1 {
            return Err(anyhow!("unsupported sandbox config type url: {type_url}"));
        }
        let uses_request_fields = type_url.is_empty();
        let config = if uses_request_fields {
            cri_api_v1::PodSandboxConfig::new()
        } else {
            cri_api_v1::PodSandboxConfig::parse_from_bytes(&from.sandbox_config.value)?
        };
        let dns = config
            .dns_config
            .into_option()
            .map(dns_config_to_resolv_conf)
            .unwrap_or_default();
        let hostname = if config.hostname.is_empty() {
            from.sandbox_id.clone()
        } else {
            config.hostname.clone()
        };
        let sandbox_id = from.sandbox_id;
        let sandbox_config = SandboxConfig {
            sandbox_id: sandbox_id.clone(),
            hostname,
            dns,
            network_env: SandboxNetworkEnv {
                netns: None,
                network_created: false,
            },
            annotations: config.annotations.clone(),
            hooks: None,
            state: runtime_spec::State {
                version: Default::default(),
                id: sandbox_id,
                status: runtime_spec::ContainerState::Creating,
                pid: 0,
                bundle: String::new(),
                annotations: config.annotations,
            },
            shm_size: DEFAULT_SHM_SIZE,
            config_path: None,
        };
        let tasks = from
            .containers
            .into_iter()
            .map(|c| -> Result<SandboxRestoreTask> {
                let base_image = restore_base_image(&c.config);
                let (reconnect_only_host_directories, has_undeclared_mounts) =
                    restore_reconnect_only_host_directories(&c.config)?;
                Ok(SandboxRestoreTask {
                    checkpoint_key: c.name,
                    task_id: c.id,
                    base_image,
                    reconnect_only_host_directories,
                    has_undeclared_mounts,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(SandboxRequest::RestoreSandbox(Box::new(
            RestoreSandboxRequest {
                sandbox_config,
                checkpoint_path: from.checkpoint_path,
                options: from.options,
                tasks,
                operation_timeout: None,
            },
        )))
    }
}

impl TryFrom<api::CreateTaskRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::CreateTaskRequest) -> Result<Self> {
        let options = if from.has_options() {
            Some(from.options().value.to_vec())
        } else {
            None
        };
        Ok(TaskRequest::CreateContainer(ContainerConfig {
            container_id: from.id.clone(),
            bundle: from.bundle.clone(),
            rootfs_mounts: from.rootfs.iter().map(trans_from_shim_mount).collect(),
            terminal: from.terminal,
            options,
            stdin: (!from.stdin.is_empty()).then(|| from.stdin.clone()),
            stdout: (!from.stdout.is_empty()).then(|| from.stdout.clone()),
            stderr: (!from.stderr.is_empty()).then(|| from.stderr.clone()),
        }))
    }
}

impl TryFrom<api::CloseIORequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::CloseIORequest) -> Result<Self> {
        Ok(TaskRequest::CloseProcessIO(
            ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
        ))
    }
}

impl TryFrom<api::DeleteRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::DeleteRequest) -> Result<Self> {
        Ok(TaskRequest::DeleteProcess(
            ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
        ))
    }
}

impl TryFrom<api::ExecProcessRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::ExecProcessRequest) -> Result<Self> {
        let spec = from.spec();
        Ok(TaskRequest::ExecProcess(ExecProcessRequest {
            process: ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
            terminal: from.terminal,
            stdin: (!from.stdin.is_empty()).then(|| from.stdin.clone()),
            stdout: (!from.stdout.is_empty()).then(|| from.stdout.clone()),
            stderr: (!from.stderr.is_empty()).then(|| from.stderr.clone()),
            spec_type_url: spec.type_url.to_string(),
            spec_value: spec.value.to_vec(),
        }))
    }
}

impl TryFrom<api::KillRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::KillRequest) -> Result<Self> {
        Ok(TaskRequest::KillProcess(KillRequest {
            process: ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
            signal: from.signal,
            all: from.all,
        }))
    }
}

impl TryFrom<api::WaitRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::WaitRequest) -> Result<Self> {
        Ok(TaskRequest::WaitProcess(
            ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
        ))
    }
}

impl TryFrom<api::StartRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::StartRequest) -> Result<Self> {
        Ok(TaskRequest::StartProcess(
            ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
        ))
    }
}

impl TryFrom<api::StateRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::StateRequest) -> Result<Self> {
        Ok(TaskRequest::StateProcess(
            ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
        ))
    }
}

impl TryFrom<api::ShutdownRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::ShutdownRequest) -> Result<Self> {
        Ok(TaskRequest::ShutdownContainer(ShutdownRequest {
            container_id: from.id.to_string(),
            is_now: from.now,
        }))
    }
}

impl TryFrom<api::ResizePtyRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::ResizePtyRequest) -> Result<Self> {
        Ok(TaskRequest::ResizeProcessPTY(ResizePTYRequest {
            process: ContainerProcess::new(&from.id, &from.exec_id).context("new process id")?,
            width: from.width,
            height: from.height,
        }))
    }
}

impl TryFrom<api::PauseRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::PauseRequest) -> Result<Self> {
        Ok(TaskRequest::PauseContainer(ContainerID::new(&from.id)?))
    }
}

impl TryFrom<api::ResumeRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::ResumeRequest) -> Result<Self> {
        Ok(TaskRequest::ResumeContainer(ContainerID::new(&from.id)?))
    }
}

impl TryFrom<api::StatsRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::StatsRequest) -> Result<Self> {
        Ok(TaskRequest::StatsContainer(ContainerID::new(&from.id)?))
    }
}

impl TryFrom<api::UpdateTaskRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::UpdateTaskRequest) -> Result<Self> {
        Ok(TaskRequest::UpdateContainer(UpdateRequest {
            container_id: from.id.to_string(),
            value: from.resources().value.to_vec(),
        }))
    }
}

impl TryFrom<api::PidsRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(_from: api::PidsRequest) -> Result<Self> {
        Ok(TaskRequest::Pid)
    }
}

impl TryFrom<api::ConnectRequest> for TaskRequest {
    type Error = anyhow::Error;
    fn try_from(from: api::ConnectRequest) -> Result<Self> {
        Ok(TaskRequest::ConnectContainer(ContainerID::new(&from.id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert_create_sandbox_dns(dns_config: Option<cri_api_v1::DNSConfig>) -> Vec<String> {
        let mut config = cri_api_v1::PodSandboxConfig::new();
        config.dns_config = dns_config.into();

        let mut options = protobuf::well_known_types::any::Any::new();
        options.type_url = SANDBOX_API_V1.into();
        options.value = config.write_to_bytes().unwrap();

        let mut request = sandbox_api::CreateSandboxRequest::new();
        request.options = protobuf::MessageField::some(options);

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::CreateSandbox(config) = request else {
            panic!("expected create sandbox request");
        };

        config.sandbox_config.dns
    }

    #[test]
    fn create_sandbox_formats_dns_config_as_resolv_conf() {
        let mut dns_config = cri_api_v1::DNSConfig::new();
        dns_config.servers = vec!["192.0.2.1".into(), "2001:db8::53".into()];
        dns_config.searches = vec!["example.test".into(), "svc.example.test".into()];
        dns_config.options = vec!["ndots:2".into(), "single-request-reopen".into()];

        assert_eq!(
            convert_create_sandbox_dns(Some(dns_config)),
            [
                "nameserver 192.0.2.1",
                "nameserver 2001:db8::53",
                "search example.test svc.example.test",
                "options ndots:2 single-request-reopen",
            ]
        );
    }

    #[test]
    fn create_sandbox_without_dns_config_has_empty_dns() {
        assert!(convert_create_sandbox_dns(None).is_empty());
    }

    #[test]
    fn create_sandbox_without_options_uses_sandbox_id_as_hostname() {
        let mut request = sandbox_api::CreateSandboxRequest::new();
        request.sandbox_id = "sandbox-id".into();
        request.bundle_path = "/run/kata/sandbox-id".into();
        request.netns_path = "/run/netns/sandbox-id".into();
        request.annotations.insert(
            "io.katacontainers.config.hypervisor.default_vcpus".into(),
            "2".into(),
        );

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::CreateSandbox(config) = request else {
            panic!("expected create sandbox request");
        };

        let config = config.sandbox_config;
        assert_eq!(config.sandbox_id, "sandbox-id");
        assert_eq!(config.hostname, "sandbox-id");
        assert!(config.dns.is_empty());
        assert_eq!(
            config.network_env.netns.as_deref(),
            Some("/run/netns/sandbox-id")
        );
        assert_eq!(config.state.bundle, "/run/kata/sandbox-id");
        assert_eq!(
            config
                .annotations
                .get("io.katacontainers.config.hypervisor.default_vcpus")
                .map(String::as_str),
            Some("2")
        );
        assert_eq!(config.annotations, config.state.annotations);
    }

    #[test]
    fn create_sandbox_rejects_unknown_options_type() {
        let mut options = protobuf::well_known_types::any::Any::new();
        options.type_url = "example.test.UnknownSandboxConfig".into();

        let mut request = sandbox_api::CreateSandboxRequest::new();
        request.options = protobuf::MessageField::some(options);

        let error = SandboxRequest::try_from(request).unwrap_err();
        assert!(error
            .to_string()
            .contains("unsupported type url: example.test.UnknownSandboxConfig"));
    }

    #[test]
    fn create_sandbox_with_cri_options_preserves_cri_config_precedence() {
        let mut config = cri_api_v1::PodSandboxConfig::new();
        config.hostname = "cri-hostname".into();
        config
            .annotations
            .insert("annotation-source".into(), "cri".into());
        config
            .annotations
            .insert("example.test.annotation".into(), "value".into());

        let mut options = protobuf::well_known_types::any::Any::new();
        options.type_url = SANDBOX_API_V1.into();
        options.value = config.write_to_bytes().unwrap();

        let mut request = sandbox_api::CreateSandboxRequest::new();
        request.sandbox_id = "sandbox-id".into();
        request
            .annotations
            .insert("annotation-source".into(), "request".into());
        request.options = protobuf::MessageField::some(options);

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::CreateSandbox(config) = request else {
            panic!("expected create sandbox request");
        };

        assert_eq!(config.sandbox_config.hostname, "cri-hostname");
        assert_eq!(
            config
                .sandbox_config
                .annotations
                .get("example.test.annotation")
                .map(String::as_str),
            Some("value")
        );
        assert_eq!(
            config
                .sandbox_config
                .annotations
                .get("annotation-source")
                .map(String::as_str),
            Some("cri")
        );
    }

    #[test]
    fn canonical_digest_accepts_id_and_digest_pinned_reference() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            canonical_sha256_digest(&format!("sha256:{digest}")),
            Some(format!("sha256:{digest}"))
        );
        assert_eq!(
            canonical_sha256_digest(&format!("registry.test/image@sha256:{digest}")),
            Some(format!("sha256:{digest}"))
        );
        assert_eq!(canonical_sha256_digest("registry.test/image:latest"), None);
    }

    #[test]
    fn existing_cri_any_fields_supply_base_image_identity() {
        let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        let mut status = cri_api_v1::ContainerStatus::new();
        status.image_ref = digest.to_string();
        let mut status_any = protobuf::well_known_types::any::Any::new();
        status_any.type_url = CONTAINER_STATUS_V1.to_string();
        status_any.value = status.write_to_bytes().unwrap();
        assert_eq!(
            checkpoint_base_image(
                &protobuf::MessageField::some(status_any),
                &Default::default()
            ),
            Some(BaseImageIdentity::OciImage {
                digest: digest.to_string()
            })
        );

        let mut config = cri_api_v1::ContainerConfig::new();
        let mut image = cri_api_v1::ImageSpec::new();
        image.image = format!("registry.test/image@{digest}");
        config.image = protobuf::MessageField::some(image);
        let mut config_any = protobuf::well_known_types::any::Any::new();
        config_any.type_url = CONTAINER_CONFIG_V1.to_string();
        config_any.value = config.write_to_bytes().unwrap();
        assert_eq!(
            restore_base_image(&protobuf::MessageField::some(config_any)),
            Some(BaseImageIdentity::OciImage {
                digest: digest.to_string()
            })
        );
    }

    #[test]
    fn create_sandbox_preserves_typed_prepare_restore() {
        let mut request = sandbox_api::CreateSandboxRequest::new();
        request.prepare_restore =
            protobuf::MessageField::some(sandbox_api::SandboxPrepareRestoreOptions {
                checkpoint_path: "/checkpoints/pod-a".into(),
                options: vec![("runtime-asset-identity".into(), "asset-a".into())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            });

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::CreateSandbox(request) = request else {
            panic!("expected create sandbox request");
        };
        let prepare = request.prepare_restore.expect("prepare restore");
        assert_eq!(prepare.checkpoint_path, "/checkpoints/pod-a");
        assert_eq!(
            prepare.options.get("runtime-asset-identity"),
            Some(&"asset-a".to_string())
        );
    }

    #[test]
    fn checkpoint_prefers_digest_pinned_image_reference() {
        let manifest_digest =
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let config_digest =
            "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let mut status = cri_api_v1::ContainerStatus::new();
        let mut image = cri_api_v1::ImageSpec::new();
        image.image = format!("registry.test/image@{manifest_digest}");
        status.image = protobuf::MessageField::some(image);
        status.image_ref = config_digest.to_string();
        let mut status_any = protobuf::well_known_types::any::Any::new();
        status_any.type_url = CONTAINER_STATUS_V1.to_string();
        status_any.value = status.write_to_bytes().unwrap();

        assert_eq!(
            checkpoint_base_image(
                &protobuf::MessageField::some(status_any),
                &Default::default()
            ),
            Some(BaseImageIdentity::OciImage {
                digest: manifest_digest.to_string()
            })
        );
    }

    #[test]
    fn checkpoint_uses_resolved_cri_image_across_index_and_config_digests() {
        let index_digest =
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let config_digest =
            "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let mut status = cri_api_v1::ContainerStatus::new();
        status.image_ref = format!("registry.test/image@{index_digest}");
        let mut status_any = protobuf::well_known_types::any::Any::new();
        status_any.type_url = CONTAINER_STATUS_V1.to_string();
        status_any.value = status.write_to_bytes().unwrap();

        let mut config = cri_api_v1::ContainerConfig::new();
        let mut image = cri_api_v1::ImageSpec::new();
        image.image = config_digest.to_string();
        config.image = protobuf::MessageField::some(image);
        let mut config_any = protobuf::well_known_types::any::Any::new();
        config_any.type_url = CONTAINER_CONFIG_V1.to_string();
        config_any.value = config.write_to_bytes().unwrap();

        assert_eq!(
            checkpoint_base_image(
                &protobuf::MessageField::some(status_any),
                &protobuf::MessageField::some(config_any)
            ),
            Some(BaseImageIdentity::OciImage {
                digest: index_digest.to_string()
            })
        );
    }

    #[test]
    fn restore_sandbox_without_config_uses_sandbox_id_as_hostname() {
        let mut request = sandbox_api::RestoreRequest::new();
        request.sandbox_id = "sandbox-id".into();

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::RestoreSandbox(request) = request else {
            panic!("expected restore sandbox request");
        };

        assert_eq!(request.sandbox_config.sandbox_id, "sandbox-id");
        assert_eq!(request.sandbox_config.hostname, "sandbox-id");
        assert!(request.sandbox_config.network_env.netns.is_none());
    }

    #[test]
    fn restore_sandbox_with_config_uses_config_hostname() {
        let mut config = cri_api_v1::PodSandboxConfig::new();
        config.hostname = "cri-hostname".into();

        let mut request = sandbox_api::RestoreRequest::new();
        request.sandbox_id = "sandbox-id".into();
        let mut sandbox_config = protobuf::well_known_types::any::Any::new();
        sandbox_config.type_url = SANDBOX_API_V1.into();
        sandbox_config.value = config.write_to_bytes().unwrap();
        request.sandbox_config = protobuf::MessageField::some(sandbox_config);

        let request = SandboxRequest::try_from(request).unwrap();
        let SandboxRequest::RestoreSandbox(request) = request else {
            panic!("expected restore sandbox request");
        };

        assert_eq!(request.sandbox_config.hostname, "cri-hostname");
    }

    #[test]
    fn reconnect_only_host_directories_require_an_exact_mount_declaration() {
        let mut mount = cri_api_v1::Mount::new();
        mount.container_path = "/data".to_string();
        mount.host_path = "/tmp".to_string();
        mount.readonly = true;

        let annotations = HashMap::from([(
            RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION.to_string(),
            "/data".to_string(),
        )]);
        assert_eq!(
            reconnect_only_host_directories(&annotations, &[mount.clone()]).unwrap(),
            (
                vec![ReconnectOnlyHostDirectory {
                    destination: "/data".to_string(),
                    readonly: true,
                    propagation: mount.propagation() as i32,
                    selinux_relabel: false,
                    recursive_read_only: false,
                    mount_options: Vec::new(),
                }],
                false,
            )
        );

        assert_eq!(
            reconnect_only_host_directories(&HashMap::new(), &[mount]).unwrap(),
            (Vec::new(), true)
        );
    }

    #[test]
    fn regenerated_cri_files_do_not_become_volume_snapshot_requirements() {
        let root = tempfile::tempdir().unwrap();
        let mut mounts = Vec::new();
        for destination in ["/etc/hosts", "/dev/termination-log"] {
            let source = match destination {
                "/etc/hosts" => root.path().join("pods/pod-uid/etc-hosts"),
                _ => root.path().join("pods/pod-uid/containers/probe/log-id"),
            };
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(&source, b"generated by kubelet").unwrap();
            let mut mount = cri_api_v1::Mount::new();
            mount.container_path = destination.to_string();
            mount.host_path = source.to_string_lossy().into_owned();
            mounts.push(mount);
        }
        assert_eq!(
            reconnect_only_host_directories(&HashMap::new(), &mounts).unwrap(),
            (Vec::new(), false)
        );
        mounts[0].host_path = mounts[1].host_path.clone();
        assert_eq!(
            reconnect_only_host_directories(&HashMap::new(), &mounts).unwrap(),
            (Vec::new(), true)
        );
        mounts[0].host_path = root
            .path()
            .join("pods/pod-uid/etc-hosts")
            .to_string_lossy()
            .into_owned();
        mounts[0].container_path = "/data".to_string();
        assert_eq!(
            reconnect_only_host_directories(&HashMap::new(), &mounts).unwrap(),
            (Vec::new(), true)
        );
    }

    #[test]
    fn reconnect_only_host_directories_reject_reserved_or_overlapping_targets() {
        let mut data = cri_api_v1::Mount::new();
        data.container_path = "/data".to_string();
        data.host_path = "/tmp".to_string();
        let mut nested = cri_api_v1::Mount::new();
        nested.container_path = "/data/cache".to_string();
        nested.host_path = "/tmp".to_string();

        let overlapping = HashMap::from([(
            RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION.to_string(),
            "/data,/data/cache".to_string(),
        )]);
        assert!(reconnect_only_host_directories(&overlapping, &[data.clone(), nested]).is_err());

        data.container_path = "/proc/data".to_string();
        let reserved = HashMap::from([(
            RECONNECT_ONLY_HOST_DIRECTORY_ANNOTATION.to_string(),
            "/proc/data".to_string(),
        )]);
        assert!(reconnect_only_host_directories(&reserved, &[data]).is_err());
    }
}
