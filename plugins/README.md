# ILP 插件开发

插件使用 ABI `0.2.0`，共享接口定义位于 `crates/ilp-plugin-wit/wit/v2/plugin.wit`。

## 快速开始

从模板复制一个插件：

```bash
cp -R plugins/template plugins/my-site
```

然后修改 `plugins/my-site/Cargo.toml` 的 package 名称，以及 `src/lib.rs` 中的 metadata 和业务逻辑。

## 构建

```bash
cargo build --manifest-path plugins/my-site/Cargo.toml \
  --target wasm32-wasip2 --release
```

宿主安装前会校验 ABI、WASM component、大小、SHA-256 和签名。

## 元数据治理

每个插件必须在 `plugin.toml` 中声明：

- `license`：代码许可证；
- `data_source`：数据来源；
- `authorization_boundary`：授权边界与不支持的行为；
- `maintenance`：`active`、`limited`、`unmaintained` 或 `archived`；
- `known_limitations`：已知限制列表；
- `permissions`：插件实际申请的宿主能力（`http`、`clock`、`storage-read`、
  `storage-write`、`secret-read`、`secret-write`）；
- `allowed_domains`：插件实际访问的域名。申请了 `http` 就必须写，且不允许通配；
  站点主域名必须被它覆盖。

发布脚本会拒绝缺少这些字段、维护状态无效、许可证不在允许列表、或
`Cargo.toml` 与 `plugin.toml` 许可证不一致的插件，并把元数据写入 registry 与插件页面。

`permissions`/`allowed_domains` 是**唯一会被要求与代码一致的元数据**：它们必须
与插件 `metadata()` 的返回值逐项相同。插件侧的契约测试
（模板里的 `manifest_matches_metadata`）让 `cargo test` 直接拦下不一致。宿主的
安全策略取的是 WASM 自己那份，registry 这份只用于展示与审核，因此宿主在
**安装时**还会再比对一次：WASM 比 registry 多申请一项能力就拒绝安装（注册表
隐瞒了插件真实能力），registry 多写则只告警（文档过错，不影响安全）。

## 离线回放与录制

插件所有网络都走宿主，所以宿主可以在 HTTP 层拦截，把真实站点换成录像。

```bash
# 录制：真实联网跑一遍，把请求/响应写成录像
ILP_RECORD_DIR=fixtures/live ilp get all <book-id> -s my-site

# 回放：完全离线，一条录像都没命中就报错
ILP_REPLAY_DIR=fixtures/live ilp get all <book-id> -s my-site
```

录像格式是 schema v1 的 JSON：请求与响应分离，body 用 base64（正文可能是
gzip 二进制）。回放未命中**报错而不是偷偷联网**：一个本该离线的测试静默打到
真实站点，既不可复现，也可能触发风控。

匹配规则：

- 严格键 = 方法 + 脱敏后的 URL + 请求体 SHA-256；
- 严格键未命中时，若同一 URL 只有一条录像，允许兜底命中（设备注册、埋点这类
  请求体每次不同的接口）；同一 URL 有多条时**报错而不是猜**。

### 脱敏

宿主写入录像前会自动脱敏，不需要手工处理：

- 头名包含 `authorization`/`cookie`/`token`/`secret`/`password`/`session` 等
  标记的值全部换成 `<redacted>`（子串匹配，不是固定名单——固定名单一定会漏）；
- 同类标记的 query 参数（含 `device_id`、`install_id`、`iid`）同样脱敏；
- 写盘前再自检一遍，命中就拒绝写入。

设备身份被脱敏还有一个副作用，而且是需要的：`iid`/`device_id` 每次都重新生成，
不脱敏的话同一个逻辑请求在录与放两侧的键永远对不上。

手工编写的 fixture 不走宿主，所以插件仓库的 CI 会跑
`python3 tools/scan_fixtures.py .` 再扫一遍。

## 测试与发布门禁

```bash
cargo fmt --check
cargo clippy --manifest-path plugins/my-site/Cargo.toml \
  --target wasm32-wasip2 -- -D warnings
cargo test --manifest-path plugins/my-site/Cargo.toml
```

模板插件位于 `plugins/template`，用于验证最小 ABI 实现和构建路径。
