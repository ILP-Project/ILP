# registry 签名夹具

由 `build_site.py` 同款逻辑（Python `cryptography`）生成，供宿主侧
`crates/ilp-host/src/signing.rs` 的测试反向校验：

- 私钥：`bytes(range(32))`，**仅测试用**；
- 公钥：`A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg=`；
- 签名对象：`registry.json` 的原始字节（不是重新序列化的结构）。

这组夹具的价值在于跨语言：Python 签出来的东西必须能被 Rust 验过。
只靠 Python 自签自验，永远发现不了"两侧对签名对象理解不一致"的问题。
