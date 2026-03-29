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
        let container_name = &event.container.name;

        let mut output = String::new();

        if should_colorize && !self.no_color {
            output.push_str(&format!(
                "[{}] [{}] ",
                pod_name.cyan(),
                container_name.yellow()
            ));
        } else {
            output.push_str(&format!("[{}] [{}] ", pod_name, container_name));
        }

        if self.timestamps {
            if let Some(ts) = &event.timestamp {
                output.push_str(&format!("{} ", ts.to_rfc3339()));
            }
        }

        // Try to colorize JSON if present
        if should_colorize && !self.no_color {
            if event.message.starts_with('{') || event.message.starts_with('[') {
                if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&event.message) {
                    output.push_str(&self.colorize_json(&json_val));
                } else {
                    output.push_str(&event.message);
                }
            } else {
                output.push_str(&event.message);
            }
        } else {
            output.push_str(&event.message);
        }

        output
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

    fn colorize_json(&self, value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::Object(map) => {
                let items: Vec<String> = map
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}: {}",
                            k.cyan(),
                            self.colorize_json_value(v)
                        )
                    })
                    .collect();
                format!("{{ {} }}", items.join(", "))
            }
            serde_json::Value::Array(arr) => {
                let items: Vec<String> = arr
                    .iter()
                    .map(|v| self.colorize_json_value(v))
                    .collect();
                format!("[ {} ]", items.join(", "))
            }
            other => self.colorize_json_value(other),
        }
    }

    fn colorize_json_value(&self, value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(s) => format!("\"{}\"", s).green().to_string(),
            serde_json::Value::Number(n) => format!("{}", n).blue().to_string(),
            serde_json::Value::Bool(b) => format!("{}", b).cyan().to_string(),
            serde_json::Value::Null => "null".bright_black().to_string(),
            _ => value.to_string(),
        }
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
