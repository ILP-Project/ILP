# AGENTS.md — ILP-rs

## 项目定位

Rust 2021 小说下载器，站点能力通过 Wasmtime WebAssembly 插件提供。插件仓库为 `ILP-Project/plugins`；本仓库提供宿主、共享类型和 WIT ABI。

完整路线图在仓库外：`/home/rhencloud/Project/ILP-Project/plan/2026-10-29_roadmap.md`。

## 必须遵守

- 插件只能通过宿主访问网络、存储、secret 和时间；不能自行发 HTTP 或读写文件。
- 宿主策略是安全边界；registry 的权限和域名字段只是声明。
- 安装前验证 ABI、版本范围、大小、SHA-256、签名和元数据。
- 签名覆盖原始字节；插件签名和 registry 签名分开验证。
- DNS rebinding 防护必须校验所有解析地址；代理场景要求敏感域名使用 `no_proxy`。
- 测试不得访问真实网络；使用 wiremock、fixture 或 replay。
- fixture 必须脱敏，不得包含 Cookie、token、密码、设备 ID 或密钥。
- 不提交 `target/`、密钥、真实凭据或未脱敏日志。
- 提交 subject 和 body 均使用英文。使用 `git commit -F <file>` 编写复杂消息。
- 提交前使用作者：`RhenCloud <rhencloud@users.noreply.github.com>`。
- 用户可见文本和代码注释使用简体中文；字符串集中在 `crates/ilp-host/src/messages.rs`。

## CLI

```text
ilp get chapter|cover|author|title|all <id> -s <site_id> [-f]
ilp update <id> -s <site_id> [--apply]
ilp search <keyword> -s <site_id>
ilp plugin list|info|install|uninstall|enable|disable ...
ilp plugin new <name> [--site-name ..] [--domain ..]
ilp doctor [--bundle <path>] [--with-manifest] [-y]
ilp --jsonl <command>
ilp --verbose <command>
```

`update` 默认 dry-run。`--jsonl` 只输出结构化事件；`--verbose` 提高日志级别。TTY 显示即时进度，非 TTY 不阻塞交互。

## 代码结构

- `crates/ilp-host`：CLI、HTTP、存储、WASM 加载器、插件管理、诊断、导出器。
- `crates/ilp-types`：共享 serde 类型、`ChapterResult`、章节顺序校验。
- `crates/ilp-plugin-wit`：WIT ABI，支持 `0.1.0` 与 `0.2.0`。
- `scripts`：签名安装器和离线安装测试。
- `.github/workflows`：CI 与 Release 构建。

ABI 0.2 的章节目录包含 `order_key`。宿主拒绝重复章节 ID 和非空重复 `order_key`；ABI 0.1 使用原目录顺序。章节结果失败状态不会被静默丢弃，只有成功且有正文时才落盘。导出通过 `Exporter` trait，默认实现为 `TxtExporter`。

## 开发与验证

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
cargo build --release
scripts/install_test.sh
```

插件必须使用：

```bash
cargo build --target wasm32-wasip2 --release
```

生成器验证：

```bash
ilp plugin new example --site-name '示例站' --domain example.com
cargo fmt --check
cargo clippy --target wasm32-wasip2 -- -D warnings
cargo build --target wasm32-wasip2 --release
```

## 分发与签名

默认 registry：`https://ilp.elysola.org`。发布的 WASM 和 Release 二进制都必须验证大小、SHA-256 和 Ed25519 签名。registry 元数据签名为旁路文件 `registry.json.sig`，签名对象是 `registry.json` 原始字节。

发布版本不可变。WASM 使用 `cargo build --target wasm32-wasip2 --release`。不要把本地 path 依赖或 `target/` 带入发布物。

## 历史兼容

ABI tag 或历史提交被重写后，必须重新推送 tag，并用干净环境验证插件构建。消费者需要：

```bash
git fetch --tags --force
```

不要恢复已移除的构建产物或旧版原生插件加载方式。
