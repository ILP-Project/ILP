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
use ilp_core::{
    CoreError, DownloadBackend, DownloadBackendError, DownloadReport, DownloadRequest,
    DownloadWorkflow, IlpCore, PluginRegistration,
};
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
    fn content_hash(&self) -> &str;
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

    fn content_hash(&self) -> &str {
        PluginInstance::content_hash(self)
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
        if state.plugin.content_hash() != registration.trust.content_hash {
            return Err(DownloadBackendError::Execution(
                "已加载插件内容摘要与 Core 注册不一致，拒绝执行".into(),
            ));
        }
        Ok(())
    }

    fn plugin_error(error: anyhow::Error) -> DownloadBackendError {
        DownloadBackendError::Execution(format!("插件执行失败: {error:#}"))
    }
}

/// 已验证插件注册与宿主下载后端组成的 host-facing 下载服务。
///
/// 构造时只接受 Core 中已经存在且完全一致的 registration；该服务不会自动注册
/// 插件，也不会因为传入了已加载实例而放宽 Core 的 capability policy。后端由调用方
/// 注入并由服务持有，旧 CLI 不经过此入口。
pub struct HostDownloadWorkflow<'a> {
    core: &'a IlpCore,
    registration: PluginRegistration,
    backend: Box<dyn DownloadBackend>,
}

impl<'a> HostDownloadWorkflow<'a> {
    /// 绑定一个已经通过宿主校验并加载的插件与 Core 下载编排。
    pub fn new(
        core: &'a IlpCore,
        registration: PluginRegistration,
        backend: Box<dyn DownloadBackend>,
    ) -> Result<Self, CoreError> {
        let service = Self {
            core,
            registration,
            backend,
        };
        service.verify_registration()?;
        Ok(service)
    }

    /// 执行下载；请求的 plugin_id 必须与构造时绑定的 registration 一致。
    pub fn run(&self, request: DownloadRequest) -> Result<DownloadReport, CoreError> {
        self.run_with_mode(request, false)
    }

    pub fn run_resumable(&self, request: DownloadRequest) -> Result<DownloadReport, CoreError> {
        self.run_with_mode(request, true)
    }

    pub fn resume_task(&self, task_id: uuid::Uuid) -> Result<DownloadReport, CoreError> {
        self.verify_registration()?;
        DownloadWorkflow::new(self.core, self.backend.as_ref()).resume_task(task_id)
    }

    fn run_with_mode(
        &self,
        request: DownloadRequest,
        resume: bool,
    ) -> Result<DownloadReport, CoreError> {
        if request.plugin_id != self.registration.metadata.plugin_id {
            return Err(CoreError::InvalidArgument(
                "下载请求插件 ID 与已验证 registration 不一致".into(),
            ));
        }
        // Core 注册表可能在构造服务后被其他调用方修改；执行前再次确认，
        // 避免把旧 registration 与新的宿主状态拼接使用。
        self.verify_registration()?;
        let workflow = DownloadWorkflow::new(self.core, self.backend.as_ref());
        if resume {
            workflow.run_resumable(request)
        } else {
            workflow.run(request)
        }
    }

    fn verify_registration(&self) -> Result<(), CoreError> {
        let registered = self
            .core
            .registered_plugin(&self.registration.metadata.plugin_id)?
            .ok_or_else(|| {
                CoreError::NotFound(format!(
                    "插件 {} 未在 Core 注册",
                    self.registration.metadata.plugin_id
                ))
            })?;
        if registered != self.registration {
            return Err(CoreError::Conflict(
                "传入的插件 registration 与 Core 注册记录不一致".into(),
            ));
        }
        Ok(())
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
    use ilp_core::{CoreBuilder, CoreConfig, LibraryService, PluginService, SearchService};
    use ilp_runtime::CapabilityPolicy;

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
                source: "registry:test".into(),
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

    fn core_with_registration() -> (ilp_core::IlpCore, PluginRegistration, std::path::PathBuf) {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data = std::env::temp_dir().join(format!("ilp-host-workflow-{suffix}"));
        let mut config = CoreConfig::new(&data);
        config.plugin_policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let core = CoreBuilder::new(config).build().unwrap();
        let registration = registration();
        PluginService::register(
            &core,
            &ilp_core::PluginRegistrationRequest {
                metadata: registration.metadata.clone(),
                trust: registration.trust.clone(),
            },
        )
        .unwrap();
        (core, registration, data)
    }

    struct FixtureBackend {
        chapters: Vec<ChapterEntry>,
    }

    impl DownloadBackend for FixtureBackend {
        fn list_chapters(
            &self,
            _plugin: &PluginRegistration,
            _request: &DownloadRequest,
        ) -> Result<Vec<ChapterEntry>, DownloadBackendError> {
            Ok(self.chapters.clone())
        }

        fn fetch_chapter(
            &self,
            _plugin: &PluginRegistration,
            _request: &DownloadRequest,
            chapter: &ChapterEntry,
        ) -> Result<ChapterResult, DownloadBackendError> {
            Ok(ChapterResult::ok(
                chapter.id.clone(),
                chapter.title.clone(),
                "fixture 正文".into(),
            ))
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

        fn content_hash(&self) -> &str {
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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

            fn content_hash(&self) -> &str {
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
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
    fn host_workflow_runs_registered_plugin_backend_end_to_end() {
        let (core, registration, data) = core_with_registration();
        let workflow = HostDownloadWorkflow::new(
            &core,
            registration,
            Box::new(FixtureBackend {
                chapters: vec![chapter("ch-1")],
            }),
        )
        .unwrap();
        let report = workflow.run(request()).unwrap();

        assert_eq!(report.progress.completed, 1);
        assert_eq!(report.progress.failed, 0);
        assert_eq!(report.chapters[0].text.as_deref(), Some("fixture 正文"));
        assert_eq!(core.list_books().unwrap(), vec!["book-1"]);
        assert_eq!(core.search("fixture 正文").unwrap(), vec!["book-1"]);
        drop(workflow);
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn host_workflow_rejects_registration_drift_and_unregistered_plugin() {
        let (core, mut registration, data) = core_with_registration();
        registration.trust.source = "different-source".into();
        let error = HostDownloadWorkflow::new(
            &core,
            registration.clone(),
            Box::new(FixtureBackend { chapters: vec![] }),
        )
        .err()
        .expect("registration 漂移必须拒绝");
        assert!(matches!(error, CoreError::Conflict(message) if message.contains("不一致")));

        registration.metadata.plugin_id = "not-registered".into();
        let error = HostDownloadWorkflow::new(
            &core,
            registration,
            Box::new(FixtureBackend { chapters: vec![] }),
        )
        .err()
        .expect("未注册插件必须拒绝");
        assert!(
            matches!(error, CoreError::NotFound(message) if message.contains("未在 Core 注册"))
        );
        drop(core);
        let _ = std::fs::remove_dir_all(data);
    }

    #[test]
    fn host_workflow_rejects_request_for_a_different_plugin_before_backend() {
        let (core, registration, data) = core_with_registration();
        let workflow = HostDownloadWorkflow::new(
            &core,
            registration,
            Box::new(FixtureBackend {
                chapters: vec![chapter("ch-1")],
            }),
        )
        .unwrap();
        let mut different_plugin = request();
        different_plugin.plugin_id = "other-plugin".into();
        let error = workflow
            .run(different_plugin)
            .expect_err("请求插件 ID 漂移必须拒绝");
        assert!(
            matches!(error, CoreError::InvalidArgument(message) if message.contains("registration"))
        );
        drop(workflow);
        drop(core);
        let _ = std::fs::remove_dir_all(data);
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
