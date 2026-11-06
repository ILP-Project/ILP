# ilp-host → ilp-core 完整迁移计划

**文档性质：** 可执行迁移计划与弃用政策，不是时间表。本文只记录已知实现、剩余职责、阻塞条件、验收标准和删除门槛；不得把 scaffold、测试 fake、可选集成或未生成的 FRB bindings 描述为产品能力。

**审查基线：** Core 外部基线 `73864cd`（`IlpCore::validate_chapters`）、`ilp-host` `0a037801`、`ILP-CLI` `68988cac`、`ILP-APP` `58b5e498`。独立仓库之间的提交对象不可互相验证时，必须明确标记为外部基线，不得声称已核对源码。

## 1. 目标与最终形态

最终目标不是立即删除 `ilp-host`，而是分阶段完成职责迁移：

```text
ilp-host 业务编排
    → ilp-core 类型化服务与工作流
    → ILP-CLI / ILP-APP 薄适配层
    → ilp-host 降级为 runtime adapter + legacy compatibility + migration tooling
    → 经过稳定发布周期后删除或归档 ilp-host
```

最终职责边界：

### ilp-core 负责

- 任务、状态、事件和恢复语义；
- 下载 workflow 与 backend 注入边界；
- 书籍、章节、manifest 和搜索索引的持久化；
- 章节目录及内容完整性校验；
- 插件注册记录、元数据和策略约束；
- Core-facing DTO、结构化错误和版本化事件；
- 不依赖 CLI 文本、终端输出或 SQLite 细节的业务服务。

### ilp-host 负责

- Wasmtime/WIT 插件运行时；
- 受策略约束的 HTTP、存储、secret、日志和时间 capability；
- 插件加载时的完整性、ABI、签名/撤销和资源门禁；
- 将已验证的插件与 `DownloadBackend` 绑定；
- 旧数据迁移工具和兼容性壳；
- 迁移完成前的旧 CLI 回滚路径。

### ILP-CLI 负责

- 参数解析；
- 调用 Core adapter；
- DTO 到文本/JSONL 的映射；
- stdout/stderr 和退出码兼容；
- 不复制 Core 业务状态机，不直接发现或调用 legacy `ilp-host`。

### ILP-APP 负责

- Flutter UI、用户交互和平台集成；
- 消费 Core/bridge DTO、任务快照、事件分页和 resync；
- 在生成并审查 FRB bindings 后接入 native Core；
- 不在 Dart 端复制任务或下载业务逻辑。

## 2. 当前已完成的迁移基础

以下能力已经存在，但不等于 `ilp-host` 已可以删除：

- Core 拥有独立 Tokio runtime、SQLite WAL storage、任务与事件持久化；
- Core 拥有 `DownloadWorkflow`、`DownloadBackend` 和默认拒绝联网的 `NoNetworkDownloadBackend`；
- Core 支持任务状态、进度事件、取消、恢复、事件 replay 和 retention/resync；
- Core 支持 `validate_chapters`，host `get/update` 在缓存或应用章节前调用该校验（根据外部基线摘要记录）；
- `ilp-host` 拥有 `PluginDownloadBackend`，可桥接加载的 WIT 插件和受策略约束的 HTTP backend；
- `ilp-host` 拥有 `HostDownloadWorkflow`，会校验已验证 registration 与 Core 记录一致，不自动注册、不绕过 deny-all；
- 独立 CLI 的 `task download` 已路由到 Core workflow，但默认注入 `NoNetworkDownloadBackend`，因此不能下载站点正文；
- CLI 的 `search`、`library list`、`task`、`plugin list`、`plugin info` 已通过统一 Core adapter；
- CLI 的 `task status` 和 `task cancel` 已通过 Core `TaskService` adapter；
- APP 已有 task/event repository、下载 DTO、controller 和 fake workflow contract，但尚未连接生成的 FRB native bindings；
- Docs 已有迁移矩阵、兼容矩阵、安全边界和状态标签。

## 3. 当前仍由 ilp-host 承担的业务职责

### 3.1 真实站点下载入口

旧 `commands::fetch_all` 仍自行管理部分批量计划、重试、进度和旧存储路径。真实站点下载仍由旧宿主执行。

迁移目标：

```text
已验证 plugin registration
→ HostDownloadWorkflow
→ Core DownloadWorkflow
→ PluginDownloadBackend
→ 受策略约束的 WIT/HTTP capability
→ Core 内容与 manifest 原子持久化
```

删除门槛：

