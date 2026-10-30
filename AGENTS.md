# AGENTS.md — ILP-rs

## 项目目标

用 Rust 重写 [ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp)：一个模块化、支持多网站的小说下载器 CLI。原版 C++ 源码是唯一蓝图，动手前先读原仓库与 [ILP-Docs](https://github.com/RhenCloud/ILP-Docs)（插件开发文档在 `src/zh/dev/plugins/`）。

## 已锁定决策（勿改）

- **插件机制：WASM + wasmtime**（不是 native cdylib/libloading）。插件产物是 `.wasm`。
- **范围：仅功能对齐，可重构存储**。`config.json`、旧 `cache.db` schema、`txt` 分章 + `covers/` 目录布局均无需兼容旧版数据。
- **任务状态用 JSON manifest，不引入数据库**（旧计划里的 SQLite 方案已废）。
- **插件安装按 registry 名 + 版本**：`ilp plugin install <name>[@<version>]`，
  默认 registry `https://ilp.elysola.org`。`owner/repo` 形式已废。
- **输出只做中文**，但用户可见字符串集中管理。

> 完整路线图和 106 项设计决策见 `ILP-Project/plan/2026-10-29_roadmap.md`
> （**在仓库之外**，不进 git tree）。本文只保留实现时必须知道的事实。

## WASM 插件模型的关键含义（易被忽略）

- 插件是沙箱，不能直接发 HTTP、读写文件、用 SQLite。这些能力必须由宿主通过 WASI 或自定义 host function（imports）注入。
- 因此网络请求、`sleep_time` 限速、`max_workers` 并发、进度判断（跳过已下载）、章节落盘等逻辑应**收敛到宿主**，而非像原版那样由 IPlugin 基类 + 各插件各自用 cpr 发请求。
- 插件安装：`.wasm` 跨平台，单一产物 URL；安装走静态 registry + ABI 预检 + size/sha256 校验。
- **权限声明在 WIT `plugin-metadata` 里，registry 不参与**。真正安全边界是宿主在
  每个 host function 调用点的策略检查，不是插件自己的声明。
- 出口的 SSRF 防线必须覆盖**解析结果**，不能只看 URL 里的主机名：主机名可以解析成公网
  IP 骗过检查后再改指向 `127.0.0.1`（DNS rebinding）。宿主自己解析、逐条校验，
  再把地址钉给 reqwest。

## 需对齐的 CLI 与行为

子命令（原版 CLI11 结构）：

- `get chapter|cover|author|all <id> -s <site_id> [-f/--force_update]`
- `search <keyword> -s <site_id>`（分页交互：`n` 翻页 / 数字跳转 / `q` 退出）
- `plugin list|info|install <name>[@<version>]|uninstall|enable|disable <name>`
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
- 站点插件在独立的 `ILP-Project/plugins` 仓库（一站点一子目录），不在本仓库。

## 命令

- 宿主：`cargo build` / `cargo run -- <args>`
- 插件（在 `ILP-Project/plugins` 仓库里，需先 `rustup target add wasm32-wasip2`）：
  `cargo build --target wasm32-wasip2 --release`
  产物 `target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm` 复制到宿主的 `plugins/` 下。
- `cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo fmt --check`

## 番茄协议链（已端到端跑通，勿重新试错）

```
① 设备注册  POST i.snssdk.com/service/2/device_register/?tt_data=a
   body = TT加密(JSON)，header: tt-data=a, UA: okhttp/4.10.0
   TT加密 = magic[116,99,5,16,0,0] ‖ rand32 ‖ AES-128-CBC(
                key=sha512(sha512(rand32)‖FIXED)[0..16],
                iv =sha512(sha512(rand32)‖FIXED)[16..32],
                sha512(gzip(json)) ‖ gzip(json))
   → device_id / install_id / device_token

② 密钥注册  POST reading.snssdk.com/reading/crypt/registerkey
   明文 = reverse(device_id 大端16字节)[0..8]
   body = gzip(JSON{content: b64(iv16 ‖ AES-256-CBC(shared_key, iv, 明文))})
   需 x-argus + x-ladon + x-khronos + x-ss-req-ticket + X-SS-STUB(md5(body))
   → key(16B 实测) / keyver

③ 正文  GET reading.snssdk.com/reading/reader/{full,batch_full}/v
   data.content = b64(iv16 ‖ AES-128-CBC(key, iv, 密文))
   compress_status==1 → gunzip → XHTML 或 JSON
```

`FIXED` 盐值（base64）：
`TdTC5rgxYgkOUrPHpnM7pByyRiuCmrWKGWs521cXdST0m69/COjWjSanLjfBqVovHwWlGJKu8pSXMrYqOKrdWA==`

**四处反直觉的坑（都是实测打脸的）**：

- 正文密钥是 **16 字节 AES-128**，不是 32 字节 AES-256。
- `FIXED` 抄错两个字节，服务端**静默**回 `device_id=0`（HTTP 仍 200）。
- 设备上报缺 `ipv6_list` 即被拒，同样是 200 + `device_id=0`。
- `now_ms` 必须是 UTC epoch 毫秒，不能用单调时钟。

**限流实测**：背靠背连发只回 1 章；间隔 ≥3 s 回满 30 章。351 章 = 12 批 ≈ 36 秒。
服务端触发限流时会**静默少返回章节**（一批 30 章只回 29 章不报错），
必须检测缺失并逐章单章重试，否则产出带洞的书。

**正文通道唯一性**：网页阅读页已被风控整体封掉（返回验证码中间页），
与并发/UA/Referer 无关；APP 网关是唯一可用正文通道。目录 API 与书籍页仍免签名可用。

**搜索响应结构**：书籍列表不在 `data.search_tab_data`，而在顶层 `search_tabs`
数组中 `tab_type == 3` 的那个 tab，条目正文在 `book_data[0]`，分页凭据是该 tab
自己的 `passback` / `search_id`。

**字体码表**：区间 `0xe3e8`(58344) .. `0xe55b`(58715)，共 372 码点，`?` 表示空位。
静态码表（非轮廓匹配）；长度不符时宁可不解密，直接返回原文。

**代理**：`https_proxy` 会让 `i.snssdk.com` TLS RST/EOF，必须直连。

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
