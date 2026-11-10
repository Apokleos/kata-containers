// Copyright (c) 2019-2022 Alibaba Cloud
// Copyright (c) 2019-2022 Ant Group
//
// SPDX-License-Identifier: Apache-2.0
//

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::Instant,
};

use super::{ConnectConfig, Sock, Stream};

#[derive(Debug)]
enum HandshakeError {
    PeerClosedBeforeResponse,
    NonReplayable(anyhow::Error),
}

#[derive(Debug, PartialEq)]
pub struct HybridVsock {
    uds: String,
    port: u32,
}

impl HybridVsock {
    pub fn new(uds: &str, port: u32) -> Self {
        Self {
            uds: uds.to_string(),
            port,
        }
    }

    async fn connect_with_budget(
        &self,
        retry_interval: Duration,
        reconnect_budget: Duration,
    ) -> Result<Stream> {
        let deadline = Instant::now() + reconnect_budget;
        let retry_interval = retry_interval.max(Duration::from_millis(1));
        let mut attempts = 0_u64;
        let mut last_err = None;

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "hybrid vsock: failed to connect UDS for {:?} within {:?} (attempts={}), last_err={:?}",
                    self,
                    reconnect_budget,
                    attempts,
                    last_err
                ));
            }

            attempts += 1;
            let stream = match tokio::time::timeout(deadline - now, UnixStream::connect(&self.uds))
                .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    let err = anyhow!(err).context("connect UDS");
                    trace!(
                        sl!(),
                        "hybrid vsock: UDS connect attempt {} failed for {:?}: {:#}",
                        attempts,
                        self,
                        err
                    );
                    last_err = Some(err);
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(anyhow!(
                            "hybrid vsock: failed to connect UDS for {:?} within {:?} (attempts={}), last_err={:?}",
                            self,
                            reconnect_budget,
                            attempts,
                            last_err
                        ));
                    }
                    tokio::time::sleep(retry_interval.min(deadline - now)).await;
                    continue;
                }
                Err(err) => {
                    return Err(anyhow!(
                        "hybrid vsock: UDS connect to {:?} timed out after {:?}: {}",
                        self,
                        reconnect_budget,
                        err
                    ));
                }
            };

            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    anyhow!(
                        "hybrid vsock: failed to connect UDS for {:?} within {:?} (attempts={}), last_err={:?}",
                        self,
                        reconnect_budget,
                        attempts,
                        last_err
                    )
                })?;

            match tokio::time::timeout(remaining, handshake(stream, self.port)).await {
                Ok(Ok(stream)) => {
                    info!(
                        sl!(),
                        "hybrid vsock: connected to {:?} after {} UDS attempt(s)", self, attempts
                    );
                    return Ok(Stream::Unix(stream));
                }
                Ok(Err(HandshakeError::PeerClosedBeforeResponse)) => {
                    let err = anyhow!("peer closed before hybrid-vsock handshake response");
                    trace!(
                        sl!(),
                        "hybrid vsock: replayable handshake attempt {} failed for {:?}: {:#}",
                        attempts,
                        self,
                        err
                    );
                    last_err = Some(err);
                }
                Ok(Err(HandshakeError::NonReplayable(err))) => {
                    return Err(err.context(format!(
                        "hybrid vsock: non-replayable handshake failed for {:?}",
                        self
                    )));
                }
                Err(err) => {
                    return Err(anyhow!(
                        "hybrid vsock: handshake for {:?} timed out after {:?}: {}",
                        self,
                        remaining,
                        err
                    ));
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "hybrid vsock: failed to connect to {:?} within {:?} (attempts={}), last_err={:?}",
                    self,
                    reconnect_budget,
                    attempts,
                    last_err
                ));
            }
            tokio::time::sleep(retry_interval.min(deadline - now)).await;
        }
    }
}

#[async_trait]
impl Sock for HybridVsock {
    async fn connect(&self, config: &ConnectConfig) -> Result<Stream> {
        self.connect_with_budget(
            Duration::from_millis(config.dial_timeout_ms),
            Duration::from_millis(config.reconnect_timeout_ms),
        )
        .await
    }
}

async fn handshake(
    mut stream: UnixStream,
    port: u32,
) -> std::result::Result<UnixStream, HandshakeError> {
    stream
        .write_all(format!("connect {port}\n").as_bytes())
        .await
        .map_err(|err| HandshakeError::NonReplayable(anyhow!(err).context("write all")))?;
    let mut reads = BufReader::new(&mut stream);
    let mut response = String::new();
    let bytes = reads
        .read_line(&mut response)
        .await
        .map_err(|err| HandshakeError::NonReplayable(anyhow!(err).context("read line")))?;
    if bytes == 0 {
        return Err(HandshakeError::PeerClosedBeforeResponse);
    }

    validate_handshake_response(&response).map_err(HandshakeError::NonReplayable)?;

    Ok(stream)
}