- 生产 `get/update` 不再旁路 Core workflow；
- 章节目录在缓存或应用前通过 Core 校验；
- 网络、插件、校验、存储失败都能映射为结构化 task error；
- 取消、重试和恢复语义在旧入口与新入口一致；
- 双轨观察完成一个稳定发布周期。

### 3.2 旧存储读写与数据生命周期

旧宿主仍处理 `data/novels`、旧数据库、covers、配置和部分旧目录布局。

迁移目标：

```text
旧数据检测
→ manifest/版本检查
→ 用户确认
→ 非覆盖备份
→ Core storage migration
→ 完整性校验
→ 可回滚结果
```

删除门槛：

- 每个支持的旧布局都有明确 fixture；
- 迁移前可预览；
- 备份绝不覆盖已有备份；
- 故障注入后可恢复旧数据；
- 未知 schema 仍返回 `UnsupportedLegacyFormat`，不得猜测迁移。

### 3.3 插件安装、加载与信任门禁

Core registration 只保存经过验证的 registration 摘要，不能单独授权执行。host 仍必须负责：

- WASM 字节完整性和 SHA-256；
- 签名、撤销和来源证明；
- ABI/WIT 版本；
- capability ceiling；
- 域名、HTTPS、资源和并发限制；
- 显式用户确认。

删除门槛：

- 安装、注册、加载和执行共享同一可信路径；
- inspection/validation 不会自动建立 trust；
- 摘要漂移、签名/ABI/权限/域名冲突均稳定拒绝；
- 成功 registration 可审计来源、确认和摘要；
- CLI/APP 不会绕过 host 的运行时门禁。

### 3.4 批量计划、重试、进度、取消、恢复和诊断

这是下一阶段的高优先级职责。需要把 host 的批量编排映射到 Core 类型化 request、task state/event 和 workflow：

- 批次 ID 与单任务 ID；
- 幂等键；
- 重试预算、退避和最终失败原因；
- 进度聚合；
- 取消点和取消后的持久化语义；
- 恢复游标和已完成章节去重；
- task/batch 诊断上下文；
- cursor/resync 后的事件恢复。

删除门槛：

- host、CLI、APP 解释同一套 task/event contract；
- `completed` 只在内容和 manifest 成功持久化后发出；
- 取消、恢复、重试和诊断可以跨入口 replay；
- 失败不会写入伪造正文或错误归属的章节。

### 3.5 兼容命令与切换/回滚

迁移期间必须保留旧 host 命令和回滚能力：

1. 增加 Core-backed 双轨入口；
2. 记录可观测结果和兼容差异；
3. 停止新安装默认使用旧入口；
4. 保留一个可选回滚周期；
5. 再删除旧命令、adapter 和旧存储代码。

## 4. 分阶段执行计划

### 阶段 A：完成 Core 业务边界

- [x] `DownloadWorkflow` 与 `DownloadBackend` 注入 seam；
- [x] 默认 `NoNetworkDownloadBackend`；
- [x] task 状态、进度、取消、恢复和事件契约；
- [x] 章节持久化与搜索索引原子写入；
- [x] `validate_chapters` Core API；
- [x] 插件 registration、metadata 和 capability ceiling 边界；
- [x] Core 错误与 DTO 版本化。

退出条件：Core 不依赖 CLI、终端输出、旧 host 内部类型或 SQLite 表结构。

### 阶段 B：完成 host adapter

- [x] `PluginDownloadBackend`；
- [x] `HostDownloadWorkflow`；
- [x] registration 与 Core 记录一致性校验；
- [x] WIT/HTTP/章节结果的 ID 和错误校验；
- [x] 离线 fixture 测试。

- [x] 将生产 `get all` 从旧 `fetch_all` 切换到 `HostDownloadWorkflow`；
- [x] 将生产 `update --apply` 从旧 `fetch_all` 切换到 `HostDownloadWorkflow`；
- [x] 将批量计划、重试、进度和恢复映射到 Core workflow；
- [x] 安装成功后将经过完整信任门禁的插件写入 Core registration；
- [x] 生产 `get all` 加载/执行路径复用 registration 与内容摘要校验；
- [x] 保留旧入口作为显式回滚而非默认路径。

退出条件：旧 host 仍可作为回滚，但默认真实下载已通过 Core workflow。

### 阶段 C：完成独立 CLI 迁移

- [x] 统一 `open_core` adapter；
- [x] `search`、`library list`、`plugin list/info` Core-backed；
- [x] `task status/cancel` Core-backed；
- [x] `task download` 调用 Core workflow；
- [x] provider seam 和稳定 NoNetwork 错误；
- [x] stdout/stderr/exit-code contract tests。
- [x] 完成插件 install/update/remove/enable/disable 的 Core-backed adapter；
- [x] 完成来源、哈希、签名、撤销和用户确认审计；
- [x] 完成网络安装前的 verified source/hash/signature contract。

