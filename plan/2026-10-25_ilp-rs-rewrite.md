# ILP-rs 重写实施计划

> 目标：用 Rust 重写 [ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp)，功能对齐，插件机制改为 WASM（wasmtime）。
> 本文档中的每个事实都经过实测验证，无推测项。

---

## 零、已锁定的决策（不要再讨论）

| # | 决策 | 说明 |
|---|---|---|
| 1 | **Rust 重写** | 用户明确要求 |
| 2 | **范围** | 功能对齐一次做完，不分期交付保底版本 |
| 3 | **插件 ABI** | wit-bindgen 组件模型，目标 `wasm32-wasip2`，宿主用 `wasmtime::component::Linker` |
| 4 | **取数模型** | 混合：目录/搜索/详情=同步重入；正文=两阶段（插件报 `fetch_url`，宿主并发取回后交回 `parse_chapter`） |
| 5 | **存储布局** | `data/novels/<sanitized-title>_<book_id>/<seq>_<sanitized-chapter>.txt` + SQLite `chapters(site_id, book_id, chapter_id, path, status, bytes, mtime)` |
| 6 | **`-f/--force_update`** | 只刷新元数据；另设 `--redownload` 重下正文 |
| 7 | **`search` 分页** | `n`/空=下一页，`p`=上一页，数字=跳转（带边界检查），`q`=退出；stdin 非 TTY 时自动全量打印 |
| 8 | **`plugin install`** | 单一 URL + 完整校验：owner/repo 正则、ref 回退 `main`→`master`、HTTP 状态与长度、wasm magic/version、name 白名单（防路径穿越） |
| 9 | **配置与日志** | 逐字段 `#[serde(default)]`，未知字段告警；日志双写 `logs_folder` 滚动文件 + stderr，级别可配 |
| 10 | **并发** | 有界 worker + 任务间限速 + 重试：sleep 在取下个任务**之前**，失败重试 2 次指数退避，最终失败记 DB `status=failed` 不中断全书 |
| 11 | **错误与退出码** | `0` 全成功 / `1` 部分章节失败 / `2` 致命；消息简洁，`RUST_LOG` 看链路 |
| 12 | **测试** | 三层，不用真实网络：单元、wiremock HTTP、wasmtime 端到端 |
| 13 | **字体解密** | 静态码表（非轮廓匹配），`0xe3e8..0xe55b` 共 372 码点，`?` 为空位 |
| 14 | **APP 通道** | 全量实现：设备注册 + argus/ladon 签名 + registerkey + AES 正文解密 |
| 15 | **WASM 边界** | 宿主只提供 `http`/`log`/`storage`；密码学全在插件内（纯 Rust crate 可编进 wasm） |
| 16 | **许可** | 照公开规范独立实现（SM3 国标 / Simon-Speck NSA 规范 / RustCrypto AES），只取常量与码表，不移植 GPLv3 的 TS 代码 |
| 17 | **代理** | 读环境变量 + `no_proxy` 豁免名单（`i.snssdk.com` 必须直连） |
| 18 | **限流** | 1 worker + 30 章/批 + 间隔 3 秒；服务端只回部分时自动降批重试 |
| 19 | **正文渲染** | scraper 抽 `<blk>` 纯文本，两条通道产出格式一致的 `.txt` |
| 20 | **设备持久化** | `device_id`/`install_id`/`key`/`keyver` 存 `cache.db`，`keyver` 不符或 `content=="Invalid"` 时自动重注册 |

---

## 一、实测验证的事实基础

### 1.1 番茄站点能力（2026-10-25 实测）

