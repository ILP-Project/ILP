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

宿主暴露给插件的能力只有四个（见 `crates/ilp-plugin-wit/wit/plugin.wit`）：

| 接口 | 用途 |
|---|---|
| `http-fetch` | 代发 HTTP（宿主统一处理代理、超时、UA、重试） |
| `log` | 写日志，转发到宿主的 `tracing` |
| `storage-get/set/delete` | 插件私有 KV，宿主按插件名隔离并做 key 校验 |
| `now-ms` | UTC epoch 毫秒（沙箱内没有系统时钟） |

正文抓取走**两阶段**：插件产出抓取计划（`plan-fetch`），宿主执行（并发/限速/重试都在宿主），
再把响应交回插件解析（`consume-fetch`）。这样"一次请求返回 30 章"的批量接口和
"一章一请求"的网页接口共用同一套 ABI，并发策略不用在每个插件里重写一遍。

## 安装

```bash
# 先看脚本再执行，别 curl | sh
curl -fsSLO https://raw.githubusercontent.com/ILP-Project/ILP/main/scripts/install.sh
less install.sh
ILP_PUBKEY='release-2026:<base64 公钥>' sh install.sh
```

`ILP_PUBKEY` 的格式与 `config.json` 里的 `plugin_trusted_keys` 一致
（`key_id:base64`）。设了它脚本会**先验签名再验校验和**——只验校验和没有
意义，校验和文件本身是从同一个地方下载的，中间人换了二进制也能顺手换掉
校验和。不设公钥会跳过验签并明确提示（不是静默跳过）。

其余开关：`ILP_VERSION`（默认 `latest`）、`ILP_PREFIX`（默认
`/usr/local/bin`）、`ILP_REPO`、`ILP_BASE_URL`（离线/镜像）。

`scripts/install_test.sh` 是它的自测，不需要网络：篡改二进制、错误公钥、
未知平台三条异常路径都必须被拒——安装脚本的失败模式不是"报个错"，而是
"装了个被篡改的二进制还告诉用户成功了"。

发布二进制用 musl 静态链接（Linux）：glibc 版本地狱是分发二进制最常见的
故障，在 ubuntu-24.04 上构建的产物到了 glibc 2.31 的机器上直接
`GLIBC_2.38 not found`。

## 构建

需要 Rust 1.75+ 与 wasm 目标：

```bash
rustup target add wasm32-wasip2
cargo build --release                      # 宿主：target/release/ilp
```

插件已拆到独立仓库：宿主只提供 ABI（`crates/ilp-plugin-wit`），
站点插件统一在 `ILP-Project/plugins` 里构建：

```bash
git clone https://github.com/ILP-Project/plugins
cd plugins
cargo build --target wasm32-wasip2 --release
cp target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm <宿主目录>/plugins/fanqie.wasm
```

或者直接装：`ilp plugin install fanqie`（自动校验 ABI 与 SHA-256）。

依赖都是 crates.io 上的常规库；`rusqlite` 用 `bundled` 特性自带 SQLite，不需要系统预装。

> 插件 crate 是 `cdylib`，**不能**为宿主目标编译（会卡在链接阶段），必须显式指定
> wasm 目标。宿主仓库不再包含插件源码。

## 使用

```bash
ilp get chapter|cover|author|title|all <id> -s <site_id> [-f]
ilp update <id> -s <site_id> [--apply]  # 默认只预览变更
ilp search <keyword> -s <site_id>
ilp plugin list|uninstall|enable|disable <name>
ilp plugin install <name>[@<version>]   # 省略版本即 latest
ilp plugin new <name> [--site-name ..] [--domain ..]   # 生成插件骨架
ilp doctor [--bundle <path>] [--with-manifest] [-y]
ilp --jsonl get all <id> -s <site_id>   # JSONL 事件流
ilp --verbose get all <id> -s <site_id>  # 详细日志
ilp version
```

- `get` 的 `kind` 可选 `chapter` / `cover` / `author` / `title` / `all`。
- `-s/--site_id` 必须是已加载插件的 `site_id`。
- `-f/--force_update` 只刷新元数据与目录；要重下正文请删掉对应章节文件或整本目录。
- `update` 会刷新目录并输出新增章节报告；默认 dry-run，不修改缓存也不下载正文。
  只有显式加 `--apply` 才会保存新目录并下载新增章节。