- [x] 迁移 `get`、`update`、`doctor`、`completion` 等剩余命令；
- [x] 接入明确的、经过验证的 host backend provider；
- [x] 完成 `ILP-CLI` 的独立发布依赖策略，避免 release 构建依赖未固定的 sibling path。

退出条件：CLI 只解析、调用 Core、映射结果，不再包含下载或任务业务状态机。

### 阶段 D：完成 APP bridge 迁移

- [x] Dart/Rust DTO contract；
- [x] task/event repository；
- [x] pagination、resync、unknown schema/state/error 兼容；
- [x] task controller 和 fake workflow；
- [x] loading/progress/failed/cancelled/resync 测试。

- [x] 安装并审查锁定版本 `flutter_rust_bridge_codegen 2.13.0`；
- [x] 生成并 review bindings；
- [ ] 接入 native library 和 Core runtime 生命周期；
- [x] 增加 Flutter ↔ Rust end-to-end 测试；
- [ ] APP 不再依赖临时 host bridge。

退出条件：APP 可通过 reviewed bindings 调用 Core，并能从事件 retention/resync 中恢复状态。

### 阶段 E：迁移旧存储和插件生命周期

- [x] 识别并固定所有支持的旧 storage schema；
- [x] 为每种 schema 增加 source fixture；
- [x] 完成 preview/backup/migrate/verify/rollback；
- [x] 完成插件 install/update/remove/enable/disable 的 Core-backed adapter；
- [x] 完成来源、哈希、签名、撤销和用户确认审计；
- [x] 完成网络安装前的 verified source/hash/signature contract。

退出条件：用户数据和插件信任状态可迁移、可审计、可回滚。

### 阶段 F：停用并删除 ilp-host

只有以下条件全部满足后才能停用默认 host 路径：

- [ ] Core workflow 已成为真实下载默认入口；
- [ ] CLI 和 APP 已接入 Core；
- [ ] 旧数据迁移和回滚完成；
- [ ] 插件信任路径闭合；
- [ ] 跨平台 CI 通过；
- [ ] JSONL、事件、退出码和旧命令兼容测试通过；
- [ ] 至少一个稳定发布周期无 host 主路径回归；
- [ ] 发布说明包含迁移和回滚方案。

删除顺序：

```text
停止默认使用
→ 保留显式回滚
→ 归档 legacy 命令
→ 删除旧业务编排
→ 删除旧存储读写
→ 最后删除 runtime/compatibility wrapper
```

## 5. 验收矩阵

| 能力 | 当前状态 | 迁移完成条件 |
| --- | --- | --- |
| Core 初始化与存储 | API 已完成 | 旧数据有显式迁移、备份和回滚 |
| 任务、状态与事件 | workflow 已接入，CLI 默认 NoNetwork | 真实 host backend 注入，状态只在内容成功落盘后完成 |
| 章节目录校验 | Core API 已提供，host get/update 已接入（外部基线摘要） | 新旧入口均使用同一校验，测试可审计 |
| 书库与搜索 | CLI 已通过 Core | library mutation 统一经过 Core service |
| 插件 inspection/registration | Core 与 host seam 已接入 | install/load/execute 共享可信门禁 |
| 旧宿主下载编排 | adapter 已完成，旧入口未完全切换 | 真实 get/update 不再旁路 Core |
| CLI | 查询、task、plugin 部分已迁移 | 剩余命令完成 Core adapter 或正式废弃 |
| APP | contract/fake 已完成 | FRB bindings、native library 和生命周期已接通 |
| 同步 | 本地事件协议部分存在 | 网络认证、加密、增量、冲突和 resync 完成 |

## 6. 下载 workflow 必须满足的契约

### 成功路径

```text
显式 request
→ plugin registration 校验
→ 章节目录获取
→ Core.validate_chapters
→ 章节抓取与结果校验
→ 内容 hash/完整性校验
→ manifest 与正文原子持久化
→ 进度事件
→ completed
```

### 失败路径

以下情况必须产生结构化失败，不得伪造成功：

- plugin 未注册或 registration 漂移；
- capability policy 拒绝；
- HTTP/HTTPS/domain policy 拒绝；
- 章节目录顺序或 ID 不合法；
- 章节正文为空、ID 不匹配或失败结果带正文；
- 重试预算耗尽；
- 取消或恢复游标无效；
- manifest、正文或索引持久化失败。

### 禁止事项