| 能力 | 端点 | 状态 |
|---|---|---|
| 目录 | `fanqienovel.com/api/reader/directory/detail?bookId=<id>` | ✅ **免签名**，98.5 KB，351 章 |
| 书籍元数据 | `/page/<book_id>` HTML | ✅ h1 标题 + `"author":"…"` + 封面 |
| 免费章正文 | `/reader/<item_id>` HTML | ✅ 完整（第1章 4344 字） |
| 锁定章正文 | 同上 | ⚠️ **仅 173 字截断**（第51章起 `isChapterLock:true`） |
| 搜索 | `reading.snssdk.com/reading/bookapi/search/tab/v` | ✅ 411 KB，需签名 |
| 单章正文 | `reading.snssdk.com/reading/reader/full/v` | ✅ 需签名 + AES 解密 |
| 批量正文 | `reading.snssdk.com/reading/reader/batch_full/v` | ✅ 30 章/批，858 KB，0.3 s |
| 详情 API | `reading/bookapi/multi-detail/v/` | ❌ 200 但 0 字节（上游已死） |
| 旧正文 API | `novel.snssdk.com/.../full/v1/` | ❌ 0 字节 / 参数错误 |

**结论**：网页通道只能拿到 1/7 正文；全本必须走 APP 通道。

### 1.2 完整协议链（已端到端跑通）

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
   → key(32B) / keyver
   响应 key 同样 b64(iv16 ‖ 密文)，用 shared_key 解

③ 正文  GET reading.snssdk.com/reading/reader/{full,batch_full}/v
   data.content = b64(iv16 ‖ AES-256-CBC(key, iv, 密文))
   compress_status==1 → gunzip → XHTML 或 JSON
