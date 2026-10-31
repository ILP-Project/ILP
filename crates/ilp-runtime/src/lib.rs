//! ILP WASM 插件运行时的稳定边界。
//!
//! 业务编排位于 `ilp-core`；本 crate 负责插件身份、能力策略和运行限制，
//! 具体 Wasmtime 适配可以在不改变上层 DTO 的情况下演进。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceLimits {
    pub max_memory_bytes: u64,
    pub max_response_bytes: u64,
    pub max_requests: u32,
    pub timeout_ms: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            max_response_bytes: 16 * 1024 * 1024,
            max_requests: 100,
            timeout_ms: 30_000,
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
    }

    #[test]
    fn abi_validation_accepts_legacy_and_current_versions() {
        assert!(validate_abi("0.1.0").is_ok());
        assert!(validate_abi(CURRENT_ABI).is_ok());
        assert!(validate_abi("9.9.9").is_err());
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
