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

pub trait PluginRuntime: Send + Sync {
    fn load(&self, wasm: &[u8], policy: &CapabilityPolicy) -> Result<PluginTrust>;
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
}
