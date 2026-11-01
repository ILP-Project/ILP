//! Core 下载编排到宿主插件执行链的适配层。
//!
//! 适配器只使用已经存在的 WIT 调用、宿主 HTTP 后端和 Core 的结果类型。
//! WIT 的单个抓取任务没有声明“覆盖哪些章节”，因此 Core 的单章接口不
//! 猜测批量任务的归属：收到非单任务或非单结果时直接拒绝。
//!
//! 该模块目前是可复用的宿主/Core seam，尚未接入旧 CLI 的
//! `commands::fetch_all`：旧入口仍自行管理批量计划和进度；在 Core 注册表、
//! 可信插件输入和能力授权边界稳定前，不能伪造注册信息或绕过 deny-all。

use crate::engine::{self, PluginInstance};
use ilp_core::{DownloadBackend, DownloadBackendError, DownloadRequest, PluginRegistration};
use ilp_types::{
    validate_chapter_order, ChapterEntry, ChapterResult, ChapterStatus, PluginMetadata,
};
use std::sync::{Arc, Mutex};

type HttpFetcher = dyn Fn(
        &[&engine::wit_types::HttpRequest],
        usize,
        u64,
    ) -> Vec<anyhow::Result<engine::wit_types::HttpResponse>>
    + Send
    + Sync;

trait PluginDownloadExecution: Send {
    fn metadata(&self) -> &PluginMetadata;
    fn init(&mut self, book_id: &str, force_update: bool) -> anyhow::Result<()>;
    fn catalog(&mut self, force: bool) -> anyhow::Result<Vec<ChapterEntry>>;
    fn plan_fetch(&mut self, ids: &[String]) -> anyhow::Result<Vec<engine::wit_types::FetchTask>>;
    fn consume_fetch(
        &mut self,
        task_id: &str,
        response: engine::wit_types::HttpResponse,
    ) -> anyhow::Result<Vec<ChapterResult>>;
}

impl PluginDownloadExecution for PluginInstance {
    fn metadata(&self) -> &PluginMetadata {
        &self.metadata
    }

    fn init(&mut self, book_id: &str, force_update: bool) -> anyhow::Result<()> {
        PluginInstance::init(self, book_id, force_update)
    }

    fn catalog(&mut self, force: bool) -> anyhow::Result<Vec<ChapterEntry>> {
        PluginInstance::catalog(self, force)
    }

    fn plan_fetch(&mut self, ids: &[String]) -> anyhow::Result<Vec<engine::wit_types::FetchTask>> {
        PluginInstance::plan_fetch(self, ids)
    }

    fn consume_fetch(
        &mut self,
        task_id: &str,
        response: engine::wit_types::HttpResponse,
    ) -> anyhow::Result<Vec<ChapterResult>> {
        PluginInstance::consume_fetch(self, task_id, response)
    }
}

struct AdapterState {
    plugin: Box<dyn PluginDownloadExecution>,
    active_book_id: Option<String>,
}

/// 把 Core 的同步 `DownloadBackend` 接到一个已加载的 WIT 插件。
///
/// 一个 `PluginInstance` 持有插件自己的状态（当前书籍、解密上下文等），
/// 所以适配器以互斥锁串行化整个“生成请求—执行 HTTP—解析响应”链，并拒绝
/// 在未建立书籍上下文或切换到另一书籍时直接抓取。
pub struct PluginDownloadBackend {
    state: Mutex<AdapterState>,
    max_workers: usize,
    http_fetcher: Arc<HttpFetcher>,
}

impl PluginDownloadBackend {
    pub fn new(plugin: PluginInstance, max_workers: usize) -> Self {
        Self::from_execution_with_fetcher(
            Box::new(plugin),
            max_workers,
            Arc::new(crate::http::fetch_requests),
        )
    }

    #[cfg(test)]
    fn from_execution(plugin: Box<dyn PluginDownloadExecution>, max_workers: usize) -> Self {
        Self::from_execution_with_fetcher(
            plugin,
            max_workers,
            Arc::new(crate::http::fetch_requests),
        )
    }

