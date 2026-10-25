use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::{Method, NoProxy, Proxy};

use crate::engine::wit_types::{HttpHeader, HttpRequest, HttpResponse};
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const TIMEOUT_SECS: u64 = 30;

/// 每个请求最多再试几次（不含首次）。网络抖动与瞬时 5xx 都能靠它自愈。
const MAX_RETRIES: u32 = 2;

/// 退避基数：第 n 次重试等 BASE * 2^n 毫秒。
const RETRY_BASE_MS: u64 = 500;

/// 全局客户端：代理与 UA 只解析一次，避免每个请求都建连接池。
static CLIENT: OnceLock<Client> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();

/// 用配置初始化全局客户端，必须在任何请求前调用一次。
pub fn init(proxy: Option<&str>, no_proxy: &[String]) -> Result<()> {
    if INIT.get().is_some() {
        return Ok(());
    }
    let client = build_client(proxy, no_proxy)?;
    let _ = CLIENT.set(client);
    let _ = INIT.set(());
    Ok(())
}

fn build_client(proxy: Option<&str>, no_proxy: &[String]) -> Result<Client> {
    let mut builder: ClientBuilder = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent(USER_AGENT);

    let matcher = if no_proxy.is_empty() {
        None
    } else {
        NoProxy::from_string(&no_proxy.join(","))
    };

    // 显式配置优先，否则读环境变量；no_proxy 里的主机始终直连
    // （实测 i.snssdk.com 走代理会 TLS RST，直连才 200）。
    match proxy.map(str::to_string).or_else(env_proxy) {
        Some(p) => {
            let px = Proxy::all(&p)
                .with_context(|| format!("无效代理地址: {p}"))?
                .no_proxy(matcher);
            builder = builder.proxy(px);
        }
        None => builder = builder.no_proxy(),
    }

    builder.build().context("构建 HTTP 客户端失败")
}

/// 按 curl 的顺序挑一个环境变量代理，全空则说明不需要代理。
fn env_proxy() -> Option<String> {
    const KEYS: [&str; 6] = [
        "https_proxy",
        "HTTPS_PROXY",
        "all_proxy",
        "ALL_PROXY",
        "http_proxy",
        "HTTP_PROXY",
    ];
    KEYS.iter()
        .filter_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())
}

fn client() -> &'static Client {
    CLIENT.get_or_init(|| {
        let cfg = crate::config::Config::default();
        build_client(cfg.proxy.as_deref(), &cfg.no_proxy).expect("构建默认 HTTP 客户端失败")
    })
}

pub fn get_text(url: &str) -> Result<String> {
    let resp = client()
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    resp.text().with_context(|| format!("读取响应失败: {url}"))
}

pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = client()
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    let bytes = resp
        .bytes()
        .with_context(|| format!("读取响应失败: {url}"))?;
    Ok(bytes.to_vec())
}

/// 把插件给的请求发给对端，返回 WIT 形状的响应。
pub fn send(request: &HttpRequest) -> Result<HttpResponse> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| anyhow!("非法 HTTP 方法: {}", request.method))?;
    let mut req = client()
        .request(method, &request.url)
        .timeout(Duration::from_secs(TIMEOUT_SECS));

    for h in &request.headers {
        req = req.header(&h.name, &h.value);
    }
    if let Some(body) = &request.body {
        req = req.body(body.clone());
    }

    let resp = req
        .send()
        .with_context(|| format!("请求失败: {}", request.url))?;
    let status = resp.status().as_u16();
    let headers: Vec<HttpHeader> = resp
        .headers()
        .iter()
        .map(|(k, v)| HttpHeader {
            name: k.as_str().to_string(),
            value: v.to_str().unwrap_or_default().to_string(),
        })
        .collect();
    let body = resp
        .bytes()
        .with_context(|| format!("读取响应失败: {}", request.url))?
        .to_vec();

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

/// 一次请求是否值得重试。
///
/// 4xx（除 408/429）是我们发错了，重试没有意义；
/// 网络错误、超时、5xx、429 都是瞬时问题，值得再试。
fn is_retryable(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    if let Some(code) = text.split("HTTP 状态码 ").nth(1).and_then(|rest| {
        // 状态码后面紧跟冒号，所以只取连续数字。
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse::<u16>().ok()
    }) {
        return code >= 500 || code == 408 || code == 429;
    }
    // 没有状态码说明请求没走通（连接失败、超时、读响应中断）。
    true
}

