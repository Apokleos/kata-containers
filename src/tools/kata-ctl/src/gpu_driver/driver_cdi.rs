use anyhow::{Context, Result};

use super::{
    cdi_spec::generate_cdi_config,
    driver_spec::config::{DriverInfo, Template},
};

pub const DEFAULT_CDI_PATH: &str = "/etc/cdi";
pub const DEFAULT_DRIVER_VERSION: &str = "525.105.27";

pub fn gpu_driver_info_dump(driver_path: &str, template_path: &str) -> Result<()> {
    let driver_version: &str = DEFAULT_DRIVER_VERSION;
    let template = Template::new(template_path).context("template new failed")?;
    let driver_info =
        DriverInfo::new(driver_version, driver_path, template).context("driver info new failed")?;
    driver_info.save().context("save driver info failed")?;

    Ok(())
}

pub fn gpu_driver_cdi_dump(
    driver_path: &str,
    template_path: &str,
    config_name: &str,
) -> Result<()> {
    let cdi_path = DEFAULT_CDI_PATH;
    let driver_version: &str = DEFAULT_DRIVER_VERSION;
    let template = Template::new(template_path).context("template new failed")?;
    let driver_info =
        DriverInfo::new(driver_version, driver_path, template).context("driver info new failed")?;
    let cdi_spec = generate_cdi_config(driver_info.spec.clone()).context("generate cdi config")?;
    cdi_spec
        .save(cdi_path, config_name)
        .context("save cdi spec failed")?;

    Ok(())
}