- `--jsonl` 输出机器可消费的 JSONL 事件（例如 `update_report`、`chapter_added`、
  `chapter_downloaded`），不会混入普通进度文本，适合 CI 和脚本。
- `--verbose` 提高日志级别；TTY 中会额外显示即时进度，管道和重定向保持稳定的非交互输出。
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

## 诊断

`ilp doctor` 做一遍自检：插件在不在、签名策略是否自洽、代理与 `no_proxy`
是否搭配、日志目录能不能写。发现问题时给出可执行的下一步，不只是报错。

`ilp doctor --bundle diag.tar.gz` 打一个诊断包。**打包前会先把清单打出来**，
告诉你每一项是什么、多大、含哪些内容，确认后才写文件（`-y` 跳过确认；
非交互环境下不加 `-y` 会直接拒绝，避免在 CI 里静默产出一个包含日志的文件）。

默认只含环境信息、配置摘要（密钥类只报数量）、插件清单、脱敏后的日志。
**不含** secret 文件、章节正文、绝对路径与用户名。manifest 摘要（含书名与
章节标题）需要显式 `--with-manifest`。

日志里命中 `cookie` / `authorization` / `*_token` / `password` 等标记的行，
值会被替换成 `<redacted>`，但时间戳、级别、字段名保留——否则日志就没法看了。
按子串匹配而不是固定名单，因为真实流量里总会有名单外的头。

日志默认保留 7 天，过期自动清理：日志里必然有书名和章节标题，长期堆在
一个没人看的目录里只会变成泄漏面。

每次运行结束会在日志里留一行结构化摘要（target `ilp::summary`）。即使没有待下载章节，
幂等运行也会记录摘要：

```text
INFO ilp::summary: 运行摘要 site=fanqie book=7290175791344651298 ok=216 failed=0 total=216
```

## 仓库结构

| 路径 | 内容 |
|---|---|
| `crates/ilp-types` | 宿主与插件共享的 serde 类型、章节结果和顺序校验 |
| `crates/ilp-host` | 宿主二进制 `ilp`：CLI、配置、存储、HTTP、WASM 加载器、导出器、诊断与插件管理 |
| `crates/ilp-plugin-wit` | 插件 ABI 定义（`wit/plugin.wit`），宿主与插件共享的唯一来源 |
| `scripts/` | 发布安装脚本及无网络安装自测 |
| `.github/workflows/` | 宿主 CI 与签名 Release 构建流程 |

站点插件不在本仓库：统一放在 `ILP-Project/plugins`（一站点一子目录），
产物经 Cloudflare Pages 的静态 registry 分发，默认地址
`https://ilp.elysola.org`（`plugin_manager::DEFAULT_REGISTRY`）。
路线图与设计决策在 `ILP-Project/plan/`（仓库之外，不进 git tree）。

## 已知限制

- 番茄插件只覆盖**已登录可读的免费章节**；锁定章节服务端只返回一段试读，无法绕过。
- `plugin install` 依赖插件站点的 `registry.json`（默认 `https://ilp.elysola.org`，
  可用 `ILP_REGISTRY_URL` 指向镜像）。
  宿主通过 `CURRENT_ABI` 和 `SUPPORTED_ABIS` 管理 ABI 兼容性：当前 ABI 为 `0.2.0`，目前支持 `0.1.0` 与 `0.2.0`。ABI `0.1.0` 仍可加载，但安装时会发出弃用警告；不在支持列表中的 ABI 会在下载前拒绝。旧的 `SUPPORTED_ABI` 常量仅作为兼容别名保留。
- 搜索是游标分页，无法跳到指定页。

## 插件开发的离线回放

插件所有网络都走宿主，所以宿主可以在 HTTP 层拦截：

```bash
ILP_RECORD_DIR=fixtures/live ilp get all <book-id> -s <site>   # 真实联网跑一遍并录像
ILP_REPLAY_DIR=fixtures/live ilp get all <book-id> -s <site>   # 完全离线重放
```

