use anyhow::Result;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Container, Pod};
use kube::Client;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio_util::compat::FuturesAsyncReadCompatExt;

#[derive(Debug, Clone)]
pub struct LogEvent {
    pub pod: Arc<Pod>,
    pub container: Arc<Container>,
    pub timestamp: Option<DateTime<Utc>>,
    pub message: String,
}

#[derive(Debug, PartialEq)]
enum TailState {
    Normal,
    Recover,
}

pub struct ContainerTailer {
    client: Client,
    pod: Pod,
    container: Container,
    from_timestamp: Option<DateTime<Utc>>,
    stop: Arc<AtomicBool>,
    state: TailState,
    last_line_checksum: Option<Vec<u8>>,
    retry_count: u32,
}

impl ContainerTailer {
    pub fn new(
        client: Client,
        pod: Pod,
        container: Container,
        from_timestamp: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            client,
            pod,
            container,
            from_timestamp,
            stop: Arc::new(AtomicBool::new(false)),
            state: TailState::Normal,
            last_line_checksum: None,
            retry_count: 0,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub async fn run<E, F>(&mut self, on_event: E, on_error: F) -> Result<()>
    where
        E: Fn(LogEvent) + Send + Sync + Clone,
        F: Fn(anyhow::Error) + Send + Sync + Clone,
    {
        while !self.stop.load(Ordering::SeqCst) {
            match self.get_stream().await {
                Ok(stream) => {
                    if let Err(e) = self.run_stream(stream, on_event.clone()).await {
                        let is_not_found = Self::is_pod_not_found(&e);
                        if !Self::is_transient_stream_error(&e) {
                            on_error(e);
                            // If pod is not found, stop retrying - it's permanently gone
                            if is_not_found {
                                break;
                            }
                        }
                        let backoff = self.calculate_backoff();
                        tokio::time::sleep(backoff).await;
                        self.retry_count += 1;
                    } else {
                        self.retry_count = 0;
                    }
                    self.state = TailState::Recover;
                }
                Err(e) => {
                    let is_not_found = Self::is_pod_not_found(&e);
                    if !Self::is_transient_stream_error(&e) {
                        on_error(e);
                        // If pod is not found, stop retrying - it's permanently gone
                        if is_not_found {
                            break;
                        }
                    }
                    let backoff = self.calculate_backoff();
                    tokio::time::sleep(backoff).await;
                    self.retry_count += 1;
                }
            }
        }
        Ok(())
    }

    fn calculate_backoff(&self) -> Duration {
        // Exponential backoff: 1s, 2s, 4s, 8s, max 30s
        let base = 1u64 << self.retry_count.min(4);
        Duration::from_secs(base.min(30))
    }

    async fn get_stream(&self) -> Result<Box<dyn AsyncRead + Unpin + Send>> {
        let pod_name = self
            .pod
            .metadata
            .name
            .as_ref()
            .ok_or(anyhow::anyhow!("No pod name"))?;
        let namespace = self
            .pod
            .metadata
            .namespace
            .as_ref()
            .ok_or(anyhow::anyhow!("No namespace"))?;

        let pods = kube::Api::<Pod>::namespaced(self.client.clone(), namespace);
        let logs = pods
            .log_stream(
                pod_name,
                &kube::api::LogParams {
                    container: Some(self.container.name.clone()),
                    follow: true,
                    since_time: self.from_timestamp,
                    ..Default::default()
                },
            )
            .await?;

        Ok(Box::new(logs.compat()))
    }

    async fn run_stream<E>(&mut self, stream: Box<dyn AsyncRead + Unpin + Send>, on_event: E) -> Result<()>
    where
        E: Fn(LogEvent),
    {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();

        while reader.read_line(&mut line).await? > 0 {
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            self.receive_line(trimmed, &on_event);
            line.clear();
        }

        Ok(())
    }

    fn receive_line<E>(&mut self, line: &str, on_event: &E)
    where
        E: Fn(LogEvent),
    {
        if line.is_empty() {
            return;
        }

        // Parse optional RFC3339 timestamp prefix. If not present, keep the full line.
        let (timestamp, message) = match line.split_once(' ') {
            Some((head, rest)) => {
                if let Ok(ts) = DateTime::parse_from_rfc3339(head) {
                    (Some(ts.with_timezone(&Utc)), rest)
                } else {
                    (None, line)
                }
            }
            None => (None, line),
        };

        let checksum = Self::checksum_line(message);

        // Handle recovery state
        if self.state == TailState::Recover {
            if let Some(ref last) = self.last_line_checksum {
                if last == &checksum {
                    return;
                }
            }
            if let Some(from_ts) = &self.from_timestamp {
                if let Some(ts) = timestamp {
                    if ts < *from_ts {
                        return;
                    }
                }
            }
        }

        self.last_line_checksum = Some(checksum);
        self.state = TailState::Normal;

        on_event(LogEvent {
            pod: Arc::new(self.pod.clone()),
            container: Arc::new(self.container.clone()),
            timestamp,
            message: message.to_string(),
        });
    }

    fn checksum_line(line: &str) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(line.as_bytes());
        hasher.finalize().to_vec()
    }

    fn is_transient_stream_error(error: &anyhow::Error) -> bool {
        let msg = error.to_string().to_ascii_lowercase();
        msg.contains("error reading a body from connection")
            || msg.contains("connection closed")
            || msg.contains("broken pipe")
            || msg.contains("connection reset")
            // Kubernetes may return 400 BadRequest while a container is still starting.
            || msg.contains("containercreating")
            || msg.contains("is waiting to start")
    }

    fn is_pod_not_found(error: &anyhow::Error) -> bool {
        let msg = error.to_string();
        msg.contains("not found") && msg.contains("404")
            || msg.contains("pods") && msg.contains("not found")
            || msg.contains("NotFound")
    }
}
