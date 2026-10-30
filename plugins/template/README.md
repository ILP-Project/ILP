# ILP 插件模板

这是一个最小的 ABI `0.2.0` 插件模板。复制此目录后修改 `Cargo.toml`、`src/lib.rs` 和 metadata，即可开始开发。

## 构建

在仓库根目录执行：

```bash
cargo build --manifest-path plugins/template/Cargo.toml --target wasm32-wasip2 --release
```

产物位于：

```text
target/wasm32-wasip2/release/ilp_plugin_template.wasm
```

## 开发约束

- `metadata()` 中的权限是申请，不是宿主授权；每次 host 调用仍会被宿主策略复查。
- 插件不应实现账号密码登录，只处理用户已授权的 Cookie/Token。
- 所有章节都应返回结构化 `ChapterResult`，不要静默丢失失败章节。
- 提交前执行 `cargo fmt --check`、`cargo clippy --target wasm32-wasip2 -- -D warnings` 和 WASM release build。
- 解析逻辑应使用 `fixtures/` 中的脱敏 JSON 回放数据，不在测试中访问真实站点。
- fixture schema v1 将请求结构与响应内容分离，正文以 Base64 保存；不得提交 Cookie、Token 或设备密钥。
