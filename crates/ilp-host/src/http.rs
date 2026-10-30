use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::{redirect::Policy, Method, NoProxy, Proxy};

use crate::engine::wit_types::{HttpHeader, HttpRequest, HttpResponse};
use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const TIMEOUT_SECS: u64 = 30;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// 每个请求最多再试几次（不含首次）。网络抖动与瞬时 5xx 都能靠它自愈。
const MAX_RETRIES: u32 = 2;

/// 退避基数：第 n 次重试等 BASE * 2^n 毫秒。
const RETRY_BASE_MS: u64 = 500;

/// 全局客户端：代理与 UA 只解析一次，避免每个请求都建连接池。
static CLIENT: OnceLock<Client> = OnceLock::new();
static INIT: OnceLock<()> = OnceLock::new();
/// 初始化时用的代理配置，后续构建“钉死 IP”的客户端要复制它。
static PROXY_CONFIG: OnceLock<(Option<String>, Vec<String>)> = OnceLock::new();
/// 按 (host, ip) 缓存的客户端。同一目标反复请求时靠它复用连接池。
type PinnedClients = Mutex<HashMap<(String, SocketAddr), Arc<Client>>>;
static PINNED: OnceLock<PinnedClients> = OnceLock::new();

/// 钉 IP 客户端的缓存上限。
///
/// 缓存是“主机 + 具体 IP”维度的，插件可以拿大量不同域名把它撞大；
/// 满额时整表清空比 LRU 简单，代价只是丢一次连接池。
const MAX_PINNED_CLIENTS: usize = 64;

/// 用配置初始化全局客户端，必须在任何请求前调用一次。
pub fn init(proxy: Option<&str>, no_proxy: &[String]) -> Result<()> {
    if INIT.get().is_some() {
        return Ok(());
    }
    let client = build_client(proxy, no_proxy)?;
    let _ = PROXY_CONFIG.set((proxy.map(str::to_string), no_proxy.to_vec()));
    let _ = CLIENT.set(client);
    let _ = INIT.set(());
    Ok(())
}

fn build_client(proxy: Option<&str>, no_proxy: &[String]) -> Result<Client> {
    build_pinned(proxy, no_proxy, None)
}

/// `pin` 为 `Some((host, addr))` 时把该主机的解析结果钉死到 `addr`。
fn build_pinned(
    proxy: Option<&str>,
    no_proxy: &[String],
    pin: Option<(&str, &[SocketAddr])>,
) -> Result<Client> {
    let mut builder: ClientBuilder = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .redirect(Policy::none())
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

    if let Some((host, addrs)) = pin {
        // 一次钉上整个解析结果：某条地址连不通时 reqwest 还能换下一条，
        // 同时保留“每条都被校验过”这个前提。
        builder = builder.resolve_to_addrs(host, addrs);
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

/// 一个 IP 是否属于绝不允许插件访问的范围。
///
/// 单独抽出来，是因为它同时服务两个阶段：URL 字面量检查，
/// 以及 DNS 解析后的每一个结果。
fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                // 0.0.0.0/8 在很多系统上指向本机
                || ip.octets()[0] == 0
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                // IPv4-mapped：`::ffff:127.0.0.1` 必须按 v4 规则判
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_forbidden_ip(IpAddr::V4(v4)))
        }
    }
}

/// 解析目标主机，返回所有通过校验的地址。
///
/// 这是堵 DNS rebinding 的关键：只在 URL 里看到主机名就放行，等于让攻击者
/// 把 `evil.example` 先解析成公网 IP 骗过检查，再让它指向 127.0.0.1。
/// 所以必须**自己解析一次**、逐条校验，再把结果钉给 reqwest——
/// 让真正建立连接用的 IP 就是我检查过的那些。
pub fn resolve_checked(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    resolve_with(host, port, |host, port| {
        (host, port)
            .to_socket_addrs()
            .map(|addrs| addrs.collect::<Vec<_>>())
            .with_context(|| format!("解析域名失败: {host}"))
    })
}

