pub mod cdi_spec;
mod driver_cdi;
pub mod driver_spec;

use anyhow::{Context, Result};

use self::driver_cdi::{gpu_driver_cdi_dump, gpu_driver_info_dump};
use crate::args::{GpuDriverCommand, GpuDriverSubCommand};

pub fn gpu_driver_dump(gpu_cmd: GpuDriverCommand) -> Result<()> {
    let command = gpu_cmd.gpudriver_cmd;
    match command {
        GpuDriverSubCommand::GenCfg(args) => {
            generate_gpu_driver_config(&args.driver_path, &args.template_path)
        }
        GpuDriverSubCommand::GenCdi(args) => {
            generate_gpu_driver_cdi(&args.driver_path, &args.template_path, &args.config_name)
        }
    }
}

pub fn generate_gpu_driver_config(driver_path: &str, template_path: &str) -> Result<()> {
    gpu_driver_info_dump(driver_path, template_path).context("gpu driver info dump failed")?;

    Ok(())
}

pub fn generate_gpu_driver_cdi(
    driver_path: &str,
    template_path: &str,
    config_name: &str,
) -> Result<()> {
    gpu_driver_cdi_dump(driver_path, template_path, config_name)
        .context("gpu driver cdi new failed")?;

    Ok(())
}
