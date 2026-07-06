# How to use Sandbox API within Runtime-rs

This document explains how to run Kata Runtime-rs with containerd Sandbox API
and how to remove the operational dependency on a Kubernetes pause image.

The target use case is Kata `runtime-rs` with containerd 2.x. With this setup,
containerd delegates PodSandbox lifecycle to the Kata shim through the
containerd Sandbox API. Kata then creates and starts the VM sandbox directly,
instead of relying on the legacy pause-container path.

## Overview

In the traditional containerd CRI path, a Kubernetes PodSandbox is implemented
with a pause container. The pause container is used to hold shared pod
resources such as network namespaces.

For Kata Containers, the natural sandbox boundary is the VM. The containerd
Sandbox API lets Kata use the VM as the sandbox environment:

1. kubelet calls CRI `RunPodSandbox`.
2. containerd creates sandbox metadata and configures networking.
3. containerd calls the selected sandbox controller.
4. With `sandboxer = "shim"`, containerd starts the Kata shim and calls the
   shim-side `CreateSandbox` and `StartSandbox` APIs.
5. Kata starts the VM sandbox and asks the guest agent to create the guest-side
   sandbox.
6. Application containers are created later through the same shim and run
   inside the VM sandbox.

This removes the hard assumption that a PodSandbox must be a pause container.
For Confidential Containers, this also avoids treating a built-in pause image
as a fundamental guest rootfs dependency.

## Requirements

| Component | Requirement |
|-----------|-------------|
| containerd | 2.0 or newer. containerd 2.3 or newer is recommended for current Sandbox API testing. |
| Kata Containers | A build that includes `runtime-rs` Sandbox API support. |
| Kubernetes | A version compatible with the containerd version in use. |
| CNI | Installed and configured for non-host-network pods. |

> **Note:**
> containerd 1.7 introduced the Sandbox API as experimental. For this guide,
> use containerd 2.x, where the sandbox service is stable and the CRI plugin
> uses sandbox controllers by default.

## Quick Start Guide

This section shows the minimum configuration needed to try the Kata
Sandbox API path with Kubernetes.

The example assumes:

- the Kata runtime handler is `kata-qemu-runtime-rs`;
- the shim runtime type is `io.containerd.kata-qemu-runtime-rs.v2`;
- the Kata configuration file is
  `/opt/kata/share/defaults/kata-containers/configuration-qemu-runtime-rs.toml`;
- containerd uses configuration schema v3.

Adjust these values if your installation uses a different handler, hypervisor,
or Kata configuration path.

### Step 1: Configure the Kata runtime handler

```bash
$ sudo mkdir -p /etc/containerd/conf.d
$ sudo tee /etc/containerd/conf.d/50-kata-sandbox-api.toml >/dev/null <<'EOF'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kata-qemu-runtime-rs]
  runtime_type = 'io.containerd.kata-qemu-runtime-rs.v2'
  sandboxer = 'shim'
  disable_pause_image_pull = true
  pod_annotations = ['io.katacontainers.*']
  container_annotations = ['io.katacontainers.*']

  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kata-qemu-runtime-rs.options]
    ConfigPath = '/opt/kata/share/defaults/kata-containers/configuration-qemu-runtime-rs.toml'
EOF
```

Restart containerd:

```bash
$ sudo systemctl restart containerd
```

Check the effective configuration:

```bash
$ sudo containerd config dump | grep -A12 'kata-qemu-runtime-rs'
```

The runtime entry should include:

```toml
sandboxer = "shim"
disable_pause_image_pull = true
```

### Step 2: Create a RuntimeClass

```bash
$ cat <<'EOF' > runtimeclass-kata-sandbox-api.yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kata-sandbox-api
handler: kata-qemu-runtime-rs
EOF

$ kubectl apply -f runtimeclass-kata-sandbox-api.yaml
```

### Step 3: Run a test pod

```bash
$ cat <<'EOF' > kata-sandbox-api-test.yaml
apiVersion: v1
kind: Pod
metadata:
  name: kata-sandbox-api-test
spec:
  runtimeClassName: kata-sandbox-api
  restartPolicy: Never
  containers:
  - name: busybox
    image: docker.io/library/busybox:latest
    command: ["sh", "-c", "echo sandbox-api-ok && sleep 3600"]
EOF

$ kubectl apply -f kata-sandbox-api-test.yaml
```

Check the result:

```bash
$ kubectl get pod kata-sandbox-api-test -o wide
$ kubectl logs kata-sandbox-api-test
```

Expected log output:

```text
sandbox-api-ok
```

### Step 4: Verify the pause image pull is skipped

If containerd debug logging is enabled, the containerd logs should include a
message similar to:

```text
Skipping pause image pull for runtime handler ... disable_pause_image_pull=true
```

You can check recent logs with:

```bash
$ sudo journalctl -u containerd --since "10 minutes ago" | grep -i pause
```

The node may already contain a pause image from previous workloads. That is
not a failure. The important point is that this runtime handler does not need
containerd to pull a host pause image for new Kata Runtime-rs pods.

## Detailed containerd configuration

The key settings are:

- `sandboxer = "shim"`
- `disable_pause_image_pull = true`

The `shim` sandboxer tells containerd to use the Runtime v2 shim Sandbox API
path. `disable_pause_image_pull` tells the CRI plugin not to pre-pull the
pause image during `RunPodSandbox`.

### containerd 2.x

Create a containerd drop-in, for example:

