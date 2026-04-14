use anyhow::Result;
use chrono::{DateTime, Utc};
use k8s_openapi::api::core::v1::{Pod, Container};
use kube::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use futures::StreamExt;
use kube::runtime::watcher;

use crate::matcher::Matcher;
use crate::tailer::{ContainerTailer, LogEvent};

pub struct ControllerOptions {
    pub namespaces: Vec<String>,
    pub inclusion_matcher: Arc<dyn Matcher>,
    pub exclusion_matcher: Arc<dyn Matcher>,
    pub since_start: bool,
    pub since: Option<DateTime<Utc>>,
}

pub struct Callbacks {
    pub on_event: Arc<dyn Fn(LogEvent) + Send + Sync>,
    pub on_enter: Arc<dyn Fn(&Arc<Pod>, &Arc<Container>, bool) -> bool + Send + Sync>,
    pub on_exit: Arc<dyn Fn(&Arc<Pod>, &Arc<Container>) + Send + Sync>,
    pub on_error: Arc<dyn Fn(&Arc<Pod>, &Arc<Container>, &anyhow::Error) + Send + Sync>,
    pub on_nothing_discovered: Arc<dyn Fn() + Send + Sync>,
}

pub struct Controller {
    client: Client,
    options: ControllerOptions,
    callbacks: Callbacks,
    tailers: Arc<RwLock<HashMap<String, TailerEntry>>>,
    init_buffer: Arc<RwLock<Option<HashMap<String, TailerTarget>>>>,
}

struct TailerEntry {
    handle: tokio::task::JoinHandle<()>,
    pod: Arc<Pod>,
    container: Arc<Container>,
}

#[derive(Clone)]
struct TailerTarget {
    pod: Arc<Pod>,
    container: Arc<Container>,
}

