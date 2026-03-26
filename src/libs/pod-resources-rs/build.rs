// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

/// This generates Device Plugin code (in v1beta1.rs) from pluginapi.proto
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false) // We only need the client
        .build_client(true)
        .out_dir("src/pod_resources")
        .compile_protos(&["proto/pod_resources.proto"], &["proto"])
        .expect("failed to compile protos");

    Ok(())
}
