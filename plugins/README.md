# ILP 插件开发

插件使用 ABI `0.2.0`，共享接口定义位于 `crates/ilp-plugin-wit/wit/v2/plugin.wit`。

## 快速开始

一条命令生成骨架：

```bash
ilp plugin new my-site --site-name "我的站点" --domain mysite.com
```

`--site-name`、`--site-id`、`--domain` 都可省略，默认从插件名推导。
生成的目录自带 `plugin.toml`、`src/lib.rs`、`src/fixture.rs` 和示例 cassette，
占位符已按你给的参数填好，可以直接构建。

然后：

```bash
cargo build --manifest-path my-site/Cargo.toml --target wasm32-wasip2 --release
ilp plugin install my-site/target/wasm32-wasip2/release/ilp_plugin_my_site.wasm
```

生成的 `plugin.toml` 里 `data_source` 和 `authorization_boundary` 还是模板文案，
**必须改成你站点的真实情况**——这两项是发布门禁的硬性要求，不是建议。

## 构建

宿主安装前会校验 ABI、WASM component、大小、SHA-256 和签名。

## 提交贡献

发布到官方 registry 之前，这些会自动跑：

| 门禁 | 内容 |
|---|---|
| `cargo test` | `manifest_matches_metadata` 校验 `plugin.toml` 与 WASM 元数据一致 |
| `scan_fixtures.py` | 扫描 fixture 里的 Cookie、token、密码、设备 ID，命中即失败 |
| `build_site.py` | 许可证白名单、权限枚举、域名格式、`min/max_host_version`、必需元数据 |
| CI 断言 | `registry.json` 必含 `permissions`、`allowed_domains`、`license`、`maintenance`、各版本 `sha256` |

**从真实流量录制 cassette 之前先读 `fixtures/README.md`**：录制器会自动脱敏
常见的秘密头与参数，但它靠的是标记匹配，不是保证——录完必须自己看一眼。

## 评审清单（人工项）

自动门禁能拦的是"格式不对"和"已知秘密形态"。下面这些机器判断不了，
由维护者在合并前确认：

- **`data_source` 与 `authorization_boundary` 是否属实。** 写了"公开内容"
  但实际需要登录、或绕过了访问控制，这是拒绝合并的理由，不是改文案。
- **源码里有没有硬编码凭据。** 扫描器不扫源码——源码里出现 `"Cookie"`
  这个字符串是完全正常的（插件要设置这个头），而高熵字面量又会被 SM3
  测试向量这类东西塞满误报。这一项只能人看。
- **新增域名是否必要。** `allowed_domains` 每多一项，插件能去的地方就多
  一处。CDN 域名、埋点域名都应该说明来源。
- **权限是否最小。** 只用到 `http` 就不要顺手加 `storage-write`；
  宿主会按 WASM 实际声明执行，多申请等于白拿。
- **cassette 是否来自公开内容。** 带登录态的录像即使脱敏过，也可能残留
  会话指纹。

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
  站点主域名必须被它覆盖；
- `min_host_version` / `max_host_version`：宿主兼容区间（闭区间，`x.y.z`）。

发布脚本会拒绝缺少这些字段、维护状态无效、许可证不在允许列表、或
`Cargo.toml` 与 `plugin.toml` 许可证不一致的插件，并把元数据写入 registry 与插件页面。

`permissions`/`allowed_domains` 是**唯一会被要求与代码一致的元数据**：它们必须
与插件 `metadata()` 的返回值逐项相同。插件侧的契约测试
（模板里的 `manifest_matches_metadata`）让 `cargo test` 直接拦下不一致。宿主的
安全策略取的是 WASM 自己那份，registry 这份只用于展示与审核，因此宿主在
**安装时**还会再比对一次：WASM 比 registry 多申请一项能力就拒绝安装（注册表
隐瞒了插件真实能力），registry 多写则只告警（文档过错，不影响安全）。

`min_host_version`/`max_host_version` 与 ABI 是两件事：ABI 只锁接口形状，
管不了燃料预算、限流策略、章节落盘语义这些行为差异。宿主安装时校验自己的
版本是否落在区间内，不在就拒绝——否则一次宿主升级会让插件静默变慢或变错，
用户只看到“下载失败”。缺这两个字段（旧 registry）视为未声明，一律放行。

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
