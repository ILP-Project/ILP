# AGENTS.md — ILP-rs

## 项目定位

Rust 2021 小说下载器，站点能力通过 Wasmtime WebAssembly 插件提供。插件仓库为 `ILP-Project/plugins`；本仓库提供宿主、共享类型和 WIT ABI。

完整路线图在仓库外：`/home/rhencloud/Project/ILP-Project/plan/2026-10-29_roadmap.md`。

## 必须遵守

- 插件只能通过宿主访问网络、存储、secret 和时间；不能自行发 HTTP 或读写文件。
- 宿主策略是安全边界；registry 的权限和域名字段只是声明。
- 安装前验证 ABI、版本范围、大小、SHA-256、签名和元数据。
- 默认强制要求 `registry.json.sig`；仅内部未签名镜像可显式关闭 `require_registry_signatures`。
- 签名覆盖原始字节；插件签名和 registry 签名分开验证。
- DNS rebinding 防护必须校验所有解析地址；代理场景要求敏感域名使用 `no_proxy`。
- 测试不得访问真实网络；使用 wiremock、fixture 或 replay。
- fixture 必须脱敏，不得包含 Cookie、token、密码、设备 ID 或密钥。
- 不提交 `target/`、密钥、真实凭据或未脱敏日志。
- 提交 subject 和 body 均使用英文。使用 `git commit -F <file>` 编写复杂消息。
- 提交前使用作者：`RhenCloud <rhencloud@users.noreply.github.com>`。
- 用户可见文本和代码注释使用简体中文；字符串集中在 `crates/ilp-host/src/messages.rs`。

## 版本与 Release 规则

- 当前正式版本为 `v0.1.0`；宿主 crate 版本为 `0.1.0`，WIT ABI 版本独立维护，当前支持 ABI `0.1.0` 和 `0.2.0`。
- 版本号遵循 SemVer，格式为 `MAJOR.MINOR.PATCH`，禁止使用与实际发布版本不一致的 tag、Cargo manifest 或安装器版本。
- `PATCH`：仅修复 bug、安全问题、构建/发布问题或不改变公开 API/ABI 行为的内部调整；例如 `0.1.0` → `0.1.1`。
- `MINOR`：在兼容现有用户、CLI、配置、数据格式和 ABI 的前提下增加功能或公开 API；例如 `0.1.0` → `0.2.0`。
- `MAJOR`：删除或修改公开 CLI/API、破坏配置/数据兼容性、改变插件加载契约，或引入不兼容 ABI；例如 `0.2.0` → `1.0.0`。在 `0.x` 阶段，`0.MINOR.0` 的 minor 升级按不兼容变更处理，patch 仍须保持兼容。
- ABI 版本独立于宿主版本：修改 WIT 接口、资源、调用语义或兼容策略时才升级 ABI；普通宿主功能、文档和修复不得升级 ABI。新增 ABI 必须保留旧 ABI 加载能力，或明确记录迁移/弃用策略。
- 版本更新必须在一次提交中同步修改所有受影响的 workspace `Cargo.toml`、`Cargo.lock`（若依赖解析发生变化）、CLI `version` 输出、README、安装器、workflow、registry 元数据和发布说明；先搜索旧版本号，避免遗漏。
- 发布前必须同步检查 workspace 中所有可发布 crate 的 `Cargo.toml` 版本、README、安装器和 workflow；path 依赖同时声明兼容的 registry `version`，确保 `cargo package --workspace --locked --no-verify` 通过。
- 版本提交必须使用英文 subject，建议格式为 `release: v<version>`；版本提交、签名 tag 和 GitHub Release 必须使用同一个版本号。
- 新版本发布前必须确认上一版本的工作区、tag、Release 资产和 registry 元数据状态，禁止覆盖已公开版本；正式版本号一旦发布，除非按本文档的发布恢复规则重建，否则不得复用。
- Release tag 使用签名的 annotated tag，格式为 `v<version>`，例如：`v0.1.0`。tag 指向的提交必须已经合并到 `main`，工作区必须干净。
- Release 私钥只能存储在 GitHub Actions Secret `ILP_SIGNING_KEYS`，不得打印、提交、写入配置或测试 fixture；仓库和用户配置只保存公钥。当前官方 key ID 为 `release-2026`。
- 发布前必须通过：
  ```bash
  cargo fmt --all --check
  cargo clippy --all-targets -- -D warnings
  cargo test --workspace
  cargo build --release --locked
  cargo package --workspace --locked --no-verify
  scripts/install_test.sh
  git diff --check
  ```
- Release workflow 必须为每个可用平台构建产物，生成 `SHA256SUMS` 和 `SHA256SUMS.sig`；签名覆盖校验清单原始字节，registry 仍须单独签名。
- 当前官方 Release 平台为 Linux x86_64、Linux ARM64 和 Windows x86_64；不可用的 runner 不得阻塞发布，应从矩阵移除并在 Release 说明中明确缺失平台。macOS 产物只有 runner 可用且构建验证通过时才加入。
- 发布失败时不得手工上传未经 workflow 验证的二进制；应修复 workflow 后删除并重新创建同名 tag，再重新触发发布。
- 发布完成后必须验证 GitHub Release、全部资产、`SHA256SUMS`、`SHA256SUMS.sig`、签名 key、安装器和 `ilp version` 启动闭环；下载真实 Release 产物进行 SHA-256 校验。
- 正式版本发布后 tag 和 Release 资产视为不可变。若确需修复发布流程，必须记录原因；重建同一版本前先取消旧 workflow、删除旧 Release/tag，并重新签名推送 tag。
- 历史提交重写、作者邮箱变更或 GPG key 变更后，必须重新签名相关提交和 tag，并 force push 前确认远端分支、tag 与 Release 的对应关系。

## CLI

```text
ilp get chapter|cover|author|title|all <id> -s <site_id> [-f]
ilp update <id> -s <site_id> [--apply]
ilp search <keyword> -s <site_id>
ilp plugin list|info|install|update|uninstall|enable|disable ...
ilp plugin new <name> [--site-name ..] [--domain ..]
ilp doctor [--bundle <path>] [--with-manifest] [-y]
ilp --jsonl <command>
ilp --verbose <command>
```

`update` 默认 dry-run；`plugin update <name>` 同样默认只报告版本差异，`--apply` 才替换插件。`--jsonl` 只输出结构化事件，字段约定见 `docs/jsonl-events.md`；`--verbose` 提高日志级别。TTY 显示即时进度，非 TTY 不阻塞交互。

## 代码结构

- `crates/ilp-host`：CLI、HTTP、存储、WASM 加载器、插件管理、诊断、导出器。
- `crates/ilp-types`：共享 serde 类型、`ChapterResult`、章节顺序校验。
- `crates/ilp-plugin-wit`：WIT ABI，支持 `0.1.0` 与 `0.2.0`。
- `docs`：用户和自动化接口文档。
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
