# ktail-rs - Rust Port of ktail

This is a Rust port of the [ktail](https://github.com/atombender/ktail) Kubernetes log tailing tool.

## Overview

`ktail` is a tool to tail Kubernetes containers with enhanced features compared to `kubectl logs`:

- **Auto-detection**: Detects pods and containers as they come and go
- **Multi-container tail**: Tails multiple pods and containers in multiple namespaces simultaneously
- **Better recovery**: Automatically retries on failure
- **Enhanced formatting**: Colored output with JSON syntax highlighting
- **Pattern matching**: Regex-based pod/container name matching
- **Label selectors**: Filter by Kubernetes labels
- **Customizable templates**: Format output with Go-like templates

## Conversion Notes

This Rust port provides feature parity with the original Go implementation while leveraging Rust's performance and safety characteristics.

### Key Architectural Differences

1. **K8s Client**: Uses `kube-rs` instead of `client-go`
2. **CLI Parsing**: Uses `clap` instead of `pflag`
3. **Configuration**: Uses `serde_yaml` for YAML parsing
4. **Async Runtime**: Built on `tokio` for async/await concurrency
5. **Error Handling**: Uses `anyhow::Result` for ergonomic error handling
6. **Matcher Pattern**: Implements trait-based matching system for flexible pod/container filtering

### Module Structure

- `config.rs` - Configuration file loading and management
- `matcher.rs` - Pod/container matching logic (regex, label selectors, boolean combinations)
- `tailer.rs` - Individual container log tailing with backoff retry
- `controller.rs` - Kubernetes watch controller for pod/container lifecycle management
- `output.rs` - Log formatting, colorization, and output handling
- `main.rs` - CLI interface and application entry point

## Dependencies

| Rust Crate | Go Package | Purpose |
|---|---|---|
| `kube` | `k8s.io/client-go` | Kubernetes client |
| `clap` | `spf13/pflag` | CLI argument parsing |
| `serde_yaml` | `gopkg.in/yaml.v2` | YAML configuration |
| `tokio` | N/A | Async runtime |
| `futures` | N/A | Async utilities |
| `colored` | `fatih/color` | Terminal colors |
| `syntect` | `alecthomas/chroma` | Syntax highlighting |
| `regex` | `regexp` | Pattern matching |
| `backoff` | `jpillora/backoff` | Exponential backoff |
| `chrono` | `time` | Time handling |
| `sha2` | `crypto/sha256` | Line deduplication |

## Building

```bash
cargo build --release
```

## Usage

Similar to the original ktail:

```bash
# Tail all containers matching pattern
ktail foo

# Tail with regex
ktail '^frontend'

# Tail with label selector
ktail -l app=myapp

# Use modern color theme
ktail --color-scheme modern

# All options
ktail --help
```

## Configuration

Configuration file at `$HOME/.config/ktail/config.yml`:

```yaml
quiet: false
noColor: false
raw: false
timestamps: false
colorScheme: modern
colorMode: auto
kubeConfigPath: ""
templateString: ""
```

Supported color schemes:

- `modern` (default): semantic highlighting for severity, status, HTTP method, and JSON keys
- `bw`: classic/basic colors

## Development

```bash
cargo test
cargo clippy
cargo fmt
```

## License

Same as the original ktail project - See LICENSE file in original repo.
