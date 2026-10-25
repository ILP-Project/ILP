use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::{NoProxy, Proxy};
use std::sync::OnceLock;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const TIMEOUT_SECS: u64 = 30;

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

type FetchResults = Arc<Mutex<Vec<Option<Result<Vec<u8>>>>>>;

/// 有界 worker + 任务间限速。
///
/// 限速在取下一个任务**之前**执行：原版把 sleep 放在任务完成后，导致
/// 休眠期间仍占着并发槽，实际吞吐远低于 max_workers。
pub fn fetch_many(urls: &[String], max_workers: usize, sleep_ms: u64) -> Vec<Result<Vec<u8>>> {
    let n = urls.len();
    if n == 0 {
        return Vec::new();
    }

    let urls = Arc::new(urls.to_vec());
    let results: FetchResults = Arc::new(Mutex::new((0..n).map(|_| None).collect()));
    let (tx, rx) = mpsc::channel::<usize>();
    let rx = Arc::new(Mutex::new(rx));

    let worker_count = max_workers.max(1).min(n);
    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let rx = Arc::clone(&rx);
        let results = Arc::clone(&results);
        let urls = Arc::clone(&urls);
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
                let res = get_bytes(&urls[idx]);
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
    fn empty_url_list_returns_empty() {
        assert!(fetch_many(&[], 4, 0).is_empty());
    }
}
