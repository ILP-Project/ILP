//! 插件开发的离线回放宿主。
//!
//! 目标：让插件作者**不发一个真实请求**就能跑通完整流程。
//! 插件是沙箱，所有网络都经过 `http::send`，所以只要在这一层拦截，
//! 就能把真实站点换成录像。
//!
//! 两个环境变量：
//!
//! - `ILP_REPLAY_DIR=<dir>`：从录像读，**不回退到网络**；
//! - `ILP_RECORD_DIR=<dir>`：放行网络，并把请求/响应写成录像。
//!
//! 回放缺失时报错而不是偷偷联网，是刻意的：一个"本该离线"的测试静默
//! 打到真实站点，既不可复现，也可能触发风控。宁可红。
//!
//! 录像格式与插件模板里的 fixture 同为 schema v1，请求与响应分离，
//! body 用 base64（正文可能是 gzip 二进制，不能塞进 JSON 字符串）。

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::engine::wit_types::{HttpHeader, HttpRequest, HttpResponse};

/// 录像里的值被替换成它，既保留了"这个头存在"的事实，又不留原文。
pub const REDACTED: &str = "<redacted>";

/// 一律脱敏的请求/响应头（子串匹配）。
///
/// 回放时我们是服务器，不需要真实凭据；而录像要进 git、进 PR，
/// 带 Cookie 的录像等于把用户凭据公开。宁可录进去也看不出来。
///
/// 用子串而不是固定名单：固定名单一定漏。真实测试里第一次录制就漏掉了
/// `x-ms-token`（CDN 防盗链票据），名单只能不断补，子串一次就能拦住。
const SECRET_HEADER_MARKERS: &[&str] = &[
    "authorization",
    "cookie",
    "token",
    "secret",
    "password",
    "api-key",
    "apikey",
    "session",
    "signature",
];

/// 一律脱敏的 query 参数名（子串匹配）。
///
/// 站点常把 token 直接写在 URL 里，只脱敏请求头会漏掉一半。
///
/// `iid` / `install_id` 属于“设备身份”：它们每次都重新生成，既不该进录像
/// （设备指纹），又会让匹配键随实例漂移。把它们脱敏后，同一个逻辑请求
/// 在录与放两侧能对上。
const SECRET_PARAM_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "cookie",
    "session",
    "sign",
    "auth",
    "device_id",
    "deviceid",
    "install_id",
    "iid",
];

fn is_secret_header(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    SECRET_HEADER_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// 按字符（不是字节）截断，避免把 UTF-8 切坏。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…（共 {} 字符）", text.chars().count())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Cassette {
    pub schema_version: u32,
    pub request: RecordedRequest,
    pub response: RecordedResponse,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub url: String,
    /// BTreeMap 让序列化结果稳定：录像进 git 后，键序变化会产生无意义 diff。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_base64: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecordedResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body_base64: String,
}

impl RecordedResponse {
    pub fn body(&self) -> Result<Vec<u8>> {
        BASE64
            .decode(&self.body_base64)
            .context("录像的 response body 不是有效 base64")
    }
}

/// 把请求头里敏感的几项换成占位符。
fn redact_headers(headers: &[HttpHeader]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|h| {
            let value = if is_secret_header(&h.name) {
                REDACTED.to_string()
            } else {
                h.value.clone()
            };
            (h.name.to_ascii_lowercase(), value)
        })
        .collect()
}

fn is_secret_param(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    SECRET_PARAM_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// 把 URL 里的敏感 query 值换成占位符，其余部分保持原样。
///
/// 录像与匹配两侧都调用它，所以"录的时候带着真实 token、回放时插件换了
/// 新 token"照样能匹配上——这正是插件测试里常见的形态。
fn redact_url(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) if is_secret_param(k) => format!("{k}={REDACTED}"),
            _ => pair.to_string(),
        })
        .collect();
    format!("{head}?{}", pairs.join("&"))
}

