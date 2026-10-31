//! ILP 面向 CLI、Flutter 和其他前端的核心 API。
//!
//! Core 不读取命令行、不输出人类文本，也不直接暴露底层数据库；调用方通过
//! 服务接口提交预定义用例，并订阅结构化事件。

use ilp_runtime::CapabilityPolicy;
use ilp_storage::StoragePaths;
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
}

impl CoreBuilder {
    pub fn new(config: CoreConfig) -> Self {
        Self { config }
    }

    pub fn build(self) -> Result<IlpCore, CoreError> {
        self.config
            .paths
            .validate()
            .map_err(|e| CoreError::InvalidArgument(e.to_string()))?;
        Ok(IlpCore {
            config: self.config,
            next_sequence: Arc::new(Mutex::new(0)),
        })
    }
}

pub struct IlpCore {
    config: CoreConfig,
    next_sequence: Arc<Mutex<u64>>,
}

impl IlpCore {
    pub fn config(&self) -> &CoreConfig {
        &self.config
    }

    pub fn new_task_id(&self) -> Uuid {
        Uuid::new_v4()
    }

    pub fn next_event_sequence(&self) -> u64 {
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
        Ok(Vec::new())
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
        Ok(Vec::new())
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
        let core = CoreBuilder::new(CoreConfig::new("data")).build().unwrap();
        assert_eq!(core.config().device_name, "设备");
        assert!(!core.new_task_id().is_nil());
    }

    #[test]
    fn search_rejects_empty_queries() {
        let core = CoreBuilder::new(CoreConfig::new("data")).build().unwrap();
        assert!(matches!(
            core.search("  "),
            Err(CoreError::InvalidArgument(_))
        ));
    }
}
