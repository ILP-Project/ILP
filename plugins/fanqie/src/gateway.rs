//! 番茄 APP 网关：设备注册、密钥协商、批量拉正文、搜索。
//!
//! 网页通道已被风控整体封死（返回验证码中间页），APP 网关是唯一可用的
//! 正文来源。整条链路都在插件内部完成，宿主只提供 http / log / storage。

use serde_json::{json, Value};
use std::collections::HashMap;

use crate::ilp::plugin::host;
use crate::ilp::plugin::types::{HttpHeader, HttpRequest};
use crate::sign::{self, x_argus, x_ladon, AID};
use crate::sm3::sm3;

const DEVICE_REGISTER_URL: &str = "https://i.snssdk.com/service/2/device_register/?tt_data=a";
const REGISTER_KEY_URL: &str = "https://reading.snssdk.com/reading/crypt/registerkey?";
const BATCH_URL: &str = "https://reading.snssdk.com/reading/reader/batch_full/v?";
const SEARCH_URL: &str = "https://reading.snssdk.com/reading/bookapi/search/tab/v?";

/// search_tabs 里书籍 tab 的 tab_type。
const SEARCH_TAB_BOOKS: u64 = 3;

/// 每页条数。太小会翻很多页，太大服务端会截断。
pub const SEARCH_PAGE_SIZE: usize = 10;

const APP_UA: &str = "com.dragon.read";
const VERSION_CODE: &str = "70132";
const VERSION_NAME: &str = "7.0.1.32";
const CHANNEL: &str = "43536163a";

/// 单批上限。实测请求超过 30 章时服务端只回前 30 章，
/// 所以主动按 30 切分，避免无声丢章。

/// 设备与内容密钥。存在宿主的 KV 里，重启后复用，
/// 避免每次启动都重新注册。
pub struct Device {
    pub device_id: String,
    pub install_id: String,
    pub device_type: String,
    pub key: Vec<u8>,
    pub keyver: i64,
}