/// `resolve_checked` 的实现，查找函数可注入以便离线测试。
fn resolve_with<F>(host: &str, port: u16, lookup: F) -> Result<Vec<SocketAddr>>
where
    F: Fn(&str, u16) -> Result<Vec<SocketAddr>>,
{
    // 字面量 IP 不需要解析，但同样要校验。
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err(anyhow!("禁止访问内网或特殊 IP: {host}"));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs = lookup(host, port)?;
    if addrs.is_empty() {
        return Err(anyhow!("域名无解析结果: {host}"));
    }
    // 任何一条落到内网就整体拒绝：攻击者可以拿一条合法记录当门面，
    // 真正被连中的却是另一条。
    if let Some(bad) = addrs.iter().find(|a| is_forbidden_ip(a.ip())) {
        return Err(anyhow!("域名 {host} 解析到禁止访问的地址 {}", bad.ip()));
    }
    Ok(addrs)
}

/// 取一个把 `host` 钉死到指定地址的客户端。
///
/// 已知限制：配了代理时，连接是发给代理的，域名由代理侧解析，
/// 这里的钉 IP 不生效。所以代理场景下防线只剩 `resolve_checked`
/// 本次解析的校验（TOCTOU 窗口依赖代理自己的解析策略）。
/// 对敏感目标请同时把它放进 `no_proxy`，走直连钉 IP 路径。
fn pinned_client(host: &str, addrs: &[SocketAddr]) -> Result<Arc<Client>> {
    let cache = PINNED.get_or_init(|| Mutex::new(HashMap::new()));
    // 缓存键用第一条地址即可：同一次解析的多条地址总是一起出现。
    let key = (host.to_string(), addrs[0]);
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = guard.get(&key).cloned() {
            return Ok(c);
        }
    }
    let (proxy, no_proxy) = PROXY_CONFIG
        .get()
        .cloned()
        .unwrap_or_else(|| (None, Vec::new()));
    let client = Arc::new(build_pinned(
        proxy.as_deref(),
        &no_proxy,
        Some((host, addrs)),
    )?);
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    // 满额就整表清空，只丢一次连接池，不报错。
    if guard.len() >= MAX_PINNED_CLIENTS {
        guard.clear();
    }
    guard.insert(key, Arc::clone(&client));
    Ok(client)
}

/// 发请求时用的客户端：能钉就钉，恒量目标（字面量 IP）直接复用全局客户端。
fn client_for(url: &reqwest::Url) -> Result<Arc<Client>> {
    let host = url.host_str().ok_or_else(|| anyhow!("URL 缺少主机名"))?;
    let port = url.port_or_known_default().unwrap_or(443);
    if host.parse::<IpAddr>().is_ok() {
        return Ok(Arc::new(client().clone()));
    }
    let addrs = resolve_checked(host, port)?;
    pinned_client(host, &addrs)
}

pub fn get_bytes(url: &str) -> Result<Vec<u8>> {
    crate::cancel::check()?;
    let parsed = reqwest::Url::parse(url).with_context(|| format!("无效 URL: {url}"))?;
    let resp = client_for(&parsed)?
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    let bytes =
        read_limited(resp, MAX_RESPONSE_BYTES).with_context(|| format!("读取响应失败: {url}"))?;
    Ok(bytes)
}

/// 取一个可能不存在的资源：404 返回 `Ok(None)`，其它错误照常上报。
///
/// 给“可选的旁路文件”用（比如 registry 的分离签名）。不能用它区分
/// “不存在”和“网络坏了”——后者必须报错，否则断网会被当成“没有签名”。
pub fn get_bytes_if_exists(url: &str) -> Result<Option<Vec<u8>>> {
    crate::cancel::check()?;
    let parsed = reqwest::Url::parse(url).with_context(|| format!("无效 URL: {url}"))?;
    let resp = client_for(&parsed)?
        .get(url)
        .send()
        .with_context(|| format!("请求失败: {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        return Err(anyhow!("HTTP 状态码 {}: {url}", resp.status()));
    }
    let bytes =
        read_limited(resp, MAX_RESPONSE_BYTES).with_context(|| format!("读取响应失败: {url}"))?;
    Ok(Some(bytes))
}

/// 把插件给的请求发给对端，返回 WIT 形状的响应。
pub fn send(request: &HttpRequest) -> Result<HttpResponse> {
    // 取消后不再发起新请求：已经排队的任务没必要再把请求打出去。
    crate::cancel::check()?;
    // 回放模式下完全离线：命中录像就返回，未命中直接报错（不偷偷联网）。
    if let Some(replayed) = crate::replay::try_replay(request) {
        return replayed;
    }
    let response = send_live(request)?;
    crate::replay::record(request, &response);
    Ok(response)
}

/// 真正发一次网络请求。
fn send_live(request: &HttpRequest) -> Result<HttpResponse> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|_| anyhow!("非法 HTTP 方法: {}", request.method))?;
    // 这里再解析一次 DNS 并把结果钉给客户端：上层的白名单只看了主机名，
    // 真正决定连到哪里的是解析结果。
    let parsed =
        reqwest::Url::parse(&request.url).with_context(|| format!("无效 URL: {}", request.url))?;
    let mut req = client_for(&parsed)?
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
    let body = read_limited(resp, MAX_RESPONSE_BYTES)
        .with_context(|| format!("读取响应失败: {}", request.url))?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn read_limited(response: reqwest::blocking::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(anyhow!("HTTP response 过大: 超过 {limit} 字节"));
    }
    let mut body = Vec::new();
    response
        .take((limit + 1) as u64)
        .read_to_end(&mut body)
        .context("读取 HTTP 响应失败")?;
    if body.len() > limit {
        return Err(anyhow!("HTTP response 过大: 超过 {limit} 字节"));
    }
    Ok(body)
}

