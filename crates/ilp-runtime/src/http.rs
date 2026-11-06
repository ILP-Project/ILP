//! 可复用的受策略约束 HTTP 网关。
//!
//! 该网关只负责 URL、解析地址和响应大小边界；重试、取消、录像回放等
//! 应用策略由上层显式组合，不能在这里隐式绕过 Core 或插件能力策略。

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::{redirect::Policy, NoProxy, Proxy};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::CapabilityPolicy;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const RETRY_BASE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct HttpGatewayConfig {
    pub proxy: Option<String>,
    pub no_proxy: Vec<String>,
    pub user_agent: Option<String>,
    pub timeout: Duration,
    pub max_response_bytes: usize,
}

impl Default for HttpGatewayConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            no_proxy: Vec::new(),
            user_agent: None,
            timeout: DEFAULT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// 显式注入到宿主、CLI 或其他前端的安全 HTTP 网关。
#[derive(Debug, Clone)]
pub struct HttpGateway {
    config: HttpGatewayConfig,
    policy: CapabilityPolicy,
}

impl HttpGateway {
    pub fn new(policy: CapabilityPolicy) -> Result<Self> {
        Self::with_config(policy, HttpGatewayConfig::default())
    }

    pub fn with_limits(
        policy: CapabilityPolicy,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self> {
        Self::with_config(
            policy,
            HttpGatewayConfig {
                timeout,
                max_response_bytes,
                ..HttpGatewayConfig::default()
            },
        )
    }

    pub fn with_config(policy: CapabilityPolicy, config: HttpGatewayConfig) -> Result<Self> {
        build_client(&config, None)?;
        Ok(Self { config, policy })
    }

    pub fn send(&self, request: &GatewayRequest) -> Result<GatewayResponse> {
        self.send_cancelable(request, &|| false)
    }

    pub fn send_cancelable(
        &self,
        request: &GatewayRequest,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<GatewayResponse> {
        validate_request(&self.policy, request)?;
        let parsed = reqwest::Url::parse(&request.url)
            .with_context(|| format!("无效 URL: {}", request.url))?;
        let host = parsed.host_str().ok_or_else(|| anyhow!("URL 缺少 host"))?;
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addresses = resolve_checked(host, port)?;
        if cancelled() {
            return Err(anyhow!("HTTP 请求已取消"));
        }
        // 对所有已解析地址执行检查，并将同一组地址固定给 reqwest，避免
        // URL 校验与实际连接之间出现可利用的 DNS 重新绑定窗口。
        let client = build_client(&self.config, Some((host, &addresses)))?;
        let mut builder = client.request(
            reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|_| anyhow!("非法 HTTP 方法: {}", request.method))?,
            &request.url,
        );
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        let response = builder
            .send()
            .with_context(|| format!("HTTP 请求失败: {}", request.url))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect();
        let body = response.bytes().context("读取 HTTP 响应失败")?.to_vec();
        if body.len() > self.config.max_response_bytes {
            return Err(anyhow!("HTTP 响应超过大小限制"));
        }
        Ok(GatewayResponse {
            status,
            headers,
            body,
        })
    }

    pub fn send_batch(
        &self,
        requests: &[GatewayRequest],
        max_workers: usize,
        spacing: Duration,
        retries: u32,
        cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Vec<Result<GatewayResponse>> {
        if requests.is_empty() {
            return Vec::new();
        }
        let requests = Arc::new(requests.to_vec());
        let results = Arc::new(Mutex::new(
            (0..requests.len())
                .map(|_| None)
                .collect::<Vec<Option<Result<GatewayResponse>>>>(),
        ));
        let next = Arc::new(Mutex::new(0usize));
        let workers = max_workers.max(1).min(requests.len());
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let requests = Arc::clone(&requests);
            let results = Arc::clone(&results);
            let next = Arc::clone(&next);
            let gateway = self.clone();
            let cancelled = Arc::clone(&cancelled);
            handles.push(thread::spawn(move || {
                let mut last_started: Option<Instant> = None;
                loop {
                    let index = {
                        let mut next = next.lock().expect("batch index mutex poisoned");
                        if *next >= requests.len() {
                            break;
                        }
                        let index = *next;
                        *next += 1;
                        index
                    };
                    if cancelled() {
                        results.lock().expect("batch result mutex poisoned")[index] =
                            Some(Err(anyhow!("HTTP 请求已取消")));
                        continue;
                    }
                    if let Some(last_started) = last_started {
                        let elapsed = last_started.elapsed();
                        if elapsed < spacing {
                            let mut remaining = spacing - elapsed;
                            while !remaining.is_zero() {
                                if cancelled() {
                                    results.lock().expect("batch result mutex poisoned")[index] =
                                        Some(Err(anyhow!("HTTP 请求已取消")));
                                    break;
                                }
                                let sleep_for = remaining.min(Duration::from_millis(25));
                                thread::sleep(sleep_for);
                                remaining = spacing.saturating_sub(last_started.elapsed());
                            }
                            if cancelled() {
                                continue;
                            }
                        }
                    }
                    last_started = Some(Instant::now());
                    let result = gateway.send_with_retry(&requests[index], retries, &*cancelled);
                    results.lock().expect("batch result mutex poisoned")[index] = Some(result);
                }
            }));
        }
        for handle in handles {
            let _ = handle.join();
        }
        Arc::try_unwrap(results)
            .expect("batch workers still hold result mutex")
            .into_inner()
            .expect("batch result mutex poisoned")
            .into_iter()
            .map(|result| result.expect("batch result missing"))
            .collect()
    }

    pub fn send_with_retry(
        &self,
        request: &GatewayRequest,
        retries: u32,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<GatewayResponse> {
        let mut last = None;
        for attempt in 0..=retries {
            if cancelled() {
                return Err(anyhow!("HTTP 请求已取消"));
            }
            if attempt > 0 {
                let delay = RETRY_BASE.saturating_mul(1u32 << (attempt - 1));
                let started = Instant::now();
                while started.elapsed() < delay {
                    if cancelled() {
                        return Err(anyhow!("HTTP 请求已取消"));
                    }
                    thread::sleep(Duration::from_millis(25).min(delay - started.elapsed()));
                }
            }
            match self.send_cancelable(request, cancelled) {
                Ok(response) if response.status < 400 => return Ok(response),
                Ok(response) => {
                    let error = anyhow!("HTTP 状态码 {}: {}", response.status, request.url);
                    if !retryable_status(response.status) {
                        return Err(error);
                    }
                    last = Some(error);
                }
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("HTTP 请求失败: {}", request.url)))
    }
}

fn build_client(config: &HttpGatewayConfig, pin: Option<(&str, &[SocketAddr])>) -> Result<Client> {
    let mut builder = ClientBuilder::new()
        .timeout(config.timeout)
        .redirect(Policy::none());
    if let Some(user_agent) = &config.user_agent {
        builder = builder.user_agent(user_agent);
    }
    if let Some(proxy) = &config.proxy {
        let no_proxy = if config.no_proxy.is_empty() {
            None
        } else {
            NoProxy::from_string(&config.no_proxy.join(","))
        };
        builder = builder.proxy(Proxy::all(proxy)?.no_proxy(no_proxy));
    } else {
        builder = builder.no_proxy();
    }
    if let Some((host, addresses)) = pin {
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder.build().context("构建共享 HTTP 网关失败")
}

fn retryable_status(status: u16) -> bool {
    status >= 500 || status == 408 || status == 429
}

fn validate_request(policy: &CapabilityPolicy, request: &GatewayRequest) -> Result<()> {
    if request
        .body
        .as_ref()
        .is_some_and(|body| body.len() > DEFAULT_MAX_RESPONSE_BYTES)
    {
        return Err(anyhow!("HTTP 请求体超过大小限制"));
    }
    policy.check_url(&request.url)
}

fn resolve_checked(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        if forbidden_ip(ip) {
            return Err(anyhow!("禁止访问内网或特殊 IP: {host}"));
        }
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addresses: Vec<_> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("解析域名失败: {host}"))?
        .collect();
    if addresses.is_empty() {
        return Err(anyhow!("域名无解析结果: {host}"));
    }
    if addresses.iter().any(|address| forbidden_ip(address.ip())) {
        return Err(anyhow!("域名解析到禁止访问的地址: {host}"));
    }
    Ok(addresses)
}

fn forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => {
            value.is_loopback()
                || value.is_private()
                || value.is_link_local()
                || value.is_unspecified()
                || value.is_broadcast()
                || value.octets()[0] == 0
        }
        IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| forbidden_ip(IpAddr::V4(mapped)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_rejects_non_https_and_unallowed_domains() {
        let policy = CapabilityPolicy::deny_all().allow_domain("example.com");
        let gateway = HttpGateway::new(policy).expect("gateway should build");
        let error = gateway
            .send(&GatewayRequest {
                method: "GET".into(),
                url: "http://example.com/".into(),
                headers: Vec::new(),
                body: None,
            })
            .expect_err("non-HTTPS request must be rejected");
        assert!(error.to_string().contains("HTTPS"));
    }

    #[test]
    fn gateway_rejects_private_literal_addresses() {
        let policy = CapabilityPolicy::deny_all().allow_domain("127.0.0.1");
        let gateway = HttpGateway::new(policy).expect("gateway should build");
        let error = gateway
            .send(&GatewayRequest {
                method: "GET".into(),
                url: "https://127.0.0.1/".into(),
                headers: Vec::new(),
                body: None,
            })
            .expect_err("private address must be rejected");
        assert!(error.to_string().contains("禁止访问"));
    }

    #[test]
    fn batch_preserves_input_order_when_cancelled() {
        let gateway = HttpGateway::new(CapabilityPolicy::deny_all()).expect("gateway should build");
        let requests = vec![
            GatewayRequest {
                method: "GET".into(),
                url: "https://example.com/one".into(),
                headers: Vec::new(),
                body: None,
            },
            GatewayRequest {
                method: "GET".into(),
                url: "https://example.com/two".into(),
                headers: Vec::new(),
                body: None,
            },
        ];
        let cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(|| true);
        let results = gateway.send_batch(&requests, 2, Duration::ZERO, 0, cancelled);
        assert_eq!(results.len(), requests.len());
        assert!(results.iter().all(|result| result.is_err()));
    }
}
