use anyhow::{Result, Context};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub quiet: bool,
    #[serde(default)]
    pub no_color: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub timestamps: bool,
    #[serde(default = "default_color_mode")]
    pub color_mode: String,
    #[serde(default = "default_color_scheme")]
    pub color_scheme: String,
    #[serde(default)]
    pub template_string: String,
    #[serde(default)]
    pub kube_config_path: String,
}

fn default_color_mode() -> String {
    "auto".to_string()
}

fn default_color_scheme() -> String {
    "modern".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            quiet: false,
            no_color: false,
            raw: false,
            timestamps: false,
            color_mode: "auto".to_string(),
            color_scheme: "modern".to_string(),
            template_string: String::new(),
            kube_config_path: String::new(),
        }
    }
}

impl Config {
    pub fn load_default() -> Result<Self> {
        let mut cfg = Self::default();
        
        if let Ok(home) = std::env::var("HOME") {
            let config_path = PathBuf::from(home)
                .join(".config")
                .join("ktail")
                .join("config.yml");
            
            if config_path.exists() {
                cfg.load_from_path(&config_path)?;
            }
        }
        
        Ok(cfg)
    }

    pub fn load_from_path(&mut self, path: &PathBuf) -> Result<()> {
        let data = std::fs::read_to_string(path)
            .context("Failed to read config file")?;
        
        let loaded: Config = serde_yaml::from_str(&data)
            .context("Failed to parse YAML config")?;
        
        *self = loaded;
        Ok(())
    }
}
