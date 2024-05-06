pub mod config;
pub mod driver_impl;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;

use anyhow::Result;

fn lookup_libraries(driver_path: &str) -> Result<HashMap<String, String>> {
    let libraries: HashMap<String, String> = fs::read_dir(driver_path)?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.is_dir() {
                return None;
            }

            let filename = path.file_name().and_then(OsStr::to_str)?;
            let lib_index = filename.rfind(".so.")?;
            let lib = &filename[..lib_index + 3];

            Some((lib.to_string(), filename.to_string()))
        })
        .collect();

    Ok(libraries)
}
