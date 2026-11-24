// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0

use super::*;
use resource::rootfs::{
    writable_snapshot::{apply_prepared_restore, resolve_restore_target, PreparedWritableRestore},
    Rootfs,
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use common::{
    types::{RestoredSandboxTask, RestoredTaskMode, SandboxCheckpointTask, SandboxRestoreTask},
    ContainerCheckpointRestore,
};
use resource::rootfs::WritableLayer;

pub(super) enum PendingRestoredTask {
    FullVm {
        checkpoint_key: String,
        rootfs: Arc<dyn Rootfs>,
        binding: bool,
    },
    ColdRootfs {
        state: ColdRestoreState,
    },
}

pub(super) enum ColdRestoreState {
    Ready {
        prepared: PreparedWritableRestore,
        reconnect_only_host_directories: Vec<ReconnectOnlyHostDirectory>,
    },
    Applying,
    Failed(String),
}

impl ColdRestoreState {
    fn claim(&mut self, container_id: &str) -> Result<()> {
        match self {
            Self::Ready { .. } => {
                *self = Self::Applying;
                Ok(())
            }
            Self::Applying => Err(anyhow!(
                "writable-rootfs restore task {container_id} is already applying"
            )),
            Self::Failed(reason) => Err(anyhow!(
                "writable-rootfs restore task {container_id} failed: {reason}"
            )),
        }
    }

    pub(super) fn fail(&mut self, reason: String) {
        if matches!(self, Self::Applying) {
            *self = Self::Failed(reason);
        }
    }
}

fn normalized_mount_options(options: Option<&Vec<String>>) -> Result<Vec<String>> {
    let mut normalized = options.cloned().unwrap_or_default();
    normalized.sort();
    if normalized.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(anyhow!("OCI mount has duplicate mount options"));
    }
    Ok(normalized)
}