/// 发送并重试。退避是 BASE * 2^attempt，最后一次失败原样返回。
fn send_with_retry(request: &HttpRequest) -> Result<HttpResponse> {
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let backoff = RETRY_BASE_MS * (1u64 << (attempt - 1));
            thread::sleep(Duration::from_millis(backoff));
        }
        match send(request) {
            Ok(resp) if resp.status < 400 => return Ok(resp),
            Ok(resp) => {
                let err = anyhow!("HTTP 状态码 {}: {}", resp.status, request.url);
                if !is_retryable(&err) {
                    return Err(err);
                }
                last = Some(err);
            }
            Err(e) => {
                if !is_retryable(&e) {
                    return Err(e);
                }
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("请求失败: {}", request.url)))
}

/// 并发执行一批请求，保持与输入同序。
///
/// 并发度由 `max_workers` 决定，`sleep_ms` 是同一 worker 上相邻两次
/// 请求的起始间隔。限速在"取下一个任务之前"完成，所以排队中并不会
/// 白白占用并发名额——这正是原版把 sleep 放在任务完成后导致吞吐塌陷的
/// 反向修正。
pub fn fetch_requests(
    requests: &[&HttpRequest],
    max_workers: usize,
    sleep_ms: u64,
) -> Vec<Result<HttpResponse>> {
    let n = requests.len();
    if n == 0 {
        return Vec::new();
    }

    let owned: Vec<HttpRequest> = requests.iter().map(|r| (*r).clone()).collect();
    let owned = Arc::new(owned);
    let results: Arc<Mutex<Vec<Option<Result<HttpResponse>>>>> =
        Arc::new(Mutex::new((0..n).map(|_| None).collect()));
    let (tx, rx) = mpsc::channel::<usize>();
    let rx = Arc::new(Mutex::new(rx));

    let worker_count = max_workers.max(1).min(n);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let rx = Arc::clone(&rx);
        let results = Arc::clone(&results);
        let owned = Arc::clone(&owned);
        handles.push(thread::spawn(move || {
            let mut last: Option<Instant> = None;
            loop {
                let idx = {
                    let guard = rx.lock().unwrap();
                    match guard.recv() {
                        Ok(i) => i,
                        Err(_) => break,
                    }
                };
                if sleep_ms > 0 {
                    if let Some(prev) = last {
                        let elapsed = prev.elapsed();
                        let gap = Duration::from_millis(sleep_ms);
                        if elapsed < gap {
                            thread::sleep(gap - elapsed);
                        }
                    }
                }
                let res = send_with_retry(&owned[idx]);
                last = Some(Instant::now());
                results.lock().unwrap()[idx] = Some(res);
            }
        }));
    }

    for i in 0..n {
        let _ = tx.send(i);
    }
    drop(tx);

    for h in handles {
        let _ = h.join();
    }

    let mut guard = results.lock().unwrap();
    std::mem::take(&mut *guard)
        .into_iter()
        .map(|o| o.expect("结果缺失"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_default_no_proxy() {
        let cfg = crate::config::Config::default();
        assert!(build_client(cfg.proxy.as_deref(), &cfg.no_proxy).is_ok());
    }

    #[test]
    fn explicit_proxy_with_exemption_builds() {
        let cfg = crate::config::Config::default();
        assert!(build_client(Some("http://127.0.0.1:7890"), &cfg.no_proxy).is_ok());
    }

    #[test]
    fn empty_request_list_returns_empty() {
        assert!(fetch_requests(&[], 4, 0).is_empty());
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试用 runtime")
    }

    /// 只把"起服务 + 挂 mock"放进 runtime。
    ///
    /// 被测试的 `send_with_retry` / `fetch_requests` 用的是阻塞 reqwest，
    /// 它自己会建一个内部 tokio runtime，绝不能在 `block_on` 的异步上下文里
    /// 调用，否则 reqwest 建 Client 时就会 panic（"Cannot drop a runtime in
    /// a context where blocking is not allowed"）。所以请求一律在 runtime
    /// 之外发。
    fn start_server(
        rt: &tokio::runtime::Runtime,
        mocks: impl FnOnce(
            wiremock::MockServer,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = wiremock::MockServer>>>,
    ) -> wiremock::MockServer {
        let server = rt.block_on(wiremock::MockServer::start());
        rt.block_on(mocks(server))
    }

    fn get_req(url: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
        }
    }

    #[test]
    fn wiremock_5xx_is_retried_then_succeeds() {
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                // 前两次 503，第三次 200：正好用掉 MAX_RETRIES 的两次重试。
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/retry"))
                    .respond_with(wiremock::ResponseTemplate::new(503))
                    .up_to_n_times(2)
                    .mount(&server)
                    .await;
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/retry"))
                    .respond_with(
                        wiremock::ResponseTemplate::new(200).set_body_string("ok-after-retry"),
                    )
                    .mount(&server)
                    .await;
                server
            })
        });

        let url = format!("{}/retry", server.uri());
        let resp = send_with_retry(&get_req(&url)).expect("重试后应成功");
        assert_eq!(resp.status, 200);
        assert_eq!(String::from_utf8_lossy(&resp.body), "ok-after-retry");

        let calls = rt
            .block_on(server.received_requests())
            .expect("应有记录")
            .len();
        assert_eq!(calls, 3, "两次重试后第三次成功");
        drop(server);
        drop(rt);
    }

    #[test]
    fn wiremock_4xx_is_not_retried() {
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/missing"))
                    .respond_with(wiremock::ResponseTemplate::new(404))
                    .mount(&server)
                    .await;
                server
            })
        });

        let url = format!("{}/missing", server.uri());
        let err = send_with_retry(&get_req(&url)).expect_err("404 应失败");
        assert!(
            format!("{err:#}").contains("404"),
            "错误里应有状态码: {err:#}"
        );

        // 关键：4xx 只发一次，不能因为分类错误白白重试三次。
        let calls = rt
            .block_on(server.received_requests())
            .expect("应有记录")
            .len();
        assert_eq!(calls, 1, "4xx 不该重试");
        drop(server);
        drop(rt);
    }

    #[test]
    fn wiremock_persistent_5xx_gives_up_after_retries() {
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/down"))
                    .respond_with(wiremock::ResponseTemplate::new(500))
                    .mount(&server)
                    .await;
                server
            })
        });

        let url = format!("{}/down", server.uri());
        let err = send_with_retry(&get_req(&url)).expect_err("应最终失败");
        assert!(
            format!("{err:#}").contains("500"),
            "错误里应有状态码: {err:#}"
        );

        let calls = rt
            .block_on(server.received_requests())
            .expect("应有记录")
            .len();
        assert_eq!(calls as u32, MAX_RETRIES + 1, "首次 + 全部重试");
        drop(server);
        drop(rt);
    }

    #[test]
    fn wiremock_fetch_requests_keeps_input_order() {
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                for i in 0..5u32 {
                    wiremock::Mock::given(wiremock::matchers::method("GET"))
                        .and(wiremock::matchers::path(format!("/p{i}")))
                        .respond_with(
                            wiremock::ResponseTemplate::new(200)
                                .set_body_string(format!("body{i}")),
                        )
                        .mount(&server)
                        .await;
                }
                server
            })
        });

        let reqs: Vec<HttpRequest> = (0..5u32)
            .map(|i| get_req(&format!("{}/p{i}", server.uri())))
            .collect();
        let refs: Vec<&HttpRequest> = reqs.iter().collect();
        let results = fetch_requests(&refs, 3, 0);

        assert_eq!(results.len(), 5);
        for (i, r) in results.iter().enumerate() {
            let resp = r.as_ref().expect("应成功");
            assert_eq!(resp.status, 200);
            // 乱序返回也必须按输入顺序归位，否则章节会串号。
            assert_eq!(String::from_utf8_lossy(&resp.body), format!("body{i}"));
        }
        drop(server);
        drop(rt);
    }

    #[test]
    fn wiremock_post_body_and_headers_round_trip() {
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                wiremock::Mock::given(wiremock::matchers::method("POST"))
                    .and(wiremock::matchers::path("/echo"))
                    .and(wiremock::matchers::header("x-test", "yes"))
                    .and(wiremock::matchers::body_string("payload"))
                    .respond_with(
                        wiremock::ResponseTemplate::new(201).insert_header("x-back", "done"),
                    )
                    .mount(&server)
                    .await;
                server
            })
        });

        let req = HttpRequest {
            method: "POST".to_string(),
            url: format!("{}/echo", server.uri()),
            headers: vec![HttpHeader {
                name: "x-test".to_string(),
                value: "yes".to_string(),
            }],
            body: Some(b"payload".to_vec()),
        };
        let resp = send_with_retry(&req).expect("应成功");
        assert_eq!(resp.status, 201);
        assert!(
            resp.headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("x-back") && h.value == "done"),
            "响应头应回传"
        );
        drop(server);
        drop(rt);
    }

    #[test]
    fn retry_classification() {
        // 5xx / 429 / 408 是瞬时问题，值得重试
        assert!(is_retryable(&anyhow!("HTTP 状态码 503: u")));
        assert!(is_retryable(&anyhow!("HTTP 状态码 429: u")));
        assert!(is_retryable(&anyhow!("HTTP 状态码 408: u")));
        // 没有状态码说明连接就没走通
        assert!(is_retryable(&anyhow!("请求失败: u")));
        // 4xx 是请求本身有问题，重试无用
        assert!(!is_retryable(&anyhow!("HTTP 状态码 404: u")));
        assert!(!is_retryable(&anyhow!("HTTP 状态码 401: u")));
    }
}