/// 严格匹配键：方法 + 脱敏后的 URL + 请求体摘要。
///
/// 不看请求头：签名、时间戳、nonce 每次都会变，算进去会让录像永远匹配不上。
fn match_key(method: &str, url: &str, body: Option<&[u8]>) -> String {
    let mut key = format!("{} {}", method.to_ascii_uppercase(), redact_url(url));
    if let Some(body) = body {
        let mut hasher = Sha256::new();
        hasher.update(body);
        key.push_str(&format!(" body:{:x}", hasher.finalize()));
    }
    key
}

/// 宽松匹配键：方法 + 脱敏后的 URL，不含 body。
///
/// 存在的理由：设备注册、埋点这类接口的请求体里带着每次新生成的随机值，
/// 严格匹配永远对不上。但放宽到“同一 URL 就同一条录像”会掩盖真正的
/// 请求差异，所以只在严格匹配失败后才用，并且必须唯一命中。
fn loose_key(method: &str, url: &str) -> String {
    format!("{} {}", method.to_ascii_uppercase(), redact_url(url))
}

fn cassette_name(key: &str) -> String {
    // 文件名要能一眼看出是什么请求，同时不能带 `/`、`?` 这类字符。
    let readable: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let readable = readable.trim_matches('-').to_string();
    let readable: String = readable.chars().take(48).collect();
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{readable}-{}.json", &digest[..12])
}

/// 回放：从目录里的录像应答。
#[derive(Debug)]
pub struct Replay {
    /// 严格键 → 响应。
    by_key: HashMap<String, RecordedResponse>,
    /// 宽松键 → 该 URL 下所有录像的响应；只有唯一一条时才允许使用。
    by_url: HashMap<String, Vec<RecordedResponse>>,
    dir: PathBuf,
}

