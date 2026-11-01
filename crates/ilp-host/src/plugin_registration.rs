//! 本地插件到 Core trusted registration 的宿主适配边界。
//!
//! 这个模块只接收已经在本地取得的 metadata 和 WASM 字节，不执行下载、安装、
//! 签名或密钥发现。调用方必须显式提供信任输入；缺少输入时传入 `None` 会被
//! 拒绝，不能通过该适配器把 metadata-only inspection 当成 trusted registration。

use anyhow::{anyhow, Result};
use ilp_core::{PluginRegistration, PluginRegistrationRequest, PluginService};
use ilp_runtime::PluginTrust;
use ilp_types::PluginMetadata;
use sha2::{Digest, Sha256};

/// 由宿主上层明确取得的本地插件信任输入。
///
/// `content_hash` 是调用方对本地 WASM 字节的预期 SHA-256；适配器会重新计算
/// 字节并逐项比对，Core 收到的 request 使用重新计算后的规范化摘要。`source`
/// 必须由调用方明确提供，适配器不会从文件路径或 registry 自动推断。`None`
/// 的 `signature_key_id` 表示调用方明确知道没有可用的签名密钥 ID，不表示适配器
/// 验证过签名。最终的 metadata、能力策略和 trust 字段校验仍由 Core 负责。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPluginTrustInputs {
    pub content_hash: String,
    pub source: String,
    pub signature_key_id: Option<String>,
    pub user_confirmed: bool,
}

/// 为一个已经存在于本地的插件构造 Core registration request。
///
/// 该函数不读取文件、不访问网络，也不生成签名或信任来源。`metadata` 应来自
/// 已加载的本地插件，`wasm` 必须是同一个本地产物的原始字节；`trust_inputs`
/// 为 `None` 时明确拒绝，保持 metadata-only 路径 default-deny。
pub fn build_local_registration_request(
    metadata: PluginMetadata,
    wasm: &[u8],
    trust_inputs: Option<&LocalPluginTrustInputs>,
) -> Result<PluginRegistrationRequest> {
    let Some(trust_inputs) = trust_inputs else {
        return Err(anyhow!(
            "无法注册本地插件：缺少明确的信任输入（需要用户确认、SHA-256 和来源）"
        ));
    };
    if !trust_inputs.user_confirmed {
        return Err(anyhow!("无法注册本地插件：用户确认必须明确为 true"));
    }
    if trust_inputs.source.trim().is_empty() {
        return Err(anyhow!("无法注册本地插件：来源必须由调用方明确提供"));
    }

    let actual_hash = format!("{:x}", Sha256::digest(wasm));
    if !trust_inputs
        .content_hash
        .trim()
        .eq_ignore_ascii_case(&actual_hash)
    {
        return Err(anyhow!(
            "无法注册本地插件：提供的 SHA-256 与本地产物不匹配（实得 {actual_hash}）"
        ));
    }

    Ok(PluginRegistrationRequest {
        metadata,
        trust: PluginTrust {
            content_hash: actual_hash,
            signature_key_id: trust_inputs.signature_key_id.clone(),
            source: trust_inputs.source.clone(),
            user_confirmed: true,
        },
    })
}

