//! ILP 面向 CLI、Flutter 和其他前端的核心 API。
//!
//! Core 不读取命令行、不输出人类文本，也不直接暴露底层数据库；调用方通过
//! 服务接口提交预定义用例，并订阅结构化事件。

use ilp_runtime::{validate_metadata, CapabilityPolicy, PluginTrust, ResourceLimits};
use ilp_storage::{LibraryStorage, SqliteStorage, StoragePaths, StoredBook, StoredPlugin};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreEvent {
    TaskCreated {
        sequence: u64,
        task_id: Uuid,
    },
    TaskStateChanged {
        sequence: u64,
        task_id: Uuid,
        state: TaskState,
    },
    ResyncRequired {
        sequence: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskState {
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Running | Self::Cancelled)
                | (
                    Self::Running,
                    Self::Paused
                        | Self::Completed
                        | Self::Failed
                        | Self::Cancelled
                        | Self::Interrupted
                )
                | (Self::Paused, Self::Queued | Self::Cancelled)
                | (Self::Interrupted, Self::Queued | Self::Cancelled)
                | (Self::Failed, Self::Queued | Self::Cancelled)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub task_id: Uuid,
    pub state: TaskState,
}

/// 已通过宿主完整性和信任检查后，交给 Core 注册的插件输入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRegistrationRequest {
    pub metadata: ilp_types::PluginMetadata,
    pub trust: PluginTrust,
}

/// Core 注册成功后返回的插件摘要；不包含 WASM 字节。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRegistration {
    pub metadata: ilp_types::PluginMetadata,
    pub trust: PluginTrust,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoreConfig {
    pub paths: StoragePaths,
    pub plugin_policy: CapabilityPolicy,
    pub device_id: Uuid,
    pub device_name: String,
}

impl CoreConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            paths: StoragePaths::new(data_dir),
            plugin_policy: CapabilityPolicy::deny_all(),
            device_id: Uuid::new_v4(),
            device_name: String::from("设备"),
        }
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("参数无效: {0}")]
    InvalidArgument(String),
    #[error("资源不存在: {0}")]
    NotFound(String),
    #[error("状态冲突: {0}")]
    Conflict(String),
    #[error("存储错误: {0}")]
    Storage(String),
    #[error("插件错误: {0}")]
    Plugin(String),
    #[error("任务已取消")]
    Cancelled,
}

pub struct CoreBuilder {
    config: CoreConfig,
    runtime_threads: usize,
}

impl CoreBuilder {
    pub fn new(config: CoreConfig) -> Self {
        Self {
            config,
            runtime_threads: 2,
        }
    }

    pub fn runtime_threads(mut self, threads: usize) -> Self {
        self.runtime_threads = threads.max(1);
        self
    }

    pub fn build(self) -> Result<IlpCore, CoreError> {
        self.config
            .paths
            .validate()
            .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
        let storage = SqliteStorage::open(self.config.paths.clone())
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(self.runtime_threads)
            .enable_all()
            .build()
            .map_err(|error| CoreError::InvalidArgument(format!("创建运行时失败: {error}")))?;
        Ok(IlpCore {
            config: self.config,
            storage: Mutex::new(storage),
            next_sequence: Arc::new(Mutex::new(0)),
            runtime,
        })
    }
}

pub struct IlpCore {
    config: CoreConfig,
    storage: Mutex<SqliteStorage>,
    next_sequence: Arc<Mutex<u64>>,
    runtime: tokio::runtime::Runtime,
}

