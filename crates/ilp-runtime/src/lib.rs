//! ILP WASM 插件运行时的稳定边界。
//!
//! 业务编排位于 `ilp-core`；本 crate 负责插件身份、能力策略和运行限制，
//! 具体 Wasmtime 适配可以在不改变上层 DTO 的情况下演进。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use wasmtime::component::Component;
use wasmtime::{Config as WasmtimeConfig, Engine, Store, StoreLimits, StoreLimitsBuilder};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_memory_bytes: u64,
    pub max_response_bytes: u64,
    pub max_requests: u32,
    pub timeout_ms: u64,
    pub fuel: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            max_response_bytes: 16 * 1024 * 1024,
            max_requests: 100,
            timeout_ms: 30_000,
            fuel: 1_000_000_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityPolicy {
    pub allowed_domains: BTreeSet<String>,
    pub allow_storage: bool,
    pub limits: ResourceLimits,
}

impl CapabilityPolicy {
    pub fn deny_all() -> Self {
        Self {
            allowed_domains: BTreeSet::new(),
            allow_storage: false,
            limits: ResourceLimits::default(),
        }
    }

    pub fn allow_domain(mut self, domain: impl Into<String>) -> Self {
        self.allowed_domains.insert(domain.into());
        self
    }

    pub fn check_domain(&self, domain: &str) -> Result<()> {
        if self.allowed_domains.contains(domain) {
            Ok(())
        } else {
            Err(anyhow!("插件无权访问域名: {domain}"))
        }
    }

    pub fn check_url(&self, url: &str) -> Result<()> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| anyhow!("网络地址缺少 scheme"))?;
        if scheme != "https" {
            return Err(anyhow!("插件网络请求必须使用 HTTPS"));
        }
        let host = rest
            .split('/')
            .next()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default();
        self.check_domain(host)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginTrust {
    pub content_hash: String,
    pub signature_key_id: Option<String>,
    pub source: String,
    pub user_confirmed: bool,
}

pub const CURRENT_ABI: &str = "0.2.0";
pub const SUPPORTED_ABIS: &[&str] = &["0.1.0", CURRENT_ABI];

pub fn validate_permissions(
    requested: &[String],
    granted: &std::collections::BTreeSet<String>,
) -> std::result::Result<(), RuntimeError> {
    if let Some(permission) = requested
        .iter()
        .find(|permission| !granted.contains(*permission))
    {
        return Err(RuntimeError::PermissionDenied(format!(
            "插件请求未授权能力: {permission}"
        )));
    }
    Ok(())
}

pub struct RuntimeStore {
    pub limits: StoreLimits,
    pub started: Instant,
    pub timeout: Duration,
    pub cancelled: bool,
}

pub fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = WasmtimeConfig::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        Engine::new(&config).expect("创建 Wasmtime engine 失败")
    })
}

pub fn new_store(policy: &CapabilityPolicy) -> Result<Store<RuntimeStore>> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(policy.limits.max_memory_bytes as usize)
        .instances(1)
        .tables(16)
        .build();
    let mut store = Store::new(
        shared_engine(),
        RuntimeStore {
            limits,
            started: Instant::now(),
            timeout: Duration::from_millis(policy.limits.timeout_ms),
            cancelled: false,
        },
    );
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(policy.limits.fuel)
        .map_err(|error| anyhow!("设置插件 fuel 失败: {error}"))?;
    Ok(store)
}

pub struct HostCapabilities {
    pub allow_http: bool,
    pub allow_storage: bool,
    pub allow_secrets: bool,
}

impl HostCapabilities {
    pub fn from_policy(policy: &CapabilityPolicy) -> Self {
        Self {
            allow_http: !policy.allowed_domains.is_empty(),
            allow_storage: policy.allow_storage,
            allow_secrets: false,
        }
    }

    pub fn can_use(&self, permission: &str) -> bool {
        match permission {
            "http" => self.allow_http,
            "storage-read" | "storage-write" => self.allow_storage,
            "secret-read" | "secret-write" => self.allow_secrets,
            "clock" => true,
            _ => false,
        }
    }
}

pub fn load_component(wasm: &[u8]) -> std::result::Result<Component, RuntimeError> {
    validate_component(wasm)?;
    Component::from_binary(shared_engine(), wasm)
        .map_err(|error| RuntimeError::InvalidFormat(format!("WASM component 加载失败: {error}")))
}

pub fn validate_component(wasm: &[u8]) -> std::result::Result<(), RuntimeError> {
    if wasm.len() < 8 || &wasm[0..4] != b"\\0asm" {
        return Err(RuntimeError::InvalidFormat(
            "插件不是有效的 WASM 文件".into(),
        ));
    }
    Component::from_binary(shared_engine(), wasm)
        .map(|_| ())
        .map_err(|error| RuntimeError::InvalidFormat(format!("WASM component 无效: {error}")))
}

pub fn cancel(store: &mut Store<RuntimeStore>) {
    store.data_mut().cancelled = true;
}

pub fn reset_execution_budget(
    store: &mut Store<RuntimeStore>,
    policy: &CapabilityPolicy,
) -> Result<()> {
    store.data_mut().started = Instant::now();
    store.data_mut().timeout = Duration::from_millis(policy.limits.timeout_ms);
    store.data_mut().cancelled = false;
    store
        .set_fuel(policy.limits.fuel)
        .map_err(|error| anyhow!("重置插件 fuel 失败: {error}"))?;
    Ok(())
}

