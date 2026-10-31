use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

/// 配置项全部带默认值：缺失字段回退到默认，不因单个字段让程序崩溃。
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Config {
    #[serde(default = "default_data_folder")]
    pub data_folder: String,
    #[serde(default = "default_novels_folder")]
    pub novels_folder: String,
    #[serde(default = "default_logs_folder")]
    pub logs_folder: String,
    #[serde(default = "default_covers_folder")]
    pub covers_folder: String,
    #[serde(default = "default_plugins_folder")]
    pub plugins_folder: String,
    #[serde(default = "default_max_workers")]
    pub max_workers: usize,
    /// 相邻两次网络请求之间的最小间隔（秒）
    #[serde(default = "default_sleep_time")]
    pub sleep_time: u64,
    /// 日志级别：trace/debug/info/warn/error
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 是否把日志同时写到 stderr
    #[serde(default = "default_true")]
    pub log_to_stderr: bool,
    /// 显式代理地址；为空则沿用环境变量
    #[serde(default)]
    pub proxy: Option<String>,
    /// 不走代理的主机名
    #[serde(default = "default_no_proxy")]
    pub no_proxy: Vec<String>,
    /// 插件签名信任根：key_id -> base64 编码的 Ed25519 公钥。
    #[serde(default)]
    pub plugin_trusted_keys: HashMap<String, String>,
    /// 已撤销的签名 key_id；撤销优先于签名验证。
    #[serde(default)]
    pub plugin_revoked_keys: Vec<String>,
    /// 配置了信任根后，是否拒绝没有有效签名的插件。
    #[serde(default)]
    pub require_plugin_signatures: bool,
    /// 安装时要求多少个“不同密钥”的有效签名。默认 1。
    #[serde(default = "default_signature_threshold")]
    pub plugin_signature_threshold: usize,
    /// 是否要求 registry 元数据本身带有效签名。
    ///
    /// 默认开启；仅在使用明确不签名的内部镜像时关闭。
    #[serde(default = "default_true")]
    pub require_registry_signatures: bool,
    /// 原始文件内容，仅用于检查未知字段，不参与序列化
    #[serde(skip)]
    raw: String,
}

pub const OFFICIAL_SIGNING_KEY_ID: &str = "release-2026";
pub const OFFICIAL_SIGNING_PUBLIC_KEY: &str = "9OkAC+RgB0unksBeNi2GDG4HVfrIaT3QwfogV3YbKzk=";

const KNOWN_FIELDS: [&str; 16] = [
    "data_folder",
    "novels_folder",
    "logs_folder",
    "covers_folder",
    "plugins_folder",
    "max_workers",
    "sleep_time",
    "log_level",
    "log_to_stderr",
    "proxy",
    "no_proxy",
    "plugin_trusted_keys",
    "plugin_revoked_keys",
    "require_plugin_signatures",
    "plugin_signature_threshold",
    "require_registry_signatures",
];

fn default_signature_threshold() -> usize {
    1
}