impl IlpCore {
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    pub fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }

    pub async fn create_download_task_async(&self) -> Result<Uuid, CoreError> {
        self.create_download_task()
    }

    pub async fn update_task_state_async(
        &self,
        task_id: Uuid,
        next: TaskState,
    ) -> Result<(), CoreError> {
        self.update_task_state(task_id, next)
    }

    pub async fn events_after_async(
        &self,
        sequence: u64,
        limit: u32,
    ) -> Result<Vec<CoreEvent>, CoreError> {
        self.events_after(sequence, limit)
    }

    pub fn new_task_id(&self) -> Uuid {
        Uuid::new_v4()
    }

    pub fn validate_plugin(&self, metadata: &ilp_types::PluginMetadata) -> Result<(), CoreError> {
        validate_metadata(metadata).map_err(|error| CoreError::Plugin(error.to_string()))
    }

    pub fn validate_plugin_registration(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        validate_registration(&self.config.plugin_policy, request)
    }

    pub fn register_plugin(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        let registration = self.validate_plugin_registration(request)?;
        let plugin_id = registration.metadata.plugin_id.as_str();
        let site_id = registration.metadata.site_id.as_str();
        let mut storage = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
        if let Some(existing) = storage
            .list_plugins()
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .into_iter()
            .find(|plugin| {
                plugin.metadata.site_id == site_id && plugin.metadata.plugin_id != plugin_id
            })
        {
            return Err(CoreError::Conflict(format!(
                "站点 {site_id} 已由插件 {} 注册",
                existing.metadata.plugin_id
            )));
        }
        if let Some(existing) = storage
            .load_plugin(plugin_id)
            .map_err(|error| CoreError::Storage(error.to_string()))?
        {
            if existing.metadata.site_id != site_id {
                return Err(CoreError::Conflict(format!(
                    "插件 {plugin_id} 已注册到站点 {}",
                    existing.metadata.site_id
                )));
            }
        }
        storage
            .save_plugin(&StoredPlugin {
                metadata: registration.metadata.clone(),
                content_hash: registration.trust.content_hash.clone(),
                signature_key_id: registration.trust.signature_key_id.clone(),
                source: registration.trust.source.clone(),
                trusted_at: Some(unix_timestamp()),
            })
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(registration)
    }

    pub fn registered_plugin(
        &self,
        plugin_id: &str,
    ) -> Result<Option<PluginRegistration>, CoreError> {
        validate_plugin_identifier("插件 ID", plugin_id)?;
        let storage = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
        storage
            .load_plugin(plugin_id)
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .map(|stored| plugin_registration_from_stored(&self.config.plugin_policy, stored))
            .transpose()
    }

    pub fn registered_plugins(&self) -> Result<Vec<PluginRegistration>, CoreError> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
        storage
            .list_plugins()
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .into_iter()
            .map(|stored| plugin_registration_from_stored(&self.config.plugin_policy, stored))
            .collect()
    }

    pub fn save_book(&self, book: &StoredBook) -> Result<(), CoreError> {
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .save_book(book)
            .map_err(|error| CoreError::Storage(error.to_string()))
    }

    fn append_event_with_payload<F>(&self, kind: &str, make_payload: F) -> Result<u64, CoreError>
    where
        F: FnOnce(u64) -> anyhow::Result<String>,
    {
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .append_event_with_payload(kind, make_payload)
            .map_err(|error| CoreError::Storage(error.to_string()))
    }

    pub fn next_event_sequence(&self) -> u64 {
        if let Ok(storage) = self.storage.lock() {
            if let Ok(sequence) = storage.next_event_sequence() {
                return sequence;
            }
        }
        let mut sequence = self.next_sequence.lock().expect("事件序列锁不应中毒");
        *sequence += 1;
        *sequence
    }

    pub fn events_after(&self, sequence: u64, limit: u32) -> Result<Vec<CoreEvent>, CoreError> {
        let (oldest, current, rows) = {
            let storage = self
                .storage
                .lock()
                .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
            let oldest = storage
                .oldest_event_sequence()
                .map_err(|error| CoreError::Storage(error.to_string()))?;
            let current = storage
                .current_event_sequence()
                .map_err(|error| CoreError::Storage(error.to_string()))?;
            let rows = storage
                .events_after(sequence, limit)
                .map_err(|error| CoreError::Storage(error.to_string()))?;
            (oldest, current, rows)
        };
        if let Some(oldest) = oldest {
            if sequence.saturating_add(1) < oldest {
                return Ok(vec![CoreEvent::ResyncRequired { sequence: current }]);
            }
        }
        rows.into_iter()
            .map(|(row_sequence, kind, payload)| {
                let event: CoreEvent = serde_json::from_str(&payload).map_err(|error| {
                    CoreError::Storage(format!("事件 {kind} 反序列化失败: {error}"))
                })?;
                let payload_sequence = event_sequence(&event);
                if payload_sequence != row_sequence {
                    return Err(CoreError::Storage(format!(
                        "事件 {kind} 序列不一致: 行 {row_sequence}, 载荷 {payload_sequence}"
                    )));
                }
                if event_kind(&event) != kind {
                    return Err(CoreError::Storage(format!(
                        "事件类型不一致: 行 {kind}, 载荷 {}",
                        event_kind(&event)
                    )));
                }
                Ok(event)
            })
            .collect()
    }

    pub fn prune_events(&self, keep: u64) -> Result<usize, CoreError> {
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .prune_events(keep)
            .map_err(|error| CoreError::Storage(error.to_string()))
    }

    pub fn task_created_event(&self, task_id: Uuid) -> CoreEvent {
        let sequence = self
            .append_event_with_payload("task_created", |sequence| {
                serde_json::to_string(&CoreEvent::TaskCreated { sequence, task_id })
                    .map_err(Into::into)
            })
            .unwrap_or_else(|_| self.next_event_sequence());
        CoreEvent::TaskCreated { sequence, task_id }
    }

    pub fn create_download_task(&self) -> Result<Uuid, CoreError> {
        let task_id = self.new_task_id();
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .create_task_with_event(
                &task_id.to_string(),
                "download",
                "queued",
                "task_created",
                |sequence| {
                    serde_json::to_string(&CoreEvent::TaskCreated { sequence, task_id })
                        .map_err(Into::into)
                },
            )
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(task_id)
    }

    pub fn task_event(&self, task_id: Uuid) -> Result<CoreEvent, CoreError> {
        let id = task_id.to_string();
        let state = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .task_state(&id)
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .ok_or(CoreError::NotFound(id))?;
        let state =
            parse_task_state(&state).ok_or_else(|| CoreError::Storage("未知任务状态".into()))?;
        let sequence = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .current_event_sequence()
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(CoreEvent::TaskStateChanged {
            sequence,
            task_id,
            state,
        })
    }

    pub fn recovery_snapshot(&self) -> Result<Vec<(Uuid, TaskState)>, CoreError> {
        Ok(self
            .recovery_snapshot_dto()?
            .into_iter()
            .map(|snapshot| (snapshot.task_id, snapshot.state))
            .collect())
    }

    pub fn recovery_snapshot_dto(&self) -> Result<Vec<TaskSnapshot>, CoreError> {
        let states = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .task_states()
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(states
            .into_iter()
            .filter_map(|(id, state)| {
                Some(TaskSnapshot {
                    task_id: Uuid::parse_str(&id).ok()?,
                    state: parse_task_state(&state)?,
                })
            })
            .collect())
    }

    pub fn update_task_state(&self, task_id: Uuid, next: TaskState) -> Result<(), CoreError> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
        let id = task_id.to_string();
        let current = storage
            .task_state(&id)
            .map_err(|error| CoreError::Storage(error.to_string()))?
            .ok_or_else(|| CoreError::NotFound(id.clone()))?;
        let current = parse_task_state(&current)
            .ok_or_else(|| CoreError::Storage(format!("未知任务状态: {current}")))?;
        if !current.can_transition_to(next) {
            return Err(CoreError::Conflict(format!(
                "任务状态不能从 {current:?} 转换为 {next:?}"
            )));
        }
        storage
            .set_task_state_with_event(
                &id,
                task_state_name(next),
                "task_state_changed",
                |sequence| {
                    serde_json::to_string(&CoreEvent::TaskStateChanged {
                        sequence,
                        task_id,
                        state: next,
                    })
                    .map_err(Into::into)
                },
            )
            .map(|_| ())
            .map_err(|error| CoreError::Storage(error.to_string()))
    }

    pub fn recover_interrupted_tasks(&self) -> Result<Vec<Uuid>, CoreError> {
        let recovered = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .recover_interrupted_tasks_with_events(|id, sequence| {
                let task_id = Uuid::parse_str(id).map_err(|error| anyhow::anyhow!(error))?;
                serde_json::to_string(&CoreEvent::TaskStateChanged {
                    sequence,
                    task_id,
                    state: TaskState::Interrupted,
                })
                .map_err(Into::into)
            })
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        recovered
            .into_iter()
            .map(|id| Uuid::parse_str(&id).map_err(|error| CoreError::Storage(error.to_string())))
            .collect()
    }
}