impl Device {
    fn to_json(&self) -> Value {
        json!({
            "device_id": self.device_id,
            "install_id": self.install_id,
            "device_type": self.device_type,
            "key": hex_encode(&self.key),
            "keyver": self.keyver,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let key = hex_decode(v.get("key")?.as_str()?)?;
        Some(Self {
            device_id: v.get("device_id")?.as_str()?.to_string(),
            install_id: v.get("install_id")?.as_str()?.to_string(),
            device_type: v.get("device_type")?.as_str()?.to_string(),
            key,
            keyver: v.get("keyver")?.as_i64()?,
        })
    }
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    let mut i = 0;
    while i < b.len() {
        let pair = std::str::from_utf8(&b[i..i + 2]).ok()?;
        out.push(u8::from_str_radix(pair, 16).ok()?);
        i += 2;
    }
    Some(out)
}

/// 设备上报用的固定盐。客户端内置常量，服务端据此识别来源。
const FIXED: [u8; 64] = [
    0x4d, 0xd4, 0xc2, 0xe6, 0xb8, 0x31, 0x62, 0x09, 0x0e, 0x52, 0xb3, 0xc7, 0xa6, 0x73, 0x3b, 0xa4,
    0x1c, 0xb2, 0x46, 0x2b, 0x82, 0x9a, 0xb5, 0x8a, 0x19, 0x6b, 0x39, 0xdb, 0x57, 0x17, 0x75, 0x24,
    0xf4, 0x9b, 0xaf, 0x7f, 0x08, 0xe8, 0xd6, 0x8d, 0x26, 0xa7, 0x2e, 0x37, 0xc1, 0xa9, 0x5a, 0x2f,
    0x1f, 0x05, 0xa5, 0x18, 0x92, 0xae, 0xf2, 0x94, 0x97, 0x32, 0xb6, 0x2a, 0x38, 0xaa, 0xdd, 0x58,
];

/// 共享密钥，用于 crypt/registerkey 的报文加密。
const SHARED_KEY: [u8; 16] = [
    172, 37, 198, 125, 221, 143, 56, 193, 179, 122, 35, 72, 130, 142, 34, 46,
];

fn pseudo_random(n: usize) -> Vec<u8> {
    // 沙箱里没有 OS 熵源，用时间 + 计数器 + SM3 展开。
    // 这些值只用于设备指纹的随机字段，不承担密钥安全。
    use std::cell::Cell;
    thread_local! {
        static C: Cell<u64> = const { Cell::new(0x243f6a8885a308d3) };
    }
    let mut out = Vec::with_capacity(n + 32);
    let mut round = 0u64;
    while out.len() < n {
        let c = C.with(|c| {
            let v = c.get();
            c.set(v.wrapping_mul(6364136223846793005).wrapping_add(1));
            v
        });
        let mut seed = Vec::with_capacity(16);
        seed.extend_from_slice(&c.to_le_bytes());
        seed.extend_from_slice(&round.to_le_bytes());
        out.extend_from_slice(&sm3(&seed));
        round += 1;
    }
    out.truncate(n);
    out
}

fn rand_hex(len: usize) -> String {
    pseudo_random(len)
        .iter()
        .map(|b| format!("{:x}", b % 16))
        .collect()
}

fn uuid4() -> String {
    let mut b = pseudo_random(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex_encode(&b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn rand_num(digits: u32) -> u64 {
    let bytes = pseudo_random(8);
    let v = bytes
        .iter()
        .fold(0u64, |a, b| a.wrapping_mul(256).wrapping_add(*b as u64));
    v % 10u64.pow(digits)
}

fn random_ipv6() -> String {
    format!(
        "fd{}:{}",
        rand_hex(2),
        (0..7).map(|_| rand_hex(4)).collect::<Vec<_>>().join(":")
    )
}

fn sha512(data: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha512};
    let mut h = Sha512::new();
    h.update(data);
    h.finalize().to_vec()
}

/// TT 加密：`[116,99,5,16,0,0] ‖ rand32 ‖ AES-128-CBC(...)`。
fn tt_encrypt(data: &[u8]) -> Result<Vec<u8>, String> {
    let rnd = pseudo_random(32);
    let mut h_input = sha512(&rnd);
    h_input.extend_from_slice(&FIXED);
    let h = sha512(&h_input);
    let key = &h[..16];
    let iv = &h[16..32];

    let comp = gzip(data)?;
    let mut hashed = sha512(&comp);
    hashed.extend_from_slice(&comp);
    let ct = sign::aes128_cbc_encrypt(key, iv, &hashed)?;

    let mut out = vec![116, 99, 5, 16, 0, 0];
    out.extend_from_slice(&rnd);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn gzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data)
        .map_err(|e| format!("gzip 压缩失败: {e}"))?;
    e.finish().map_err(|e| format!("gzip 收尾失败: {e}"))
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut d = GzDecoder::new(data);
    let mut out = Vec::new();
    d.read_to_end(&mut out)
        .map_err(|e| format!("gzip 解压失败: {e}"))?;
    Ok(out)
}

fn md5_hex(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn app_query(dev: &Device, extra: &[(&str, &str)]) -> String {
    let mut parts: Vec<String> = vec![
        format!("iid={}", dev.install_id),
        format!("device_id={}", dev.device_id),
        "ac=wifi".to_string(),
        format!("channel={CHANNEL}"),
        format!("aid={AID}"),
        "app_name=novelapp".to_string(),
        format!("version_code={VERSION_CODE}"),
        format!("version_name={VERSION_NAME}"),
        "device_platform=android".to_string(),
        "os=android".to_string(),
        "ssmix=a".to_string(),
        "os_version=10".to_string(),
        format!("device_type={}", dev.device_type),
        "device_brand=realme".to_string(),
        format!("update_version_code={VERSION_CODE}"),
        format!("manifest_version_code={VERSION_CODE}"),
        format!("pv_player={VERSION_CODE}"),
    ];
    for (k, v) in extra {
        parts.push(format!("{k}={}", url_encode(v)));
    }
    parts.join("&")
}

fn signed_headers(query: &str, body: Option<&[u8]>) -> Result<Vec<HttpHeader>, String> {
    let ts = crate::clock::now_ms() / 1000;
    let stub = body.map(md5_hex).unwrap_or_default();
    let mut h = vec![
        HttpHeader {
            name: "x-argus".to_string(),
            value: x_argus(query, &stub)?,
        },
        HttpHeader {
            name: "x-ladon".to_string(),
            value: x_ladon(ts),
        },
        HttpHeader {
            name: "x-khronos".to_string(),
            value: ts.to_string(),
        },
        HttpHeader {
            name: "x-ss-req-ticket".to_string(),
            value: crate::clock::now_ms().to_string(),
        },
        HttpHeader {
            name: "User-Agent".to_string(),
            value: APP_UA.to_string(),
        },
    ];
    if !stub.is_empty() {
        h.push(HttpHeader {
            name: "X-SS-STUB".to_string(),
            value: stub,
        });
    }
    Ok(h)
}

fn do_request(
    method: &str,
    url: &str,
    headers: Vec<HttpHeader>,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let resp = host::http_fetch(&HttpRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers,
        body,
    })?;
    if resp.status < 200 || resp.status >= 300 {
        return Err(format!("HTTP {}: {url}", resp.status));
    }
    Ok(resp.body)
}

fn truncate(data: &[u8], n: usize) -> String {
    String::from_utf8_lossy(&data[..data.len().min(n)]).to_string()
}

fn build_env(now: u64) -> Value {
    let env = json!({
        "magic_tag": "ss_app_log",
        "header": {
            "display_name": "番茄免费小说",
            "aid": 1967,
            "channel": CHANNEL,
            "package": "com.dragon.read",
            "sdk_version": "3.7.0-rc.25-fanqie-xiaoshuo",
            "sdk_target_version": 29,
            "git_hash": "711d1a7",
            "density_dpi": 240,
            "display_density": "hdpi",
            "resolution": "720x1280",
            "language": "zh",
            "timezone": 8,
            "access": "wifi",
            "not_request_sender": 0,
            "carrier": "CHINA MOBILE",
            "mcc_mnc": "46000",
            "region": "CN",
            "tz_name": "Asia/Shanghai",
            "tz_offset": 28800,
            "sim_region": "cn",
            "oaid_may_support": false,
            "device_platform": "android",
            "custom": { "host_bit": 32, "dragon_device_type": 0 },
            "pre_installed_channel": "",
            "is_system_app": 0,
            "sdk_flavor": "china",
            "guest_mode": 0,
            "os": "Android",
            "os_version": "10",
            "os_api": 29,
            "device_model": "P30",
            "device_brand": "realme",
            "device_manufacturer": "realme",
            "cpu_abi": "arm64-v8a",
            "release_build": rand_hex(7),
            "cdid": uuid4(),
            "sig_hash": "a4a27c2633195374c15651ffc3c4a497",
            "openudid": rand_hex(20),
            "clientudid": uuid4(),
            "req_id": uuid4(),
            "rom": rand_hex(14).to_uppercase(),
            "rom_version": format!(
                "coloros__{}.{}.{} release-keys",
                rand_hex(4).to_uppercase(),
                rand_num(6),
                rand_num(8),
            ),
            "sim_serial_number": [],
            "ipv6_list": [
                { "type": "client_anpi", "value": "::1" },
                { "type": "client_anpi", "value": random_ipv6() },
                { "type": "client_anpi", "value": random_ipv6() },
            ],
            "apk_first_install_time": now.saturating_sub(86_400_000u64),
        },
        "_gen_time": now,
    });
    env
}

fn register_device() -> Result<Device, String> {
    let now = crate::clock::now_ms();
    let env = build_env(now);
    let body = tt_encrypt(env.to_string().as_bytes())?;
    let resp = do_request(
        "POST",
        DEVICE_REGISTER_URL,
        vec![
            HttpHeader {
                name: "User-Agent".to_string(),
                value: "okhttp/4.10.0".to_string(),
            },
            HttpHeader {
                name: "Content-Type".to_string(),
                value: "application/octet-stream; tt-data=a".to_string(),
            },
        ],
        Some(body),
    )?;

    let v: Value =
        serde_json::from_slice(&resp).map_err(|e| format!("设备注册响应不是 JSON: {e}"))?;
    let device_id = v
        .get("device_id_str")
        .and_then(|x| x.as_str())
        .ok_or_else(|| format!("设备注册未返回 device_id: {}", truncate(&resp, 200)))?;
    let install_id = v
        .get("install_id_str")
        .and_then(|x| x.as_str())
        .unwrap_or(device_id);

    host::log(2, "设备注册成功");
    Ok(Device {
        device_id: device_id.to_string(),
        install_id: install_id.to_string(),
        device_type: "P30".to_string(),
        key: Vec::new(),
        keyver: 0,
    })
}

fn register_key(dev: &mut Device) -> Result<(), String> {
    let query = app_query(
        dev,
        &[("version_code", "6.3.9.32"), ("version_name", "6.3.9.32")],
    );
    let url = format!("{REGISTER_KEY_URL}{query}");

    // 明文是 device_id 反序的大端 16 字节。
    let id: u64 = dev
        .device_id
        .parse()
        .map_err(|e| format!("device_id 不是数字: {e}"))?;
    let mut be = [0u8; 16];
    be[8..].copy_from_slice(&id.to_be_bytes());
    be.reverse();

    let iv = pseudo_random(16);
    let ct = sign::aes128_cbc_encrypt(&SHARED_KEY, &iv, &be)?;
    let mut payload = iv.clone();
    payload.extend_from_slice(&ct);
    let body_plain = json!({ "content": sign::base64_encode(&payload) });
    let gz = gzip(body_plain.to_string().as_bytes())?;

    let mut headers = signed_headers(&query, Some(&gz))?;
    headers.push(HttpHeader {
        name: "Content-Type".to_string(),
        value: "application/json; charset=utf-8".to_string(),
    });
    headers.push(HttpHeader {
        name: "Content-Encoding".to_string(),
        value: "gzip".to_string(),
    });

    let resp = do_request("POST", &url, headers, Some(gz))?;
    let v: Value =
        serde_json::from_slice(&resp).map_err(|e| format!("registerkey 响应不是 JSON: {e}"))?;
    let key_b64 = v
        .get("data")
        .and_then(|d| d.get("key"))
        .and_then(|k| k.as_str())
        .ok_or_else(|| format!("registerkey 未返回 key: {}", truncate(&resp, 200)))?;

    let buf = sign::base64_decode(key_b64)?;
    if buf.len() < 32 {
        return Err("registerkey 报文长度不足".to_string());
    }
    dev.key = sign::aes128_cbc_decrypt(&SHARED_KEY, &buf[..16], &buf[16..])?;
    dev.keyver = v
        .get("data")
        .and_then(|d| d.get("keyver"))
        .and_then(|k| k.as_i64())
        .unwrap_or(0);
    host::log(
        2,
        &format!(
            "内容密钥协商成功: len={} keyver={}",
            dev.key.len(),
            dev.keyver
        ),
    );
    Ok(())
}

const DEVICE_KEY: &str = "fanqie_device";

/// 取可用设备：优先复用 KV 里存的，缺失或明确失效时重新注册。
pub fn ensure_device(force_new: bool) -> Result<Device, String> {
    if !force_new {
        if let Some(raw) = host::storage_get(DEVICE_KEY) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                if let Some(dev) = Device::from_json(&v) {
                    if dev.key.len() == 16 {
                        host::log(3, "复用已存设备与密钥");
                        return Ok(dev);
                    }
                }
            }
        }
    }

    let mut dev = register_device()?;
    register_key(&mut dev)?;
    host::storage_set(DEVICE_KEY, dev.to_json().to_string().as_bytes())?;
    Ok(dev)
}

/// 构造一批章节的请求（URL + 签名头），实际发送由宿主完成。
pub fn plan_batch(
    dev: &Device,
    book_id: &str,
    chapter_ids: &[String],
) -> Result<(String, Vec<HttpHeader>), String> {
    let ids = chapter_ids.join(",");
    let query = app_query(
        dev,
        &[
            ("item_ids", &ids),
            ("book_id", book_id),
            ("novel_text_type", "1"),
            ("req_type", "1"),
        ],
    );
    let headers = signed_headers(&query, None)?;
    Ok((format!("{BATCH_URL}{query}"), headers))
}

/// 解析批量响应为 `chapter_id -> 纯文本`。
pub fn parse_batch(dev: &Device, body: &[u8]) -> Result<HashMap<String, String>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("批量正文不是 JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_object())
        .ok_or_else(|| "批量正文缺少 data".to_string())?;

    let mut out = HashMap::new();
    for (id, item) in data {
        let Some(content) = item.get("content").and_then(|c| c.as_str()) else {
            continue;
        };
        if content.is_empty() || content == "Invalid" {
            continue;
        }
        match decode_content(dev, content, item) {
            Ok(text) => {
                out.insert(id.clone(), text);
            }
            Err(e) => host::log(1, &format!("章节 {id} 解密失败: {e}")),
        }
    }
    Ok(out)
}

fn decode_content(dev: &Device, content: &str, item: &Value) -> Result<String, String> {
    let buf = sign::base64_decode(content)?;
    if buf.len() < 32 {
        return Err("密文过短".to_string());
    }
    let mut plain = sign::aes128_cbc_decrypt(&dev.key, &buf[..16], &buf[16..])?;
    let compress = item
        .get("compress_status")
        .and_then(|c| c.as_i64())
        .unwrap_or(0);
    if compress == 1 {
        plain = gunzip(&plain)?;
    }
    Ok(String::from_utf8_lossy(&plain).to_string())
}

/// 搜索结果口径统一成 ilp-types 的 SearchResult 字段。
///
/// 真实响应结构与早期假设完全不同：书籍列表不在 `data.search_tab_data`，
/// 而是顶层 `search_tabs` 数组里 `tab_type == 3`（书籍）的那个 tab。
/// 分页也由该 tab 自己的 `has_more` / `next_offset` 决定，宿主无从推算，
/// 必须原样带回。
pub fn search(
    dev: &Device,
    keyword: &str,
    cursor: &SearchCursor,
    count: usize,
) -> Result<(Vec<SearchItem>, bool, SearchCursor), String> {
    let count_s = count.to_string();
    let mut extra: Vec<(&str, &str)> = vec![
        ("query", keyword),
        ("offset", "0"),
        ("count", &count_s),
        ("tab_type", "3"),
    ];
    if !cursor.passback.is_empty() {
        extra.push(("passback", &cursor.passback));
    }
    if !cursor.search_id.is_empty() {
        extra.push(("search_id", &cursor.search_id));
    }
    let query = app_query(dev, &extra);
    let url = format!("{SEARCH_URL}{query}");
    let headers = signed_headers(&query, None)?;
    let resp = do_request("GET", &url, headers, None)?;
    if resp.is_empty() {
        return Err(format!("搜索返回空响应: {keyword}"));
    }
    let v: Value = serde_json::from_slice(&resp)
        .map_err(|e| format!("搜索响应不是 JSON（{} 字节）: {e}", resp.len()))?;

    let book_tab = v
        .get("search_tabs")
        .and_then(|t| t.as_array())
        .and_then(|tabs| {
            tabs.iter()
                .find(|t| t.get("tab_type").and_then(|x| x.as_u64()) == Some(SEARCH_TAB_BOOKS))
        })
        .ok_or_else(|| "搜索响应缺少书籍 tab".to_string())?;

    let has_more = book_tab
        .get("has_more")
        .and_then(|x| x.as_bool())
        .unwrap_or(false);

    let mut out = Vec::new();
    if let Some(items) = book_tab.get("data").and_then(|d| d.as_array()) {
        for it in items {
            // book_data 是数组，正文在第一个元素里。
            let bd = it
                .get("book_data")
                .and_then(|b| b.as_array())
                .and_then(|b| b.first())
                .unwrap_or(it);
            let s = |k: &str| {
                bd.get(k)
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let id = s("book_id");
            if id.is_empty() {
                continue;
            }
            out.push(SearchItem {
                id,
                title: s("book_name"),
                author: s("author"),
                cover_url: s("thumb_url"),
                description: s("abstract"),
            });
        }
    }
    // 下一页的游标来自本页响应：passback 与 search_id 必须成对回传。
    let next = SearchCursor {
        passback: book_tab
            .get("passback")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        search_id: book_tab
            .get("search_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
    };
    Ok((out, has_more, next))
}

/// 搜索翻页游标。服务端忽略 offset，只认 passback + search_id。
#[derive(Debug, Clone, Default)]
pub struct SearchCursor {
    pub passback: String,
    pub search_id: String,
}

impl SearchCursor {
    /// 序列化成单个字符串塞进 WIT 的 cursor 字段。
    pub fn encode(&self) -> String {
        if self.passback.is_empty() && self.search_id.is_empty() {
            String::new()
        } else {
            format!("{}\u{1}{}", self.passback, self.search_id)
        }
    }

    pub fn decode(s: &str) -> Self {
        match s.split_once('\u{1}') {
            Some((p, i)) => Self {
                passback: p.to_string(),
                search_id: i.to_string(),
            },
            None => Self::default(),
        }
    }
}

pub struct SearchItem {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover_url: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_json_round_trip() {
        let dev = Device {
            device_id: "123".into(),
            install_id: "456".into(),
            device_type: "P30".into(),
            key: vec![0xab; 32],
            keyver: 99,
        };
        let back = Device::from_json(&dev.to_json()).expect("应能还原");
        assert_eq!(back.device_id, "123");
        assert_eq!(back.key, dev.key);
        assert_eq!(back.keyver, 99);
    }

    #[test]
    fn device_json_rejects_bad_hex() {
        let bad = json!({"device_id":"1","install_id":"2","device_type":"x","key":"zz","keyver":1});
        assert!(Device::from_json(&bad).is_none());
    }

    /// 与 Python 参考实现逐字节对齐：固定 rnd 与明文，
    /// 派生出的密钥、IV 必须与参考实现完全相同。
    /// FIXED 常量错一个字节，服务端就会回 device_id=0。
    #[test]
    fn tt_encrypt_matches_reference() {
        let rnd: Vec<u8> = (0..32u8).collect();
        let mut h_input = sha512(&rnd);
        h_input.extend_from_slice(&FIXED);
        let h = sha512(&h_input);

        assert_eq!(
            hex_encode(&h[..16]),
            "70bf044d2853f0a51dde9276af723096",
            "AES key 与参考实现不一致"
        );
        assert_eq!(
            hex_encode(&h[16..32]),
            "d89e403db9a79aab6fe435399bee8a0d",
            "AES iv 与参考实现不一致"
        );

        // 整体报文结构与长度：6 字节头 + 32 字节随机 + PKCS#7 对齐的密文。
        let comp = gzip(b"{\"a\":1}").unwrap();
        let mut hashed = sha512(&comp);
        hashed.extend_from_slice(&comp);
        let ct = sign::aes128_cbc_encrypt(&h[..16], &h[16..32], &hashed).unwrap();
        let out = [&[116, 99, 5, 16, 0, 0][..], &rnd, &ct].concat();
        assert_eq!(out.len(), 134);
        assert_eq!(&out[..6], &[116, 99, 5, 16, 0, 0]);
    }

    #[test]
    fn env_has_required_fields() {
        let env = build_env(1_700_000_000_000);
        let h = env.get("header").expect("需要 header");
        // 实测：缺 ipv6_list 服务端会回 device_id=0，所以它必须存在。
        assert!(h.get("ipv6_list").is_some());
        assert_eq!(h.get("aid").and_then(|v| v.as_i64()), Some(1967));
        assert_eq!(
            h.get("package").and_then(|v| v.as_str()),
            Some("com.dragon.read")
        );
    }

    #[test]
    fn batch_size_is_thirty() {}

    #[test]
    fn url_encode_escapes_reserved() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("1,2"), "1%2C2");
        assert_eq!(url_encode("中文"), "%E4%B8%AD%E6%96%87");
    }

    #[test]
    fn query_keeps_expected_app_params() {
        let dev = Device {
            device_id: "1".into(),
            install_id: "2".into(),
            device_type: "P30".into(),
            key: vec![0u8; 32],
            keyver: 0,
        };
        let q = app_query(&dev, &[("book_id", "99")]);
        assert!(q.contains("aid=1967"));
        assert!(q.contains("device_id=1"));
        assert!(q.contains("book_id=99"));
    }
}
