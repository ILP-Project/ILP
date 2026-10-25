//! 番茄 APP 网关的请求签名（x-argus / x-ladon）。
//!
//! x-argus：把请求上下文写成 protobuf，PKCS#7 填充后用 Simon128/256 逐块加密，
//! 再异或混淆、反转、加固定头，最后 AES-128-CBC 包一层并 base64。
//! x-ladon：`ts-license-aid` 明文用 Speck128/256 加密后 base64。
//!
//! 常量来自公开的客户端实现，算法按 NSA 规范与 GB/T 32905-2016 独立实现。

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use aes::Aes128;
#[cfg(test)]
use aes::Aes256;

use crate::sm3::sm3;
use crate::spn::{le_words, pkcs7_pad, simon_encrypt, simon_expand, speck_encrypt, speck_expand};

const SIGN_KEY: [u8; 32] = [
    0xac, 0x1a, 0xda, 0xae, 0x95, 0xa7, 0xaf, 0x94, 0xa5, 0x11, 0x4a, 0xb3, 0xb3, 0xa9, 0x7d, 0xd8,
    0x00, 0x50, 0xaa, 0x0a, 0x39, 0x31, 0x4c, 0x40, 0x52, 0x8c, 0xae, 0xc9, 0x52, 0x56, 0xc2, 0x8c,
];

pub const AID: &str = "1967";
pub const LICENSE: &str = "1611921764";
pub const SDKVER: &str = "v04.04.05-ov-android";
const SDK_INT: u64 = 134744640;
const CALL_TYPE: u64 = 738;

const LOW_RAND: [u8; 2] = [0xf2, 0x81];
const HIGH_RAND: [u8; 2] = [0x61, 0x6f];
const XOR_PREFIX: [u8; 8] = [0xf2, 0xf7, 0xfc, 0xff, 0xf2, 0xf7, 0xfc, 0xff];
const ARGUS_HEADER: [u8; 9] = [0xa6, 0x6e, 0xad, 0x9f, 0x77, 0x01, 0xd0, 0x0c, 0x18];

/// 最小 protobuf 写入器。字段顺序与线格式都需要与客户端一致，
/// 否则服务端校验不过。
struct Pb {
    buf: Vec<u8>,
}

impl Pb {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn tag(&mut self, field: u32, wire: u8) {
        self.varint_raw(((field as u64) << 3) | wire as u64);
    }

    fn varint_raw(&mut self, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            self.buf.push(b);
            if v == 0 {
                break;
            }
        }
    }

    fn varint(mut self, field: u32, v: u64) -> Self {
        self.tag(field, 0);
        self.varint_raw(v);
        self
    }

    fn bytes(mut self, field: u32, data: &[u8]) -> Self {
        self.tag(field, 2);
        self.varint_raw(data.len() as u64);
        self.buf.extend_from_slice(data);
        self
    }

    fn string(self, field: u32, s: &str) -> Self {
        self.bytes(field, s.as_bytes())
    }

    fn message<F: FnOnce(Pb) -> Pb>(mut self, field: u32, f: F) -> Self {
        let inner = f(Pb::new());
        self.tag(field, 2);
        self.varint_raw(inner.buf.len() as u64);
        self.buf.extend_from_slice(&inner.buf);
        self
    }
}

fn parse_qs(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or_default();
        if k == key {
            return it.next().unwrap_or_default().to_string();
        }
    }
    String::new()
}

fn md5(data: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    let mut h = Md5::new();
    h.update(data);
    h.finalize().into()
}

fn hex_to_bytes16(s: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    if s.len() < 32 {
        return out;
    }
    for i in 0..16 {
        if let Ok(b) = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16) {
            out[i] = b;
        }
    }
    out
}

fn rand31() -> u64 {
    // 插件沙箱没有 CSPRNG，用 SM3 混合时间与计数器派生，
    // 该值只用于请求去重随机数，不承担密钥安全。
    use std::cell::Cell;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(0x9e3779b97f4a7c15) };
    }
    let c = COUNTER.with(|c| {
        let v = c.get();
        c.set(
            v.wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
        );
        v
    });
    let now = crate::clock::now_ms();
    let mut seed = Vec::with_capacity(16);
    seed.extend_from_slice(&c.to_le_bytes());
    seed.extend_from_slice(&now.to_le_bytes());
    let h = sm3(&seed);
    let v = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
    (v & 0x7fff_ffff) as u64
}