fn event_kind(event: &CoreEvent) -> &'static str {
    match event {
        CoreEvent::TaskCreated { .. } => "task_created",
        CoreEvent::TaskStateChanged { .. } => "task_state_changed",
        CoreEvent::ResyncRequired { .. } => "resync_required",
    }
}

fn event_sequence(event: &CoreEvent) -> u64 {
    match event {
        CoreEvent::TaskCreated { sequence, .. }
        | CoreEvent::TaskStateChanged { sequence, .. }
        | CoreEvent::ResyncRequired { sequence } => *sequence,
    }
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Queued => "queued",
        TaskState::Running => "running",
        TaskState::Paused => "paused",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
        TaskState::Interrupted => "interrupted",
    }
}

fn parse_task_state(state: &str) -> Option<TaskState> {
    match state {
        "queued" => Some(TaskState::Queued),
        "running" => Some(TaskState::Running),
        "paused" => Some(TaskState::Paused),
        "completed" => Some(TaskState::Completed),
        "failed" => Some(TaskState::Failed),
        "cancelled" => Some(TaskState::Cancelled),
        "interrupted" => Some(TaskState::Interrupted),
        _ => None,
    }
}

pub trait LibraryService {
    fn list_books(&self) -> Result<Vec<String>, CoreError>;
}