impl Controller {
    pub fn new(client: Client, options: ControllerOptions, callbacks: Callbacks) -> Self {
        Self {
            client,
            options,
            callbacks,
            tailers: Arc::new(RwLock::new(HashMap::new())),
            init_buffer: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn run(self: Arc<Self>, token: tokio_util::sync::CancellationToken) -> Result<()> {
        let mut discovered_any = false;
        let mut inaccessible_namespaces = std::collections::HashSet::new();

        // Initial discovery across all namespaces
        for namespace in &self.options.namespaces {
            let actual_namespace = if namespace == "*" {
                None
            } else {
                Some(namespace.clone())
            };

            let api = if let Some(ns) = actual_namespace {
                kube::Api::<Pod>::namespaced(self.client.clone(), &ns)
            } else {
                kube::Api::<Pod>::all(self.client.clone())
            };

            match api.list(&Default::default()).await {
                Ok(pod_list) => {
                    for pod in pod_list.items {
                        if self.on_initial_add(&Arc::new(pod)).await {
                            discovered_any = true;
                        }
                    }
                }
                Err(e) => {
                    let error_str = e.to_string();
                    if error_str.contains("forbidden") || error_str.contains("Forbidden") || error_str.contains("403") {
                        eprintln!("Permission denied for namespace '{}' - skipping", namespace);
                        inaccessible_namespaces.insert(namespace.clone());
                    } else {
                        eprintln!("Failed to list pods in namespace {}: {}", namespace, e);
                    }
                }
            }
        }

        if !discovered_any {
            (self.callbacks.on_nothing_discovered)();
        }

        // Watch all accessible namespaces concurrently
        let mut watch_tasks = vec![];
        for namespace in &self.options.namespaces {
            // Skip namespaces that were inaccessible during discovery
            if inaccessible_namespaces.contains(namespace) {
                continue;
            }

            let actual_namespace = if namespace == "*" {
                None
            } else {
                Some(namespace.clone())
            };

            let api = if let Some(ns) = actual_namespace {
                kube::Api::<Pod>::namespaced(self.client.clone(), &ns)
            } else {
                kube::Api::<Pod>::all(self.client.clone())
            };

            let token_clone = token.clone();
            let self_clone = self.clone();
            let namespace_clone = namespace.clone();
            
            let task = tokio::spawn(async move {
                let mut stream = watcher(api, Default::default()).boxed();
                loop {
                    tokio::select! {
                        _ = token_clone.cancelled() => break,
                        result = stream.next() => {
                            match result {
                                Some(Ok(event)) => {
                                    use kube::runtime::watcher::Event;
                                    match event {
                                        Event::Apply(pod) => {
                                            self_clone.on_add(&Arc::new(pod)).await;
                                        }
                                        Event::Delete(pod) => {
                                            self_clone.on_delete(&Arc::new(pod)).await;
                                        }
                                        Event::Init => {
                                            self_clone.on_init().await;
                                        }
                                        Event::InitApply(pod) => {
                                            self_clone.on_init_apply(&Arc::new(pod)).await;
                                        }
                                        Event::InitDone => {
                                            self_clone.on_init_done().await;
                                        }
                                    }
                                }
                                Some(Err(e)) => {
                                    let error_str = e.to_string();
                                    if error_str.contains("forbidden") || error_str.contains("Forbidden") || error_str.contains("403") {
                                        eprintln!("Permission denied for namespace '{}' - stopping watch", namespace_clone);
                                        break;
                                    } else {
                                        eprintln!("Watcher error in namespace '{}': {}", namespace_clone, e);
                                    }
                                }
                                None => break,
                            }
                        }
                    }
                }
            });
            watch_tasks.push(task);
        }

        // Wait for cancellation signal instead of waiting for watch tasks to complete
        // The tailers will continue streaming logs in the background
        token.cancelled().await;

        Ok(())
    }

    async fn on_initial_add(&self, pod: &Arc<Pod>) -> bool {
        let targets = self.matching_targets(pod);
        let added = !targets.is_empty();

        for target in targets {
            self.add_container(&target.pod, &target.container, true).await;
        }

        added
    }

    async fn on_add(&self, pod: &Arc<Pod>) {
        self.reconcile_pod_targets(pod, false).await;
    }

    async fn on_delete(&self, pod: &Arc<Pod>) {
        for target in self.remove_tailers_for_pod(pod).await {
            (self.callbacks.on_exit)(&target.pod, &target.container);
        }
    }

    async fn on_init(&self) {
        *self.init_buffer.write().await = Some(HashMap::new());
    }

    async fn on_init_apply(&self, pod: &Arc<Pod>) {
        let targets = self.matching_targets(pod);
        let mut init_buffer = self.init_buffer.write().await;
        let buffer = init_buffer.get_or_insert_with(HashMap::new);
        for target in targets {
            buffer.insert(self.tailer_id(&target.pod, &target.container), target);
        }
    }

    async fn on_init_done(&self) {
        let desired = self
            .init_buffer
            .write()
            .await
            .take()
            .unwrap_or_default();

        let desired_ids = desired.keys().cloned().collect::<std::collections::HashSet<_>>();
        let mut removed = Vec::new();

        {
            let mut tailers = self.tailers.write().await;
            let stale_ids = tailers
                .keys()
                .filter(|tailer_id| !desired_ids.contains(*tailer_id))
                .cloned()
                .collect::<Vec<_>>();

            for tailer_id in stale_ids {
                if let Some(entry) = tailers.remove(&tailer_id) {
                    entry.handle.abort();
                    removed.push(TailerTarget {
                        pod: entry.pod,
                        container: entry.container,
                    });
                }
            }
        }

        for target in removed {
            (self.callbacks.on_exit)(&target.pod, &target.container);
        }

        for target in desired.into_values() {
            self.add_container(&target.pod, &target.container, false).await;
        }
    }

    fn should_include_container(&self, pod: &Pod, container: &Container) -> bool {
        if !self.pod_allows_tailing(pod) {
            return false;
        }
        if !self.container_has_known_status(pod, &container.name) {
            return false;
        }
        if !self.options.inclusion_matcher.matches_container(pod, container) {
            return false;
        }
        if self.options.exclusion_matcher.matches_container(pod, container) {
            return false;
        }
        true
    }

    fn pod_allows_tailing(&self, pod: &Pod) -> bool {
        matches!(
            pod.status.as_ref().and_then(|status| status.phase.as_deref()),
            Some("Running") | Some("Pending")
        )
    }

    fn container_has_known_status(&self, pod: &Pod, container_name: &str) -> bool {
        let Some(status) = &pod.status else {
            return false;
        };

        status
            .container_statuses
            .iter()
            .flatten()
            .chain(status.init_container_statuses.iter().flatten())
            .any(|container_status| {
                let Some(state) = &container_status.state else {
                    return false;
                };
                container_status.name == container_name
                    && (state.running.is_some()
                        || state.waiting.is_some()
                        || state.terminated.is_some())
            })
    }

    fn matching_targets(&self, pod: &Arc<Pod>) -> Vec<TailerTarget> {
        let mut targets = Vec::new();

        if let Some(spec) = &pod.spec {
            if let Some(init_containers) = &spec.init_containers {
                for container in init_containers {
                    if self.should_include_container(pod, container) {
                        targets.push(TailerTarget {
                            pod: pod.clone(),
                            container: Arc::new(container.clone()),
                        });
                    }
                }
            }

            for container in &spec.containers {
                if self.should_include_container(pod, container) {
                    targets.push(TailerTarget {
                        pod: pod.clone(),
                        container: Arc::new(container.clone()),
                    });
                }
            }
        }

        targets
    }

    fn tailer_id(&self, pod: &Arc<Pod>, container: &Arc<Container>) -> String {
        let pod_name = pod.metadata.name.as_deref().unwrap_or_default();
        format!("{}/{}", pod_name, container.name)
    }

    async fn remove_tailers_for_pod(&self, pod: &Arc<Pod>) -> Vec<TailerTarget> {
        let pod_name = pod.metadata.name.as_deref().unwrap_or_default();
        let pod_namespace = pod.metadata.namespace.as_deref().unwrap_or_default();
        let mut removed = Vec::new();

        let mut tailers = self.tailers.write().await;
        let stale_ids = tailers
            .iter()
            .filter(|(_, entry)| {
                entry.pod.metadata.name.as_deref().unwrap_or_default() == pod_name
                    && entry.pod.metadata.namespace.as_deref().unwrap_or_default() == pod_namespace
            })
            .map(|(tailer_id, _)| tailer_id.clone())
            .collect::<Vec<_>>();

        for tailer_id in stale_ids {
            if let Some(entry) = tailers.remove(&tailer_id) {
                entry.handle.abort();
                removed.push(TailerTarget {
                    pod: entry.pod,
                    container: entry.container,
                });
            }
        }

        removed
    }

    async fn reconcile_pod_targets(&self, pod: &Arc<Pod>, initial_add: bool) {
        let desired_targets = self.matching_targets(pod);
        let desired_ids = desired_targets
            .iter()
            .map(|target| self.tailer_id(&target.pod, &target.container))
            .collect::<std::collections::HashSet<_>>();
        let pod_name = pod.metadata.name.as_deref().unwrap_or_default();
        let pod_namespace = pod.metadata.namespace.as_deref().unwrap_or_default();
        let mut removed = Vec::new();

        {
            let mut tailers = self.tailers.write().await;
            let stale_ids = tailers
                .iter()
                .filter(|(tailer_id, entry)| {
                    entry.pod.metadata.name.as_deref().unwrap_or_default() == pod_name
                        && entry.pod.metadata.namespace.as_deref().unwrap_or_default() == pod_namespace
                        && !desired_ids.contains(*tailer_id)
                })
                .map(|(tailer_id, _)| tailer_id.clone())
                .collect::<Vec<_>>();

            for tailer_id in stale_ids {
                if let Some(entry) = tailers.remove(&tailer_id) {
                    entry.handle.abort();
                    removed.push(TailerTarget {
                        pod: entry.pod,
                        container: entry.container,
                    });
                }
            }
        }

        for target in removed {
            (self.callbacks.on_exit)(&target.pod, &target.container);
        }

        for target in desired_targets {
            self.add_container(&target.pod, &target.container, initial_add).await;
        }
    }

    async fn add_container(&self, pod: &Arc<Pod>, container: &Arc<Container>, initial_add: bool) {
        let tailer_id = self.tailer_id(pod, container);

        // Keep callback+insert atomic under one lock to avoid duplicate starts from rapid Apply events.
        {
            let mut tailers = self.tailers.write().await;
            if tailers.contains_key(&tailer_id) {
                return;
            }

            let should_tail = (self.callbacks.on_enter)(pod, container, initial_add);
            if !should_tail {
                return;
            }

            let client = self.client.clone();
            let pod_clone = pod.clone();
            let container_clone = container.clone();
            let on_event = self.callbacks.on_event.clone();
            let on_error = self.callbacks.on_error.clone();
            let on_exit = self.callbacks.on_exit.clone();
            let from_timestamp = self.options.since;
            let tailers_map = self.tailers.clone();
            let tailer_id_for_cleanup = tailer_id.clone();

            let handle = tokio::spawn(async move {
                let mut tailer = ContainerTailer::new(
                    client,
                    pod_clone.as_ref().clone(),
                    container_clone.as_ref().clone(),
                    from_timestamp,
                );

                let event_callback = |event: LogEvent| {
                    on_event(event);
                };

                let error_callback = |e: anyhow::Error| {
                    on_error(&pod_clone, &container_clone, &e);
                };

                let _ = tailer.run(event_callback, error_callback).await;

                on_exit(&pod_clone, &container_clone);

                // Remove completed tailer so future pod/container instances can be tailed.
                tailers_map.write().await.remove(&tailer_id_for_cleanup);
            });

            tailers.insert(
                tailer_id,
                TailerEntry {
                    handle,
                    pod: pod.clone(),
                    container: container.clone(),
                },
            );
        }
    }
}