- 不得在 `create_task` 后直接推进 `running → completed` 模拟下载；
- 不得伪造正文、manifest、`chapter_downloaded` 或成功事件；
- fixture/cassette 只能用于测试，未命中必须失败；
- 不得绕过 HTTPS、域名/IP、凭据、插件 capability 或资源限制；
- 不得把 failed/no-network task 描述为正文已下载；
- 不得把 metadata inspection 当作 trusted registration；
- 不得从未知 legacy schema 推断迁移规则。

## 7. 兼容性与安全规则

- Core 不解析 CLI 参数，不打印人类可读输出，不直接暴露 SQLite；
- CLI 只负责参数、Core 调用、DTO/JSONL 映射；
- Core 事件使用全局单调序列、分页、retention 和 `resync_required`；
- 未知事件类型、状态、错误码和未来 schema 必须安全保留或映射为 unknown；
- JSONL 成功事件写 stdout；runtime error 保持 stdout 为空并写 stderr；
- 插件权限默认 deny；未签名本地插件必须警告并要求显式确认；
- 不自动信任本地插件，不实现未验证来源/哈希/签名的网络安装；
- 同步不能上传正文内容；
- 不提交预编译 WASM fixture，只提交 source fixture 和可选构建说明；
- 旧备份不得覆盖，未知旧格式必须返回 `UnsupportedLegacyFormat`；
- sibling path dependency 不能视为最终发布方案，发布前必须版本化或固定可复现 revision。

## 8. 每次迁移提交必须附带的证据

每个迁移职责必须同时提交：

1. Core API 或稳定 adapter；
2. host/CLI/APP 调用改动；
3. 正常、失败、取消、恢复或兼容测试；
4. 文档状态和限制更新；
5. 可复现验证命令及结果；
6. `git diff --check`；
7. 工作区 clean；
8. 明确未完成项和回滚路径。

Core 仓库最低验证：

```bash
cargo fmt --all --check
cargo test --workspace
a cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

> 注意：上面的 clippy 命令应实际执行为 `cargo clippy --workspace --all-targets -- -D warnings`。

CLI 仓库最低验证：

```bash
cargo fmt -- --check
cargo check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Docs 仓库最低验证：

```bash
npm ci
npm run docs:build
npm audit --audit-level=moderate
git diff --check
```

APP 仓库最低验证：

```bash
flutter analyze
flutter test
cargo fmt --manifest-path rust/Cargo.toml --all -- --check
cargo check --locked --manifest-path rust/Cargo.toml
cargo test --locked --manifest-path rust/Cargo.toml
```

## 9. 当前判定

当前不能宣布 `ilp-host` 已删除或完全弃用。准确状态是：

```text
Core 业务边界                         已建立
Core DownloadWorkflow                 已建立
ilp-host DownloadBackend adapter      已建立
HostDownloadWorkflow                  已建立
CLI task/query Core-backed            已建立
APP workflow/controller contract      已建立
章节目录校验下沉 Core                 已建立（外部基线摘要）
任务编排下沉 Core                     已建立
真实 host 下载入口切换                `get all` 与 `update --apply` 已切换
旧存储迁移与回滚                      未完成
插件生产信任链                        安装路径已接入 Core registration；执行闭环未完成
APP FRB/native runtime                未接通
跨平台发布验收                        未完成
```

因此当前最短安全路径是：

```text
迁移批量计划、重试、进度、取消、恢复和诊断
→ 完成 registration 与执行路径闭环
→ 迁移批量/重试/进度/取消/恢复/诊断
→ 完成存储迁移与回滚
→ 完成 CLI/APP 发布验收
→ 停止 host 默认使用
→ 经过一个稳定发布周期
→ 删除 host
```

任何未满足的删除门槛都必须保持旧 host 回滚路径，不得用任务状态、fake fixture 或 NoNetwork backend 冒充真实下载完成。

## 10. ILP-CLI 完整实施计划

### 10.1 CLI 的目标职责

`ILP-CLI` 是独立产品，不应成为第二个 host，也不应复制 Core 的业务状态机。最终边界固定为：

```text
命令行参数
→ 参数/输入校验
→ Core adapter 或稳定 provider
→ Core service/workflow
→ DTO/事件映射
→ text / JSONL 输出
```

CLI 不负责：

- 章节抓取、重试、取消和恢复状态机；
- 直接访问 SQLite 表；
- 直接调用 Wasmtime/WIT；
- 自动发现或信任本地 WASM；
- 自行实现插件 capability policy；
- 把任务创建或状态推进伪装成正文下载；
- 把错误事件写到成功 JSONL stdout。

### 10.2 CLI 当前已完成

