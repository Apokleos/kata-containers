extern crate serde;
extern crate serde_json;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct Metadata {
    #[serde(rename = "driverVersion")]
    pub driver_version: String,
    #[serde(rename = "driverPath")]
    pub driver_path: String,
    #[serde(rename = "productVendor")]
    pub product_vendor: String,
    #[serde(rename = "productType")]
    pub product_type: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub annotations: HashMap<String, String>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct Template {
    #[serde(rename = "libs")]
    pub libs: HashMap<String, Vec<String>>,
    #[serde(rename = "bins")]
    pub bins: HashMap<String, Vec<String>>,
    #[serde(rename = "options", skip_serializing_if = "HashMap::is_empty")]
    pub options: HashMap<String, Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct DriverSpec {
    #[serde(default)]
    pub libs: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub bins: HashMap<String, HashMap<String, String>>,
}

#[derive(Serialize, Deserialize, Debug, Default, Clone, PartialEq)]
pub struct DriverInfo {
    #[serde(rename = "metaData")]
    pub metadata: Metadata,
    #[serde(rename = "driverSpec")]
    pub spec: DriverSpec,
}
