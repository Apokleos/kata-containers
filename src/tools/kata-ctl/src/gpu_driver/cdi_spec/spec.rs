extern crate serde;
extern crate serde_json;

use std::collections::HashMap;

use libc::{self, mode_t};
use serde::{Deserialize, Serialize};

// Spec is the base configuration for CDI
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct CdiSpec {
    #[serde(rename = "cdiVersion")]
    pub(crate) version: String,
    #[serde(rename = "kind")]
    pub(crate) kind: String,
    // Annotations add meta information per CDI spec. Note these are CDI-specific and do not affect container metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) annotations: HashMap<String, String>,
    #[serde(rename = "devices", skip_serializing_if = "Vec::is_empty")]
    pub(crate) devices: Vec<Device>,
    #[serde(rename = "containerEdits", skip_serializing_if = "Option::is_none")]
    pub(crate) container_edits: Option<ContainerEdits>,
}

// Device is a "Device" a container runtime can add to a container
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct Device {
    #[serde(rename = "name")]
    pub(crate) name: String,
    #[serde(rename = "containerEdits")]
    pub(crate) container_edits: ContainerEdits,
    // Annotations add meta information per device. Note these are CDI-specific and do not affect container metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(crate) annotations: HashMap<String, String>,
}

// ContainerEdits are edits a container runtime must make to the OCI spec to expose the device.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct ContainerEdits {
    #[serde(rename = "env", skip_serializing_if = "Vec::is_empty")]
    pub(crate) env: Vec<String>,
    #[serde(rename = "deviceNodes", skip_serializing_if = "Vec::is_empty")]
    pub(crate) device_nodes: Vec<DeviceNode>,
    #[serde(rename = "hooks", skip_serializing_if = "Vec::is_empty")]
    pub(crate) hooks: Vec<Hook>,
    #[serde(rename = "mounts", skip_serializing_if = "Vec::is_empty")]
    pub(crate) mounts: Vec<Mount>,
    #[serde(rename = "intelRdt", skip_serializing_if = "Option::is_none")]
    pub(crate) intel_rdt: Option<IntelRdt>,
    #[serde(rename = "additionalGids", skip_serializing_if = "Vec::is_empty")]
    pub(crate) additional_gids: Vec<u32>,
}

// DeviceNode represents a device node that needs to be added to the OCI spec.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct DeviceNode {
    #[serde(rename = "path", skip_serializing_if = "String::is_empty")]
    pub(crate) path: String,
    #[serde(rename = "hostPath", skip_serializing_if = "String::is_empty")]
    pub(crate) host_path: String,
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    pub(crate) r#type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<i64>,
    #[serde(rename = "fileMode", skip_serializing_if = "Option::is_none")]
    pub file_mode: Option<mode_t>,
    #[serde(rename = "permissions", skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
}

// Mount represents a mount that needs to be added to the OCI spec.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct Mount {
    #[serde(rename = "hostPath")]
    pub(crate) host_path: String,
    #[serde(rename = "containerPath")]
    pub(crate) container_path: String,
    #[serde(rename = "type")]
    pub(crate) r#type: String,
    #[serde(rename = "options", skip_serializing_if = "Vec::is_empty")]
    pub(crate) options: Vec<String>,
}

// Hook represents a hook that needs to be added to the OCI spec.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct Hook {
    #[serde(rename = "hookName")]
    pub(crate) hook_name: String,
    #[serde(rename = "path")]
    pub(crate) path: String,
    #[serde(rename = "args", skip_serializing_if = "Vec::is_empty")]
    pub(crate) args: Vec<String>,
    #[serde(rename = "env", skip_serializing_if = "Vec::is_empty")]
    pub(crate) env: Vec<String>,
    #[serde(rename = "timeout", skip_serializing_if = "Option::is_none")]
    pub(crate) timeout: Option<i32>,
}

// IntelRdt describes the Linux IntelRdt parameters to set in the OCI spec.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct IntelRdt {
    #[serde(rename = "closID", skip_serializing_if = "String::is_empty")]
    pub(crate) clos_id: String,
    #[serde(rename = "l3CacheSchema", skip_serializing_if = "String::is_empty")]
    pub(crate) l3_cache_schema: String,
    #[serde(rename = "memBwSchema", skip_serializing_if = "String::is_empty")]
    pub(crate) mem_bw_schema: String,
    #[serde(default, rename = "enableCMT")]
    pub(crate) enable_cmt: bool,
    #[serde(default, rename = "enableMBM")]
    pub(crate) enable_mbm: bool,
}