    fn from_execution_with_fetcher(
        plugin: Box<dyn PluginDownloadExecution>,
        max_workers: usize,
        http_fetcher: Arc<HttpFetcher>,
    ) -> Self {
        Self {
            state: Mutex::new(AdapterState {
                plugin,
                active_book_id: None,
            }),
            max_workers: max_workers.max(1),
            http_fetcher,
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, AdapterState>, DownloadBackendError> {
        self.state
            .lock()
            .map_err(|_| DownloadBackendError::Execution("插件适配器状态锁已中毒".into()))
    }

    fn validate_registration(
        state: &AdapterState,
        registration: &PluginRegistration,
        request: &DownloadRequest,
    ) -> Result<(), DownloadBackendError> {
        if request.plugin_id != registration.metadata.plugin_id {
            return Err(DownloadBackendError::Execution(format!(
                "下载请求插件 ID 与 Core 注册不一致: {}",
                request.plugin_id
            )));
        }
        if state.plugin.metadata() != &registration.metadata {
            return Err(DownloadBackendError::Execution(
                "已加载插件 metadata 与 Core 注册不一致，拒绝执行".into(),
            ));
        }
        Ok(())
    }

    fn plugin_error(error: anyhow::Error) -> DownloadBackendError {
        DownloadBackendError::Execution(format!("插件执行失败: {error:#}"))
    }
}

impl DownloadBackend for PluginDownloadBackend {
    fn list_chapters(
        &self,
        plugin: &PluginRegistration,
        request: &DownloadRequest,
    ) -> Result<Vec<ChapterEntry>, DownloadBackendError> {
        let mut state = self.lock_state()?;
        Self::validate_registration(&state, plugin, request)?;
        state
            .plugin
            .init(&request.book_id, false)
            .map_err(Self::plugin_error)?;
        let chapters = state.plugin.catalog(false).map_err(Self::plugin_error)?;
        validate_chapter_order(&chapters)
            .map_err(|error| DownloadBackendError::Execution(format!("目录顺序无效: {error}")))?;
        state.active_book_id = Some(request.book_id.clone());
        Ok(chapters)
    }

    fn fetch_chapter(
        &self,
        plugin: &PluginRegistration,
        request: &DownloadRequest,
        chapter: &ChapterEntry,
    ) -> Result<ChapterResult, DownloadBackendError> {
        let mut state = self.lock_state()?;
        Self::validate_registration(&state, plugin, request)?;
        if state.active_book_id.as_deref() != Some(request.book_id.as_str()) {
            return Err(DownloadBackendError::Unavailable(
                "插件尚未为该书籍建立目录上下文".into(),
            ));
        }

        let ids = [chapter.id.clone()];
        let tasks = state.plugin.plan_fetch(&ids).map_err(Self::plugin_error)?;
        let task = require_single_fetch_task(tasks)?;

        // 持有插件锁直到响应交回，防止另一个工作流改变插件内部书籍/解密状态。
        let responses = (self.http_fetcher)(&[&task.request], self.max_workers, 0);
        let response = responses
            .into_iter()
            .next()
            .ok_or_else(|| DownloadBackendError::Execution("HTTP 后端未返回响应".into()))?
            .map_err(Self::plugin_error)?;
        let results = state
            .plugin
            .consume_fetch(&task.id, response)
            .map_err(Self::plugin_error)?;
        require_single_chapter_result(results, &chapter.id)
    }
}

fn require_single_fetch_task(
    tasks: Vec<engine::wit_types::FetchTask>,
) -> Result<engine::wit_types::FetchTask, DownloadBackendError> {
    match tasks.len() {
        1 => tasks
            .into_iter()
            .next()
            .ok_or_else(|| DownloadBackendError::Execution("抓取计划为空".into())),
        0 => Err(DownloadBackendError::Execution(
            "插件没有为章节生成抓取任务".into(),
        )),
        count => Err(DownloadBackendError::Execution(format!(
            "单章抓取计划返回 {count} 个任务，无法安全确定章节归属"
        ))),
    }
}

fn require_single_chapter_result(
    results: Vec<ChapterResult>,
    chapter_id: &str,
) -> Result<ChapterResult, DownloadBackendError> {
    if results.len() != 1 {
        return Err(DownloadBackendError::Execution(format!(
            "单章抓取返回 {} 个结果，拒绝猜测正文归属",
            results.len()
        )));
    }
    let result = results
        .into_iter()
        .next()
        .ok_or_else(|| DownloadBackendError::Execution("插件没有返回章节结果".into()))?;
    if result.id != chapter_id {
        return Err(DownloadBackendError::Execution(format!(
            "插件返回章节 ID {}，预期 {chapter_id}",
            result.id
        )));
    }
    if result.status != ChapterStatus::Ok && result.text.is_some() {
        return Err(DownloadBackendError::Execution(
            "失败状态携带正文，拒绝写入不可信内容".into(),
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ilp_core::DownloadBackend;

    fn metadata() -> PluginMetadata {
        PluginMetadata {
            abi_version: "0.2.0".into(),
            site_name: "测试站".into(),
            site_id: "test-site".into(),
            site_domain: "example.com".into(),
            plugin_name: "测试插件".into(),
            plugin_id: "plugin-test".into(),
            version: "1.0.0".into(),
            permissions: vec!["http".into()],
            allowed_domains: vec!["example.com".into()],
        }
    }

    fn registration() -> PluginRegistration {
        PluginRegistration {
            metadata: metadata(),
            trust: ilp_runtime::PluginTrust {
                content_hash: "a".repeat(64),
                signature_key_id: None,
                source: "test".into(),
                user_confirmed: true,
            },
        }
    }

    fn request() -> DownloadRequest {
        DownloadRequest {
            plugin_id: "plugin-test".into(),
            book_id: "book-1".into(),
            title: "测试书".into(),
            author: "测试作者".into(),
            source_type: "test".into(),
            source_id: "book-1".into(),
        }
    }

    struct FakePlugin {
        metadata: PluginMetadata,
        init_book: Option<String>,
        chapters: Vec<ChapterEntry>,
        tasks: Vec<engine::wit_types::FetchTask>,
    }

    impl PluginDownloadExecution for FakePlugin {
        fn metadata(&self) -> &PluginMetadata {
            &self.metadata
        }

        fn init(&mut self, book_id: &str, _force_update: bool) -> anyhow::Result<()> {
            self.init_book = Some(book_id.into());
            Ok(())
        }

        fn catalog(&mut self, _force: bool) -> anyhow::Result<Vec<ChapterEntry>> {
            Ok(self.chapters.clone())
        }

        fn plan_fetch(
            &mut self,
            _ids: &[String],
        ) -> anyhow::Result<Vec<engine::wit_types::FetchTask>> {
            Ok(self.tasks.clone())
        }

        fn consume_fetch(
            &mut self,
            _task_id: &str,
            _response: engine::wit_types::HttpResponse,
        ) -> anyhow::Result<Vec<ChapterResult>> {
            Ok(Vec::new())
        }
    }

    fn chapter(id: &str) -> ChapterEntry {
        ChapterEntry {
            id: id.into(),
            title: format!("章节 {id}"),
            url: String::new(),
            fetch_url: String::new(),
            update_time: 0,
            order_key: String::new(),
        }
    }

    fn fake_backend(tasks: Vec<engine::wit_types::FetchTask>) -> PluginDownloadBackend {
        PluginDownloadBackend::from_execution(
            Box::new(FakePlugin {
                metadata: metadata(),
                init_book: None,
                chapters: vec![chapter("ch-1")],
                tasks,
            }),
            1,
        )
    }

    fn fake_backend_with(
        tasks: Vec<engine::wit_types::FetchTask>,
        results: Vec<ChapterResult>,
    ) -> PluginDownloadBackend {
        struct ResultPlugin {
            metadata: PluginMetadata,
            chapters: Vec<ChapterEntry>,
            tasks: Vec<engine::wit_types::FetchTask>,
            results: Vec<ChapterResult>,
        }

        impl PluginDownloadExecution for ResultPlugin {
            fn metadata(&self) -> &PluginMetadata {
                &self.metadata
            }

            fn init(&mut self, _book_id: &str, _force_update: bool) -> anyhow::Result<()> {
                Ok(())
            }

            fn catalog(&mut self, _force: bool) -> anyhow::Result<Vec<ChapterEntry>> {
                Ok(self.chapters.clone())
            }

            fn plan_fetch(
                &mut self,
                _ids: &[String],
            ) -> anyhow::Result<Vec<engine::wit_types::FetchTask>> {
                Ok(self.tasks.clone())
            }

            fn consume_fetch(
                &mut self,
                _task_id: &str,
                _response: engine::wit_types::HttpResponse,
            ) -> anyhow::Result<Vec<ChapterResult>> {
                Ok(self.results.clone())
            }
        }

        PluginDownloadBackend::from_execution_with_fetcher(
            Box::new(ResultPlugin {
                metadata: metadata(),
                chapters: vec![chapter("ch-1")],
                tasks,
                results,
            }),
            1,
            Arc::new(|requests, _, _| {
                assert_eq!(requests.len(), 1);
                vec![Ok(engine::wit_types::HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: b"fixture".to_vec(),
                })]
            }),
        )
    }

    #[test]
    fn list_chapters_initializes_the_plugin_and_binds_the_book() {
        let backend = fake_backend(Vec::new());
        let chapters = backend
            .list_chapters(&registration(), &request())
            .expect("目录应可读取");
        assert_eq!(chapters.len(), 1);

        let other = DownloadRequest {
            book_id: "book-2".into(),
            source_id: "book-2".into(),
            ..request()
        };
        let error = backend
            .fetch_chapter(&registration(), &other, &chapters[0])
            .expect_err("不能跨书籍复用插件上下文");
        assert!(error.to_string().contains("尚未为该书籍建立目录上下文"));
    }

    #[test]
    fn mismatched_registration_is_rejected_before_plugin_execution() {
        let backend = fake_backend(Vec::new());
        let mut mismatched = registration();
        mismatched.metadata.plugin_id = "other-plugin".into();
        let error = backend
            .list_chapters(&mismatched, &request())
            .expect_err("注册信息不一致必须拒绝");
        assert!(error.to_string().contains("插件 ID 与 Core 注册不一致"));
    }

    #[test]
    fn ambiguous_fetch_tasks_are_rejected_without_network_or正文() {
        let tasks = vec![
            engine::wit_types::FetchTask {
                id: "one".into(),
                request: engine::wit_types::HttpRequest {
                    method: "GET".into(),
                    url: "https://example.com/one".into(),
                    headers: Vec::new(),
                    body: None,
                },
            },
            engine::wit_types::FetchTask {
                id: "two".into(),
                request: engine::wit_types::HttpRequest {
                    method: "GET".into(),
                    url: "https://example.com/two".into(),
                    headers: Vec::new(),
                    body: None,
                },
            },
        ];
        let error = require_single_fetch_task(tasks).expect_err("批量任务归属不明确");
        assert!(error.to_string().contains("无法安全确定章节归属"));
    }

    #[test]
    fn malformed_result_never_passes正文_for_a_failure_status() {
        let result = ChapterResult {
            id: "ch-1".into(),
            title: "第一章".into(),
            status: ChapterStatus::Captcha,
            text: Some("这不是正文".into()),
            error_code: "captcha".into(),
            retryable: false,
            message: None,
        };
        let error = require_single_chapter_result(vec![result], "ch-1")
            .expect_err("失败状态带正文必须拒绝");
        assert!(error.to_string().contains("拒绝写入不可信内容"));
    }

    #[test]
    fn result_id_mismatch_is_rejected() {
        let result = ChapterResult::ok("other".into(), "第一章".into(), "正文".into());
        let error = require_single_chapter_result(vec![result], "ch-1")
            .expect_err("章节 ID 不匹配必须拒绝");
        assert!(error.to_string().contains("预期 ch-1"));
    }

    #[test]
    fn single_fetch_uses_injected_http_backend_and_returns_plugin_result() {
        let backend = fake_backend_with(
            vec![engine::wit_types::FetchTask {
                id: "task-1".into(),
                request: engine::wit_types::HttpRequest {
                    method: "GET".into(),
                    url: "https://example.com/ch-1".into(),
                    headers: Vec::new(),
                    body: None,
                },
            }],
            vec![ChapterResult::ok(
                "ch-1".into(),
                "章节 ch-1".into(),
                "脱敏 fixture 正文".into(),
            )],
        );
        let chapters = backend
            .list_chapters(&registration(), &request())
            .expect("目录应可读取");
        let result = backend
            .fetch_chapter(&registration(), &request(), &chapters[0])
            .expect("单章应可通过注入 HTTP 后端执行");
        assert_eq!(result.text.as_deref(), Some("脱敏 fixture 正文"));
    }
}