fn default_data_folder() -> String {
    "data".into()
}
fn default_novels_folder() -> String {
    "novels".into()
}
fn default_logs_folder() -> String {
    "logs".into()
}
fn default_covers_folder() -> String {
    "covers".into()
}
fn default_plugins_folder() -> String {
    "plugins".into()
}
/// 单 worker 起步。实测并发一高就被番茄风控回验证码中间页，
/// 全书一次全废；宁慢勿废。
fn default_max_workers() -> usize {
    1
}
fn default_sleep_time() -> u64 {
    3
}
fn default_log_level() -> String {
    "info".into()
}
fn default_true() -> bool {
    true
}
fn default_no_proxy() -> Vec<String> {
    vec!["i.snssdk.com".into()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_folder: default_data_folder(),
            novels_folder: default_novels_folder(),
            logs_folder: default_logs_folder(),
            covers_folder: default_covers_folder(),
            plugins_folder: default_plugins_folder(),
            max_workers: default_max_workers(),
            sleep_time: default_sleep_time(),
            log_level: default_log_level(),
            log_to_stderr: default_true(),
            proxy: None,
            no_proxy: default_no_proxy(),
            plugin_trusted_keys: [(
                OFFICIAL_SIGNING_KEY_ID.to_string(),
                OFFICIAL_SIGNING_PUBLIC_KEY.to_string(),
            )]
            .into_iter()
            .collect(),
            plugin_revoked_keys: Vec::new(),
            require_plugin_signatures: false,
            plugin_signature_threshold: default_signature_threshold(),
            require_registry_signatures: true,
            raw: String::new(),
        }
    }
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let p = Path::new(path);
        if !p.exists() {
            let cfg = Config::default();
            std::fs::write(p, serde_json::to_string_pretty(&cfg)?)
                .with_context(|| format!("写入默认配置失败: {path}"))?;
            return Ok(cfg);
        }

        let text = std::fs::read_to_string(p).with_context(|| format!("读取配置失败: {path}"))?;
        let mut cfg: Config =
            serde_json::from_str(&text).with_context(|| format!("解析配置失败: {path}"))?;
        cfg.plugin_trusted_keys
            .entry(OFFICIAL_SIGNING_KEY_ID.to_string())
            .or_insert_with(|| OFFICIAL_SIGNING_PUBLIC_KEY.to_string());
        cfg.raw = text;
        Ok(cfg)
    }

    /// 配置出问题时用户能立刻看到，但程序继续跑。
    /// 需在日志初始化之后调用，否则告警无处可去。
    pub fn report_problems(&self) {
        self.warn_unknown_fields();
        self.warn_odd_values();
    }

    /// 拼错字段名比直接报错更难排查：只告警不中断。
    fn warn_unknown_fields(&self) {
        let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(&self.raw)
        else {
            return;
        };
        for key in map.keys() {
            if !KNOWN_FIELDS.contains(&key.as_str()) {
                warn!("配置中有未知字段 {}，已忽略", key);
            }
        }
    }

    /// 只告警、不改值：配置出问题时用户能立刻看到，但程序继续跑。
    fn warn_odd_values(&self) {
        if self.max_workers == 0 {
            warn!("max_workers 为 0，已按 1 处理");
        }
        if !matches!(
            self.log_level.to_ascii_lowercase().as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            warn!("未知的 log_level {}，回退到 info", self.log_level);
        }
    }

    pub fn effective_max_workers(&self) -> usize {
        self.max_workers.max(1)
    }

    pub fn novels_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join(&self.novels_folder)
    }

    pub fn covers_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join(&self.covers_folder)
    }

    pub fn logs_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join(&self.logs_folder)
    }

    pub fn plugins_dir(&self) -> PathBuf {
        Path::new(&self.plugins_folder).to_path_buf()
    }

    pub fn disabled_plugins_dir(&self) -> PathBuf {
        self.plugins_dir().join("disabled")
    }

    /// 插件私有 KV 存储的根目录，与 engine 里注入的 storage 路径保持一致。
    pub fn plugins_data_dir(&self) -> PathBuf {
        Path::new(&self.data_folder).join("plugins")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"max_workers": 3}"#).unwrap();
        assert_eq!(cfg.max_workers, 3);
        assert_eq!(cfg.sleep_time, default_sleep_time());
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.no_proxy.contains(&"i.snssdk.com".to_string()));
        assert!(cfg.require_registry_signatures);
    }

    #[test]
    fn empty_object_yields_all_defaults() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.data_folder, "data");
        assert_eq!(cfg.novels_dir(), Path::new("data/novels"));
        assert_eq!(cfg.covers_dir(), Path::new("data/covers"));
        assert_eq!(cfg.logs_dir(), Path::new("data/logs"));
        assert!(cfg.require_registry_signatures);
    }

    #[test]
    fn unknown_field_is_ignored_not_fatal() {
        let raw = r#"{"typo_field": 1, "max_workers": 2}"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.max_workers, 2);
        cfg.raw = raw.to_string();
        cfg.report_problems();
    }

    #[test]
    fn raw_snapshot_detects_unknown_keys() {
        let raw = r#"{"log_level": "info", "nope": true}"#;
        let mut cfg: Config = serde_json::from_str(raw).unwrap();
        cfg.raw = raw.to_string();
        let map: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&cfg.raw).unwrap();
        assert!(map.keys().any(|k| !KNOWN_FIELDS.contains(&k.as_str())));
    }

    #[test]
    fn zero_workers_is_clamped() {
        let cfg: Config = serde_json::from_str(r#"{"max_workers": 0}"#).unwrap();
        cfg.warn_odd_values();
        assert_eq!(cfg.effective_max_workers(), 1);
    }
}
