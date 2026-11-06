//! ILP WASM 插件运行时的稳定边界。
//!
//! 业务编排位于 `ilp-core`；本 crate 负责插件身份、能力策略和运行限制，
//! 具体 Wasmtime 适配可以在不改变上层 DTO 的情况下演进。

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

pub mod http;

use wasmtime::component::Component;
use wasmtime::{
    Config as WasmtimeConfig, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder,
    UpdateDeadline,
};

/// Wasmtime-side bindings for the current WIT ABI. Concrete host state is kept
/// outside these generated types so adapters can be supplied by CLI/App.
pub mod component_bindings {
    wasmtime::component::bindgen!({
        world: "novel-plugin",
        path: "../ilp-plugin-wit/wit/v2",
    });
}

pub trait HostAdapter: Send + Sync {
    fn log(&self, level: u8, message: &str);
    fn now_ms(&self) -> u64;
}

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
    pub allow_secrets: bool,
    pub limits: ResourceLimits,
}

impl CapabilityPolicy {
    pub fn deny_all() -> Self {
        Self {
            allowed_domains: BTreeSet::new(),
            allow_storage: false,
            allow_secrets: false,
            limits: ResourceLimits::default(),
        }
    }

    pub fn allow_domain(mut self, domain: impl Into<String>) -> Self {
        self.allowed_domains.insert(domain.into());
        self
    }

    pub fn check_domain(&self, domain: &str) -> Result<()> {
        let domain = domain.trim_end_matches('.').to_ascii_lowercase();
        if self.allowed_domains.iter().any(|allowed| {
            let allowed = allowed.trim().trim_end_matches('.').to_ascii_lowercase();
            allowed == "*"
                || allowed == domain
                || (allowed
                    .strip_prefix("*.")
                    .is_some_and(|suffix| domain.ends_with(&format!(".{suffix}"))))
        }) {
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
        let authority = rest.split('/').next().unwrap_or_default();
        if authority.is_empty()
            || authority.contains('@')
            || authority.chars().any(char::is_whitespace)
        {
            return Err(anyhow!("网络地址的 host 无效"));
        }
        let host = authority
            .strip_prefix('[')
            .and_then(|value| value.split_once(']').map(|(host, _)| host))
            .unwrap_or_else(|| {
                authority
                    .rsplit_once(':')
                    .map_or(authority, |(host, port)| {
                        if port.chars().all(|character| character.is_ascii_digit()) {
                            host
                        } else {
                            authority
                        }
                    })
            });
        if host.is_empty() {
            return Err(anyhow!("网络地址缺少 host"));
        }
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
    pub host: Option<RuntimeHost>,
    pub adapter: Option<Box<dyn HostAdapter>>,
}

const EPOCH_TICK: Duration = Duration::from_millis(100);

fn epoch_deadline(mut cx: StoreContextMut<'_, RuntimeStore>) -> wasmtime::Result<UpdateDeadline> {
    let state = cx.data_mut();
    if state.cancelled {
        return Ok(UpdateDeadline::Interrupt);
    }
    let remaining = state.timeout.saturating_sub(state.started.elapsed());
    if remaining.is_zero() {
        return Ok(UpdateDeadline::Interrupt);
    }
    Ok(UpdateDeadline::Continue(
        (remaining.as_millis() / EPOCH_TICK.as_millis()).max(1) as u64,
    ))
}

pub fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = WasmtimeConfig::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("创建 Wasmtime engine 失败");
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("ilp-runtime-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(EPOCH_TICK);
                ticker.increment_epoch();
            })
            .expect("启动插件 epoch 线程失败");
        engine
    })
}

pub fn new_store_with_host(
    policy: &CapabilityPolicy,
    host: RuntimeHost,
) -> Result<Store<RuntimeStore>> {
    let mut store = new_store(policy)?;
    store.data_mut().host = Some(host);
    Ok(store)
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
            host: None,
            adapter: None,
        },
    );
    store.limiter(|state| &mut state.limits);
    store.epoch_deadline_callback(epoch_deadline);
    store.set_epoch_deadline((policy.limits.timeout_ms / EPOCH_TICK.as_millis() as u64).max(1));
    store
        .set_fuel(policy.limits.fuel)
        .map_err(|error| anyhow!("设置插件 fuel 失败: {error}"))?;
    Ok(store)
}

pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub trait HttpBackend: Send + Sync {
    fn fetch(&self, url: &str, body: &[u8]) -> Result<Vec<u8>>;

    fn fetch_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        self.fetch(
            request.url.as_str(),
            request.body.as_deref().unwrap_or_default(),
        )
        .map(|body| HttpResponse {
            status: 200,
            headers: Vec::new(),
            body,
        })
    }
}

pub struct RuntimeHost {
    pub policy: CapabilityPolicy,
    pub capabilities: HostCapabilities,
    pub storage: Option<Arc<dyn StorageBackend>>,
    pub secrets: Option<Arc<dyn SecretBackend>>,
    pub http: Option<Arc<dyn HttpBackend>>,
    pub adapter: Option<Box<dyn HostAdapter>>,
}

impl RuntimeHost {
    pub fn new(policy: CapabilityPolicy) -> Self {
        let capabilities = HostCapabilities::from_policy(&policy);
        Self {
            policy,
            capabilities,
            storage: None,
            secrets: None,
            http: None,
            adapter: None,
        }
    }

    pub fn with_storage(mut self, storage: Arc<dyn StorageBackend>) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn with_secret_backend(mut self, secrets: Arc<dyn SecretBackend>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn with_http_backend(mut self, http: Arc<dyn HttpBackend>) -> Self {
        self.http = Some(http);
        self
    }

    pub fn with_adapter(mut self, adapter: Box<dyn HostAdapter>) -> Self {
        self.adapter = Some(adapter);
        self
    }

    pub fn set_policy(&mut self, policy: CapabilityPolicy) {
        self.capabilities = HostCapabilities::from_policy(&policy);
        self.policy = policy;
    }

    pub fn fetch(&mut self, url: &str, body: &[u8]) -> std::result::Result<Vec<u8>, RuntimeError> {
        let request = HttpRequest {
            method: "POST".to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: Some(body.to_vec()),
        };
        Ok(self.fetch_request(&request)?.body)
    }

    pub fn fetch_request(
        &mut self,
        request: &HttpRequest,
    ) -> std::result::Result<HttpResponse, RuntimeError> {
        validate_http_request(&self.policy, &mut self.capabilities, &request.url)?;
        let backend = self
            .http
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("HTTP backend 未配置".into()))?;
        let response = backend
            .fetch_request(request)
            .map_err(|error| RuntimeError::ExecutionFailed(error.to_string()))?;
        if response.body.len() as u64 > self.policy.limits.max_response_bytes {
            return Err(RuntimeError::ResourceLimit("插件响应超过大小限制".into()));
        }
        Ok(response)
    }

    pub fn storage_get(&self, key: &str) -> std::result::Result<Option<Vec<u8>>, RuntimeError> {
        if !self.capabilities.can_use("storage-read") {
            return Err(RuntimeError::PermissionDenied(
                "插件未获存储读取权限".into(),
            ));
        }
        self.storage
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("storage backend 未配置".into()))?
            .get(key)
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))
    }

    pub fn secret_get(&self, key: &str) -> std::result::Result<Option<Vec<u8>>, RuntimeError> {
        if !self.capabilities.can_use("secret-read") {
            return Err(RuntimeError::PermissionDenied(
                "插件未获 secret 读取权限".into(),
            ));
        }
        self.secrets
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("secret backend 未配置".into()))?
            .get(key)
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))
    }

    pub fn secret_set(&self, key: &str, value: &[u8]) -> std::result::Result<(), RuntimeError> {
        if !self.capabilities.can_use("secret-write") {
            return Err(RuntimeError::PermissionDenied(
                "插件未获 secret 写入权限".into(),
            ));
        }
        if value.len() as u64 > self.policy.limits.max_response_bytes {
            return Err(RuntimeError::ResourceLimit(
                "插件 secret 值超过大小限制".into(),
            ));
        }
        self.secrets
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("secret backend 未配置".into()))?
            .set(key, value)
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))
    }

    pub fn secret_delete(&self, key: &str) -> std::result::Result<(), RuntimeError> {
        if !self.capabilities.can_use("secret-write") {
            return Err(RuntimeError::PermissionDenied(
                "插件未获 secret 写入权限".into(),
            ));
        }
        self.secrets
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("secret backend 未配置".into()))?
            .delete(key)
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))
    }

    pub fn storage_set(&self, key: &str, value: &[u8]) -> std::result::Result<(), RuntimeError> {
        if !self.capabilities.can_use("storage-write") {
            return Err(RuntimeError::PermissionDenied(
                "插件未获存储写入权限".into(),
            ));
        }
        if value.len() as u64 > self.policy.limits.max_response_bytes {
            return Err(RuntimeError::ResourceLimit("插件存储值超过大小限制".into()));
        }
        self.storage
            .as_ref()
            .ok_or_else(|| RuntimeError::ExecutionFailed("storage backend 未配置".into()))?
            .set(key, value)
            .map_err(|e| RuntimeError::ExecutionFailed(e.to_string()))
    }
}