impl Replay {
    pub fn load(dir: &Path) -> Result<Self> {
        let mut by_key = HashMap::new();
        let mut by_url: HashMap<String, Vec<RecordedResponse>> = HashMap::new();
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("读取录像目录失败: {}", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("读取录像失败: {}", path.display()))?;
            let cassette: Cassette = serde_json::from_str(&text)
                .with_context(|| format!("解析录像失败: {}", path.display()))?;
            if cassette.schema_version != 1 {
                return Err(anyhow!(
                    "录像 {} 的 schema_version 是 {}，本版本只支持 1",
                    path.display(),
                    cassette.schema_version
                ));
            }
            let body = cassette
                .request
                .body_base64
                .as_deref()
                .map(|b| BASE64.decode(b).unwrap_or_else(|_| b.as_bytes().to_vec()));
            let key = match_key(
                &cassette.request.method,
                &cassette.request.url,
                body.as_deref(),
            );
            let loose = loose_key(&cassette.request.method, &cassette.request.url);
            by_url
                .entry(loose)
                .or_default()
                .push(cassette.response.clone());
            by_key.insert(key, cassette.response);
        }
        Ok(Self {
            by_key,
            by_url,
            dir: dir.to_path_buf(),
        })
    }

    /// 当前是否包含任何录像。
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn respond(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let key = match_key(&request.method, &request.url, request.body.as_deref());
        let found = self.by_key.get(&key).or_else(|| {
            // 严格匹配失败后允许“同 URL 唯一录像”兑底，解决设备注册、
            // 埋点这类每次请求体都不同的接口。非唯一时宁可报错：
            // 猜错一条录像会让插件拿到完全无关的响应，比缺录像难查得多。
            let loose = loose_key(&request.method, &request.url);
            match self.by_url.get(&loose) {
                Some(list) if list.len() == 1 => {
                    tracing::debug!("回放：按宽松键命中（请求体不同）: {loose}");
                    list.first()
                }
                _ => None,
            }
        });
        let Some(recorded) = found else {
            // 报错里给出可用项，插件作者才知道该补哪条录像。
            // 但 URL 可能长达几千字符（批量接口把章节 id 全列在 query 里），
            // 原样打印会把真正的错误信息淹没，所以截断并限量。
            let mut available: Vec<&str> = self.by_key.keys().map(String::as_str).collect();
            available.sort();
            let preview: Vec<String> = available.iter().take(5).map(|k| truncate(k, 140)).collect();
            return Err(anyhow!(
                "回放模式：{} 没有匹配的录像\n  请求: {}\n  录像目录: {}\n  可用录像({}): {}{}",
                self.dir.display(),
                truncate(&key, 200),
                self.dir.display(),
                available.len(),
                preview.join("\n              "),
                if available.len() > preview.len() {
                    "\n              …"
                } else {
                    ""
                }
            ));
        };
        Ok(HttpResponse {
            status: recorded.status,
            headers: recorded
                .headers
                .iter()
                .map(|(name, value)| HttpHeader {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            body: recorded.body()?,
        })
    }
}

/// 录制：把请求与（脱敏后的）响应写进目录。
pub struct Recorder {
    dir: PathBuf,
}

impl Recorder {
    pub fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建录像目录失败: {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    pub fn record(&self, request: &HttpRequest, response: &HttpResponse) -> Result<PathBuf> {
        let body = request.body.as_deref();
        let key = match_key(&request.method, &request.url, body);
        let cassette = Cassette {
            schema_version: 1,
            request: RecordedRequest {
                method: request.method.to_ascii_uppercase(),
                url: redact_url(&request.url),
                headers: redact_headers(&request.headers),
                body_base64: request.body.as_ref().map(|b| BASE64.encode(b)),
            },
            response: RecordedResponse {
                status: response.status,
                headers: redact_headers(&response.headers),
                body_base64: BASE64.encode(&response.body),
            },
            notes: format!("由 ILP_RECORD_DIR 录制；匹配键 {key}"),
        };
        let text = serde_json::to_string_pretty(&cassette)? + "\n";
        // 兜底自检：脱敏逻辑若被改坏，这里要挡住，而不是把凭据写进 git。
        assert_no_secrets(&cassette, &text)?;

        let path = self.dir.join(cassette_name(&key));
        // 同名录像视为同一请求，直接覆盖：重复录制不应该产生一堆近似文件。
        std::fs::write(&path, text).with_context(|| format!("写入录像失败: {}", path.display()))?;
        Ok(path)
    }
}

/// 录像内容不得出现明文凭据。
///
/// 这里只查最容易漏的几种形态——它不是完整的 secret scanner，
/// `plugins/tools/scan_fixtures.py` 会在 CI 里做更全面的检查。
fn assert_no_secrets(cassette: &Cassette, serialized: &str) -> Result<()> {
    // 逐头检查：名字命中敏感标记的，值必须是占位符。
    for (side, headers) in [
        ("request", &cassette.request.headers),
        ("response", &cassette.response.headers),
    ] {
        for (name, value) in headers {
            if is_secret_header(name) && value != REDACTED {
                return Err(anyhow!(
                    "录像的 {side} 头 {name} 未被脱敏（值长度 {}），已拒绝写入",
                    value.len()
                ));
            }
        }
    }
    // 请求体里带凭据（登录/换 token 类接口）同样不能落盘。
    if let Some(body) = &cassette.request.body_base64 {
        if let Ok(decoded) = BASE64.decode(body) {
            let text = String::from_utf8_lossy(&decoded).to_ascii_lowercase();
            for marker in SECRET_PARAM_MARKERS {
                if text.contains(&format!("\"{marker}\"")) {
                    return Err(anyhow!(
                        "录像的 request body 包含疑似凭据字段 {marker}，已拒绝写入。\
                         请手写一条脱敏 fixture 代替"
                    ));
                }
            }
        }
    }
    // 响应正文里也可能带着会话票据。
    const FORBIDDEN_IN_BODY: &[&str] =
        &["passport_csrf_token=", "\"access_token\":", "\"password\":"];
    let lowered = serialized.to_ascii_lowercase();
    for needle in FORBIDDEN_IN_BODY {
        if lowered.contains(needle) {
            return Err(anyhow!(
                "录制结果疑似包含明文凭据（命中 {needle}），已拒绝写入。\
                 请先用脱敏过的 fixture 代替真实响应"
            ));
        }
    }
    Ok(())
}

enum Mode {
    Replay(Replay),
    Record(Recorder),
}

static MODE: OnceLock<Option<Mode>> = OnceLock::new();

fn mode() -> Option<&'static Mode> {
    MODE.get_or_init(|| {
        // 回放优先级高于录制：同时设置时应当离线跑，而不是一边联网一边录。
        if let Ok(dir) = std::env::var("ILP_REPLAY_DIR") {
            if !dir.is_empty() {
                return match Replay::load(Path::new(&dir)) {
                    Ok(replay) => {
                        // 空目录也能加载成功，但那意味着“每条录像都未命中”，
                        // 插件作者很容易以为是插件写错了，所以先说清楚。
                        if replay.is_empty() {
                            tracing::warn!(
                                "ILP_REPLAY_DIR={dir} 里没有可用录像，所有请求都会报未命中"
                            );
                        }
                        tracing::info!("插件 HTTP 回放已启用（目录 {dir}）");
                        Some(Mode::Replay(replay))
                    }
                    Err(e) => {
                        tracing::error!("加载插件录像失败: {e:#}");
                        None
                    }
                };
            }
        }
        if let Ok(dir) = std::env::var("ILP_RECORD_DIR") {
            if !dir.is_empty() {
                return match Recorder::new(Path::new(&dir)) {
                    Ok(recorder) => Some(Mode::Record(recorder)),
                    Err(e) => {
                        tracing::error!("初始化录像目录失败: {e:#}");
                        None
                    }
                };
            }
        }
        None
    })
    .as_ref()
}

/// 当前是否处于回放模式。
///
/// 回放时不该再写 manifest、不该走真实重试语义，调用方靠它区分。
pub fn is_replaying() -> bool {
    matches!(mode(), Some(Mode::Replay(_)))
}

/// 若启用了回放，用录像应答；返回 `None` 表示应当走真实网络。
///
/// 未匹配到录像时**返回错误而不是 None**：回放模式下偷偷联网，
/// 会让测试变得依赖外部站点，且结果不可复现。
pub fn try_replay(request: &HttpRequest) -> Option<Result<HttpResponse>> {
    match mode() {
        Some(Mode::Replay(replay)) => Some(replay.respond(request)),
        _ => None,
    }
}

/// 记录一次成功的往返。未启用录制时是空操作。
pub fn record(request: &HttpRequest, response: &HttpResponse) {
    if let Some(Mode::Record(recorder)) = mode() {
        if let Err(e) = recorder.record(request, response) {
            // 录制失败不该打断真实下载：用户可能只是想顺手存一份录像。
            tracing::warn!("写入插件录像失败: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ilp_replay_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn request(url: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: vec![
                HttpHeader {
                    name: "Cookie".into(),
                    value: "session=deadbeef".into(),
                },
                HttpHeader {
                    name: "User-Agent".into(),
                    value: "ilp-test".into(),
                },
            ],
            body: None,
        }
    }

    fn response(body: &[u8]) -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: vec![
                HttpHeader {
                    name: "Set-Cookie".into(),
                    value: "session=deadbeef".into(),
                },
                HttpHeader {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                },
            ],
            body: body.to_vec(),
        }
    }

    #[test]
    fn url_secrets_are_redacted_but_structure_survives() {
        let url = "https://api.example.com/list?book_id=42&token=abc123&sign=zzz";
        let redacted = redact_url(url);
        assert!(
            redacted.contains("book_id=42"),
            "普通参数要保留: {redacted}"
        );
        assert!(!redacted.contains("abc123"), "token 不能留: {redacted}");
        assert!(!redacted.contains("zzz"), "sign 不能留: {redacted}");
        assert_eq!(redacted.matches(REDACTED).count(), 2);
    }

    #[test]
    fn recorded_headers_have_no_plaintext_credentials() {
        let dir = tmp_dir("redact");
        let recorder = Recorder::new(&dir).expect("录制器");
        let path = recorder
            .record(&request("https://api.example.com/a"), &response(b"ok"))
            .expect("录制");
        let text = std::fs::read_to_string(&path).expect("读录像");
        assert!(!text.contains("deadbeef"), "凭据泄漏到录像里: {text}");
        assert!(
            text.to_ascii_lowercase().contains("cookie"),
            "应保留头名以便断言插件确实发了它"
        );
        assert!(text.contains("ilp-test"), "非敏感头应保留原值");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_secret_header_names_are_still_redacted() {
        // 真实回归：第一次对着番茄录制时，`x-ms-token`（CDN 防盗链票据）
        // 因为不在固定名单里而被原样写进了录像。子串匹配才能拦住这一类。
        let dir = tmp_dir("substring");
        let recorder = Recorder::new(&dir).expect("录制器");
        let mut resp = response(b"ok");
        resp.headers.push(HttpHeader {
            name: "x-ms-token".into(),
            value: "j2UZf05AmUY71ThR_secret".into(),
        });
        let path = recorder
            .record(&request("https://api.example.com/a"), &resp)
            .expect("录制");
        let text = std::fs::read_to_string(&path).expect("读录像");
        assert!(!text.contains("j2UZf05AmUY71ThR_secret"), "实得: {text}");
        assert!(text.contains("x-ms-token"), "头名应保留");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_refuses_to_write_a_leaked_header() {
        // 直接构造一条“没脱敏”的录像，确保兜底检查真的会挡住。
        let mut headers = BTreeMap::new();
        headers.insert("set-cookie".to_string(), "session=deadbeef".to_string());
        let cassette = Cassette {
            schema_version: 1,
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://api.example.com/a".into(),
                headers: BTreeMap::new(),
                body_base64: None,
            },
            response: RecordedResponse {
                status: 200,
                headers,
                body_base64: String::new(),
            },
            notes: String::new(),
        };
        let text = serde_json::to_string(&cassette).expect("序列化");
        let err = assert_no_secrets(&cassette, &text).expect_err("必须拒绝写入");
        assert!(format!("{err:#}").contains("未被脱敏"), "实得: {err:#}");
    }

    #[test]
    fn replay_roundtrips_a_recorded_cassette() {
        let dir = tmp_dir("roundtrip");
        let recorder = Recorder::new(&dir).expect("录制器");
        recorder
            .record(
                &request("https://api.example.com/a?book_id=1"),
                &response(b"body!"),
            )
            .expect("录制");

        let replay = Replay::load(&dir).expect("加载录像");
        let got = replay
            .respond(&request("https://api.example.com/a?book_id=1"))
            .expect("应命中");
        assert_eq!(got.body, b"body!");
        assert_eq!(got.status, 200);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_matches_even_when_the_token_changed() {
        // 真实场景：录像里存着一次采样的 token，回放时插件生成了新的。
        // 若把 token 算进匹配键，录像永远匹配不上。
        let dir = tmp_dir("token");
        let recorder = Recorder::new(&dir).expect("录制器");
        recorder
            .record(
                &request("https://api.example.com/a?token=old-token&book_id=7"),
                &response(b"hit"),
            )
            .expect("录制");

        let replay = Replay::load(&dir).expect("加载");
        let got = replay
            .respond(&request(
                "https://api.example.com/a?token=new-token&book_id=7",
            ))
            .expect("换 token 后仍应命中");
        assert_eq!(got.body, b"hit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_miss_is_an_error_not_a_network_fallback() {
        // 静默联网会让"离线测试"变成不可复现的假通过。
        let dir = tmp_dir("miss");
        let replay = Replay::load(&dir).expect("加载空目录");
        assert!(replay.is_empty());
        let err = replay
            .respond(&request("https://api.example.com/nope"))
            .expect_err("未命中必须报错");
        let text = format!("{err:#}");
        assert!(text.contains("没有匹配的录像"), "实得: {text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_body_produces_a_different_match_key() {
        // GET 靠 URL 区分就够了；POST 必须靠 body 区分，否则两个不同请求会共用一条录像。
        let a = match_key("POST", "https://api.example.com/x", Some(b"a"));
        let b = match_key("POST", "https://api.example.com/x", Some(b"b"));
        assert_ne!(a, b);
    }

    #[test]
    fn device_identity_is_redacted_so_match_keys_are_stable() {
        // 真实回归：一次录制里 install_id/device_id 每次都是新生成的，
        // 不脱敏则同一个逻辑请求在录与放两侧的键永远对不上。
        let a = match_key(
            "GET",
            "https://reading.snssdk.com/v?iid=111&device_id=aaa&book_id=9",
            None,
        );
        let b = match_key(
            "GET",
            "https://reading.snssdk.com/v?iid=222&device_id=bbb&book_id=9",
            None,
        );
        assert_eq!(a, b, "设备身份不应影响匹配键");
        assert!(!a.contains("111") && !a.contains("aaa"), "实得: {a}");
        assert!(a.contains("book_id=9"), "业务参数要保留: {a}");
    }

    #[test]
    fn volatile_body_falls_back_to_the_unique_url_recording() {
        // 设备注册接口的请求体每次不同（随机 nonce），严格匹配必失败。
        let dir = tmp_dir("loose");
        let recorder = Recorder::new(&dir).expect("录制器");
        let mut reg = request("https://i.snssdk.com/service/2/device_register/?tt_data=a");
        reg.method = "POST".into();
        reg.body = Some(b"nonce=first".to_vec());
        recorder
            .record(&reg, &response(b"registered"))
            .expect("录制");

        let replay = Replay::load(&dir).expect("加载");
        let mut again = request("https://i.snssdk.com/service/2/device_register/?tt_data=a");
        again.method = "POST".into();
        again.body = Some(b"nonce=second".to_vec());
        let got = replay.respond(&again).expect("应通过宽松键命中");
        assert_eq!(got.body, b"registered");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ambiguous_url_does_not_guess_a_recording() {
        // 同一 URL 有两条不同请求体时，猜错比报错难查得多。
        let dir = tmp_dir("ambiguous");
        let recorder = Recorder::new(&dir).expect("录制器");
        for body in [b"a".as_slice(), b"b".as_slice()] {
            let mut req = request("https://api.example.com/x");
            req.method = "POST".into();
            req.body = Some(body.to_vec());
            recorder.record(&req, &response(body)).expect("录制");
        }
        let replay = Replay::load(&dir).expect("加载");
        let mut unknown = request("https://api.example.com/x");
        unknown.method = "POST".into();
        unknown.body = Some(b"c".to_vec());
        let err = replay.respond(&unknown).expect_err("不唯一时必须报错");
        assert!(
            format!("{err:#}").contains("没有匹配的录像"),
            "实得: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_schema_is_rejected() {
        let dir = tmp_dir("schema");
        let bad = serde_json::json!({
            "schema_version": 2,
            "request": {"method": "GET", "url": "https://a/x"},
            "response": {"status": 200, "body_base64": ""}
        });
        std::fs::write(dir.join("bad.json"), bad.to_string()).expect("写");
        let err = Replay::load(&dir).expect_err("未知 schema 必须拒绝");
        assert!(
            format!("{err:#}").contains("schema_version"),
            "实得: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recording_is_deterministic_for_the_same_request() {
        // 同一请求录两次应覆盖同一文件，而不是堆出一串近似文件。
        let dir = tmp_dir("deterministic");
        let recorder = Recorder::new(&dir).expect("录制器");
        let req = request("https://api.example.com/same");
        let first = recorder.record(&req, &response(b"one")).expect("录制");
        let second = recorder.record(&req, &response(b"two")).expect("再录");
        assert_eq!(first, second, "同一请求应落到同一文件");
        let count = std::fs::read_dir(&dir).expect("列目录").count();
        assert_eq!(count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
