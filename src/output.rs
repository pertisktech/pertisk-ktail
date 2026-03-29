use crate::tailer::LogEvent;
use colored::*;
use std::collections::HashMap;

pub struct OutputFormatter {
    pub raw: bool,
    pub timestamps: bool,
    pub quiet: bool,
    pub template: Option<String>,
    pub color_mode: String,
    pub color_scheme: String,
    pub no_color: bool,
}

impl OutputFormatter {
    pub fn new(
        raw: bool,
        timestamps: bool,
        quiet: bool,
        template: Option<String>,
        color_mode: String,
        color_scheme: String,
        no_color: bool,
    ) -> Self {
        Self {
            raw,
            timestamps,
            quiet,
            template,
            color_mode,
            color_scheme,
            no_color,
        }
    }

    pub fn format_log_event(&self, event: &LogEvent) -> String {
        if let Some(ref tmpl) = self.template {
            self.format_with_template(event, tmpl)
        } else if self.raw {
            self.format_raw(event)
        } else {
            self.format_default(event)
        }
    }

    fn format_raw(&self, event: &LogEvent) -> String {
        let mut output = String::new();

        if self.timestamps {
            if let Some(ts) = &event.timestamp {
                output.push_str(&format!("{} ", ts.to_rfc3339()));
            }
        }

        output.push_str(&event.message);
        output
    }

    fn format_default(&self, event: &LogEvent) -> String {
        let should_colorize = match self.color_mode.as_str() {
            "always" => true,
            "never" => false,
            "auto" => atty::is(atty::Stream::Stdout),
            _ => true,
        };

        let pod_name = event.pod.metadata.name.as_ref()
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let workload_name = self.resolve_workload_name(event);

        let mut output = String::new();

        if should_colorize && !self.no_color {
            output.push_str(&format!(
                "{}:{} ",
                pod_name.cyan(),
                workload_name.yellow()
            ));
        } else {
            output.push_str(&format!("{}:{} ", pod_name, workload_name));
        }

        if self.timestamps {
            if let Some(ts) = &event.timestamp {
                output.push_str(&format!("{} ", ts.to_rfc3339()));
            }
        }

        if should_colorize && !self.no_color {
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&event.message) {
                output.push_str(&self.colorize_json(&json_val, None));
            } else {
                output.push_str(&event.message);
            }
        } else {
            output.push_str(&event.message);
        }

        output
    }

    fn resolve_workload_name(&self, event: &LogEvent) -> String {
        if let Some(owner_refs) = &event.pod.metadata.owner_references {
            for owner in owner_refs {
                match owner.kind.as_str() {
                    // Deployment pods are owned by ReplicaSets. Strip the RS hash suffix.
                    "ReplicaSet" => {
                        if let Some(deployment_name) = Self::strip_hash_suffix(&owner.name) {
                            return deployment_name;
                        }
                        return owner.name.clone();
                    }
                    "StatefulSet" | "DaemonSet" | "Job" | "CronJob" => {
                        return owner.name.clone();
                    }
                    _ => {}
                }
            }
        }

        event.container.name.clone()
    }

    fn strip_hash_suffix(name: &str) -> Option<String> {
        let (prefix, suffix) = name.rsplit_once('-')?;
        if suffix.len() < 9 || suffix.len() > 10 {
            return None;
        }
        if suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(prefix.to_string())
        } else {
            None
        }
    }

    fn colorize_json(&self, value: &serde_json::Value, parent_key: Option<&str>) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let items: Vec<String> = map
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "\"{}\":{}",
                            k.cyan(),
                            self.colorize_json(v, Some(k))
                        )
                    })
                    .collect();
                format!("{{{}}}", items.join(","))
            }
            serde_json::Value::Array(arr) => {
                let items: Vec<String> = arr
                    .iter()
                    .map(|v| self.colorize_json(v, parent_key))
                    .collect();
                format!("[{}]", items.join(","))
            }
            serde_json::Value::String(s) => self.colorize_json_string(s, parent_key),
            serde_json::Value::Number(n) => self.colorize_json_number(n, parent_key),
            serde_json::Value::Bool(b) => b.to_string().magenta().to_string(),
            serde_json::Value::Null => "null".bright_black().to_string(),
        }
    }

    fn colorize_json_string(&self, value: &str, parent_key: Option<&str>) -> String {
        if Self::is_http_status_key(parent_key) {
            if let Ok(status) = value.parse::<u16>() {
                return format!("\"{}\"", self.colorize_http_status(status));
            }
        }

        format!("\"{}\"", value.green())
    }

    fn colorize_json_number(&self, value: &serde_json::Number, parent_key: Option<&str>) -> String {
        if Self::is_http_status_key(parent_key) {
            if let Some(status) = value.as_u64().and_then(|v| u16::try_from(v).ok()) {
                return self.colorize_http_status(status);
            }
        }

        value.to_string().blue().to_string()
    }

    fn is_http_status_key(parent_key: Option<&str>) -> bool {
        matches!(
            parent_key.map(|key| key.to_ascii_lowercase()),
            Some(key)
                if key == "status"
                    || key == "status_code"
                    || key == "statuscode"
                    || key == "http_status"
                    || key == "httpstatus"
                    || key == "http_status_code"
                    || key == "response_status"
        )
    }

    fn colorize_http_status(&self, status: u16) -> String {
        match status {
            100..=199 => status.to_string().cyan().to_string(),
            200..=299 => status.to_string().green().bold().to_string(),
            300..=399 => status.to_string().yellow().to_string(),
            400..=499 => status.to_string().bright_red().to_string(),
            500..=599 => status.to_string().red().bold().to_string(),
            _ => status.to_string().blue().to_string(),
        }
    }

    fn format_with_template(&self, event: &LogEvent, template: &str) -> String {
        let mut data: HashMap<String, String> = HashMap::new();

        data.insert(
            "Pod".to_string(),
            event.pod.metadata.name.as_ref()
                .map(|s| s.clone())
                .unwrap_or_default(),
        );
        data.insert(
            "Container".to_string(),
            event.container.name.clone(),
        );
        data.insert(
            "Message".to_string(),
            event.message.clone(),
        );
        if let Some(ts) = &event.timestamp {
            data.insert("Timestamp".to_string(), ts.to_rfc3339());
        }

        // Simple template substitution (a real implementation would use a proper template engine)
        let mut result = template.to_string();
        for (key, value) in data {
            result = result.replace(&format!("{{{{{}}}}}", key), &value);
        }

        result
    }

    pub fn print_event_notification(&self, pod_name: &str, container_name: &str, what: &str) {
        if !self.quiet {
            eprintln!(
                "{} container {} in pod {}",
                what.yellow(),
                container_name.blue(),
                pod_name.cyan()
            );
        }
    }

    pub fn print_error(&self, pod_name: &str, container_name: &str, error: &str) {
        eprintln!(
            "{} Error tailing {}/{}: {}",
            "[ERROR]".red(),
            pod_name.cyan(),
            container_name.blue(),
            error.red()
        );
    }
}
