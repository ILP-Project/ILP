//! 分发物的 Ed25519 校验。
//!
//! 插件产物和 registry 元数据共用同一套信任根（`plugin_trusted_keys`）、
//! 同一份撤销列表和同一个阈值语义，所以校验逻辑只写一遍：
//! 两个调用点若各写一份，迟早会在"撤销优先"这类细节上走偏。
//!
//! 签名对象一律是**原始字节**。JSON 规范化（键序、空白、数字表示）是个坑：
//! 只要签名与校验两侧的规范化不完全一致，就会出现"我们签的"和"他们验的"
//! 不是同一串字节的情况。因此 registry 的签名是对 `registry.json` 文件原样
//! 签的，而不是对重新序列化后的结构签的。

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;

use crate::config::Config;

/// 单个签名者的声明。
#[derive(Deserialize, Debug, Clone)]
pub struct SignatureEntry {
    pub key_id: String,
    pub signature: String,
}

/// 签名集合。
///
/// registry 历史上只发单个对象，后续要支持多签，因此两种形态都要能解析：
/// 拒掉旧格式会让已经发布的 registry 直接装不了。
#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum SignatureSet {
    One(SignatureEntry),
    Many(Vec<SignatureEntry>),
}

impl SignatureSet {
    pub fn iter(&self) -> impl Iterator<Item = &SignatureEntry> {
        match self {
            SignatureSet::One(sig) => std::slice::from_ref(sig).iter(),
            SignatureSet::Many(sigs) => sigs.iter(),
        }
    }
}

/// 校验 `bytes` 的签名集合，要求有效签名者数量达到配置的阈值。
///
/// `subject` 只进错误信息（如 "插件 fanqie 1.0.1" 或 "registry.json"），
/// 让用户一眼看出是哪一层没通过。
pub fn verify(config: &Config, subject: &str, bytes: &[u8], set: &SignatureSet) -> Result<()> {
    // 同一 key 重复出现不能算两个签名，否则满足阈值只需把同一行复制两遍。
    let mut verified_keys: Vec<&str> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for signature in set.iter() {
        match verify_one(config, subject, bytes, signature) {
            Ok(()) => {
                if verified_keys.contains(&signature.key_id.as_str()) {
                    tracing::warn!(
                        subject = %subject,
                        key_id = %signature.key_id,
                        "重复的签名密钥被去重，不重复计入阈值"
                    );
                } else {
                    verified_keys.push(&signature.key_id);
                }
            }
            Err(err) => errors.push(format!("{err:#}")),
        }
    }

    if verified_keys.len() >= config.plugin_signature_threshold {
        return Ok(());
    }

    let mut message = format!(
        "{subject} 的有效签名数 {} 未达到要求的 {}",
        verified_keys.len(),
        config.plugin_signature_threshold
    );
    if !errors.is_empty() {
        message.push_str(&format!("；签名错误: {}", errors.join(" | ")));
    }
    Err(anyhow!(message))
}

fn verify_one(
    config: &Config,
    subject: &str,
    bytes: &[u8],
    signature: &SignatureEntry,
) -> Result<()> {
    // 撤销必须排在最前：一把泄露后已作废的密钥，即使仍在信任列表里
    // 也不该再通过。放在后面会被"配了信任根"的事实盖过去。
    if config
        .plugin_revoked_keys
        .iter()
        .any(|key_id| key_id == &signature.key_id)
    {
        return Err(anyhow!(
            "{subject} 使用已撤销的签名密钥 {}",
            signature.key_id
        ));
    }
    let key_text = config
        .plugin_trusted_keys
        .get(&signature.key_id)
        .ok_or_else(|| anyhow!("{subject} 使用未信任的签名密钥 {}", signature.key_id))?;
    let key = decode_key(key_text)?;
    let sig = decode_signature(&signature.signature)?;
    key.verify(bytes, &sig)
        .map_err(|_| anyhow!("{subject} 的签名校验失败（密钥 {}）", signature.key_id))
}

fn decode_key(text: &str) -> Result<VerifyingKey> {
    let bytes = BASE64.decode(text).context("信任公钥不是有效的 base64")?;
    let array: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("Ed25519 公钥必须是 32 字节"))?;
    VerifyingKey::from_bytes(&array).context("Ed25519 公钥无效")
}