- [x] `open_core` 统一 Core 构造边界；
- [x] `search` 通过 Core `SearchService`；
- [x] `library list` 通过 Core `LibraryService`；
- [x] `plugin list` / `plugin info` 通过 Core 注册记录；
- [x] `task status` 通过 Core `TaskService`；
- [x] `task cancel` 通过 Core `TaskService`；
- [x] `task download` 进入 Core `DownloadWorkflow`；
- [x] `DownloadBackendProvider` 注入 seam；
- [x] `NoNetworkBackendProvider` 安全默认实现；
- [x] 无网络 backend 的稳定错误、失败任务持久化和空 stdout contract；
- [x] `site_id/-s`、`force_update/-f`、`ILP_DATA_FOLDER` 等兼容契约；
- [x] JSONL 成功输出、stderr 错误输出和退出码测试；
- [x] Core adapter 文档和兼容性说明。

### 10.3 CLI `task download` 真实接入计划

当前显式下载 request 需要：

- `plugin_id`；
- `book_id`；
- `title`；
- `author`；
- `source_type`；
- `source_id`。

当前默认行为：

```text
显式 request
→ Core DownloadWorkflow
→ NoNetworkDownloadBackend
→ 结构化 backend 错误
→ task failed
→ stdout 为空，stderr 有错误，退出码保持契约
```

这不是正文下载完成。真实接入必须新增**内部 provider 注入**，而不是新增一个让用户选择 `--backend no-network` 的产品参数：

1. CLI 启动时打开 Core；
2. 通过明确的产品构造路径获得已验证的 host provider；
3. provider 只返回经过信任门禁的 `DownloadBackend`；
4. CLI 将 request 原样交给 Core workflow；
5. Core 负责章节、状态、事件、持久化和错误；
6. CLI 只负责把 report/event/error 映射为文本或 JSONL。

在 host provider 未接入前，`NoNetworkBackendProvider` 必须继续是默认安全行为，不能隐式联网。

### 10.4 CLI 剩余命令迁移

| 命令 | 当前状态 | 实施计划 | 完成标准 |
| --- | --- | --- | --- |
| `get` | 未完整迁移 | 先定义 Core-backed book/source request，再接入真实 backend；不把旧 host 参数直接复制到 CLI | 目录、章节、缓存、状态和错误都走 Core；旧参数有兼容映射或正式废弃说明 |
| `update` | 未完整迁移 | 复用 Core workflow 的 manifest/章节差异和强制更新策略；不在 CLI 实现 diff | 更新前后 manifest、章节校验和失败回滚可审计 |
| `doctor` | 仍有 scaffold 边界 | 拆成 Core health/storage/plugin checks 与 CLI environment checks | Core 检查不访问 CLI 输出层；每个检查有稳定 code/status |
| `completion` | 产品辅助能力 | 保留在 CLI，命令元数据不进入 Core | shell completion 覆盖正式命令，变更有 golden test |
| `version` | 基础命令 | 输出 CLI/Core/协议版本和构建信息 | 不读旧 host 内部版本；JSONL 和 text 输出稳定 |
| `plugin validate` | 本地 deny-all 检查边界 | 保留为显式本地验证，不写 registration；未来接入可信 source/hash/signature inputs | inspection 永不变成 trust，失败原因稳定 |
| `plugin install` | 未实现网络安装 | 先实现 verified source/hash/signature/user-confirmation contract，再实现安装；禁止裸 URL 下载 | 来源、摘要、签名、撤销、ABI、权限和确认均可审计 |
| `plugin update` | 未完整实现 | 复用 install trust path，先比对新旧 metadata 和权限，再原子替换 | 更新失败保留旧插件，可回滚 |
| `plugin enable/disable` | Core/host 边界未完全收拢 | 由 Core 保存启用状态，host 加载时再次应用 policy | 状态变更不等于执行授权，加载门禁不能被绕过 |
| `plugin remove/uninstall` | 未完整实现 | Core 处理注册/tombstone，host 处理文件删除与引用检查 | 有用户确认、引用检查和可恢复失败路径 |

### 10.5 CLI 迁移阶段

#### CLI-A：边界固化（已完成）

- Core adapter 集中构造；
- 任务和插件查询不再各自打开 Core；
- JSONL/error/exit code contract 固化；
- `task download` 不再使用假的 `queued → running → completed`。

#### CLI-B：真实 provider 注入（当前）

- [ ] host 创建 Core 并提供 verified `DownloadBackendProvider`；
- [ ] provider 验证 registration 与 Core 记录一致；
- [ ] provider 复用 host 的 Wasmtime/WIT/HTTP policy；
- [ ] 生产 `task download` 使用真实 provider；
- [ ] provider 不自动注册、不降低 deny-all、不绕过用户确认；
- [ ] 失败时保持现有 JSONL/stderr/exit-code 契约。