```

**实测结果**：第 121 章网页只给 173 字 → APP 通道拿到 **20,725 字**；30 章批 617,675 字全解。

### 1.3 限流规律（实测）

| 场景 | 服务端返回 |
|---|---|
| 背靠背连发 | 只回 **1 章** |
| 间隔 ≥3 s | 回满 **30 章** |

351 章 = 12 批 ≈ 36 秒。

### 1.4 字体码表（已验证）

```
区间 0xe3e8(58344) .. 0xe55b(58715)，共 372 码点
其中 '?' 10 个（字体里无字形的空位）→ 实际字形 362 个
与探测到的 cmap 362 码点精确吻合（双重验证）
```

三张表：`DNMrHsV173Pd4pgy`（正文，**搜索页也用它但语义不同**）、`fKts9tCXDjS49UhH`（正文第二套）、`_search`（搜索页强制覆盖）。

第 1 章 4223 字符 / 2244 PUA 命中 / 248 个不同字符 → 解密后**残留 PUA 为 0**。

### 1.5 WASM 工具链（编译 + 运行均已验证）

| 项 | 结论 |
|---|---|
| wasmtime | `48.0.1`（实际锁定版本） |
| wit-bindgen | `0.46` |
| guest target | `wasm32-wasip2`，**不需要 `cargo component`** |
| host 注册 | `NovelPlugin::add_to_linker::<St, HasSelf<St>>(&mut linker, \|s\| s)?` |
| WASI | `wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?`（必需，组件会带入 `wasi:io/poll@0.2.9`） |
| Store 数据 | `impl WasiView for St { fn ctx(&mut self) -> WasiCtxView { WasiCtxView { ctx, table } } }` |
| guest 侧 | `wit_bindgen::generate!({ world, path })` + `impl Guest` + `export!(Comp)` |

实测输出：
```
[guest log] hello from guest
metadata = site=fanqie|v=1.0
fetch    = Ok("guest got: host-fetched(https://example.com/x)")
```

### 1.6 当前代码库问题清单

`cargo check --all-targets` 报 **7 个错误**：

| 位置 | 问题 |
|---|---|
| `engine.rs:158/184` | `&Store<WasiP1Ctx>` 不满足 `AsContextMut`；还对 `Option` 用了 `?` |
| `engine.rs:245` | `caller` 在 `ilp_log` 里被 move 后又借用 |
| `http.rs:47` | `anyhow::Error` 非 `Clone` → `Vec<Option<Result<Vec<u8>>>>` |
| `main.rs:20` | `cli::Command::Plugin` 是结构体变体却按元组匹配 |
| `storage.rs:4` | 未使用的 `use std::path::Path` |

其他差距：无 `.wit`、无构建好的 `plugins/*.wasm`、无 `version` 子命令、`search` 非交互、fanqie `ilp_search` 返回空数组、`strip_html` 用朴素正则、`plugin install` 只读单个 `url` 字段、`refs/heads/main` 硬编码、无校验、`name` 可路径穿越、`logs_folder` 未使用、零测试。

---

## 二、提交计划（11 个，每个独立可编译）

### ① 修复 7 个编译错误
- `cargo check --all-targets` 通过
- 不改逻辑，只修类型与借用

### ② 配置类型化 + tracing 双写
- `config.rs`：逐字段 `#[serde(default)]`，未知字段 `warn!`
- 日志：`logs_folder` 滚动文件 + stderr，级别可配
- 新增 `proxy` / `no_proxy` 配置（默认豁免 `i.snssdk.com`）

### ③ `.wit` + 组件加载器 + 宿主能力
- `wit/plugin.wit`：`host` 接口（`log` / `http-fetch` / `cache-get` / `cache-set`）
- `engine.rs` 重写为 `wasmtime::component`
- `Store<St>` + `WasiView` + `HasSelf<St>`

### ④ fanqie 改为 wit guest
- `plugins/fanqie` 目标改 `wasm32-wasip2`
- `wit_bindgen::generate!` + `impl Guest` + `export!`
- 行为与现状等价（此提交不引入新功能）

### ⑤ 网页通道打通
- 目录 API（`bookId` 参数、免签名）
- `/page/<id>` 抓标题/作者/封面
- `/reader/<id>` 抓正文 + 字体码表解密
- **里程碑**：`ilp get chapter <id> -s fanqie` 能下第 1 章

### ⑥ storage 重构 + 续传校验
- 临时文件 + rename 原子写
- SQLite `chapters` 表，续传校验 DB + 文件大小

### ⑦ 并发 / 重试 / 退出码
- 有界 worker + sleep 在取任务前 + 2 次指数退避
- 退出码 0/1/2

### ⑧ search 分页交互
- `n`/`p`/数字/`q`，修正原版 `page++` 双重递增 bug
- 非 TTY 自动全量打印

### ⑨ plugin 子命令加固 + `version`
- `list` / `install` / `uninstall` / `enable` / `disable`
- name 白名单、wasm magic 校验、ref 回退
- `version` 子命令

### ⑩ APP 通道
- 设备注册（TT 加密）
- argus/ladon 签名（SM3 / Simon128-256 / Speck128-256 / protobuf / AES）
- registerkey + 正文 AES 解密
- 30 章/批 + 3 s 限速 + 自动降批重试
- **里程碑**：351 章全本

### ⑪ 测试三件套
- 单元：sanitize / 分页状态机 / plugin.json 解析校验 / config 回退 / 字体码表 / SM3 测试向量
- HTTP：wiremock
- 端到端：wasmtime + mock 插件

---

## 三、目录结构（目标）

```
ILP-rs/
├── Cargo.toml                     # workspace
├── wit/plugin.wit                 # 组件接口定义
├── crates/
│   ├── ilp-types/                 # 共享 serde 类型
│   └── ilp-host/
│       ├── src/
│       │   ├── main.rs
│       │   ├── cli.rs
│       │   ├── commands.rs
│       │   ├── config.rs          # 类型化配置
│       │   ├── logging.rs         # 双写
│       │   ├── engine.rs          # wasmtime component 加载
│       │   ├── http.rs            # reqwest + 代理/重试/UA
│       │   ├── storage.rs         # 原子写 + SQLite
│       │   ├── plugin_manager.rs
│       │   └── fanqie/            # APP 通道协议实现
│       │       ├── mod.rs
│       │       ├── device.rs      # 设备注册 + TT 加密
│       │       ├── sign.rs        # argus / ladon
│       │       ├── crypto/
│       │       │   ├── sm3.rs
│       │       │   ├── simon.rs
│       │       │   ├── speck.rs
│       │       │   └── protobuf.rs
│       │       └── content.rs     # registerkey + 正文解密
│       └── tests/
├── plugins/
│   ├── fanqie/
│   │   ├── Cargo.toml             # cdylib, wasm32-wasip2
│   │   └── src/lib.rs             # wit guest
│   └── mock/                      # 测试用契约插件
└── flake.nix
```

---

## 四、关键实现细节

### 4.1 代理处理

```rust
// i.snssdk.com 走代理会 TLS RST，必须直连
let mut builder = reqwest::Client::builder().no_proxy();  // 基线：直连
if let Some(p) = config.proxy {
    builder = reqwest::Client::builder().proxy(reqwest::Proxy::all(p)?);
    for host in &config.no_proxy {          // 默认含 "i.snssdk.com"
        builder = builder.no_proxy(reqwest::NoProxy::from_string(host)?);
    }
}
```

### 4.2 正文批量拉取状态机

```
for chunk in chapters.chunks(30):
    resp = batch_full(chunk)
    if resp.len() == 1 && chunk.len() > 1:
        // 触发限流：退避后降批重试
        sleep(3s)
        for c in chunk: 单章重试
    else:
        write each
    sleep(3s)          // 批间隔
```

### 4.3 字体解密

```rust
const CODE_ST: u32 = 58344;
const CODE_ED: u32 = 58715;
const NO_GLYPH: char = '?';

fn decrypt(text: &str, font_id: &str) -> String {
    let Some(table) = mapping(font_id) else { return text.into() };
    if table.len() != (CODE_ED - CODE_ST + 1) as usize { return text.into() }  // 长度不对宁可不解
    text.chars().map(|c| {
        let cp = c as u32;
        if !(CODE_ST..=CODE_ED).contains(&cp) { return c }
        match table[(cp - CODE_ST) as usize] {
            NO_GLYPH => c,
            m => m,
        }
    }).collect()
}
```

### 4.4 续传校验

```rust
// 原版只看文件存在 → 写一半崩溃永久跳过。改：
match db.get(site_id, book_id, chapter_id) {
    Some(rec) if rec.status == "ok"
        && Path::new(&rec.path).metadata().map(|m| m.len()).ok() == Some(rec.bytes) => skip,
    _ => download(),
}
```

### 4.5 标题清理

```rust
// Windows 非法字符 + 长度 + 尾随点/空格
fn sanitize(title: &str) -> String {
    let s: String = title.chars()
        .map(|c| match c { '<'|'>'|':'|'"'|'/'|'\\'|'|'|'?'|'*'|'\0' => '_', c => c })
        .collect();
    let s = s.trim().trim_end_matches('.').trim();
    s.chars().take(120).collect()
}
```

---

## 五、验收标准

| 项 | 标准 |
|---|---|
| 编译 | `cargo build --all-targets` 零错误零警告 |
| 测试 | `cargo test` 全绿，无真实网络 |
| Lint | `cargo clippy --all-targets -- -D warnings` 通过 |
| 格式 | `cargo fmt --check` 通过 |
| 功能 | `ilp get chapter 7069948840148732967 -s fanqie` 下完 351 章，正文通顺无 PUA |
| CLI | `get chapter\|cover\|author\|all` / `search` / `plugin list\|install\|uninstall\|enable\|disable` / `version` 全部可用 |
| 退出码 | 0 全成功 / 1 部分失败 / 2 致命 |

---

## 六、原版已知问题对照（重写时规避）

| 原版问题 | 规避方式 |
|---|---|
| 翻页 `page++` 双重递增 | 分页状态机 + 单元测试 |
| 标题未清洗 → 崩溃 / SQL 注入 | `sanitize()` + 固定 schema + id 作键 |
| 续传只看文件存在 | DB + 文件大小双重校验 |
| 写一半被永久跳过 | 临时文件 + rename 原子写 |
| HTTP 全裸（无超时/重试/状态码/UA） | 收敛到宿主统一处理 |
| 每章 `std::async` 无限线程 | 有界 worker + 信号量 |
| `sleep_time` 占并发槽 | sleep 在取任务前 |
| 配置存字符串反复 `stoi` | 类型化配置一次解析 |
| fanqie 重复拉 detail 4 次 | `novel_data` 内嵌元数据，零额外请求 |
| 正则解析 HTML | scraper / serde |
| 无日志 | tracing 双写 |
| 插件安装无校验 | 状态码 + 长度 + magic + 白名单 |
| 一个坏插件拖垮启动 | 单个失败隔离降级 |
| 原生插件任意代码执行 | WASM 沙箱 |
| detail / content API 已死 | 改走官网 HTML + APP 网关 |
