use anyhow::Result;
use chrono::Duration;
use clap::Parser;
use k8s_openapi::api::core::v1::{Container, Pod};
use kube::Client;
use ktail::*;
use ktail::matcher::{Matcher, RegexMatcher, LabelSelectorMatcher, OrMatcher};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Parser, Debug)]
#[command(
    name = "ktail",
    about = "Tail Kubernetes containers",
    long_about = "A tool to tail Kubernetes containers with enhanced features"
)]
struct Args {
    /// Pod/container name patterns (regex)
    #[arg(value_name = "PATTERN")]
    patterns: Vec<String>,

    /// Kubernetes context name
    #[arg(long)]
    context: Option<String>,

    /// Kubernetes namespace (can be repeated)
    #[arg(short, long)]
    namespace: Vec<String>,

    /// Apply to all Kubernetes namespaces
    #[arg(long)]
    all_namespaces: bool,

    /// Exclude using a regular expression (can be repeated)
    #[arg(short, long)]
    exclude: Vec<String>,

    /// Match pods by label selector (can be repeated)
    #[arg(short = 'l', long, action = clap::ArgAction::Append)]
    selector: Vec<String>,

    /// Start reading log from the beginning
    #[arg(long)]
    since_start: bool,

    /// Get logs since a given time or duration (e.g., 2023-03-30 or 1h)
    #[arg(short = 'S', long)]
    since: Option<String>,

    /// Show version
    #[arg(long)]
    version: bool,

    /// Path to kubeconfig file
    #[arg(long)]
    kubeconfig: Option<String>,

    /// Template to format each line
    #[arg(short, long)]
    template: Option<String>,

    /// Don't format output; output messages only
    #[arg(short, long)]
    raw: bool,

    /// Include timestamps on each line
    #[arg(short = 'T', long)]
    timestamps: bool,

    /// Don't print events about new/deleted pods
    #[arg(short, long)]
    quiet: bool,

    /// Don't use color in output
    #[arg(long)]
    no_color: bool,

    /// Set color mode: auto, never, always
    #[arg(long, default_value = "auto")]
    color: String,

    /// Set color scheme
    #[arg(long, default_value = "bw")]
    color_scheme: String,
}

const VERSION: &str = "0.1.0";