fn mount_options_for_destination(spec: &oci::Spec, destination: &str) -> Result<Vec<String>> {
    let destination = Path::new(destination);
    let mounts = spec
        .mounts()
        .as_ref()
        .map(|mounts| {
            mounts
                .iter()
                .filter(|mount| mount.destination() == destination)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if mounts.len() != 1 {
        return Err(anyhow!(
            "expected exactly one OCI mount for reconnect-only destination {}, found {}",
            destination.display(),
            mounts.len()
        ));
    }
    normalized_mount_options(mounts[0].options().as_ref())
}

pub(super) fn record_reconnect_only_host_directory_mount_options(
    directories: &mut [ReconnectOnlyHostDirectory],
    spec: &oci::Spec,
) -> Result<()> {
    for directory in directories {
        directory.mount_options = mount_options_for_destination(spec, &directory.destination)?;
    }
    Ok(())
}

pub(super) fn validate_reconnect_only_host_directory_mount_options(
    directories: &[ReconnectOnlyHostDirectory],
    spec: &oci::Spec,
) -> Result<()> {
    for directory in directories {
        let observed = mount_options_for_destination(spec, &directory.destination)?;
        if directory.mount_options != observed {
            return Err(anyhow!(
                "OCI mount options for reconnect-only destination {} do not match checkpoint",
                directory.destination
            ));
        }
    }
    Ok(())
}

impl VirtContainerManager {
    pub(super) async fn apply_cold_restore_before_create(
        &self,
        config: &mut ContainerConfig,
        spec: &oci::Spec,
        prepared: &PreparedWritableRestore,
        directories: &[ReconnectOnlyHostDirectory],
    ) -> Result<()> {
        validate_reconnect_only_host_directory_mount_options(directories, spec)?;
        let target = resolve_restore_target(&config.rootfs_mounts, prepared)?;
        let container_id = &config.container_id;
        if self.containers.read().await.contains_key(container_id) {
            return Err(anyhow!("container {container_id} already exists"));
        }

        {
            let mut pending = self.pending_restored_tasks.write().await;
            let task = pending
                .get_mut(container_id)
                .ok_or_else(|| anyhow!("writable-rootfs restore task disappeared before claim"))?;
            match task {
                PendingRestoredTask::ColdRootfs { state } => state.claim(container_id)?,
                PendingRestoredTask::FullVm { .. } => {
                    return Err(anyhow!(
                        "writable-rootfs restore task {container_id} changed mode"
                    ))
                }
            }
        }

        apply_prepared_restore(&mut config.rootfs_mounts, prepared, &target)
    }

    pub(super) async fn bind_restored_container(
        &self,
        config: ContainerConfig,
        spec: oci::Spec,
    ) -> Result<PID> {
        let container_id = config.container_id.clone();
        if self.containers.read().await.contains_key(&container_id) {
            return Err(anyhow!("container {container_id} already exists"));
        }

        let (restored_rootfs, checkpoint_key) = {
            let mut pending = self.pending_restored_tasks.write().await;
            let slot = pending.get_mut(&container_id).ok_or_else(|| {
                anyhow!("VM-restored task {container_id} has no pending restore record")
            })?;
            match slot {
                PendingRestoredTask::FullVm {
                    checkpoint_key,
                    rootfs,
                    binding,
                } => {
                    if *binding {
                        return Err(anyhow!(
                            "VM-restored task {container_id} is already being bound"
                        ));
                    }
                    *binding = true;
                    (Arc::clone(rootfs), checkpoint_key.clone())
                }
                PendingRestoredTask::ColdRootfs { .. } => {
                    return Err(anyhow!("task {container_id} is a cold restore"));
                }
            }
        };

        let bind_result = async {
            let vmm_master_tid = self.get_vmm_master_tid().await?;
            let resources = spec
                .linux()
                .as_ref()
                .and_then(|linux| linux.resources().clone());

            let container = Container::new_restored(
                vmm_master_tid,
                config,
                spec,
                self.agent.clone(),
                self.resource_manager.clone(),
                restored_rootfs,
                self.use_guest_resolv_conf,
            )
            .await
            .context(format!(
                "construct VM-restored container from pending checkpoint {checkpoint_key}"
            ))?;

            // Publish the formal container and consume the pending slot while
            // both maps are locked. Readers can observe either Pending or
            // Bound, never an unowned gap between them.
            let mut containers = self.containers.write().await;
            let mut pending = self.pending_restored_tasks.write().await;
            if containers.contains_key(&container_id) {
                return Err(anyhow!("container {container_id} already exists"));
            }
            if !matches!(
                pending.get(&container_id),
                Some(PendingRestoredTask::FullVm { binding: true, .. })
            ) {
                return Err(anyhow!(
                    "pending VM-restored task {container_id} left binding state"
                ));
            }
            // Finish fallible construction/validation before adopting resources.
            // After adoption, publication below has no await or fallible operation.
            self.resource_manager
                .adopt_restored_container(&container_id, resources.as_ref())
                .await
                .context("adopt restored task resource accounting")?;

            containers.insert(container_id.clone(), container);
            pending.remove(&container_id);

            Ok(PID {
                pid: vmm_master_tid,
            })
        }
        .await;

        if bind_result.is_err() {
            if let Some(PendingRestoredTask::FullVm { binding, .. }) = self
                .pending_restored_tasks
                .write()
                .await
                .get_mut(&container_id)
            {
                *binding = false;
            }
        }

        bind_result
    }
}

#[async_trait]
impl ContainerCheckpointRestore for VirtContainerManager {
    async fn ensure_restore_tasks_consumed(&self) -> Result<()> {
        let pending = self.pending_restored_tasks.read().await;
        if pending.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "restore has {} unconsumed task record(s)",
                pending.len()
            ))
        }
    }

    async fn prepare_checkpoint_tasks(&self, tasks: &mut [SandboxCheckpointTask]) -> Result<()> {
        if tasks.is_empty() {
            return Err(anyhow!("checkpoint task inventory is empty"));
        }

        let containers = self.containers.read().await;
        if containers.len() != tasks.len() {
            return Err(anyhow!(
                "checkpoint inventory has {} tasks, sandbox has {}",
                tasks.len(),
                containers.len()
            ));
        }

        let (mut seen, mut seen_keys) = (HashSet::new(), HashSet::new());
        for task in tasks.iter_mut() {
            if task.checkpoint_key.is_empty()
                || !seen_keys.insert(task.checkpoint_key.as_str())
                || !seen.insert(task.task_id.as_str())
            {
                return Err(anyhow!("invalid or duplicate checkpoint task inventory"));
            }

            let container = containers
                .get(&task.task_id)
                .ok_or_else(|| anyhow!("checkpoint task {} is not in sandbox", task.task_id))?;

            let state = container
                .state_process(&ContainerProcess::new(&task.task_id, "")?)
                .await
                .context(format!("inspect checkpoint task {}", task.task_id))?;

            if state.status != ProcessStatus::Running {
                return Err(anyhow!(
                    "checkpoint task {} is {:?}, expected running",
                    task.task_id,
                    state.status
                ));
            }

            let (rootfs_device_ids, rootfs_external_device_ids, writable_layer) = container
                .checkpoint_rootfs_info()
                .await
                .context(format!("inspect checkpoint task {} rootfs", task.task_id))?;
            task.rootfs_device_ids = rootfs_device_ids;
            task.rootfs_external_device_ids = rootfs_external_device_ids;
            task.writable_layer = Some(writable_layer);
            record_reconnect_only_host_directory_mount_options(
                &mut task.reconnect_only_host_directories,
                &container.spec().await,
            )?;
        }

        Ok(())
    }

    async fn pause_checkpoint_tasks(&self, tasks: &[SandboxCheckpointTask]) -> Result<()> {
        let containers = self.containers.read().await;
        let mut paused = Vec::new();
        for task in tasks {
            let container = containers
                .get(&task.task_id)
                .ok_or_else(|| anyhow!("checkpoint task {} is not in sandbox", task.task_id))?;

            if let Err(err) = container.pause().await {
                let mut rollback_failures = Vec::new();
                for paused_id in paused.iter().rev() {
                    if let Some(paused_container) = containers.get(paused_id) {
                        if let Err(resume_err) = paused_container.resume().await {
                            rollback_failures.push(format!("{paused_id}: {resume_err:#}"));
                        }
                    }
                }

                let pause_error: Result<()> =
                    Err(err).context(format!("pause checkpoint task {}", task.task_id));
                return if rollback_failures.is_empty() {
                    pause_error
                } else {
                    pause_error.context(format!(
                        "failed to resume partially frozen tasks: {}",
                        rollback_failures.join(", ")
                    ))
                };
            }

            paused.push(task.task_id.clone());
        }

        Ok(())
    }

    async fn sync_checkpoint_filesystems(&self, tasks: &[SandboxCheckpointTask]) -> Result<()> {
        // A host OverlayFS upper is synchronized by the host immediately before
        // it is archived.  The guest RPC remains required for guest-managed
        // writable filesystems such as external block rootfs devices.
        let container_ids = tasks
            .iter()
            .filter(|task| {
                !matches!(
                    task.writable_layer.as_ref(),
                    Some(WritableLayer::HostDirectory { .. })
                )
            })
            .map(|task| task.task_id.clone())
            .collect::<Vec<_>>();
        if container_ids.is_empty() {
            return Ok(());
        }
        self.agent
            .sync_container_filesystems(agent::SyncContainerFilesystemsRequest { container_ids })
            .await
            .context("sync checkpoint task filesystems")?;
        Ok(())
    }

    async fn resume_checkpoint_tasks(&self, tasks: &[SandboxCheckpointTask]) -> Result<()> {
        let containers = self.containers.read().await;
        let mut first_error = None;

        for task in tasks {
            if let Some(container) = containers.get(&task.task_id) {
                if let Err(err) = container.resume().await {
                    first_error.get_or_insert(err);
                }
            }
        }

        first_error.map_or(Ok(()), Err)
    }

    async fn restore_tasks(
        &self,
        tasks: &[SandboxRestoreTask],
        restored: &[RestoredSandboxTask],
    ) -> Result<()> {
        if tasks.is_empty() || tasks.len() != restored.len() {
            return Err(anyhow!(
                "restore task inventory does not match restored sandbox"
            ));
        }

        let restored_by_key = restored
            .iter()
            .map(|task| (task.checkpoint_key.as_str(), task))
            .collect::<HashMap<_, _>>();
        if restored_by_key.len() != restored.len() {
            return Err(anyhow!("duplicate restored task checkpoint key"));
        }

        if !self.containers.read().await.is_empty() {
            return Err(anyhow!("restore requires an empty container manager"));
        }

        if !self.pending_restored_tasks.read().await.is_empty() {
            return Err(anyhow!("restore already has pending task records"));
        }

        let mut task_ids = HashSet::new();
        let mut rootfs_ids = HashSet::new();
        let mut pending_tasks = Vec::with_capacity(tasks.len());
        for task in tasks {
            let restored_task = restored_by_key
                .get(task.checkpoint_key.as_str())
                .ok_or_else(|| {
                    anyhow!(
                        "restore task {} is absent from restored sandbox",
                        task.checkpoint_key
                    )
                })?;

            if restored_task.task_id != task.task_id {
                return Err(anyhow!(
                    "restored task {} has invalid identity or rootfs device",
                    task.task_id
                ));
            }

            if !task_ids.insert(task.task_id.as_str()) {
                return Err(anyhow!("duplicate restored task ID {}", task.task_id));
            }

            let pending_task = match &restored_task.mode {
                RestoredTaskMode::FullVm { rootfs_device_ids } => {
                    if rootfs_device_ids.is_empty()
                        || rootfs_device_ids.iter().any(String::is_empty)
                    {
                        return Err(anyhow!(
                            "restored task {} has invalid rootfs device",
                            task.task_id
                        ));
                    }
                    for device_id in rootfs_device_ids {
                        if !rootfs_ids.insert(device_id.as_str()) {
                            return Err(anyhow!(
                                "restored rootfs device {} is assigned to multiple tasks",
                                device_id
                            ));
                        }
                    }
                    let rootfs = self
                        .resource_manager
                        .adopt_restored_rootfs(rootfs_device_ids)
                        .await
                        .context(format!("adopt restored rootfs for task {}", task.task_id))?;
                    PendingRestoredTask::FullVm {
                        checkpoint_key: task.checkpoint_key.clone(),
                        rootfs,
                        binding: false,
                    }
                }
                RestoredTaskMode::ColdWritableRootfs {
                    prepared,
                    reconnect_only_host_directories,
                } => PendingRestoredTask::ColdRootfs {
                    state: ColdRestoreState::Ready {
                        prepared: prepared.clone(),
                        reconnect_only_host_directories: reconnect_only_host_directories.clone(),
                    },
                },
            };

            pending_tasks.push((task.task_id.clone(), pending_task));
        }

        let containers = self.containers.read().await;
        let mut pending = self.pending_restored_tasks.write().await;
        if !containers.is_empty() {
            return Err(anyhow!("restore requires an empty container manager"));
        }

        if !pending.is_empty() {
            return Err(anyhow!("restore already has pending task records"));
        }

        for (task_id, task) in pending_tasks {
            pending.insert(task_id, task);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resource::rootfs::writable_snapshot::{VerifiedArtifact, VerifiedArtifactIdentity};

    fn ready_cold_state() -> ColdRestoreState {
        ColdRestoreState::Ready {
            prepared: PreparedWritableRestore::OverlayUpper {
                artifact: VerifiedArtifact {
                    path: "/unused-test-artifact".into(),
                    file: Arc::new(tempfile::tempfile().unwrap()),
                    size: 0,
                    identity: VerifiedArtifactIdentity {
                        device: 0,
                        inode: 0,
                        ctime_seconds: 0,
                        ctime_nanoseconds: 0,
                    },
                    deadline: None,
                },
            },
            reconnect_only_host_directories: Vec::new(),
        }
    }

    #[tokio::test]
    async fn cold_restore_claim_is_single_use_and_failure_is_terminal() {
        let state = Arc::new(tokio::sync::Mutex::new(ready_cold_state()));
        let first = async { state.lock().await.claim("task").is_ok() };
        let second = async { state.lock().await.claim("task").is_ok() };
        let (first, second) = tokio::join!(first, second);
        assert_ne!(first, second);

        let mut state = state.lock().await;
        state.fail("copy failed".to_string());
        assert!(matches!(&*state, ColdRestoreState::Failed(reason) if reason == "copy failed"));
        assert!(state.claim("task").is_err());
    }
}