#### CLI-C：业务命令迁移

- [ ] `get` / `update` 使用 Core book/chapter workflow；
- [ ] `doctor` 拆分 Core 检查与 CLI 环境检查；
- [ ] plugin lifecycle 使用统一 trust path；
- [ ] legacy aliases 有兼容测试；
- [ ] 旧 host 入口只作为显式兼容/回滚。

#### CLI-D：独立发布

- [ ] 解决 sibling path dependency；
- [ ] 固定可复现 Core 版本或发布依赖；
- [ ] Windows/macOS/Linux 包和安装器验证；
- [ ] 跨平台数据目录和 `ILP_DATA_FOLDER` 验证；
- [ ] 发布包不隐式携带未审查 host 行为；
- [ ] 发布说明包含旧命令迁移和回滚方案。

### 10.6 CLI 完成验收

- [ ] CLI 所有业务命令都有 Core adapter 或明确的 CLI-only 说明；
- [ ] 没有 CLI 自己的下载状态机；
- [ ] 没有 CLI 直接访问 SQLite 或 Wasmtime；
- [ ] 真实下载 provider 由显式可信构造路径注入；
- [ ] JSONL 成功、错误 stdout 为空、stderr 和退出码与兼容矩阵一致；
- [ ] `task download` 的 `completed` 只代表 Core 已完成内容和 manifest 持久化；
- [ ] `get/update` 的缓存和更新失败可回滚；
- [ ] plugin validate/install/update/remove 的 trust 语义一致；
- [ ] 跨平台 CLI contract/golden tests 通过；
- [ ] 至少一个稳定发布周期不依赖旧 host 默认入口。

## 11. ILP-APP 完整实施计划

### 11.1 APP 的目标职责

`ILP-APP` 是 Flutter desktop/Android 产品。它只消费稳定的 Core/bridge contract，不在 Dart 层复制 Core 或 host 业务。

最终边界：

```text
Flutter UI
→ TaskController / feature controller
→ FRB-generated bridge
→ Rust Core repository/service
→ ilp-core
```

APP 不负责：

- 直接读取 SQLite、旧 `data/novels` 或 WASM 文件；
- 直接执行 Wasmtime/WIT；
- 直接实现 HTTP、插件 capability 或下载重试策略；
- 解释 Core 内部错误字符串；
- 把本地 fake workflow 当作真实生产 backend；
- 在没有 binding 和 native library 时声称已连接 Core。

### 11.2 APP 当前已完成

- [x] bridge error DTO 和稳定 error code；
- [x] task snapshot、event page、exclusive cursor；
- [x] `schema_version`、unknown event/state/error 前向兼容；
- [x] `ResyncRequired` 和 retention/resync 映射；
- [x] Dart `BridgeTaskEventRepository`；
- [x] Rust `CoreTaskEventRepository`；
- [x] `cancelTask` / `cancel_task` contract；
- [x] `TaskController` / `RepositoryTaskController`；
- [x] `BridgeDownloadRequestDto`；
- [x] `BridgeDownloadProgressDto`；
- [x] `BridgeDownloadStatusDto`；
- [x] `BridgeDownloadWorkflow` contract；
- [x] loading/progress/failed/cancelled/resync 状态；
- [x] deterministic in-memory fake workflow；
- [x] Dart/Rust contract fixtures 和测试；
- [x] FRB codegen preflight，锁定 `2.13.0`；
- [x] 明确 APP contract 与 `ilp-host` 解耦。

### 11.3 APP 当前未完成

- [ ] 生成实际 FRB bindings；
- [ ] 接入平台 native library；
- [ ] 让 Dart repository 调用 Rust Core，而不是 fake/hand-written seam；
- [ ] Core runtime 生命周期管理；
- [ ] bookshelf UI；
- [ ] reader UI；
- [ ] progress/bookmark UI；
- [ ] task center；
- [ ] plugin search/management UI；
- [ ] Android storage/network/foreground task integration；
- [ ] desktop packaging and native integration；
- [ ] Flutter ↔ Rust end-to-end tests；
- [ ] release signing and crash/diagnostic integration。

### 11.4 APP 分层架构

#### 表现层

负责：

- bookshelf、reader、task center、plugin screens；
- loading/empty/error/retry/cancel/resync UI；
- 用户确认和危险操作提示；
- 不直接拼接 Core 错误字符串。

#### Feature controller 层

负责：

