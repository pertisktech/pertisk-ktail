use anyhow::Result;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Container, Pod};
use kube::Client;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

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
                        on_error(e);
                        let backoff = self.calculate_backoff();
                        tokio::time::sleep(backoff).await;
                        self.retry_count += 1;
                    } else {
                        self.retry_count = 0;
                    }
                    self.state = TailState::Recover;
                }
                Err(e) => {
                    on_error(e);
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
        let _logs = pods
            .log_stream(
                pod_name,
                &kube::api::LogParams {
                    container: Some(self.container.name.clone()),
                    follow: true,
                    ..Default::default()
                },
            )
            .await?;

        // Placeholder: return empty stream for now
        // In production, you'd need to properly convert the kube-rs stream
        let empty: Box<dyn AsyncRead + Unpin + Send> = Box::new(tokio::io::empty());
        Ok(empty)
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

        // Parse timestamp and message
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            return;
        }

        let timestamp_str = parts[0];
        let message = parts[1];

        let timestamp = DateTime::parse_from_rfc3339(timestamp_str)
            .ok()
            .map(|dt| dt.with_timezone(&Utc));

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
}