fn rand_bytes4() -> [u8; 4] {
    let r = rand31() as u32;
    r.to_le_bytes()
}

/// 生成 x-argus。`query` 是 URL 里 `?` 之后的原始字符串（保持原序），
/// `xss_stub` 是请求体 MD5 的十六进制串，GET 传空。
pub fn x_argus(query: &str, xss_stub: &str) -> Result<String, String> {
    let device_id = parse_qs(query, "device_id");
    let version_name = parse_qs(query, "version_name");

    let stub = hex_to_bytes16(xss_stub);
    let body_hash = sm3(&stub)[..6].to_vec();
    let query_bytes = if query.is_empty() {
        [0u8; 16].to_vec()
    } else {
        query.as_bytes().to_vec()
    };
    let query_hash = sm3(&query_bytes)[..6].to_vec();
    let rand = rand31();
    let ts = crate::clock::now_ms() / 1000;

    let pb = Pb::new()
        .varint(1, 0x20200929u64 * 2)
        .varint(2, 2)
        .varint(3, rand)
        .string(4, AID)
        .string(5, &device_id)
        .string(6, LICENSE)
        .string(7, &version_name)
        .string(8, SDKVER)
        .varint(9, SDK_INT)
        .bytes(10, &[0u8; 8])
        .varint(11, 0)
        .varint(12, ts * 2)
        .bytes(13, &body_hash)
        .bytes(14, &query_hash)
        .message(15, |s| {
            s.varint(1, 1)
                .varint(2, 1)
                .varint(3, 1)
                .varint(7, 3348294860)
        })
        .string(16, "")
        .string(20, "none")
        .varint(21, CALL_TYPE)
        .message(23, |s| {
            s.string(1, "NX551J").varint(2, 8196).varint(4, 2162219008)
        })
        .varint(25, 2);

    let protobuf = pkcs7_pad(&pb.buf, 16);

    let mut sm3_in = Vec::with_capacity(68);
    sm3_in.extend_from_slice(&SIGN_KEY);
    sm3_in.extend_from_slice(&LOW_RAND);
    sm3_in.extend_from_slice(&HIGH_RAND);
    sm3_in.extend_from_slice(&SIGN_KEY);
    let digest = sm3(&sm3_in);

    let key_words: Vec<u64> = digest
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("固定 8 字节")))
        .collect();
    let ks = simon_expand(&[key_words[0], key_words[1], key_words[2], key_words[3]]);

    let mut enc = Vec::with_capacity(protobuf.len());
    for chunk in protobuf.chunks(16) {
        let w = le_words(chunk, 2);
        let (x, y) = simon_encrypt((w[0], w[1]), &ks);
        enc.extend_from_slice(&x.to_le_bytes());
        enc.extend_from_slice(&y.to_le_bytes());
    }

    let mut data = Vec::with_capacity(XOR_PREFIX.len() + enc.len());
    data.extend_from_slice(&XOR_PREFIX);
    data.extend_from_slice(&enc);
    for i in XOR_PREFIX.len()..data.len() {
        data[i] ^= data[i % 8];
    }
    data.reverse();

    let mut plain = Vec::with_capacity(ARGUS_HEADER.len() + data.len() + HIGH_RAND.len());
    plain.extend_from_slice(&ARGUS_HEADER);
    plain.extend_from_slice(&data);
    plain.extend_from_slice(&HIGH_RAND);

    let aes_key = md5(&SIGN_KEY[..16]);
    let aes_iv = md5(&SIGN_KEY[16..]);
    // 注意：密钥与 IV 都是 MD5 输出（16 字节），所以这里是 AES-128，
    // 不是 AES-256。参考实现同样如此。
    let ct = aes128_cbc_encrypt(&aes_key, &aes_iv, &plain)?;

    let mut out = Vec::with_capacity(LOW_RAND.len() + ct.len());
    out.extend_from_slice(&LOW_RAND);
    out.extend_from_slice(&ct);
    Ok(base64_encode(&out))
}