回放未命中会**报错而不是回退到网络**：一个本该离线的测试静默打到真实站点，
既不可复现，也可能触发风控。录制产物落盘前会被脱敏（Cookie/Token/设备 ID/
签名密钥），脱敏后的录像能直接进 git。手工编写的 fixture 不走宿主，
所以插件仓库 CI 会跑 `tools/scan_fixtures.py` 再扫一遍。

## 插件签名与信任治理

配置项（`config.json`）：

| 字段 | 说明 |
|---|---|
| `plugin_trusted_keys` | `key_id` → Base64 的 32 字节 Ed25519 公钥 |
| `plugin_revoked_keys` | 已撤销的 `key_id`；撤销优先于验证，命中即拒绝 |
| `require_plugin_signatures` | 是否拒绝无签名的插件 |
| `plugin_signature_threshold` | 要求多少个**不同密钥**的有效签名，默认 `1` |
| `require_registry_signatures` | 是否要求 registry 元数据本身带有效签名，默认 `false` |

签名分两层，防的是两类不同的攻击：

- **产物签名**（`registry.json` 里每个版本的 `signatures`）：证明这段 WASM 出自可信发布者。
- **元数据签名**（旁路文件 `registry.json.sig`）：证明「`install fanqie` 该落到哪个名字、版本和地址」。

只签产物时，攻击者不需要伪造签名也能把 `latest` 改成某个**有效签名但已知有漏洞**的旧版本，或把名字重定向到别的插件。两层都打开才闭环：

```json
{
  "plugin_trusted_keys": { "release-2026": "fCwdShD1t/UBgVqcn/h90gSdhFAvI6ld4EEpYe/HGgc=" },
  "require_plugin_signatures": true,
  "require_registry_signatures": true
}
```

`registry.json.sig` 签的是 `registry.json` 的**原始字节**，不是重新序列化后的结构——JSON 规范化（键序、空白、数字写法）只要两侧有一点不一致，就会出现「我们签的」和「用户验的」不是同一串字节。签名放在文件旁边而不是写进 JSON 内部，也避免自指。

轮换流程：新密钥加入 `plugin_trusted_keys` 后重新发布；确认旧密钥停用后把它的 `key_id` 加入 `plugin_revoked_keys`，此时带旧签名的元数据即使公钥仍在信任列表里也会被拒绝。

多签时候选签名集为数组，同一 `key_id` 重复出现只计一次，因此阈值无法靠复制同一行凑够；未知或已撤销的签名者会被跳过，只要剩余有效签名数达到阈值就允许安装。

## 网络边界

宿主是插件唯一的出口，边界在 `http.rs` 里按三层收紧：

1. **协议**：只允许 `https`，不自动跟随重定向（避免 302 跳到内网）。
2. **地址**：拒绝回环、私网、链路本地、`0.0.0.0/8`、广播、文档网段，以及 IPv6 唯一本地 / 链路本地；`::ffff:127.0.0.1` 这类 v4-mapped 写法按 v4 规则判。
3. **解析**：检查不只停在 URL 里的主机名。真正发请求前先自己解析一次域名，**逐条**校验结果，再把通过校验的地址钉给 reqwest（`resolve_to_addrs`）。这样域名无法先解析成公网 IP 骗过检查、再改指向 `127.0.0.1`——DNS rebinding 走的正是这条路。

另外还有响应体上限、超时、重试与调用截止时间（见 `engine.rs` 与 `http.rs` 顶部常量）。

> 已知限制：配置了 HTTP 代理时，连接发给代理、域名由代理侧解析，第 3 层的钉 IP 不生效，只剩本次解析的校验。对安全敏感的目标请同时写进 `no_proxy` 走直连。

## 致谢与许可

协议细节（设备注册、Argus 签名、密钥协商、正文解密）的实现参考了
[fanqie-assistant](https://github.com/fanqie-assistant) 公开的接口分析，以及
[ILP-Cpp](https://github.com/RhenCloud/ILP-Cpp)��原 C++ 实现。本仓库为独立实现的 Rust 代码，
仅沿用其中的公开常量与字符映射表。

本仓库以 **GPL-3.0** 发布，与原版 ILP-Cpp 保持一致。