pub fn check_execution_budget(
    store: &Store<RuntimeStore>,
) -> std::result::Result<(), RuntimeError> {
    if store.data().cancelled {
        return Err(RuntimeError::ExecutionFailed("插件执行已取消".into()));
    }
    if store.data().started.elapsed() > store.data().timeout {
        return Err(RuntimeError::ResourceLimit("插件执行超时".into()));
    }
    if store.get_fuel().unwrap_or(0) == 0 {
        return Err(RuntimeError::ResourceLimit("插件 fuel 已耗尽".into()));
    }
    Ok(())
}

pub trait PluginRuntime: Send + Sync {
    fn load(&self, wasm: &[u8], policy: &CapabilityPolicy) -> Result<PluginTrust>;
}

pub fn validate_abi(abi: &str) -> std::result::Result<(), RuntimeError> {
    if SUPPORTED_ABIS.contains(&abi) {
        Ok(())
    } else {
        Err(RuntimeError::AbiMismatch(format!(
            "不支持插件 ABI {abi}，支持版本: {}",
            SUPPORTED_ABIS.join(", ")
        )))
    }
}

pub fn validate_metadata(
    metadata: &ilp_types::PluginMetadata,
) -> std::result::Result<(), RuntimeError> {
    validate_abi(&metadata.abi_version)?;
    if metadata.plugin_id.trim().is_empty() || metadata.site_id.trim().is_empty() {
        return Err(RuntimeError::InvalidFormat(
            "插件 ID 和站点 ID 不能为空".into(),
        ));
    }
    for domain in &metadata.allowed_domains {
        if domain.trim().is_empty() || domain.contains('/') || domain.contains(' ') {
            return Err(RuntimeError::InvalidFormat(format!(
                "非法允许域名: {domain}"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginErrorCode {
    InvalidFormat,
    AbiMismatch,
    SignatureRejected,
    PermissionDenied,
    ResourceLimit,
    ExecutionFailed,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("插件格式无效: {0}")]
    InvalidFormat(String),
    #[error("插件 ABI 不兼容: {0}")]
    AbiMismatch(String),
    #[error("插件签名被拒绝: {0}")]
    SignatureRejected(String),
    #[error("插件权限不足: {0}")]
    PermissionDenied(String),
    #[error("插件资源超限: {0}")]
    ResourceLimit(String),
    #[error("插件运行失败: {0}")]
    ExecutionFailed(String),
}

impl RuntimeError {
    pub fn code(&self) -> PluginErrorCode {
        match self {
            Self::InvalidFormat(_) => PluginErrorCode::InvalidFormat,
            Self::AbiMismatch(_) => PluginErrorCode::AbiMismatch,
            Self::SignatureRejected(_) => PluginErrorCode::SignatureRejected,
            Self::PermissionDenied(_) => PluginErrorCode::PermissionDenied,
            Self::ResourceLimit(_) => PluginErrorCode::ResourceLimit,
            Self::ExecutionFailed(_) => PluginErrorCode::ExecutionFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn default_policy_denies_network_and_storage() {
        let policy = CapabilityPolicy::deny_all();
        assert!(!policy.allow_storage);
        assert!(policy.check_domain("example.com").is_err());
    }

    #[test]
    fn policy_can_allow_an_explicit_domain() {
        let policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        assert!(policy.check_domain("example.com").is_ok());
        assert!(policy.check_domain("other.example.com").is_err());
        assert!(policy.check_url("http://example.com").is_err());
        assert!(policy.check_url("https://example.com/path").is_ok());
    }

    #[test]
    fn abi_validation_accepts_legacy_and_current_versions() {
        assert!(validate_abi("0.1.0").is_ok());
        assert!(validate_abi(CURRENT_ABI).is_ok());
        assert!(validate_abi("9.9.9").is_err());
    }

    #[test]
    fn capability_mapping_is_default_deny() {
        let capabilities = HostCapabilities::from_policy(&CapabilityPolicy::deny_all());
        assert!(!capabilities.can_use("http"));
        assert!(!capabilities.can_use("storage-read"));
        assert!(!capabilities.can_use("secret-read"));
        assert!(capabilities.can_use("clock"));
    }

    #[test]
    fn invalid_component_is_rejected() {
        assert!(validate_component(b"not wasm").is_err());
    }

    #[test]
    fn bounded_wasmtime_store_has_fuel_and_memory_limit() {
        let policy = CapabilityPolicy::deny_all();
        let store = new_store(&policy).unwrap();
        assert!(store.get_fuel().unwrap() > 0);
        assert!(check_execution_budget(&store).is_ok());
    }

    #[test]
    fn permissions_are_default_deny() {
        let requested = vec!["http".to_string()];
        assert!(validate_permissions(&requested, &BTreeSet::new()).is_err());
        let granted = ["http".to_string()].into_iter().collect();
        assert!(validate_permissions(&requested, &granted).is_ok());
    }

    #[test]
    fn metadata_validation_rejects_invalid_identity_and_domains() {
        let metadata = ilp_types::PluginMetadata {
            abi_version: CURRENT_ABI.into(),
            site_name: "site".into(),
            site_id: "site".into(),
            site_domain: "example.com".into(),
            plugin_name: "plugin".into(),
            plugin_id: "plugin".into(),
            version: "0.1.0".into(),
            permissions: vec![],
            allowed_domains: vec!["https://example.com".into()],
        };
        assert!(validate_metadata(&metadata).is_err());
    }
}
