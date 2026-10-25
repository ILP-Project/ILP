# AGENTS.md — ILP-rs

## 项目目标

用 Rust 重写 [ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp)：一个模块化、支持多网站的小说下载器 CLI。原版 C++ 源码是唯一蓝图，动手前先读原仓库与 [ILP-Docs](https://github.com/RhenCloud/ILP-Docs)（插件开发文档在 `src/zh/dev/plugins/`）。

## 已锁定决策（勿改）

- **插件机制：WASM + wasmtime**（不是 native cdylib/libloading）。插件产物是 `.wasm`。
- **范围：仅功能对齐，可重构存储**。`config.json`、`cache.db` schema、`txt` 分章 + `covers/` 目录布局均无需兼容旧版数据。

## WASM 插件模型的关键含义（易被忽略）

- 插件是沙箱，不能直接发 HTTP、读写文件、用 SQLite。这些能力必须由宿主通过 WASI 或自定义 host function（imports）注入。
- 因此网络请求、`sleep_time` 限速、`max_workers` 并发、进度判断（跳过已下载）、章节落盘等逻辑应**收敛到宿主**，而非像原版那样由 IPlugin 基类 + 各插件各自用 cpr 发请求。
- 插件安装：`.wasm` 跨平台，`plugin.json` 里按 OS 区分的 `url` 映射可简化为单一产物 URL；但「拉 GitHub 上的 `plugin.json` 再下载产物」的安装流程应保留。

## 需对齐的 CLI 与行为

子命令（原版 CLI11 结构）：

- `get chapter|cover|author|all <id> -s <site_id> [-f/--force_update]`
- `search <keyword> -s <site_id>`（分页交互：`n` 翻页 / 数字跳转 / `q` 退出）
- `plugin list|install <owner/repo>|uninstall|enable|disable <name>`
- `version`

`-s/--site_id` 需校验是否为已加载插件的 `site_id`（原版 `CLI::IsMember(site_ids)`）。

## 插件接口（等价 trait）

元数据：`site_name` / `site_id` / `site_domain` / `plugin_name` / `plugin_id` / `version`。
方法：`init(id, force_update)`、`getContent(force_update)`（目录抓取 + 缓存）、`getTitle` / `getAuthor` / `getCover`、`fetchAllChapter`（并发 + 跳过已下载）、`search(keyword)`、`parseChapter`（HTML/JSON → 纯文本）。

章节目录条目字段：`id`、`title`、`url`（阅读页）、`fetch_url`（内容 API，`fetchOneChapter` 用它）、`update_time`（Unix 时间戳）。

## 原版依赖 → Rust 建议（非强制）

`cpr`→`reqwest` · `fmt`→`println!` · `nlohmann/json`→`serde`/`serde_json` · `SQLiteCpp`→`rusqlite` · `CLI11`→`clap`。

## 工程结构

- `crates/ilp-types`：共享 serde 类型（`PluginMetadata` / `ChapterEntry` / `SearchResult` / `ParseInput`）。
- `crates/ilp-host`：宿主二进制 `ilp`（clap CLI、config、storage、http、wasmtime loader、plugin manager）。
- `crates/ilp-plugin-wit`：插件 ABI（`wit/plugin.wit`），宿主与插件共享的唯一来源。
- 站点插件独立仓库：`ILP-Project/plugin-<site>`（如 `plugin-fanqie`），不在本仓库。

## 命令

- 宿主：`cargo build` / `cargo run -- <args>`
- 插件（在 `plugin-<site>` 仓库里，需先 `rustup target add wasm32-wasip2`）：
  `cargo build --target wasm32-wasip2 --release`
  产物 `target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm` 复制到宿主的 `plugins/` 下。
- `cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`

## 原版已知问题（重写时规避）

- 翻页：search 交互里 `page++` 双重递增导致跳页错位；重写注意翻页/跳转逻辑。
- 标题未清洗：原版用 `title` 直接作目录名/文件名/SQLite 表名 → Windows 非法字符崩溃 + SQL 注入。重写时对标题 sanitize，DB 表名改用固定 schema + id。
- 续传误判：只看文件是否存在，写一半崩溃被永久跳过；落盘用「临时文件 + rename」原子写，或写入后校验完整性。
- HTTP 全裸：无超时/重试/状态码检查/UA（`RandomUserAgent` 写了从未用）。收敛到宿主后统一加超时、重试、UA。
- 并发：`TaskManager` 每章 `std::async` 一个线程且无上限，`sleep_time` 在任务完成后仍占并发槽；宿主改用有界 worker + 信号量限速。
- 配置：值存字符串反复 `stoi`，缺失即抛异常崩溃；改为类型化配置一次解析。
- fanqie 重复请求 `detail` API（`getTitle`/`getCover`/`getContent` 各拉一次）；加缓存。
- HTML 用正则 `(<.*?>)+` 解析脆弱；用 html5ever/scraper，JSON 用 serde。
- 无日志：`logs_folder` 配了但 Logger 被注释；补 tracing。
- 插件安装无校验：不查下载状态/大小、`source` 未校验、`refs/heads/main` 硬编码。
- 一个坏插件拖垮启动（`loadPlugin` 抛异常不捕获）；单个插件加载失败应隔离降级。
- 原生插件加载 = 任意代码执行；WASM 沙箱天然解决（这是选 wasmtime 的额外收益）。

## 约定

- CLI 用户交互字符串与注释用中文；代码不加多余注释（见全局 AGENTS.md）。
- 依赖/开发环境优先 Nix flake devShell。

## 参考

- 插件示例：[FanqiePlugin](https://github.com/RhenCloud/FanqiePlugin)（fanqienovel.com）、[plugin-template](https://github.com/RhenCloud/plugin-template)