/// Reject destinations that must never be reachable by plugins.
pub fn validate_plugin_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).context("invalid URL")?;
    if parsed.scheme() != "https" {
        return Err(anyhow!("只允许 HTTPS 请求"));
    }
    let host = parsed.host_str().ok_or_else(|| anyhow!("URL 缺少主机名"))?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
        return Err(anyhow!("禁止访问 localhost"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err(anyhow!("禁止访问内网或特殊 IP"));
        }
    }
    Ok(())
}

/// 一次请求是否值得重试。
///
/// 4xx（除 408/429）是我们发错了，重试没有意义；
/// 网络错误、超时、5xx、429 都是瞬时问题，值得再试。
fn is_retryable(err: &anyhow::Error) -> bool {
    // 用户取消不是瞬时故障，重试只会拖慢退出。
    if err.downcast_ref::<crate::cancel::Cancelled>().is_some() {
        return false;
    }
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
    // 回放模式下“失败”是确定性的（录像就是缺），重试三次只会得到三个同样错误。
    let retries = if crate::replay::is_replaying() {
        0
    } else {
        MAX_RETRIES
    };
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..=retries {
        if attempt > 0 {
            let backoff = RETRY_BASE_MS * (1u64 << (attempt - 1));
            if crate::cancel::sleep(Duration::from_millis(backoff)) {
                return Err(anyhow::Error::new(crate::cancel::Cancelled));
            }
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
                // 取消后还没开跑的任务直接记成取消，不再占着连接。
                if crate::cancel::requested() {
                    results.lock().unwrap()[idx] =
                        Some(Err(anyhow::Error::new(crate::cancel::Cancelled)));
                    continue;
                }
                if sleep_ms > 0 {
                    if let Some(prev) = last {
                        let elapsed = prev.elapsed();
                        let gap = Duration::from_millis(sleep_ms);
                        if elapsed < gap && crate::cancel::sleep(gap - elapsed) {
                            results.lock().unwrap()[idx] =
                                Some(Err(anyhow::Error::new(crate::cancel::Cancelled)));
                            break;
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
    fn rejects_private_and_redirect_urls() {
        assert!(validate_plugin_url("https://127.0.0.1/api").is_err());
        assert!(validate_plugin_url("https://localhost/api").is_err());
        assert!(validate_plugin_url("http://example.com").is_err());
        assert!(validate_plugin_url("https://example.com/api").is_ok());
    }

    #[test]
    fn forbidden_ip_ranges_are_rejected() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "0.1.2.3",
            "255.255.255.255",
            "192.0.2.1",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            // IPv4-mapped 是最容易被漏掉的一种写法。
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(
                is_forbidden_ip(ip.parse().expect("测试 IP 合法")),
                "{ip} 应该被拒绝"
            );
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(
                !is_forbidden_ip(ip.parse().expect("测试 IP 合法")),
                "{ip} 应该放行"
            );
        }
    }

    /// DNS rebinding 的核心用例：主机名本身没问题，解析结果才暴露攻击。
    #[test]
    fn rebinding_domain_resolving_to_loopback_is_rejected() {
        let err = resolve_with("evil.example", 443, |_, port| {
            Ok(vec![SocketAddr::from(([93, 184, 216, 34], port))])
        });
        assert!(err.is_ok(), "公网地址应通过");

        let err = resolve_with("rebind.example", 443, |_, port| {
            Ok(vec![
                SocketAddr::from(([93, 184, 216, 34], port)),
                SocketAddr::from(([127, 0, 0, 1], port)),
            ])
        })
        .expect_err("解析到回环必须拒绝");
        assert!(format!("{err:#}").contains("127.0.0.1"));
    }

    #[test]
    fn empty_resolution_is_rejected() {
        let err = resolve_with("empty.example", 443, |_, _| Ok(Vec::new()))
            .expect_err("无解析结果必须拒绝");
        assert!(format!("{err:#}").contains("无解析结果"));
    }

    #[test]
    fn literal_private_ip_skips_lookup_and_is_rejected() {
        let err = resolve_with("10.0.0.5", 443, |_, _| {
            panic!("字面量 IP 不应该触发 DNS 查询")
        })
        .expect_err("字面量内网 IP 必须拒绝");
        assert!(format!("{err:#}").contains("内网"));
        let ok = resolve_with("1.1.1.1", 443, |_, _| {
            panic!("字面量 IP 不应该触发 DNS 查询")
        })
        .expect("公网字面量 IP 应通过");
        assert_eq!(ok[0].port(), 443);
    }

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

    /// “可选资源”不能被网络故障伪装成“不存在”。
    ///
    /// 若 404 和其它错误混淆，registry 签名校验会在断网时静默跳过，
    /// 那等于关掉了整条元数据信任链。
    #[test]
    fn optional_fetch_reports_missing_but_not_broken() {
        let _guard = crate::cancel::test_guard();
        let rt = rt();
        let server = start_server(&rt, |server| {
            Box::pin(async move {
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/present.sig"))
                    .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("sig"))
                    .mount(&server)
                    .await;
                wiremock::Mock::given(wiremock::matchers::method("GET"))
                    .and(wiremock::matchers::path("/gone.sig"))
                    .respond_with(wiremock::ResponseTemplate::new(404))
                    .mount(&server)
                    .await;
                server
            })
        });

        let present = get_bytes_if_exists(&format!("{}/present.sig", server.uri()))
            .expect("200 应成功")
            .expect("应有内容");
        assert_eq!(present, b"sig");

        let missing =
            get_bytes_if_exists(&format!("{}/gone.sig", server.uri())).expect("404 不是错误");
        assert!(missing.is_none(), "404 应表示不存在");

        // 指向一个不该解析的地址：必须报错，而不是 Ok(None)。
        let broken = get_bytes_if_exists("https://127.0.0.1/sig");
        assert!(broken.is_err(), "网络/地址错误不能退化成“不存在”");
        drop(server);
        drop(rt);
    }

    #[test]
    fn wiremock_5xx_is_retried_then_succeeds() {
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
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
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
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
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
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
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
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
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
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
        // 用户取消不是瞬时故障，不能拿去重试
        assert!(!is_retryable(&anyhow::Error::new(crate::cancel::Cancelled)));
    }

    #[test]
    fn cancelled_request_is_not_sent() {
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
        crate::cancel::request();
        let request = HttpRequest {
            method: "GET".into(),
            url: "https://example.com/a".into(),
            headers: Vec::new(),
            body: None,
        };
        let err = send(&request).expect_err("取消后不应再发请求");
        assert!(err.downcast_ref::<crate::cancel::Cancelled>().is_some());
    }

    #[test]
    fn queued_requests_after_cancel_are_marked_cancelled() {
        // 取消标志是进程级的，碰请求的用例必须串行。
        let _guard = crate::cancel::test_guard();
        crate::cancel::request();
        let requests: Vec<HttpRequest> = (0..3)
            .map(|i| HttpRequest {
                method: "GET".into(),
                url: format!("https://example.com/{i}"),
                headers: Vec::new(),
                body: None,
            })
            .collect();
        let refs: Vec<&HttpRequest> = requests.iter().collect();
        let results = fetch_requests(&refs, 2, 0);
        assert_eq!(results.len(), 3);
        assert!(
            results.iter().all(|r| r
                .as_ref()
                .err()
                .is_some_and(|e| e.downcast_ref::<crate::cancel::Cancelled>().is_some())),
            "排队中的请求应直接标为取消"
        );
    }
}