pub trait DownloadService {
    fn create_task(&self) -> Result<Uuid, CoreError>;
}

pub trait SearchService {
    fn search(&self, query: &str) -> Result<Vec<String>, CoreError>;
}

pub trait SyncService {
    fn export_schema_version(&self) -> u32;
}

pub trait PluginService {
    fn policy(&self) -> &CapabilityPolicy;
    fn resource_limits(&self) -> &ResourceLimits;

    fn registered_plugin(&self, plugin_id: &str) -> Result<Option<PluginRegistration>, CoreError> {
        let _ = plugin_id;
        Err(CoreError::Plugin("插件注册服务未实现".into()))
    }

    fn validate_registration(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        let _ = request;
        Err(CoreError::Plugin("插件注册服务未实现".into()))
    }

    fn register(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        let _ = request;
        Err(CoreError::Plugin("插件注册服务未实现".into()))
    }

    fn registered(&self) -> Result<Vec<PluginRegistration>, CoreError> {
        Err(CoreError::Plugin("插件注册服务未实现".into()))
    }
}

impl LibraryService for IlpCore {
    fn list_books(&self) -> Result<Vec<String>, CoreError> {
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .list_book_ids()
            .map_err(|error| CoreError::Storage(error.to_string()))
    }
}

impl DownloadService for IlpCore {
    fn create_task(&self) -> Result<Uuid, CoreError> {
        self.create_download_task()
    }
}

impl SearchService for IlpCore {
    fn search(&self, query: &str) -> Result<Vec<String>, CoreError> {
        if query.trim().is_empty() {
            return Err(CoreError::InvalidArgument("搜索关键词不能为空".into()));
        }
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .search(query)
            .map_err(|error| CoreError::Storage(error.to_string()))
    }
}

impl SyncService for IlpCore {
    fn export_schema_version(&self) -> u32 {
        1
    }
}

impl PluginService for IlpCore {
    fn policy(&self) -> &CapabilityPolicy {
        &self.config.plugin_policy
    }

    fn resource_limits(&self) -> &ResourceLimits {
        &self.config.plugin_policy.limits
    }

    fn validate_registration(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        self.validate_plugin_registration(request)
    }

    fn register(
        &self,
        request: &PluginRegistrationRequest,
    ) -> Result<PluginRegistration, CoreError> {
        self.register_plugin(request)
    }

    fn registered_plugin(&self, plugin_id: &str) -> Result<Option<PluginRegistration>, CoreError> {
        IlpCore::registered_plugin(self, plugin_id)
    }

    fn registered(&self) -> Result<Vec<PluginRegistration>, CoreError> {
        self.registered_plugins()
    }
}

