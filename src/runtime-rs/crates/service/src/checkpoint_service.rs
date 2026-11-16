// Copyright (c) 2026 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//
#[allow(dead_code)]
use std::{convert::TryInto, sync::Arc};

use async_trait::async_trait;
use common::error::Error as CommonError;
use common::types::utils::sandbox_operation_timeout;
use common::types::SandboxRequest;
use containerd_shim_protos::{sandbox_api, sandbox_async};
use runtimes::RuntimeHandlerManager;
use ttrpc::{self, r#async::TtrpcContext};

pub(crate) struct CheckpointService {
    handler: Arc<RuntimeHandlerManager>,
}

impl CheckpointService {
    pub(crate) fn new(handler: Arc<RuntimeHandlerManager>) -> Self {
        Self { handler }
    }
}

fn map_runtime_error(err: anyhow::Error) -> ttrpc::Error {
    let code = match err.downcast_ref::<CommonError>() {
        Some(CommonError::InvalidSandboxOperation(_)) => ttrpc::Code::INVALID_ARGUMENT,
        Some(CommonError::SandboxOperationPrecondition(_)) => ttrpc::Code::FAILED_PRECONDITION,
        Some(CommonError::SandboxOperationUnsupported(_)) => ttrpc::Code::UNIMPLEMENTED,
        Some(CommonError::SandboxOperationDeadline(_)) => ttrpc::Code::DEADLINE_EXCEEDED,
        _ => ttrpc::Code::INTERNAL,
    };
    ttrpc::Error::RpcStatus(ttrpc::get_status(
        code,
        format!("failed to handle checkpoint message: {err:#}"),
    ))
}

macro_rules! impl_checkpoint_service {
    ($($name: tt | $req: ty | $resp: ty),*) => {
        #[async_trait]
        impl sandbox_async::Checkpoint for CheckpointService {
            $(async fn $name(&self, ctx: &TtrpcContext, req: $req) -> ttrpc::Result<$resp> {
                let result: ttrpc::Result<$resp> = async {
                    let mut r: SandboxRequest = req.try_into().map_err(|err| {
                        ttrpc::Error::RpcStatus(ttrpc::get_status(
                            ttrpc::Code::INVALID_ARGUMENT,
                            format!("failed to translate checkpoint request: {err:?}"),
                        ))
                    })?;
                    if let SandboxRequest::CheckpointSandbox(checkpoint) = &mut r {
                        checkpoint.operation_timeout = sandbox_operation_timeout(ctx.timeout_nano);
                    }
                    if let SandboxRequest::RestoreSandbox(restore) = &mut r {
                        restore.operation_timeout = sandbox_operation_timeout(ctx.timeout_nano);
                    }
                    let handler = self.handler.clone();
                    let response = tokio::spawn(async move { handler.handler_sandbox_message(r).await })
                        .await
                        .map_err(|err| {
                            ttrpc::Error::RpcStatus(ttrpc::get_status(
                                ttrpc::Code::INTERNAL,
                                format!("checkpoint operation worker failed: {err}"),
                            ))
                        })?;
                    let resp = response.map_err(map_runtime_error)?;
                    resp.try_into()
                        .map_err(|err| ttrpc::Error::Others(format!("failed to translate to checkpoint response {err:?}")))
                }.await;
                result
            })*
        }
    };
}

impl_checkpoint_service!(
    checkpoint | sandbox_api::CheckpointRequest | sandbox_api::CheckpointResponse,
    restore | sandbox_api::RestoreRequest | sandbox_api::RestoreResponse
);
