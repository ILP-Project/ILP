//! ILP 面向 CLI、Flutter 和其他前端的核心 API。
//!
//! Core 不读取命令行、不输出人类文本，也不直接暴露底层数据库；调用方通过
//! 服务接口提交预定义用例，并订阅结构化事件。

use ilp_runtime::CapabilityPolicy;
use ilp_storage::{LibraryStorage, SqliteStorage, StoragePaths, StoredBook};
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

    pub fn new_task_id(&self) -> Uuid {
        Uuid::new_v4()
    }

    pub fn save_book(&self, book: &StoredBook) -> Result<(), CoreError> {
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .save_book(book)
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

    pub fn task_created_event(&self, task_id: Uuid) -> CoreEvent {
        CoreEvent::TaskCreated {
            sequence: self.next_event_sequence(),
            task_id,
        }
    }

    pub fn create_download_task(&self) -> Result<Uuid, CoreError> {
        let task_id = self.new_task_id();
        self.storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .create_task(&task_id.to_string(), "download", "queued")
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
        Ok(CoreEvent::TaskStateChanged {
            sequence: self.next_event_sequence(),
            task_id,
            state,
        })
    }

    pub fn recovery_snapshot(&self) -> Result<Vec<(Uuid, TaskState)>, CoreError> {
        let states = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?
            .task_states()
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        Ok(states
            .into_iter()
            .filter_map(|(id, state)| Some((Uuid::parse_str(&id).ok()?, parse_task_state(&state)?)))
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
            .set_task_state(&id, task_state_name(next))
            .map_err(|error| CoreError::Storage(error.to_string()))
    }

    pub fn recover_interrupted_tasks(&self) -> Result<Vec<Uuid>, CoreError> {
        let storage = self
            .storage
            .lock()
            .map_err(|_| CoreError::Storage("存储锁已中毒".into()))?;
        let tasks = storage
            .task_states()
            .map_err(|error| CoreError::Storage(error.to_string()))?;
        let mut recovered = Vec::new();
        for (id, state) in tasks {
            if matches!(parse_task_state(&state), Some(TaskState::Running)) {
                storage
                    .set_task_state(&id, "interrupted")
                    .map_err(|error| CoreError::Storage(error.to_string()))?;
                if let Ok(task_id) = Uuid::parse_str(&id) {
                    recovered.push(task_id);
                }
            }
        }
        Ok(recovered)
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
        Ok(self.new_task_id())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