fn plugin_registration_from_stored(
    policy: &CapabilityPolicy,
    stored: StoredPlugin,
) -> Result<PluginRegistration, CoreError> {
    validate_registration(
        policy,
        &PluginRegistrationRequest {
            metadata: stored.metadata,
            trust: PluginTrust {
                content_hash: stored.content_hash,
                signature_key_id: stored.signature_key_id,
                source: stored.source,
                user_confirmed: true,
            },
        },
    )
}

fn unix_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".into())
}

const MAX_PLUGIN_TEXT_CHARS: usize = 256;
const MAX_PLUGIN_DOMAIN_CHARS: usize = 253;
const MAX_PLUGIN_PERMISSIONS: usize = 32;
const MAX_PLUGIN_DOMAINS: usize = 64;

fn validate_registration(
    policy: &CapabilityPolicy,
    request: &PluginRegistrationRequest,
) -> Result<PluginRegistration, CoreError> {
    validate_metadata(&request.metadata).map_err(|error| CoreError::Plugin(error.to_string()))?;
    let metadata = &request.metadata;
    for (label, value) in [
        ("站点名称", metadata.site_name.as_str()),
        ("站点域名", metadata.site_domain.as_str()),
        ("插件名称", metadata.plugin_name.as_str()),
        ("插件版本", metadata.version.as_str()),
    ] {
        validate_plugin_text(label, value, MAX_PLUGIN_TEXT_CHARS)?;
    }
    for (label, value) in [
        ("站点 ID", metadata.site_id.as_str()),
        ("插件 ID", metadata.plugin_id.as_str()),
    ] {
        validate_plugin_identifier(label, value)?;
    }
    validate_plugin_domain("站点域名", &metadata.site_domain)?;
    if metadata.permissions.len() > MAX_PLUGIN_PERMISSIONS {
        return Err(CoreError::Plugin("插件权限数量超过限制".into()));
    }
    if metadata.allowed_domains.len() > MAX_PLUGIN_DOMAINS {
        return Err(CoreError::Plugin("插件允许域名数量超过限制".into()));
    }
    for permission in &metadata.permissions {
        validate_plugin_text("插件权限", permission, MAX_PLUGIN_TEXT_CHARS)?;
    }
    for domain in &metadata.allowed_domains {
        validate_plugin_domain("允许域名", domain)?;
    }
    let hash = request.trust.content_hash.trim();
    if hash.len() != 64 || !hash.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(CoreError::Plugin("插件内容 SHA-256 无效".into()));
    }
    if !request.trust.user_confirmed {
        return Err(CoreError::Plugin("插件尚未获得用户确认".into()));
    }
    validate_plugin_text("插件来源", &request.trust.source, MAX_PLUGIN_TEXT_CHARS)?;
    if request
        .trust
        .signature_key_id
        .as_deref()
        .is_some_and(|key_id| {
            key_id.trim().is_empty()
                || key_id.chars().any(char::is_control)
                || key_id.chars().count() > MAX_PLUGIN_TEXT_CHARS
        })
    {
        return Err(CoreError::Plugin("插件签名密钥 ID 无效".into()));
    }
    for permission in &metadata.permissions {
        let allowed = match permission.as_str() {
            "http" => !policy.allowed_domains.is_empty(),
            "storage-read" | "storage-write" => policy.allow_storage,
            "secret-read" | "secret-write" => policy.allow_secrets,
            "clock" => true,
            _ => false,
        };
        if !allowed {
            return Err(CoreError::Plugin(format!(
                "插件请求未获授权能力: {permission}"
            )));
        }
    }
    if metadata
        .permissions
        .iter()
        .any(|permission| permission == "http")
    {
        if metadata.allowed_domains.is_empty() {
            return Err(CoreError::Plugin("申请 HTTP 权限时必须声明允许域名".into()));
        }
        policy
            .check_domain(&metadata.site_domain)
            .map_err(|error| CoreError::Plugin(error.to_string()))?;
        for domain in &metadata.allowed_domains {
            if domain.contains('*') {
                return Err(CoreError::Plugin("允许域名不支持通配符".into()));
            }
            policy
                .check_domain(domain)
                .map_err(|error| CoreError::Plugin(error.to_string()))?;
        }
    } else if !metadata.allowed_domains.is_empty() {
        return Err(CoreError::Plugin("未申请 HTTP 权限却声明了允许域名".into()));
    }
    Ok(PluginRegistration {
        metadata: metadata.clone(),
        trust: PluginTrust {
            content_hash: hash.to_ascii_lowercase(),
            signature_key_id: request.trust.signature_key_id.clone(),
            source: request.trust.source.clone(),
            user_confirmed: true,
        },
    })
}