fn decode_signature(text: &str) -> Result<Signature> {
    let bytes = BASE64.decode(text).context("签名不是有效的 base64")?;
    let array: [u8; 64] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("Ed25519 签名必须是 64 字节"))?;
    Ok(Signature::from_bytes(&array))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn trust(keys: &[(&str, &SigningKey)]) -> Config {
        let mut config = Config::default();
        config.plugin_trusted_keys = keys
            .iter()
            .map(|(id, key)| {
                (
                    id.to_string(),
                    BASE64.encode(key.verifying_key().to_bytes()),
                )
            })
            .collect();
        config.require_plugin_signatures = true;
        config
    }

    fn entry(key_id: &str, key: &SigningKey, bytes: &[u8]) -> SignatureEntry {
        SignatureEntry {
            key_id: key_id.to_string(),
            signature: BASE64.encode(key.sign(bytes).to_bytes()),
        }
    }

    const BYTES: &[u8] = b"artifact";

    #[test]
    fn single_signature_passes() {
        let a = SigningKey::from_bytes(&[11u8; 32]);
        let set = SignatureSet::One(entry("a", &a, BYTES));
        verify(&trust(&[("a", &a)]), "registry.json", BYTES, &set).expect("单签应通过");
    }

    #[test]
    fn tampered_payload_is_rejected() {
        // 这正是签名要防的：元数据被改了，签名就不再匹配。
        let a = SigningKey::from_bytes(&[12u8; 32]);
        let set = SignatureSet::One(entry("a", &a, BYTES));
        let err = verify(&trust(&[("a", &a)]), "registry.json", b"tampered", &set)
            .expect_err("篡改必须拒绝");
        assert!(format!("{err:#}").contains("签名校验失败"), "实得: {err:#}");
    }

    #[test]
    fn revoked_key_is_rejected_even_when_still_trusted() {
        let a = SigningKey::from_bytes(&[13u8; 32]);
        let set = SignatureSet::One(entry("a", &a, BYTES));
        let mut config = trust(&[("a", &a)]);
        config.plugin_revoked_keys = vec!["a".into()];
        let err = verify(&config, "registry.json", BYTES, &set).expect_err("撤销密钥必须拒绝");
        assert!(format!("{err:#}").contains("已撤销"), "实得: {err:#}");
    }

    #[test]
    fn unknown_key_is_rejected() {
        let a = SigningKey::from_bytes(&[14u8; 32]);
        let b = SigningKey::from_bytes(&[15u8; 32]);
        let set = SignatureSet::One(entry("b", &b, BYTES));
        let err = verify(&trust(&[("a", &a)]), "registry.json", BYTES, &set)
            .expect_err("未信任的密钥必须拒绝");
        assert!(format!("{err:#}").contains("未信任"), "实得: {err:#}");
    }

    #[test]
    fn threshold_counts_distinct_keys_only() {
        let a = SigningKey::from_bytes(&[16u8; 32]);
        let b = SigningKey::from_bytes(&[17u8; 32]);
        let mut config = trust(&[("a", &a), ("b", &b)]);

        let both = SignatureSet::Many(vec![entry("a", &a, BYTES), entry("b", &b, BYTES)]);
        config.plugin_signature_threshold = 2;
        verify(&config, "registry.json", BYTES, &both).expect("双签满足阈值 2");

        // 同一把密钥签两次不能凑够 2。
        let twice = SignatureSet::Many(vec![entry("a", &a, BYTES), entry("a", &a, BYTES)]);
        let err = verify(&config, "registry.json", BYTES, &twice).expect_err("重复密钥不能凑数");
        assert!(
            format!("{err:#}").contains("未达到要求的 2"),
            "实得: {err:#}"
        );
    }

    #[test]
    fn unknown_signer_does_not_spoil_a_met_threshold() {
        // 轮换期间 registry 可能带上一把宿主还不认识的签名。
        // 只要已信任的那签够了，就不该因为多一行而整包失败。
        let a = SigningKey::from_bytes(&[18u8; 32]);
        let b = SigningKey::from_bytes(&[19u8; 32]);
        let set = SignatureSet::Many(vec![entry("a", &a, BYTES), entry("b", &b, BYTES)]);
        verify(&trust(&[("a", &a)]), "registry.json", BYTES, &set).expect("已信任的一签足够");
    }

    /// 跨语言夹具：Python `cryptography` 签的 registry，Rust 必须能验过。
    ///
    /// 只做自签自验永远发现不了"两边对签名对象理解不一致"——
    /// 那正是签名机制最典型的静默失效方式。
    #[test]
    fn python_signed_registry_verifies_in_rust() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/registry-signature");
        let body = std::fs::read(dir.join("registry.json")).expect("夹具 registry.json");
        let sig_text = std::fs::read_to_string(dir.join("registry.json.sig")).expect("夹具签名");
        let set: SignatureSet = serde_json::from_str(&sig_text).expect("签名格式");
        let mut config = Config::default();
        config.plugin_trusted_keys = [(
            "fixture".to_string(),
            "A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg=".to_string(),
        )]
        .into_iter()
        .collect();
        verify(&config, "registry 元数据", &body, &set).expect("Python 签的必须能被 Rust 验过");

        // 同一份签名对改动过的 body 必须失败，证明验签真的覆盖了内容。
        let mut tampered = body.clone();
        tampered.push(b' ');
        verify(&config, "registry 元数据", &tampered, &set).expect_err("改动后必须拒绝");
    }

    #[test]
    fn signature_shapes_parse_from_json() {
        let one: SignatureSet =
            serde_json::from_str(r#"{"key_id":"k","signature":"c2ln"}"#).expect("单签对象");
        assert_eq!(one.iter().count(), 1);
        let many: SignatureSet = serde_json::from_str(
            r#"[{"key_id":"k1","signature":"c2ln"},{"key_id":"k2","signature":"c2ln"}]"#,
        )
        .expect("多签数组");
        assert_eq!(many.iter().count(), 2);
    }

    #[test]
    fn malformed_key_material_is_reported() {
        let set = SignatureSet::One(SignatureEntry {
            key_id: "a".into(),
            signature: BASE64.encode([0u8; 64]),
        });
        let mut config = Config::default();
        config.plugin_trusted_keys = [("a".to_string(), "not base64!!".to_string())]
            .into_iter()
            .collect();
        let err = verify(&config, "registry.json", BYTES, &set).expect_err("坏公钥必须报错");
        assert!(format!("{err:#}").contains("base64"), "实得: {err:#}");
    }
}
