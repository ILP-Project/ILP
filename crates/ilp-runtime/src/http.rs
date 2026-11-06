//! 可复用的受策略约束 HTTP 网关。
//!
//! 该网关只负责 URL、解析地址和响应大小边界；重试、取消、录像回放等
//! 应用策略由上层显式组合，不能在这里隐式绕过 Core 或插件能力策略。

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest::redirect::Policy;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use crate::CapabilityPolicy;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

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
    client: Client,
    policy: CapabilityPolicy,
    max_response_bytes: usize,
}

impl HttpGateway {
    pub fn new(policy: CapabilityPolicy) -> Result<Self> {
        Self::with_limits(policy, DEFAULT_TIMEOUT, DEFAULT_MAX_RESPONSE_BYTES)
    }

    pub fn with_limits(
        policy: CapabilityPolicy,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self> {
        let client = ClientBuilder::new()
            .timeout(timeout)
            .redirect(Policy::none())
            .build()
            .context("构建共享 HTTP 网关失败")?;
        Ok(Self {
            client,
            policy,
            max_response_bytes,
        })
    }

    pub fn send(&self, request: &GatewayRequest) -> Result<GatewayResponse> {
        validate_request(&self.policy, request)?;
        let parsed = reqwest::Url::parse(&request.url)
            .with_context(|| format!("无效 URL: {}", request.url))?;
        let host = parsed.host_str().ok_or_else(|| anyhow!("URL 缺少 host"))?;
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addresses = resolve_checked(host, port)?;
        let client = self.client.clone();
        let mut builder = client.request(
            reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|_| anyhow!("非法 HTTP 方法: {}", request.method))?,
            &request.url,
        );
        // 对所有已解析地址执行检查，并将同一组地址固定给 reqwest，避免
        // URL 校验与实际连接之间出现可利用的 DNS 重新绑定窗口。
        if host.parse::<IpAddr>().is_err() {
            builder = ClientBuilder::new()
                .timeout(DEFAULT_TIMEOUT)
                .redirect(Policy::none())
                .resolve_to_addrs(host, &addresses)
                .build()
                .context("构建固定解析地址的 HTTP 客户端失败")?
                .request(
                    reqwest::Method::from_bytes(request.method.as_bytes())
                        .map_err(|_| anyhow!("非法 HTTP 方法: {}", request.method))?,
                    &request.url,
                );
        }
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
        if body.len() > self.max_response_bytes {
            return Err(anyhow!("HTTP 响应超过大小限制"));
        }
        Ok(GatewayResponse {
            status,
            headers,
            body,
        })
    }
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
}
