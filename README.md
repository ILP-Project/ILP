# ILP-rs

基于 Rust 和 WebAssembly 插件的小说下载器。插件只负责站点协议；网络、限速、重试、存储和安全策略由宿主控制。

> 与 ILP-Cpp 不兼容旧配置、数据库和目录格式，首次使用需要重新下载。

## 安装

```bash
curl -fsSLO https://raw.githubusercontent.com/ILP-Project/ILP/main/scripts/install.sh
ILP_PUBKEY='release-2026:<base64-public-key>' sh install.sh
```

安装器支持 `ILP_VERSION`、`ILP_PREFIX`、`ILP_REPO`、`ILP_BASE_URL`。设置 `ILP_PUBKEY` 时先验证 Ed25519 签名，再验证 SHA-256；未设置时会明确提示跳过签名验证。发布平台为 Linux musl、macOS x86_64、macOS arm64。

## 使用

```bash
ilp get chapter|cover|author|title|all <id> -s <site_id> [-f]
ilp update <id> -s <site_id> [--apply]
ilp search <keyword> -s <site_id>
ilp plugin list|info|install|uninstall|enable|disable ...
ilp plugin new <name> [--site-name ..] [--domain ..]
ilp doctor [--bundle <path>] [--with-manifest] [-y]
ilp --jsonl get all <id> -s <site_id>
ilp --verbose get all <id> -s <site_id>
ilp version
```

- `update` 默认只生成新增章节报告；`--apply` 才保存新目录并下载。
- `--jsonl` 输出 `update_report`、`chapter_added`、`chapter_downloaded` 等事件，不混入普通进度文本；字段约定见 [`docs/jsonl-events.md`](docs/jsonl-events.md)。
- `--verbose` 提高日志级别；TTY 显示即时进度，管道和重定向保持稳定输出。
- `search` 在 TTY 中使用 `n`/回车下一页、`p` 上一页、`q` 退出；非 TTY 自动输出全部结果。
- 退出码：`0` 成功，`1` 部分失败，`2` 致命错误。

## 构建与测试

```bash
rustup target add wasm32-wasip2
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
scripts/install_test.sh
```

插件位于独立仓库 `ILP-Project/plugins`：

```bash
cargo build --target wasm32-wasip2 --release
```

主仓库只提供 ABI：`crates/ilp-plugin-wit`。生成插件骨架：

```bash
ilp plugin new my-site --site-name '示例站' --domain example.com
```

## 架构

- `crates/ilp-host`：CLI、WASM 宿主、HTTP、存储、插件管理、诊断。
- `crates/ilp-types`：共享类型、`ChapterResult`、章节顺序校验。
- `crates/ilp-plugin-wit`：WIT ABI，当前支持 ABI `0.1.0` 和 `0.2.0`。
- `docs/`：用户和自动化接口文档。
- `scripts/`：安装器和无网络安装测试。
- `.github/workflows/`：CI 与签名 Release 构建。

插件通过 `plan-fetch` 生成请求，宿主执行请求，再由 `consume-fetch` 解析响应。插件不能直接访问网络、文件或数据库。

正文结果保留 `ok`、`locked`、`captcha`、`auth-required`、`not-found`、`invalid`、`error` 状态；只有成功且有正文的结果才会写入文件。导出通过 `Exporter` trait，目前使用 `TxtExporter`。

章节目录支持 `order_key`。重复章节 ID 或非空重复 `order_key` 会在保存前拒绝；ABI 0.1 没有该字段时使用原目录顺序。

## 数据与诊断

```text
data/
├── novels/<book>_<id>/0001_<chapter>.txt
├── covers/
├── cache.db
└── plugins/<plugin>/<key>
```

章节采用临时文件、校验和和原子 rename；数据库状态、文件存在性和文件大小共同决定是否已完成。

`ilp doctor` 检查环境、插件、签名、网络和日志。`--bundle` 生成 tar.gz 诊断包，打包前显示清单；非交互环境需要 `-y`。默认不包含正文、secret、绝对路径或用户名；日志凭据会脱敏，日志默认保留 7 天。

日志包含 `ilp::summary` 运行摘要，幂等运行也会记录。

## 插件分发与安全

默认 registry：`https://ilp.elysola.org`，可用 `ILP_REGISTRY_URL` 指定镜像。

安装前验证 ABI、版本范围、大小、SHA-256、WASM 签名和元数据。插件权限与域名由宿主策略强制执行；registry 声明只用于展示和审核。registry 元数据签名文件为 `registry.json.sig`，签名对象是原始 JSON 字节。

宿主只允许 HTTPS，并检查解析得到的每个 IP，防止回环、私网和 DNS rebinding。使用 HTTP 代理时无法钉住代理侧解析结果；敏感目标应加入 `no_proxy`。

录制与回放：

```bash
ILP_RECORD_DIR=fixtures/live ilp get all <book-id> -s <site>
ILP_REPLAY_DIR=fixtures/live ilp get all <book-id> -s <site>
```

回放未命中会失败，不会回退到真实网络。fixture 必须脱敏并通过插件仓库的 scanner。

## 限制与许可

番茄插件只覆盖已授权可读的免费章节；锁定章节不能绕过。搜索使用游标分页。

本项目 GPL-3.0，协议实现参考公开的 ILP-Cpp 和 fanqie-assistant 资料。