```bash
$ sudo mkdir -p /etc/containerd/conf.d
$ sudo tee /etc/containerd/conf.d/50-kata-sandbox-api.toml >/dev/null <<'EOF'
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kata-qemu-runtime-rs]
  runtime_type = 'io.containerd.kata-qemu-runtime-rs.v2'
  sandboxer = 'shim'
  disable_pause_image_pull = true
  pod_annotations = ['io.katacontainers.*']
  container_annotations = ['io.katacontainers.*']

  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.kata-qemu-runtime-rs.options]
    ConfigPath = '/opt/kata/share/defaults/kata-containers/configuration-qemu-runtime-rs.toml'
EOF
```

If your installation uses a different runtime handler or hypervisor, adjust
the runtime table and `runtime_type`. Common examples are:

```toml
runtime_type = 'io.containerd.kata.v2'
runtime_type = 'io.containerd.kata-qemu-runtime-rs.v2'
runtime_type = 'io.containerd.kata-clh-runtime-rs.v2'
```

Restart containerd:

```bash
$ sudo systemctl restart containerd
```

Verify that containerd loaded the runtime configuration:

```bash
$ sudo containerd config dump | grep -A12 'kata-qemu-runtime-rs'
```

The output should include:

```toml
sandboxer = "shim"
disable_pause_image_pull = true
```

### Compatibility fallback

To fall back to the legacy pause-container path, use:

```toml
sandboxer = 'podsandbox'
disable_pause_image_pull = false
```

Use this fallback if the runtime build does not implement the shim-side
Sandbox API, or if you need to compare behavior with the legacy containerd CRI
pause-container implementation.

## Detailed Kubernetes RuntimeClass configuration

Create a RuntimeClass that points to the containerd runtime handler configured
above:

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kata-sandbox-api
handler: kata-qemu-runtime-rs
```

Apply it:

```bash
$ kubectl apply -f runtimeclass-kata-sandbox-api.yaml
```

## Detailed test pod example

Create a test pod:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: kata-sandbox-api-test
spec:
  runtimeClassName: kata-sandbox-api
  restartPolicy: Never
  containers:
  - name: busybox
    image: docker.io/library/busybox:latest
    command: ["sh", "-c", "echo sandbox-api-ok && sleep 3600"]
```

Apply it:

```bash
$ kubectl apply -f kata-sandbox-api-test.yaml
```

Check the pod:

```bash
$ kubectl get pod kata-sandbox-api-test -o wide
$ kubectl logs kata-sandbox-api-test
```

Expected log output:

```text
sandbox-api-ok
```

## Verify that pause image is not required

On the node, check the containerd logs during pod creation:

```bash
$ sudo journalctl -u containerd --since "10 minutes ago" | grep -i pause
```

With `disable_pause_image_pull = true`, the CRI plugin should not pull the
configured pause image for this runtime handler during `RunPodSandbox`. A
debug log like the following is expected when containerd is running with debug
logging enabled:

```text
Skipping pause image pull for runtime handler ... disable_pause_image_pull=true
```

You can also inspect local images:

```bash
$ sudo crictl images | grep pause || true
$ sudo ctr -n k8s.io images ls | grep pause || true
```

The absence of a newly pulled pause image confirms that the runtime handler is
not depending on containerd's host-side pause image pre-pull path.

> **Note:**
> A pause image may still exist on the node from previous workloads or other
> runtime handlers. The important property is that this Kata Sandbox API
> runtime class does not require containerd to pull or use it for new pods.

## Verify the sandbox path

Check that the pod uses the Kata runtime handler:

```bash
$ sudo crictl pods --name kata-sandbox-api-test
$ sudo crictl inspectp <pod-sandbox-id> | grep -E 'runtime|sandbox'
```

Check for a Kata shim process:

```bash
$ ps -ef | grep containerd-shim-kata
```

For runtime-rs Sandbox API, containerd calls into the Kata shim SandboxService.
The sandbox container ID may be equal to the sandbox ID, but the sandbox
container is not stored as a normal application container in the runtime-rs
container map. runtime-rs returns synthetic state for that sandbox container
where needed.

## Notes for CoCo deployments

The Sandbox API and no-pause-image direction are especially useful for
Confidential Containers.

Older guest-pull CoCo setups could rely on a guest rootfs containing a built-in
pause image. That approach works, but it couples CoCo guest image composition
to Kubernetes/containerd pause-image behavior.

With the Sandbox API:

- containerd delegates sandbox lifecycle to the Kata shim;
- Kata starts the VM sandbox directly;
- the guest agent receives a sandbox setup request rather than a request to run
  a Kubernetes pause image;
- the guest rootfs no longer needs a built-in pause image as a fundamental
  dependency;
- CoCo images become easier to keep composable and independent from CRI pause
  image changes.

This does not remove the need to validate image pull, policy, attestation, and
guest storage paths. It only removes the pause image as a required base guest
asset for sandbox creation.

## Extra resources

## Why the pause image is not Required

There are two separate pause-image dependencies to avoid:

1. Host-side CRI pause image pull.
   - containerd normally ensures the configured pause image exists during
     `RunPodSandbox`.
   - For shim-managed sandbox runtimes, set `disable_pause_image_pull = true`.
2. Guest-side built-in pause image.
   - Older CoCo guest-pull setups could depend on a pause image embedded in
     the guest rootfs.
   - With Sandbox API, the sandbox lifecycle is owned by the Kata shim and VM,
     so the guest rootfs does not need a built-in pause image as a fundamental
     requirement.

Kata can keep compatibility fallback paths, but production Sandbox API
deployments should not require custom guest rootfs content just to provide a
pause image.

## containerd `disable_pause_image_pull` option

containerd PR
[#13424](https://github.com/containerd/containerd/pull/13424) adds a runtime
option to skip the CRI pause image pull for sandboxers that do not use the
traditional `podsandbox` pause-container path:

```toml
disable_pause_image_pull = true
```

For Kata, use this option with `sandboxer = "shim"` so containerd does not
pre-pull a host pause image for the VM-backed sandbox path.