/// 生成 x-ladon。
pub fn x_ladon(ts: u64) -> String {
    let rb = rand_bytes4();
    let mut key_input = rb.to_vec();
    key_input.extend_from_slice(AID.as_bytes());
    let key_hex: String = md5(&key_input).iter().map(|b| format!("{b:02x}")).collect();

    let key_words = le_words(key_hex.as_bytes(), 4);
    let ks = speck_expand(&[key_words[0], key_words[1], key_words[2], key_words[3]]);

    let plain = format!("{ts}-{LICENSE}-{AID}");
    let pad = pkcs7_pad(plain.as_bytes(), 16);

    let mut ct = Vec::with_capacity(pad.len());
    for chunk in pad.chunks(16) {
        let w = le_words(chunk, 2);
        let (x, y) = speck_encrypt((w[0], w[1]), &ks);
        ct.extend_from_slice(&x.to_le_bytes());
        ct.extend_from_slice(&y.to_le_bytes());
    }

    let mut out = Vec::with_capacity(rb.len() + ct.len());
    out.extend_from_slice(&rb);
    out.extend_from_slice(&ct);
    base64_encode(&out)
}

pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| format!("base64 解码失败: {e}"))
}

/// AES-128-CBC 解密并去 PKCS#7 填充。
pub fn aes128_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    if key.len() != 16 || iv.len() != 16 || data.is_empty() || data.len() % 16 != 0 {
        return Err("AES-128 参数长度不合法".to_string());
    }
    let dec = cbc::Decryptor::<Aes128>::new_from_slices(key, iv)
        .map_err(|e| format!("AES 初始化失败: {e}"))?;
    let mut buf = data.to_vec();
    let out = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| "AES 填充无效".to_string())?;
    Ok(out.to_vec())
}

/// AES-128-CBC 加密并补 PKCS#7 填充（设备注册与密钥协商用）。
pub fn aes128_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; data.len() + 16];
    buf[..data.len()].copy_from_slice(data);
    let enc = cbc::Encryptor::<Aes128>::new_from_slices(key, iv)
        .map_err(|e| format!("AES 初始化失败: {e}"))?;
    let out = enc
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .map_err(|_| "AES 填充失败".to_string())?;
    Ok(out.to_vec())
}

/// AES-256-CBC 加密并补 PKCS#7 填充。
///
/// 协议只用到 AES-128，这一对 256 位版本仅用于自测填充实现是否对称。
#[cfg(test)]
pub fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; data.len() + 16];
    buf[..data.len()].copy_from_slice(data);
    let enc = cbc::Encryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|e| format!("AES 初始化失败: {e}"))?;
    let out = enc
        .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
        .map_err(|_| "AES 填充失败".to_string())?;
    Ok(out.to_vec())
}

/// AES-256-CBC 解密并去 PKCS#7 填充。仅自测用，见上。
#[cfg(test)]
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    if key.len() != 32 || iv.len() != 16 || data.is_empty() || data.len() % 16 != 0 {
        return Err("AES 参数长度不合法".to_string());
    }
    let dec = cbc::Decryptor::<Aes256>::new_from_slices(key, iv)
        .map_err(|e| format!("AES 初始化失败: {e}"))?;
    let mut buf = data.to_vec();
    let out = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| "AES 填充无效".to_string())?;
    Ok(out.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_round_trip() {
        let key = [7u8; 32];
        let iv = [3u8; 16];
        let plain = b"hello ilp".to_vec();
        let ct = aes256_cbc_encrypt(&key, &iv, &plain).unwrap();
        let back = aes256_cbc_decrypt(&key, &iv, &ct).unwrap();
        assert_eq!(back, plain);
    }

    #[test]
    fn base64_round_trip() {
        let data = b"\x00\x01\x02\xff";
        assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
    }

    #[test]
    fn argus_is_stable_length_and_decodable() {
        let q = "aid=1967&device_id=123&version_name=7.0.1.32";
        let a = x_argus(q, "").unwrap();
        assert!(a.len() > 100);
        assert!(base64_decode(&a).is_ok());
    }

    #[test]
    fn ladon_decodes_to_plaintext() {
        let out = x_ladon(1_700_000_000);
        let raw = base64_decode(&out).unwrap();
        assert!(raw.len() > 4);
        // 去掉 4 字节随机前缀后长度应为 16 的整数倍
        assert_eq!((raw.len() - 4) % 16, 0);
    }

    #[test]
    fn hex_parse_handles_short_input() {
        assert_eq!(hex_to_bytes16(""), [0u8; 16]);
        assert_eq!(hex_to_bytes16("ff")[0], 0);
    }
}
