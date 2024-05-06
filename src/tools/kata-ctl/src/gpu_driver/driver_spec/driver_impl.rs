use std::{collections::HashMap, fs::File, io::Write, path::Path};

use anyhow::{Context, Result};
use kata_sys_util::sl;

use super::{
    config::{DriverInfo, DriverSpec, Metadata, Template},
    lookup_libraries,
};

impl Template {
    pub fn new(template_path: &str) -> Result<Self> {
        let template_yaml = File::open(template_path).context("open template yaml file failed")?;
        let template: Template =
            serde_yaml::from_reader(template_yaml).context("serde template yaml failed")?;

        Ok(template)
    }

    pub fn update_libraries(
        &self,
        libs: HashMap<String, String>,
        driver_path: &str,
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        let mut driver_libs: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (key, template_libs) in self.libs.iter() {
            let mut kv_updating: HashMap<String, String> = HashMap::new();
            for target_lib in template_libs {
                if let Some(real_name) = libs.get(target_lib) {
                    let lib_path = driver_path.to_owned() + "/" + real_name;
                    kv_updating.insert(target_lib.to_string(), lib_path);
                } else {
                    warn!(sl!(), "target lib {:?} not found!", target_lib);
                }
            }
            driver_libs.insert(key.clone(), kv_updating);
        }

        Ok(driver_libs)
    }

    pub fn update_binaries(
        &self,
        driver_path: &str,
    ) -> Result<HashMap<String, HashMap<String, String>>> {
        let mut driver_bins: HashMap<String, HashMap<String, String>> = HashMap::new();
        for (key, template_bins) in self.bins.iter() {
            let mut kv_updating: HashMap<String, String> = HashMap::new();
            for target_bin in template_bins.iter() {
                let bin_path = driver_path.to_owned() + "/" + target_bin;
                if Path::new(&bin_path).exists() {
                    kv_updating.insert(target_bin.to_string(), bin_path);
                } else {
                    warn!(sl!(), "gpu driver bin file {} not exists", target_bin);
                }
            }
            driver_bins.insert(key.to_string(), kv_updating);
        }

        Ok(driver_bins)
    }
}

impl DriverInfo {
    pub fn new(driver_version: &str, driver_path: &str, template: Template) -> Result<Self> {
        // let driver_version = "525.105.17";
        let origin_libs =
            lookup_libraries(driver_path).context("lookup gpu driver libraries failed")?;

        let driver_libs = template
            .update_libraries(origin_libs, driver_path)
            .context("update libraries failed")?;
        let driver_bins = template
            .update_binaries(driver_path)
            .context("update binaries failed")?;

        Ok(DriverInfo {
            metadata: Metadata {
                product_vendor: "nvidia".to_owned(),
                product_type: "tesla".to_owned(),
                driver_version: driver_version.to_string(),
                driver_path: driver_path.to_string(),
                annotations: HashMap::new(),
            },
            spec: DriverSpec {
                libs: driver_libs,
                bins: driver_bins,
            },
        })
    }

    pub fn save(&self) -> Result<()> {
        let driver_version = self.metadata.driver_version.clone();
        let generated_config = format!("driver-config-{}.yaml", driver_version);

        let yaml = serde_yaml::to_string(self).context("do serde yaml failed")?;
        let mut config_file = File::create(generated_config).context("unable to create file")?;

        config_file
            .write_all(yaml.as_bytes())
            .context("unable to write to file")?;

        Ok(())
    }
}