- 调用 bridge service；
- 将 DTO 转为 UI state；
- 管理订阅、分页、cursor 和 resync；
- 处理生命周期取消；
- 不实现下载、重试、存储或插件策略。

#### Bridge 层

负责：

- FRB-generated API；
- DTO、错误码、schema version；
- `u64`/cursor 安全转换；
- task/event repository；
- Core runtime 和 native handle 生命周期。

#### Core 层

负责：

- task/download workflow；
- storage、events、plugin registration 和 policy；
- 结构化错误与状态转移；
- 不依赖 Flutter 或 Dart。

### 11.5 APP MVP 实施阶段

#### APP-A：contract foundation（已完成）

- DTO/error/event schema；
- task repository；
- pagination/resync；
- controller/fake；
- forward-compatible unknown mapping；
- cancel and failure semantics。

#### APP-B：FRB 生成与 native smoke test（阻塞）

- [ ] 在批准的网络/工具环境安装或提供 `flutter_rust_bridge_codegen 2.13.0`；
- [ ] 运行 preflight；
- [ ] 生成 bindings；
- [ ] review generated diff；
- [ ] 确认 Rust/Dart 类型映射，尤其是 `u64`、错误和 enum；
- [ ] 生成最小 desktop/Android native library；
- [ ] 加载 Core runtime；
- [ ] 运行最小 `create task → snapshot → events → cancel` smoke test。

在 APP-B 未完成前：

```text
Dart fake/hand-written contract = 测试与接口验证
Dart fake/hand-written contract ≠ 生产 Core 连接
```

#### APP-C：任务中心与下载 UI

- [ ] request form：plugin/book/source/title/author；
- [ ] 明确显示 backend unavailable / NoNetwork 错误；
- [ ] queued/running/progress/failed/cancelled/completed UI；
- [ ] 事件分页和 cursor；
- [ ] retention 后 `resync_required` 自动恢复 snapshot；
- [ ] cancel confirmation 和不可逆操作提示；
- [ ] 不显示“下载完成”，除非 Core report/state 明确完成持久化；
- [ ] task detail 能关联 error code、task id 和诊断信息。

#### APP-D：书架、阅读器和用户数据

- [ ] bookshelf 通过 Core book/library API；
- [ ] reader 通过 Core chapter/content API；
- [ ] bookmarks/progress 通过 Core service；
- [ ] 删除使用 tombstone/确认/恢复语义；
- [ ] 正文不经同步上传；
- [ ] reader 只读取完整性校验通过的内容；
- [ ] 空数据、损坏内容和迁移中状态有明确 UI。

#### APP-E：插件与平台能力

- [ ] plugin list/info/validation UI 只显示 Core/host 返回的可信状态；
- [ ] 本地插件导入显示来源、摘要、签名/确认状态；
- [ ] 不把 metadata inspection 显示为 trusted；
- [ ] Android 网络、存储、后台任务、权限和生命周期；
- [ ] desktop 文件选择、网络代理和日志目录；
- [ ] 所有平台差异通过 bridge/platform adapter 隔离。

#### APP-F：发布和稳定性

- [ ] Windows/macOS/Linux desktop smoke tests；
- [ ] Android install/upgrade/storage migration tests；
- [ ] native library loading and architecture matrix；
- [ ] event replay/resync after app restart；
- [ ] crash-safe task recovery；
- [ ] release signing；
- [ ] diagnostics redaction，禁止上传正文；
- [ ] full Flutter ↔ Rust integration suite；
- [ ] 一个稳定发布周期无 host 依赖回归。

### 11.6 APP 状态与事件契约

APP 必须按以下规则渲染状态：

| Core/bridge 状态 | UI 语义 | 允许的操作 |
| --- | --- | --- |
| `queued` | 等待执行 | cancel |
| `running` | 执行中，显示 progress | cancel、refresh |
| `paused` | 暂停/等待恢复 | resume（若 Core contract 支持） |
| `completed` | 已完成并可读取 | open、refresh |
| `failed` | 失败，显示稳定 error code | retry、diagnose、dismiss |
| `cancelled` | 已取消 | retry、dismiss |
| `interrupted` | 中断待恢复 | recover、refresh |
| unknown state | 未知未来状态 | 只读、显示升级提示 |

事件规则：

- `sequence` 只能向前移动；
- 分页使用 exclusive cursor；
- cursor 过期必须处理 `resync_required`；
- resync 后以 snapshot 替换本地状态，再继续拉取事件；
- 未知 event type 保留 raw payload，不崩溃；
- future schema 不得静默按旧 schema 解析；
- native error 映射为稳定 code，UI 不依赖错误字符串。

### 11.7 APP 完成验收