fn validate_plugin_text(label: &str, value: &str, max_chars: usize) -> Result<(), CoreError> {
    if value.trim().is_empty() {
        return Err(CoreError::InvalidArgument(format!("{label}不能为空")));
    }
    if value.chars().count() > max_chars {
        return Err(CoreError::Plugin(format!("{label}长度超过限制")));
    }
    if value.chars().any(char::is_control) {
        return Err(CoreError::Plugin(format!("{label}包含控制字符")));
    }
    Ok(())
}

fn validate_plugin_identifier(label: &str, value: &str) -> Result<(), CoreError> {
    validate_plugin_text(label, value, MAX_PLUGIN_TEXT_CHARS)?;
    if value.chars().any(char::is_whitespace) {
        return Err(CoreError::Plugin(format!("{label}包含空白字符")));
    }
    Ok(())
}

fn validate_plugin_domain(label: &str, value: &str) -> Result<(), CoreError> {
    validate_plugin_text(label, value, MAX_PLUGIN_DOMAIN_CHARS)?;
    if value.chars().any(|character| {
        character.is_whitespace() || matches!(character, '/' | ':' | '@' | '[' | ']')
    }) {
        return Err(CoreError::Plugin(format!("{label}无效")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin_metadata() -> ilp_types::PluginMetadata {
        ilp_types::PluginMetadata {
            abi_version: ilp_runtime::CURRENT_ABI.into(),
            site_name: "示例站".into(),
            site_id: "example".into(),
            site_domain: "example.com".into(),
            plugin_name: "example-plugin".into(),
            plugin_id: "plugin-example".into(),
            version: "1.0.0".into(),
            permissions: vec!["http".into()],
            allowed_domains: vec!["example.com".into()],
        }
    }

    fn plugin_request() -> PluginRegistrationRequest {
        PluginRegistrationRequest {
            metadata: plugin_metadata(),
            trust: PluginTrust {
                content_hash: "A".repeat(64),
                signature_key_id: Some("release-2026".into()),
                source: "registry:test".into(),
                user_confirmed: true,
            },
        }
    }

    #[test]
    fn task_state_machine_rejects_terminal_restarts() {
        assert!(TaskState::Queued.can_transition_to(TaskState::Running));
        assert!(TaskState::Running.can_transition_to(TaskState::Interrupted));
        assert!(!TaskState::Completed.can_transition_to(TaskState::Running));
    }

    #[test]
    fn builder_validates_and_exposes_structured_config() {
        let data = std::env::temp_dir().join(format!("ilp-core-test-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();
        assert_eq!(core.config().device_name, "设备");
        assert!(!core.new_task_id().is_nil());
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn plugin_registration_validates_trust_and_policy_and_persists() {
        let data = std::env::temp_dir().join(format!("ilp-core-plugin-{}", Uuid::new_v4()));
        let mut config = CoreConfig::new(&data);
        config.plugin_policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let core = CoreBuilder::new(config).build().unwrap();
        let request = plugin_request();

        let registered = PluginService::register(&core, &request).unwrap();
        assert_eq!(registered.trust.content_hash, "a".repeat(64));
        assert_eq!(
            PluginService::registered(&core).unwrap(),
            vec![registered.clone()]
        );
        assert_eq!(
            PluginService::register(&core, &request).unwrap(),
            registered
        );
        assert_eq!(
            PluginService::registered_plugin(&core, "plugin-example").unwrap(),
            Some(registered.clone())
        );
        assert!(PluginService::registered_plugin(&core, "missing")
            .unwrap()
            .is_none());

        let mut invalid = request;
        invalid.trust.user_confirmed = false;
        assert!(matches!(
            PluginService::validate_registration(&core, &invalid),
            Err(CoreError::Plugin(message)) if message.contains("用户确认")
        ));
        drop(core);

        let mut config = CoreConfig::new(&data);
        config.plugin_policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let reopened = CoreBuilder::new(config).build().unwrap();
        assert_eq!(
            PluginService::registered_plugin(&reopened, "plugin-example")
                .unwrap()
                .unwrap()
                .trust
                .content_hash,
            "a".repeat(64)
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn plugin_registration_rejects_site_conflicts_and_unauthorized_domains() {
        let data = std::env::temp_dir().join(format!("ilp-core-plugin-{}", Uuid::new_v4()));
        let mut config = CoreConfig::new(&data);
        config.plugin_policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let core = CoreBuilder::new(config).build().unwrap();
        PluginService::register(&core, &plugin_request()).unwrap();

        let mut conflict = plugin_request();
        conflict.metadata.plugin_id = "other-plugin".into();
        assert!(matches!(
            PluginService::register(&core, &conflict),
            Err(CoreError::Conflict(message)) if message.contains("example")
        ));

        let mut unauthorized = plugin_request();
        unauthorized.metadata.allowed_domains = vec!["attacker.example".into()];
        assert!(matches!(
            PluginService::validate_registration(&core, &unauthorized),
            Err(CoreError::Plugin(message)) if message.contains("无权访问域名")
        ));

        let mut storage_permission = plugin_request();
        storage_permission.metadata.permissions = vec!["storage-read".into()];
        storage_permission.metadata.allowed_domains.clear();
        assert!(matches!(
            PluginService::validate_registration(&core, &storage_permission),
            Err(CoreError::Plugin(message)) if message.contains("未获授权能力")
        ));

        let mut wrong_site = plugin_request();
        wrong_site.metadata.site_id = "other-site".into();
        assert!(matches!(
            PluginService::register(&core, &wrong_site),
            Err(CoreError::Conflict(message)) if message.contains("example")
        ));
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn plugin_registration_rejects_oversized_and_malformed_identity_fields() {
        let data = std::env::temp_dir().join(format!("ilp-core-plugin-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();

        let mut oversized = plugin_request();
        oversized.metadata.plugin_id = "x".repeat(MAX_PLUGIN_TEXT_CHARS + 1);
        assert!(matches!(
            PluginService::validate_registration(&core, &oversized),
            Err(CoreError::Plugin(message)) if message.contains("长度超过限制")
        ));

        let mut malformed_domain = plugin_request();
        malformed_domain.metadata.site_domain = "https://example.com/path".into();
        assert!(matches!(
            PluginService::validate_registration(&core, &malformed_domain),
            Err(CoreError::Plugin(message)) if message.contains("无效")
        ));

        let mut malformed_permission = plugin_request();
        malformed_permission.metadata.permissions = vec!["unknown".into()];
        malformed_permission.metadata.allowed_domains.clear();
        assert!(matches!(
            PluginService::validate_registration(&core, &malformed_permission),
            Err(CoreError::Plugin(message)) if message.contains("未获授权能力")
        ));
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn plugin_registration_rejects_unbounded_and_ambiguous_metadata() {
        let data = std::env::temp_dir().join(format!("ilp-core-plugin-{}", Uuid::new_v4()));
        let mut config = CoreConfig::new(&data);
        config.plugin_policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let core = CoreBuilder::new(config).build().unwrap();

        let mut missing_domains = plugin_request();
        missing_domains.metadata.allowed_domains.clear();
        assert!(matches!(
            PluginService::validate_registration(&core, &missing_domains),
            Err(CoreError::Plugin(message)) if message.contains("必须声明允许域名")
        ));

        let mut wildcard = plugin_request();
        wildcard.metadata.allowed_domains = vec!["*.example.com".into()];
        assert!(matches!(
            PluginService::validate_registration(&core, &wildcard),
            Err(CoreError::Plugin(message)) if message.contains("不支持通配符")
        ));

        let mut oversized = plugin_request();
        oversized.metadata.plugin_name = "x".repeat(MAX_PLUGIN_TEXT_CHARS + 1);
        assert!(matches!(
            PluginService::validate_registration(&core, &oversized),
            Err(CoreError::Plugin(message)) if message.contains("长度超过限制")
        ));

        let mut wrong_site_domain = plugin_request();
        wrong_site_domain.metadata.site_domain = "other.example".into();
        assert!(matches!(
            PluginService::validate_registration(&core, &wrong_site_domain),
            Err(CoreError::Plugin(message)) if message.contains("无权访问域名")
        ));

        assert!(matches!(
            PluginService::registered_plugin(&core, "  "),
            Err(CoreError::InvalidArgument(_))
        ));
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn search_rejects_empty_queries() {
        let data = std::env::temp_dir().join(format!("ilp-core-test-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();
        assert!(matches!(
            core.search("  "),
            Err(CoreError::InvalidArgument(_))
        ));
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn task_operations_persist_matching_json_events() {
        let data = std::env::temp_dir().join(format!("ilp-core-test-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();
        let task_id = DownloadService::create_task(&core).unwrap();
        core.update_task_state(task_id, TaskState::Running).unwrap();

        let events = core.events_after(0, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            CoreEvent::TaskCreated { sequence: 1, task_id: id } if id == task_id
        ));
        assert!(matches!(
            events[1],
            CoreEvent::TaskStateChanged {
                sequence: 2,
                task_id: id,
                state: TaskState::Running,
            } if id == task_id
        ));
        assert_eq!(
            serde_json::to_value(&events[0]).unwrap(),
            serde_json::json!({
                "type": "task_created",
                "sequence": 1,
                "task_id": task_id,
            })
        );
        assert_eq!(
            serde_json::to_value(&events[1]).unwrap(),
            serde_json::json!({
                "type": "task_state_changed",
                "sequence": 2,
                "task_id": task_id,
                "state": "running",
            })
        );
        assert_eq!(
            serde_json::to_value(CoreEvent::ResyncRequired { sequence: 2 }).unwrap(),
            serde_json::json!({
                "type": "resync_required",
                "sequence": 2,
            })
        );
        assert_eq!(
            serde_json::from_value::<CoreEvent>(serde_json::json!({
                "type": "task_state_changed",
                "sequence": 2,
                "task_id": task_id,
                "state": "running",
            }))
            .unwrap(),
            events[1]
        );
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn recovery_and_snapshot_have_contracts() {
        let data = std::env::temp_dir().join(format!("ilp-core-test-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();
        let task_id = core.create_download_task().unwrap();
        core.update_task_state(task_id, TaskState::Running).unwrap();
        assert_eq!(core.recover_interrupted_tasks().unwrap(), vec![task_id]);
        assert_eq!(
            core.recovery_snapshot_dto().unwrap(),
            vec![TaskSnapshot {
                task_id,
                state: TaskState::Interrupted,
            }]
        );
        let snapshot_json = serde_json::to_value(core.recovery_snapshot_dto().unwrap()).unwrap();
        assert_eq!(
            snapshot_json,
            serde_json::json!([{"task_id": task_id, "state": "interrupted"}])
        );
        let events = core.events_after(0, 10).unwrap();
        assert!(matches!(
            events.last(),
            Some(CoreEvent::TaskStateChanged {
                sequence: 3,
                task_id: id,
                state: TaskState::Interrupted,
            }) if *id == task_id
        ));
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn pruned_history_requires_resync_without_consuming_sequence() {
        let data = std::env::temp_dir().join(format!("ilp-core-test-{}", Uuid::new_v4()));
        let core = CoreBuilder::new(CoreConfig::new(&data)).build().unwrap();
        for _ in 0..3 {
            core.create_download_task().unwrap();
        }
        assert_eq!(core.prune_events(2).unwrap(), 1);
        assert_eq!(
            core.events_after(0, 10).unwrap(),
            vec![CoreEvent::ResyncRequired { sequence: 3 }]
        );
        assert_eq!(
            core.events_after(1, 10).unwrap().len(),
            2,
            "sequence 1 is still a valid cursor after retention"
        );
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }
}