pub fn validate_http_request(
    policy: &CapabilityPolicy,
    capabilities: &mut HostCapabilities,
    url: &str,
) -> std::result::Result<(), RuntimeError> {
    if !capabilities.can_use("http") {
        return Err(RuntimeError::PermissionDenied("插件未获 HTTP 权限".into()));
    }
    policy
        .check_url(url)
        .map_err(|error| RuntimeError::PermissionDenied(error.to_string()))?;
    capabilities.consume_request()
}

pub trait SecretBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

pub struct FileSecretBackend {
    inner: FileStorageBackend,
}

impl FileSecretBackend {
    pub fn new(root: PathBuf) -> Result<Self> {
        Ok(Self {
            inner: FileStorageBackend::new(root)?,
        })
    }
}

impl SecretBackend for FileSecretBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner.get(key)
    }
    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        self.inner.set(key, value)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                self.inner.root.join(key),
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }
}

pub struct FileStorageBackend {
    root: PathBuf,
}

impl FileStorageBackend {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        if key.is_empty() || key.contains('/') || key.contains('\\') || key == "." || key == ".." {
            return Err(anyhow!("非法插件存储键"));
        }
        Ok(self.root.join(key))
    }
}

impl StorageBackend for FileStorageBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path(key)?;
        match std::fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn set(&self, key: &str, value: &[u8]) -> Result<()> {
        std::fs::write(self.path(key)?, value)?;
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path(key)?;
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

pub struct HostCapabilities {
    pub allow_http: bool,
    pub allow_storage: bool,
    pub allow_secrets: bool,
    pub requests_used: u32,
    pub max_requests: u32,
}

impl HostCapabilities {
    pub fn from_policy(policy: &CapabilityPolicy) -> Self {
        Self {
            allow_http: !policy.allowed_domains.is_empty(),
            allow_storage: policy.allow_storage,
            allow_secrets: policy.allow_secrets,
            requests_used: 0,
            max_requests: policy.limits.max_requests,
        }
    }

    pub fn consume_request(&mut self) -> std::result::Result<(), RuntimeError> {
        if self.requests_used >= self.max_requests {
            return Err(RuntimeError::ResourceLimit("插件请求次数超限".into()));
        }
        self.requests_used += 1;
        Ok(())
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

pub fn instantiate_component(
    component: &Component,
    _store: &mut Store<RuntimeStore>,
) -> std::result::Result<(), RuntimeError> {
    let _ = component;
    Err(RuntimeError::ExecutionFailed(
        "WIT host capability adapter 尚未配置".into(),
    ))
}

pub fn load_component(wasm: &[u8]) -> std::result::Result<Component, RuntimeError> {
    validate_component(wasm)?;
    Component::from_binary(shared_engine(), wasm)
        .map_err(|error| RuntimeError::InvalidFormat(format!("WASM component 加载失败: {error}")))
}

/// 已通过 Core registration contract 的插件组件。
///
/// 该句柄只保存经过校验的组件和执行策略，不提供绕过 registration 的构造器。
/// WIT host bindings 与具体业务调用由上层 adapter 注入；runtime 不会自行联网、
/// 注册插件或猜测书籍上下文。
pub struct RegisteredPluginComponent {
    component: Component,
    metadata: ilp_types::PluginMetadata,
    trust: PluginTrust,
    policy: CapabilityPolicy,
}

impl RegisteredPluginComponent {
    pub fn load(
        wasm: &[u8],
        metadata: ilp_types::PluginMetadata,
        trust: PluginTrust,
        policy: CapabilityPolicy,
    ) -> std::result::Result<Self, RuntimeError> {
        validate_registration(&metadata, &trust, &policy, wasm)?;
        let component = load_component(wasm)?;
        Ok(Self {
            component,
            metadata,
            trust,
            policy,
        })
    }

    pub fn component(&self) -> &Component {
        &self.component
    }

    pub fn metadata(&self) -> &ilp_types::PluginMetadata {
        &self.metadata
    }

    pub fn trust(&self) -> &PluginTrust {
        &self.trust
    }

    pub fn policy(&self) -> &CapabilityPolicy {
        &self.policy
    }

    pub fn instantiate_store(&self, host: RuntimeHost) -> Result<Store<RuntimeStore>> {
        new_store_with_host(&self.policy, host)
    }
}

pub fn validate_component(wasm: &[u8]) -> std::result::Result<(), RuntimeError> {
    if wasm.len() < 8 || &wasm[0..4] != b"\0asm" {
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
    store.set_epoch_deadline((policy.limits.timeout_ms / EPOCH_TICK.as_millis() as u64).max(1));
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

pub fn content_hash(wasm: &[u8]) -> String {
    format!("{:x}", Sha256::digest(wasm))
}

pub fn validate_content_hash(wasm: &[u8], expected: &str) -> std::result::Result<(), RuntimeError> {
    let expected = expected.trim().to_ascii_lowercase();
    if expected.len() != 64
        || !expected
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(RuntimeError::SignatureRejected(
            "插件内容 SHA-256 无效".into(),
        ));
    }
    if content_hash(wasm) != expected {
        return Err(RuntimeError::SignatureRejected("插件内容摘要不匹配".into()));
    }
    Ok(())
}

pub fn validate_trust_source(source: &str) -> std::result::Result<(), RuntimeError> {
    if !(source.starts_with("registry:") || source.starts_with("local:")) {
        return Err(RuntimeError::SignatureRejected(
            "插件来源必须使用 registry: 或 local: 受信前缀".into(),
        ));
    }
    if source.len() <= source.find(':').unwrap_or(source.len()) + 1 {
        return Err(RuntimeError::SignatureRejected(
            "插件来源标识不能为空".into(),
        ));
    }
    Ok(())
}

pub fn validate_registration(
    metadata: &ilp_types::PluginMetadata,
    trust: &PluginTrust,
    policy: &CapabilityPolicy,
    wasm: &[u8],
) -> std::result::Result<(), RuntimeError> {
    validate_metadata(metadata)?;
    validate_trust_source(&trust.source)?;
    if !trust.user_confirmed {
        return Err(RuntimeError::SignatureRejected(
            "插件尚未获得用户确认".into(),
        ));
    }
    validate_content_hash(wasm, &trust.content_hash)?;
    let granted = metadata
        .permissions
        .iter()
        .filter(|permission| match permission.as_str() {
            "http" => !policy.allowed_domains.is_empty(),
            "storage-read" | "storage-write" => policy.allow_storage,
            "secret-read" | "secret-write" => policy.allow_secrets,
            "clock" => true,
            _ => false,
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    validate_permissions(&metadata.permissions, &granted)?;
    for domain in &metadata.allowed_domains {
        policy
            .check_domain(domain)
            .map_err(|error| RuntimeError::PermissionDenied(error.to_string()))?;
    }
    Ok(())
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
        assert!(policy.check_url("https://user@example.com/path").is_err());
        assert!(policy.check_url("https://example.com:443/path").is_ok());
    }

    #[test]
    fn abi_validation_accepts_legacy_and_current_versions() {
        assert!(validate_abi("0.1.0").is_ok());
        assert!(validate_abi(CURRENT_ABI).is_ok());
        assert!(validate_abi("9.9.9").is_err());
    }

    #[test]
    fn capability_mapping_is_default_deny() {
        let mut policy = CapabilityPolicy::deny_all();
        policy.limits.max_requests = 1;
        let mut capabilities = HostCapabilities::from_policy(&policy);
        assert!(!capabilities.can_use("http"));
        assert!(!capabilities.can_use("storage-read"));
        assert!(!capabilities.can_use("secret-read"));
        assert!(capabilities.can_use("clock"));
        assert!(capabilities.consume_request().is_ok());
        assert!(capabilities.consume_request().is_err());
    }

    #[test]
    fn http_validation_requires_permission_domain_and_budget() {
        let mut policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        policy.limits.max_requests = 1;
        let mut capabilities = HostCapabilities::from_policy(&policy);
        assert!(validate_http_request(&policy, &mut capabilities, "https://example.com/a").is_ok());
        assert!(
            validate_http_request(&policy, &mut capabilities, "https://example.com/b").is_err()
        );
        assert!(
            validate_http_request(&policy, &mut capabilities, "https://other.example/a").is_err()
        );
    }

    struct EchoHttp;

    impl HttpBackend for EchoHttp {
        fn fetch(&self, _url: &str, body: &[u8]) -> Result<Vec<u8>> {
            Ok(body.to_vec())
        }
    }

    #[test]
    fn runtime_host_enforces_http_and_storage_boundaries() {
        let root = std::env::temp_dir().join(format!("ilp-runtime-host-{}", std::process::id()));
        let storage = Arc::new(FileStorageBackend::new(root.clone()).unwrap());
        let mut policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        policy.allow_storage = true;
        let mut host = RuntimeHost::new(policy)
            .with_storage(storage)
            .with_http_backend(Arc::new(EchoHttp));
        assert_eq!(host.fetch("https://example.com", b"ok").unwrap(), b"ok");
        host.storage_set("key", b"value").unwrap();
        assert_eq!(host.storage_get("key").unwrap(), Some(b"value".to_vec()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn invalid_component_is_rejected() {
        assert!(validate_component(b"not wasm").is_err());
    }

    #[test]
    fn registration_validation_rejects_hash_drift_and_untrusted_source() {
        let wasm = b"fixture wasm bytes";
        let metadata = ilp_types::PluginMetadata {
            abi_version: CURRENT_ABI.into(),
            site_name: "示例站".into(),
            site_id: "example".into(),
            site_domain: "example.com".into(),
            plugin_name: "示例插件".into(),
            plugin_id: "example-plugin".into(),
            version: "1.0.0".into(),
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
        };
        let mut trust = PluginTrust {
            content_hash: content_hash(wasm),
            signature_key_id: None,
            source: "registry:test".into(),
            user_confirmed: true,
        };
        let policy = CapabilityPolicy::deny_all();
        assert!(validate_registration(&metadata, &trust, &policy, wasm).is_ok());
        trust.content_hash = "0".repeat(64);
        assert!(matches!(
            validate_registration(&metadata, &trust, &policy, wasm),
            Err(RuntimeError::SignatureRejected(_))
        ));
        trust.content_hash = content_hash(wasm);
        trust.source = "https://untrusted.example/plugin".into();
        assert!(matches!(
            validate_registration(&metadata, &trust, &policy, wasm),
            Err(RuntimeError::SignatureRejected(_))
        ));
    }

    #[test]
    fn registered_component_rejects_non_component_bytes() {
        let wasm = b"fixture wasm bytes";
        let metadata = ilp_types::PluginMetadata {
            abi_version: CURRENT_ABI.into(),
            site_name: "示例站".into(),
            site_id: "example".into(),
            site_domain: "example.com".into(),
            plugin_name: "示例插件".into(),
            plugin_id: "example-plugin".into(),
            version: "1.0.0".into(),
            permissions: Vec::new(),
            allowed_domains: Vec::new(),
        };
        let trust = PluginTrust {
            content_hash: content_hash(wasm),
            signature_key_id: None,
            source: "local:test".into(),
            user_confirmed: true,
        };
        assert!(matches!(
            RegisteredPluginComponent::load(wasm, metadata, trust, CapabilityPolicy::deny_all()),
            Err(RuntimeError::InvalidFormat(_))
        ));
    }

    #[test]
    fn deny_all_registration_rejects_http_permission() {
        let wasm = b"fixture wasm bytes";
        let metadata = ilp_types::PluginMetadata {
            abi_version: CURRENT_ABI.into(),
            site_name: "示例站".into(),
            site_id: "example".into(),
            site_domain: "example.com".into(),
            plugin_name: "示例插件".into(),
            plugin_id: "example-plugin".into(),
            version: "1.0.0".into(),
            permissions: vec!["http".into()],
            allowed_domains: vec!["example.com".into()],
        };
        let trust = PluginTrust {
            content_hash: content_hash(wasm),
            signature_key_id: None,
            source: "local:test".into(),
            user_confirmed: true,
        };
        assert!(matches!(
            validate_registration(&metadata, &trust, &CapabilityPolicy::deny_all(), wasm),
            Err(RuntimeError::PermissionDenied(_))
        ));
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

    struct FixedHttp(Vec<u8>);

    impl HttpBackend for FixedHttp {
        fn fetch(&self, _url: &str, _body: &[u8]) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ilp-runtime-{name}-{}", std::process::id()))
    }

    #[test]
    fn runtime_host_denies_every_capability_by_default() {
        let mut host = RuntimeHost::new(CapabilityPolicy::deny_all());
        assert!(matches!(
            host.fetch("https://example.com", b""),
            Err(RuntimeError::PermissionDenied(_))
        ));
        assert!(matches!(
            host.storage_get("key"),
            Err(RuntimeError::PermissionDenied(_))
        ));
        assert!(matches!(
            host.secret_get("key"),
            Err(RuntimeError::PermissionDenied(_))
        ));
    }

    #[test]
    fn domain_policy_requires_https_and_respects_wildcard_boundaries() {
        let policy = CapabilityPolicy::deny_all().allow_domain("*.example.com");
        assert!(policy.check_url("https://api.example.com/path").is_ok());
        assert!(policy.check_url("https://example.com/path").is_err());
        assert!(policy
            .check_url("https://example.com.attacker.test")
            .is_err());
        assert!(policy.check_url("http://api.example.com/path").is_err());
        assert!(policy
            .check_url("https://user:pass@api.example.com")
            .is_err());
    }

    #[test]
    fn runtime_host_enforces_request_and_response_budgets() {
        let mut policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        policy.limits.max_requests = 1;
        policy.limits.max_response_bytes = 2;
        let mut host =
            RuntimeHost::new(policy).with_http_backend(Arc::new(FixedHttp(vec![1, 2, 3])));
        assert!(matches!(
            host.fetch("https://example.com", b""),
            Err(RuntimeError::ResourceLimit(_))
        ));
        // 请求在调用后端前就已计数，响应过大时重试也不能绕过次数限制。
        assert!(matches!(
            host.fetch("https://example.com", b""),
            Err(RuntimeError::ResourceLimit(_))
        ));
    }

    #[test]
    fn file_storage_isolated_by_root_and_rejects_unsafe_keys() {
        let first_root = temp_root("storage-a");
        let second_root = temp_root("storage-b");
        let _ = std::fs::remove_dir_all(&first_root);
        let _ = std::fs::remove_dir_all(&second_root);
        let first = FileStorageBackend::new(first_root.clone()).unwrap();
        let second = FileStorageBackend::new(second_root.clone()).unwrap();
        first.set("same-key", b"first").unwrap();
        assert_eq!(first.get("same-key").unwrap(), Some(b"first".to_vec()));
        assert_eq!(second.get("same-key").unwrap(), None);
        assert!(first.set("../escape", b"bad").is_err());
        let _ = std::fs::remove_dir_all(first_root);
        let _ = std::fs::remove_dir_all(second_root);
    }

    #[cfg(unix)]
    #[test]
    fn file_secret_backend_writes_secrets_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("secrets");
        let _ = std::fs::remove_dir_all(&root);
        let backend = FileSecretBackend::new(root.clone()).unwrap();
        backend.set("token", b"private").unwrap();
        let mode = std::fs::metadata(root.join("token"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cancellation_timeout_and_fuel_are_reported_separately() {
        let mut policy = CapabilityPolicy::deny_all();
        policy.limits.timeout_ms = 1;
        let mut store = new_store(&policy).unwrap();
        cancel(&mut store);
        assert!(matches!(
            check_execution_budget(&store),
            Err(RuntimeError::ExecutionFailed(_))
        ));
        reset_execution_budget(&mut store, &policy).unwrap();
        store.data_mut().started = Instant::now() - Duration::from_millis(10);
        assert!(matches!(
            check_execution_budget(&store),
            Err(RuntimeError::ResourceLimit(_))
        ));
        reset_execution_budget(&mut store, &policy).unwrap();
        store.set_fuel(0).unwrap();
        assert!(matches!(
            check_execution_budget(&store),
            Err(RuntimeError::ResourceLimit(_))
        ));
    }

    #[test]
    fn metadata_validation_accepts_both_supported_abi_boundaries() {
        for abi in ["0.1.0", "0.2.0"] {
            let metadata = ilp_types::PluginMetadata {
                abi_version: abi.into(),
                site_name: "site".into(),
                site_id: "site".into(),
                site_domain: "example.com".into(),
                plugin_name: "plugin".into(),
                plugin_id: "plugin".into(),
                version: "0.1.0".into(),
                permissions: vec![],
                allowed_domains: vec!["example.com".into()],
            };
            assert!(validate_metadata(&metadata).is_ok(), "ABI {abi}");
        }
    }
}