#[tokio::main]
async fn main() -> Result<()> {
    use chrono::Utc;

    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    let args = Args::parse();

    if args.version {
        println!("ktail version {}", VERSION);
        return Ok(());
    }

    // Load configuration
    let config = Config::load_default()?;

    // Create Kubernetes client
    let client = Client::try_default().await?;

    // Determine namespaces
    let namespaces = if args.all_namespaces {
        vec!["*".to_string()]
    } else if !args.namespace.is_empty() {
        args.namespace.clone()
    } else if !config.kube_config_path.is_empty() {
        // Try to get current namespace from kubeconfig
        vec!["default".to_string()]
    } else {
        vec!["default".to_string()]
    };

    // Build inclusion matcher
    let inclusion_matcher: Arc<dyn Matcher> = if !args.patterns.is_empty() {
        // Combine multiple patterns with OR
        let mut matchers: Vec<Box<dyn Matcher>> = Vec::new();
        for pattern in &args.patterns {
            matchers.push(Box::new(RegexMatcher::new(pattern)?));
        }
        Arc::new(OrMatcher(matchers))
    } else if !args.selector.is_empty() {
        // Parse label selectors (combine multiple with OR)
        let mut matchers: Vec<Box<dyn Matcher>> = Vec::new();
        for selector in &args.selector {
            let labels = parse_label_selector(selector)?;
            matchers.push(Box::new(LabelSelectorMatcher::new(labels)));
        }
        Arc::new(OrMatcher(matchers))
    } else {
        // Match all if no patterns specified
        Arc::new(RegexMatcher::new(".*")?)
    };

    // Build exclusion matcher
    let exclusion_matcher: Arc<dyn Matcher> = if !args.exclude.is_empty() {
        let mut matchers: Vec<Box<dyn Matcher>> = Vec::new();
        for pattern in &args.exclude {
            matchers.push(Box::new(RegexMatcher::new(pattern)?));
        }
        Arc::new(OrMatcher(matchers))
    } else {
        // Match nothing if no excludes specified
        Arc::new(RegexMatcher::new(r"\b\B")?)
    };

    // Parse since duration
    let since_ts = if args.since_start {
        None
    } else if let Some(since_str) = &args.since {
        parse_since(since_str)?
    } else {
        Some(Utc::now())
    };

    // Create output formatter
    let formatter = OutputFormatter::new(
        args.raw,
        args.timestamps,
        args.quiet,
        args.template.clone(),
        args.color.clone(),
        args.color_scheme.clone(),
        args.no_color,
    );

    // Create controller options
    let options = ControllerOptions {
        namespaces: namespaces.clone(),
        inclusion_matcher: inclusion_matcher.clone(),
        exclusion_matcher: exclusion_matcher.clone(),
        since_start: args.since_start,
        since: since_ts,
    };

    // Create callbacks
    let formatter_clone = std::sync::Arc::new(formatter);
    let callbacks = Callbacks {
        on_event: Arc::new({
            let fmt = formatter_clone.clone();
            move |event: LogEvent| {
                println!("{}", fmt.format_log_event(&event));
            }
        }),
        on_enter: Arc::new({
            let fmt = formatter_clone.clone();
            move |pod: &Arc<Pod>, container: &Arc<Container>, initial: bool| {
                let pod_name = pod.metadata.name.as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("unknown");
                let action = if initial { "Discovered" } else { "Started" };
                fmt.print_event_notification(pod_name, &container.name, action);
                true
            }
        }),
        on_exit: Arc::new({
            let fmt = formatter_clone.clone();
            move |pod: &Arc<Pod>, container: &Arc<Container>| {
                let pod_name = pod.metadata.name.as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("unknown");
                fmt.print_event_notification(pod_name, &container.name, "Stopped");
            }
        }),
        on_error: Arc::new({
            let fmt = formatter_clone.clone();
            move |pod: &Arc<Pod>, container: &Arc<Container>, error: &anyhow::Error| {
                let pod_name = pod.metadata.name.as_ref()
                    .map(|s| s.as_str())
                    .unwrap_or("unknown");
                fmt.print_error(pod_name, &container.name, &error.to_string());
            }
        }),
        on_nothing_discovered: Arc::new({
            move || {
                eprintln!("No matching pods or containers found");
            }
        }),
    };

    // Create and run controller
    let controller = std::sync::Arc::new(Controller::new(client, options, callbacks));

    // Setup Ctrl+C handler
    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        token_clone.cancel();
    });

    controller.run(token).await?;

    Ok(())
}

fn parse_label_selector(selector: &str) -> Result<HashMap<String, String>> {
    let mut labels = HashMap::new();
    for pair in selector.split(',') {
        let parts: Vec<&str> = pair.split('=').collect();
        if parts.len() == 2 {
            labels.insert(parts[0].trim().to_string(), parts[1].trim().to_string());
        } else {
            return Err(anyhow::anyhow!("Invalid label selector: {}", selector));
        }
    }
    Ok(labels)
}

fn parse_since(since_str: &str) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    use chrono::Utc;

    // Try parsing as duration (e.g., "1h", "30m")
    if let Some(duration) = parse_duration(since_str) {
        return Ok(Some(Utc::now() - duration));
    }

    // Try parsing as date (e.g., "2023-03-30")
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(&format!("{}T00:00:00Z", since_str)) {
        return Ok(Some(date.with_timezone(&Utc)));
    }

    Err(anyhow::anyhow!(
        "Cannot parse since value: {}. Use format like '1h' or '2023-03-30'",
        since_str
    ))
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.to_lowercase();

    if let Some(n) = s.strip_suffix("s") {
        if let Ok(v) = n.parse::<i64>() {
            return Some(Duration::seconds(v));
        }
    }

    if let Some(n) = s.strip_suffix("m") {
        if let Ok(v) = n.parse::<i64>() {
            return Some(Duration::minutes(v));
        }
    }

    if let Some(n) = s.strip_suffix("h") {
        if let Ok(v) = n.parse::<i64>() {
            return Some(Duration::hours(v));
        }
    }

    if let Some(n) = s.strip_suffix("d") {
        if let Ok(v) = n.parse::<i64>() {
            return Some(Duration::days(v));
        }
    }

    None
}