fn validate_handshake_response(response: &str) -> Result<()> {
    let mut fields = response.split_whitespace();
    let status = fields.next();
    let local_port = fields.next().and_then(|value| value.parse::<u32>().ok());
    if !response.ends_with('\n')
        || status != Some("OK")
        || local_port.is_none()
        || fields.next().is_some()
    {
        return Err(anyhow!(
            "handshake error: malformed response code: {:?}",
            response
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    use super::*;

    static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

    fn socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "kata-hvsock-{name}-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ))
    }

    async fn read_connect_command(stream: &mut UnixStream) {
        let mut command = String::new();
        BufReader::new(stream)
            .read_line(&mut command)
            .await
            .unwrap();
        assert_eq!(command, "connect 1024\n");
    }

    // Retries when the UDS is not ready.
    // Retries after an empty EOF.
    // Does not replay a malformed ACK.
    #[tokio::test]
    async fn retries_only_safe_connection_failures() {
        let path = socket_path("retry-boundary");
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let listener = UnixListener::bind(server_path).unwrap();

            let (mut first, _) = listener.accept().await.unwrap();
            read_connect_command(&mut first).await;
            drop(first);

            let (mut second, _) = listener.accept().await.unwrap();
            read_connect_command(&mut second).await;
            second.write_all(b"NO 9\n").await.unwrap();

            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });

        let sock = HybridVsock::new(path.to_str().unwrap(), 1024);
        let err = match sock
            .connect_with_budget(Duration::from_millis(10), Duration::from_millis(500))
            .await
        {
            Ok(_) => panic!("malformed ACK was accepted"),
            Err(err) => err,
        };
        assert!(format!("{err:#}").contains("malformed response"));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
        let _ = std::fs::remove_file(path);
    }

    // Connects successfully when the UDS becomes available late in the retry budget.
    // Limits a subsequent stalled handshake to the remaining budget.
    // Ensures the entire connection attempt never exceeds the absolute deadline.
    #[tokio::test]
    async fn one_budget_covers_late_uds_and_handshake() {
        let path = socket_path("one-budget");
        let server_path = path.clone();
        let server = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let listener = UnixListener::bind(server_path).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            read_connect_command(&mut stream).await;
            let mut drained = Vec::new();
            let _ = stream.read_to_end(&mut drained).await;
        });

        let sock = HybridVsock::new(path.to_str().unwrap(), 1024);
        let result = tokio::time::timeout(
            Duration::from_millis(500),
            sock.connect_with_budget(Duration::from_millis(10), Duration::from_millis(300)),
        )
        .await
        .expect("connect must not outlive its global budget");
        assert!(result.is_err());
        tokio::time::timeout(Duration::from_millis(100), server)
            .await
            .expect("the late UDS must be reached before the deadline")
            .unwrap();
        let _ = std::fs::remove_file(path);
    }

    // Accepts a valid ACK.
    // Accepts a fragmented but complete ACK.
    // Rejects a truncated ACK.
    // Rejects an invalid status, invalid port, or extra fields.
    #[tokio::test]
    async fn validates_complete_ack_frames() {
        assert!(validate_handshake_response("OK 42\n").is_ok());
        for malformed in [
            "OK\n",
            "OK nope\n",
            "OK 42 extra\n",
            "NO 42\n",
            "OK 12",
            "OK 12345",
            "OK",
            "OK ",
        ] {
            assert!(
                validate_handshake_response(malformed).is_err(),
                "{:?}",
                malformed
            );
        }

        let (stream, mut peer) = UnixStream::pair().unwrap();
        let fragmented = tokio::spawn(async move {
            read_connect_command(&mut peer).await;
            peer.write_all(b"OK 12").await.unwrap();
            tokio::task::yield_now().await;
            peer.write_all(b"345\n").await.unwrap();
        });
        handshake(stream, 1024).await.unwrap();
        fragmented.await.unwrap();

        let (stream, mut peer) = UnixStream::pair().unwrap();
        let truncated = tokio::spawn(async move {
            read_connect_command(&mut peer).await;
            peer.write_all(b"OK 12").await.unwrap();
        });
        assert!(matches!(
            handshake(stream, 1024).await,
            Err(HandshakeError::NonReplayable(_))
        ));
        truncated.await.unwrap();
    }
}
