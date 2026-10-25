use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub data_folder: String,
    pub novels_folder: String,
    pub logs_folder: String,
    pub covers_folder: String,
    pub max_workers: usize,
    pub sleep_time: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_folder: "data".to_string(),
            novels_folder: "novels".to_string(),
            logs_folder: "logs".to_string(),
            covers_folder: "covers".to_string(),
            max_workers: 7,
            sleep_time: 5,
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let p = Path::new(path);
        if p.exists() {
            let text =
                std::fs::read_to_string(p).with_context(|| format!("读取配置失败: {path}"))?;
            serde_json::from_str(&text).with_context(|| format!("解析配置失败: {path}"))
        } else {
            let cfg = Config::default();
            std::fs::write(p, serde_json::to_string_pretty(&cfg)?)?;
            Ok(cfg)
        }
    }

    pub fn novels_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join(&self.novels_folder)
    }

    pub fn covers_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join(&self.covers_folder)
    }
}
