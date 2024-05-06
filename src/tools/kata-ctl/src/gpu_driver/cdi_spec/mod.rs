pub mod cdi_impl;
pub mod spec;

// CURRENT_VERSION is the current version of the Spec.
pub const CURRENT_CDI_VERSION: &str = "0.5.0";

pub const NVIDIA_GPU_KIND: &str = "nvidia.com/gpu";

use std::ffi::OsString;
use std::path::Path;

use anyhow::{anyhow, Result};

use self::spec::{CdiSpec, ContainerEdits, Mount};

use super::driver_spec::config::DriverSpec;

const X86_64_LIB64_DIR: &str = "/usr/lib64";
const USR_BIN_PATH: &str = "/usr/bin";

/// Get the basename of the canonicalized path
pub fn get_base_name<P: AsRef<Path>>(src: P) -> Result<OsString> {
    let s = src.as_ref().canonicalize()?;
    let file_name = s.file_name().map(|v| v.to_os_string());
    match file_name {
        Some(f) => Ok(f),
        None => Err(anyhow!("os string not ok")),
    }
}

pub fn generate_cdi_config(driver_spec: DriverSpec) -> Result<CdiSpec> {
    let driver_libs = driver_spec.libs.values();
    let driver_bins = driver_spec.bins.values();

    let mut cdi_spec = CdiSpec::new();
    let mut container_edits = ContainerEdits::new();
    let mut mounts: Vec<Mount> = Vec::new();
    let mut envs: Vec<String> = container_edits.env.clone();

    for libs in driver_libs {
        for host_path in libs.values() {
            let base_name = get_base_name(host_path)?
                .into_string()
                .map_err(|e| anyhow!("failed to convert to string {:?}", e))?;
            if base_name.starts_with("libnvidia-ml.so") {
                let env_ml = format!("LD_PRELOAD={}/{}", X86_64_LIB64_DIR, base_name);
                envs.push(env_ml);
            }
            let container_path = format!("{}/{}", X86_64_LIB64_DIR, base_name);

            mounts.push(Mount::new(host_path, &container_path));
        }
    }

    for bins in driver_bins {
        for (base_name, host_path) in bins {
            let container_path = format!("{}/{}", USR_BIN_PATH, base_name);
            mounts.push(Mount::new(host_path, &container_path));
        }
    }

    container_edits.env = envs;
    container_edits.mounts = mounts;
    cdi_spec.container_edits = Some(container_edits);

    Ok(cdi_spec)
}
