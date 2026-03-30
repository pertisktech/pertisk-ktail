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
    tailers: Arc<RwLock<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl Controller {
    pub fn new(client: Client, options: ControllerOptions, callbacks: Callbacks) -> Self {
        Self {
            client,
            options,
            callbacks,
            tailers: Arc::new(RwLock::new(HashMap::new())),
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
                                        _ => {}
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
        let mut added = false;

        if let Some(spec) = &pod.spec {
            if let Some(init_containers) = &spec.init_containers {
                for container in init_containers {
                    if self.should_include_container(pod, container) {
                        self.add_container(pod, &Arc::new(container.clone()), true).await;
                        added = true;
                    }
                }
            }

            for container in &spec.containers {
                if self.should_include_container(pod, container) {
                    self.add_container(pod, &Arc::new(container.clone()), true).await;
                    added = true;
                }
            }
        }

        added
    }

    async fn on_add(&self, pod: &Arc<Pod>) {
        if let Some(spec) = &pod.spec {
            if let Some(init_containers) = &spec.init_containers {
                for container in init_containers {
                    if self.should_include_container(pod, container) {
                        self.add_container(pod, &Arc::new(container.clone()), false).await;
                    }
                }
            }

            for container in &spec.containers {
                if self.should_include_container(pod, container) {
                    self.add_container(pod, &Arc::new(container.clone()), false).await;
                }
            }
        }
    }

    async fn on_delete(&self, pod: &Arc<Pod>) {
        if let Some(spec) = &pod.spec {
            for container in &spec.containers {
                (self.callbacks.on_exit)(pod, &Arc::new(container.clone()));
            }
        }
    }

    fn should_include_container(&self, pod: &Pod, container: &Container) -> bool {
        if !self.options.inclusion_matcher.matches_container(pod, container) {
            return false;
        }
        if self.options.exclusion_matcher.matches_container(pod, container) {
            return false;
        }
        true
    }

    async fn add_container(&self, pod: &Arc<Pod>, container: &Arc<Container>, initial_add: bool) {
        let should_tail = (self.callbacks.on_enter)(pod, container, initial_add);
        
        if !should_tail {
            return;
        }

        let pod_name = pod.metadata.name.as_ref().map(|s| s.clone()).unwrap_or_default();
        let container_name = container.name.clone();
        let tailer_id = format!("{}/{}", pod_name, container_name);

        {
            let tailers = self.tailers.read().await;
            if tailers.contains_key(&tailer_id) {
                return;
            }
        }

        let client = self.client.clone();
        let pod_clone = pod.clone();
        let container_clone = container.clone();
        let on_event = self.callbacks.on_event.clone();
        let on_error = self.callbacks.on_error.clone();
        let from_timestamp = self.options.since;

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
        });

        self.tailers.write().await.insert(tailer_id, handle);
    }
}
