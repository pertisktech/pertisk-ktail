use anyhow::Result;
use regex::Regex;
use k8s_openapi::api::core::v1::{Pod, Container};
use std::collections::HashMap;

/// Trait for matching pods and containers
pub trait Matcher: Send + Sync {
    fn matches_pod(&self, pod: &Pod) -> bool;
    fn matches_container(&self, pod: &Pod, container: &Container) -> bool;
}

/// Regex-based matcher for pod/container names
pub struct RegexMatcher {
    regex: Regex,
}

impl RegexMatcher {
    pub fn new(pattern: &str) -> Result<Self> {
        Ok(Self {
            regex: Regex::new(pattern)?,
        })
    }
}

impl Matcher for RegexMatcher {
    fn matches_pod(&self, pod: &Pod) -> bool {
        if let Some(name) = &pod.metadata.name {
            self.regex.is_match(name)
        } else {
            false
        }
    }

    fn matches_container(&self, _pod: &Pod, container: &Container) -> bool {
        self.regex.is_match(&container.name)
    }
}

/// Label selector matcher
pub struct LabelSelectorMatcher {
    labels: HashMap<String, String>,
}

impl LabelSelectorMatcher {
    pub fn new(labels: HashMap<String, String>) -> Self {
        Self { labels }
    }
}

impl Matcher for LabelSelectorMatcher {
    fn matches_pod(&self, pod: &Pod) -> bool {
        if let Some(pod_labels) = &pod.metadata.labels {
            self.labels.iter().all(|(k, v)| {
                pod_labels.get(k).map_or(false, |pv| pv == v)
            })
        } else {
            false
        }
    }

    fn matches_container(&self, _pod: &Pod, _container: &Container) -> bool {
        // Label selectors apply to pods, not individual containers
        false
    }
}

/// Composite AND matcher
pub struct AndMatcher(pub Vec<Box<dyn Matcher>>);

impl Matcher for AndMatcher {
    fn matches_pod(&self, pod: &Pod) -> bool {
        if self.0.is_empty() {
            return false;
        }
        self.0.iter().all(|m| m.matches_pod(pod))
    }

    fn matches_container(&self, pod: &Pod, container: &Container) -> bool {
        if self.0.is_empty() {
            return false;
        }
        self.0.iter().all(|m| m.matches_container(pod, container))
    }
}

/// Composite OR matcher
pub struct OrMatcher(pub Vec<Box<dyn Matcher>>);

impl Matcher for OrMatcher {
    fn matches_pod(&self, pod: &Pod) -> bool {
        if self.0.is_empty() {
            return false;
        }
        self.0.iter().any(|m| m.matches_pod(pod))
    }

    fn matches_container(&self, pod: &Pod, container: &Container) -> bool {
        if self.0.is_empty() {
            return false;
        }
        self.0.iter().any(|m| m.matches_container(pod, container))
    }
}

/// Negation matcher
pub struct NotMatcher(pub Box<dyn Matcher>);

impl Matcher for NotMatcher {
    fn matches_pod(&self, pod: &Pod) -> bool {
        !self.0.matches_pod(pod)
    }

    fn matches_container(&self, pod: &Pod, container: &Container) -> bool {
        !self.0.matches_container(pod, container)
    }
}