/// 将已有本地插件注册到 Core。
///
/// Core 的 [`PluginService`] 是唯一的 registration authority；适配器不会直接
/// 写存储，也不会绕过 Core 的 capability policy。没有可靠 trust inputs 时请传
/// `None`，函数会在调用 Core 前拒绝，而不是自动把本地 metadata 视为可信。
pub fn register_local_plugin<S: PluginService + ?Sized>(
    service: &S,
    metadata: PluginMetadata,
    wasm: &[u8],
    trust_inputs: Option<&LocalPluginTrustInputs>,
) -> Result<PluginRegistration> {
    let request = build_local_registration_request(metadata, wasm, trust_inputs)?;
    service
        .register(&request)
        .map_err(|error| anyhow!("Core 拒绝插件注册: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilp_core::CoreError;
    use ilp_runtime::{CapabilityPolicy, ResourceLimits};

    fn metadata() -> PluginMetadata {
        PluginMetadata {
            abi_version: ilp_runtime::CURRENT_ABI.into(),
            site_name: "示例站".into(),
            site_id: "example".into(),
            site_domain: "example.com".into(),
            plugin_name: "example-plugin".into(),
            plugin_id: "plugin-example".into(),
            version: "1.0.0".into(),
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
        }
    }

    fn trust(wasm: &[u8]) -> LocalPluginTrustInputs {
        LocalPluginTrustInputs {
            content_hash: format!("{:x}", Sha256::digest(wasm)),
            source: "local:/plugins/example.wasm".into(),
            signature_key_id: None,
            user_confirmed: true,
        }
    }

    struct RecordingService {
        request: std::cell::RefCell<Option<PluginRegistrationRequest>>,
        policy: CapabilityPolicy,
        limits: ResourceLimits,
    }

    impl RecordingService {
        fn new() -> Self {
            Self {
                request: std::cell::RefCell::new(None),
                policy: CapabilityPolicy::deny_all(),
                limits: ResourceLimits::default(),
            }
        }
    }

    impl PluginService for RecordingService {
        fn policy(&self) -> &CapabilityPolicy {
            &self.policy
        }

        fn resource_limits(&self) -> &ResourceLimits {
            &self.limits
        }

        fn register(
            &self,
            request: &PluginRegistrationRequest,
        ) -> std::result::Result<PluginRegistration, CoreError> {
            self.request.replace(Some(request.clone()));
            Ok(PluginRegistration {
                metadata: request.metadata.clone(),
                trust: request.trust.clone(),
            })
        }
    }

    #[test]
    fn builds_request_from_local_bytes_and_preserves_explicit_unsigned_boundary() {
        let wasm = b"local wasm bytes";
        let request = build_local_registration_request(metadata(), wasm, Some(&trust(wasm)))
            .expect("匹配的本地产物应构造 request");

        assert_eq!(
            request.trust.content_hash,
            format!("{:x}", Sha256::digest(wasm))
        );
        assert_eq!(request.trust.source, "local:/plugins/example.wasm");
        assert_eq!(request.trust.signature_key_id, None);
        assert!(request.trust.user_confirmed);
    }

    #[test]
    fn rejects_registration_without_explicit_trust_inputs() {
        let error = build_local_registration_request(metadata(), b"local wasm bytes", None)
            .expect_err("没有 trust inputs 必须拒绝");
        assert!(error.to_string().contains("缺少明确的信任输入"));
    }

    #[test]
    fn rejects_unconfirmed_or_mismatched_local_artifact() {
        let wasm = b"local wasm bytes";
        let mut unconfirmed = trust(wasm);
        unconfirmed.user_confirmed = false;
        assert!(
            build_local_registration_request(metadata(), wasm, Some(&unconfirmed))
                .expect_err("未确认必须拒绝")
                .to_string()
                .contains("用户确认")
        );

        let mut wrong_hash = trust(wasm);
        wrong_hash.content_hash = "a".repeat(64);
        let error =
            build_local_registration_request(metadata(), b"different bytes", Some(&wrong_hash))
                .expect_err("摘要不匹配必须拒绝");
        assert!(error.to_string().contains("SHA-256") && error.to_string().contains("不匹配"));
    }

    #[test]
    fn delegates_registration_to_core_service_without_fabricating_signature() {
        let service = RecordingService::new();
        let wasm = b"local wasm bytes";
        let registered = register_local_plugin(&service, metadata(), wasm, Some(&trust(wasm)))
            .expect("Core service 应收到本地 registration");
        assert_eq!(registered.trust.signature_key_id, None);
        assert_eq!(
            service
                .request
                .borrow()
                .as_ref()
                .expect("应记录 request")
                .trust
                .signature_key_id,
            None
        );
    }
}
