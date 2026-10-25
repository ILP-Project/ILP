# ILP-rs

小说下载器：模块化 CLI，站点能力以 **WebAssembly 插件**提供。

本仓库是 [ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp) 的 Rust 重写，目标是**功能对齐**，
而非数据格式兼容——`config.json`、`cache.db` 的 schema、以及落盘目录布局都与旧版不同，
首次运行会重新下载。

## 为什么用 WASM 插件

原版插件是原生动态库（`.so`/`.dll`），加载即等于执行任意代码，且每个插件各自用 `cpr`
发请求、各自管并发与限速。这里改成 WebAssembly 组件（`wasm32-wasip2`）：

- 插件跑在 wasmtime 沙箱里，**没有**网络、文件、数据库能力，只能调用宿主注入的接口。
- 网络请求、限速、并发、重试、章节落盘全部收敛到宿主，插件只负责"这个站点的协议长什么样"。
- 产物跨平台，`plugin.json` 里按 OS 分发的 URL 映射可以退化成单一产物地址。

宿主暴露给插件的能力只有四个（见 `wit/plugin.wit`）：

| 接口 | 用途 |
|---|---|
| `http-fetch` | 代发 HTTP（宿主统一处理代理、超时、UA、重试） |
| `log` | 写日志，转发到宿主的 `tracing` |
| `storage-get/set/delete` | 插件私有 KV，宿主按插件名隔离并做 key 校验 |
| `now-ms` | UTC epoch 毫秒（沙箱内没有系统时钟） |

正文抓取走**两阶段**：插件产出抓取计划（`plan-fetch`），宿主执行（并发/限速/重试都在宿主），
再把响应交回插件解析（`consume-fetch`）。这样"一次请求返回 30 章"的批量接口和
"一章一请求"的网页接口共用同一套 ABI，并发策略不用在每个插件里重写一遍。

## 构建

需要 Rust 1.75+ 与 wasm 目标：

```bash
rustup target add wasm32-wasip2
cargo build --release                      # 宿主：target/release/ilp
cargo build --target wasm32-wasip2 --release -p ilp-plugin-fanqie
cp target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm plugins/fanqie.wasm
```

依赖都是 crates.io 上的常规库；`rusqlite` 用 `bundled` 特性自带 SQLite，不需要系统预装。

> 插件 crate 是 `cdylib`，**不能**为宿主目标编译（`cargo build -p ilp-plugin-fanqie` 会在
> 链接阶段失败）。它已从 `default-members` 里排除，所以裸 `cargo build` 不会碰到它。

## 使用

```bash
ilp get chapter|cover|author|title|all <id> -s <site_id> [-f]
ilp search <keyword> -s <site_id>
ilp plugin list|install <owner/repo>|uninstall|enable|disable <name>
ilp version
```

- `get` 的 `kind` 可选 `chapter` / `cover` / `author` / `title` / `all`。
- `-s/--site_id` 必须是已加载插件的 `site_id`。
- `-f/--force_update` 只刷新元数据与目录；要重下正文请删掉对应章节文件或整本目录。
- `search` 的翻页键是 `n`/回车（下一页）、`p`（上一页，走缓存）、`q`（退出）。
  非终端场景（管道、重定向）会自动沿游标翻到底并逐行打印。

退出码：`0` 全部成功、`1` 部分失败（有章节没下成，但书基本下完了）、`2` 致命错误。

### 落盘布局

```bash
data/
├── novels/<书名>_<book_id>/0001_<章节名>.txt
├── covers/<书名>_<book_id>.<jpg|png|webp|gif|bin>
├── cache.db                     # 书籍 + 章节状态
└── plugins/<插件名>/<key>       # 插件 KV
```

章节状态记在 SQLite 里，续传同时校验「DB 里是 ok」+「文件存在」+「文件大小与记录一致」。
正文用临时文件 + rename 原子写，先落盘再记 DB，崩溃不会留下"标记完成但内容是半截"的章节。

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/ilp-types` | 宿主与插件共享的 serde 类型 |
| `crates/ilp-host` | 宿主二进制 `ilp`：CLI、配置、存储、HTTP、wasmtime 加载器、插件管理 |
| `plugins/fanqie` | 番茄小说插件（WASM） |
| `wit/plugin.wit` | 插件 ABI 定义 |

## 已知限制

- 番茄插件只覆盖**已登录可读的免费章节**；锁定章节服务端只返回一段试读，无法绕过。
- `plugin install` 依赖上游仓库的 `plugin.json`；`url` 里若仍指向旧版 `.so` 产物会下载失败
  （产物必须是 WASM 组件）。
- 搜索是游标分页，无法跳到指定页。

## 致谢与许可

协议细节（设备注册、Argus 签名、密钥协商、正文解密）的实现参考了
[fanqie-assistant](https://github.com/fanqie-assistant) 公开的接口分析，以及
[ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp)��原 C++ 实现。本仓库为独立实现的 Rust 代码，
仅沿用其中的公开常量与字符映射表。

本仓库以 **GPL-3.0** 发布，与原版 ILP-Cpp 保持一致。