- [ ] 所有生产 UI 通过 generated FRB bridge 访问 Core；
- [ ] 不存在生产路径 fake workflow；
- [ ] 不存在 Dart 端任务/下载状态机副本；
- [ ] bookshelf、reader、task center、plugin UI 通过 Core service；
- [ ] cancel/retry/resume/resync 在重启后仍可解释；
- [ ] unknown schema/state/event/error 安全处理；
- [ ] 正文不进入同步和诊断上传；
- [ ] Android/desktop native library 和 runtime 生命周期稳定；
- [ ] Flutter/Rust E2E、平台 smoke 和 release signing 通过；
- [ ] APP 不再依赖 `ilp-host` 默认业务入口。

## 12. CLI 与 APP 联合验收

CLI 和 APP 必须消费同一 Core contract，而不是各自复制实现：

| 联合能力 | CLI | APP | 共同完成条件 |
| --- | --- | --- | --- |
| 创建下载任务 | 参数解析 + Core adapter | 表单 + bridge | 相同 request 字段和校验错误 |
| 状态显示 | text/JSONL | UI state | 相同 task state、progress 和 error code |
| 取消 | `task cancel` | cancel action | 相同 Core transition 和幂等语义 |
| 事件分页 | JSONL/replay | cursor subscription | 相同 sequence、exclusive cursor、resync 规则 |
| 失败处理 | stderr/exit code | error UI/retry | 相同结构化 error code，不依赖字符串 |
| 正文完成 | 只在 Core completed 后报告 | 只在 Core completed 后展示 | 内容/manifest 原子落盘后才 completed |
| 插件状态 | Core registration summary | Core/bridge DTO | inspection 不显示为 trusted |
| 离线默认 | NoNetwork 错误 | backend unavailable 状态 | 不隐式联网、不伪造成功 |

联合验收必须包括：

- 同一 fixture 产生同一 request、状态和事件序列；
- CLI 取消后 APP 能通过 resync 看到 `cancelled`；
- APP 取消后 CLI status 能看到同一状态；
- retention 后两端都能恢复 snapshot；
- 失败原因在 text、JSONL、Dart error UI 中保持同一 code；
- 任一端不能把 fake/NoNetwork 结果显示为正文已下载。

## 13. CLI/APP 发布前检查清单

### CLI

- [ ] Core 依赖版本可复现；
- [ ] 不依赖 sibling checkout 才能构建发布包；
- [ ] Windows/macOS/Linux 命令、路径、编码和退出码验证；
- [ ] JSONL golden tests；
- [ ] `ILP_DATA_FOLDER` 和默认目录迁移测试；
- [ ] task/plugin/library 命令兼容测试；
- [ ] no-network、backend unavailable 和失败持久化测试；
- [ ] 安装器、升级、降级和回滚测试。

### APP

- [ ] FRB codegen 版本锁定且生成 diff 已审查；
- [ ] native library 覆盖目标平台和架构；
- [ ] desktop/Android runtime smoke；
- [ ] app restart/recovery/resync；
- [ ] storage permission、后台任务和网络策略；
- [ ] release signing；
- [ ] crash/diagnostic redaction；
- [ ] 不上传正文、不泄露 secret、token 或本地路径；
- [ ] 端到端测试不依赖未审查的 fake 代替 native Core。

## 14. 总体完成判定

只有以下三组条件同时满足，才能宣布“全面转向 ilp-core”：

### Core/host

- [ ] 真实 host 下载默认走 Core workflow；
- [ ] host 只保留 runtime、trust、legacy migration 和兼容职责；
- [ ] 旧存储完成可回滚迁移；
- [ ] 插件信任链闭合；
- [ ] 无 host 独立业务状态机。

### CLI

- [ ] 所有正式业务命令都有 Core adapter；
- [ ] 真实 backend 通过显式可信 provider 注入；
- [ ] CLI 不直接访问 host runtime 或 SQLite；
- [ ] 独立发布依赖可复现；
- [ ] 兼容矩阵和回滚周期完成。

### APP

- [ ] generated FRB/native runtime 已连接 Core；
- [ ] 生产 UI 不使用 fake workflow；
- [ ] task/library/reader/plugin 功能均通过 Core；
- [ ] 全平台 smoke/E2E/release 验证通过；
- [ ] APP 不依赖 host 默认入口。

最终宣布顺序：

```text
Core API 稳定
→ host 默认入口切换
→ CLI 真实 provider 接入
→ APP native bridge 接通
→ 数据/插件迁移完成
→ 跨平台发布验收
→ 一个稳定发布周期
→ 宣布 ilp-host deprecated
→ 再决定删除或归档 ilp-host
```
