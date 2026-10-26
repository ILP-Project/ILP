# ILP 插件分发方案（单仓库 + CF Pages）

> 状态：**方案定稿，本轮落地代码**。
> 本文档**取代** `2026-10-26_plugin-distribution.md` 的组织结论（一站点一仓库 + GitHub
> Pages）。数据源结论（全部编译进 wasm）沿用不变。
> 所有"现状"描述均出自当前代码，非推测。

## 变更摘要

| 项 | 旧方案（2026-10-26） | 新方案（本文） |
|---|---|---|
| 仓库 | 一站点一仓库 `plugin-<site>` | **单个 `plugins` 仓库**，一站点一子目录 |
| 托管 | GitHub Pages | **Cloudflare Pages** |
| 站点 | 无 | **有**：插件列表 + 详情页 + 安装命令 |
| metadata | 手写 JSON | **CI 从源码生成**，不手写 |
| `install` 入口 | `owner/repo` | **`<name>[@<version>]`** |
| ABI 共享 | 未定 | **git 依赖 ILP 仓库 tag** |
| 版本钉选 | 做不到（缺口 ②） | 落地 |

旧方案的选择理由仍成立（版本可 pin、可声明 ABI、一次请求完成发现），变的只是
**"一个仓库还是一个站点一个仓库"** 这个组织问题——用户判断单仓库更好管理。

## 仓库组织

```
ILP-Project/
├── ILP                宿主本体（CLI、存储、HTTP、wasmtime、插件管理、ABI crate）
└── plugins            全部站点插件 + 静态 registry + 站点
    ├── fanqie/        一个站点的源码
    │   ├── Cargo.toml
    │   ├── src/
    │   └── plugin.toml    站点元数据（人写；生成物不写这里）
    ├── qidian/        （未来）
    ├── site/            站点模板（CI 生成页面时用）
    └── .github/workflows/
```

**为什么 `plugin.toml` 而不是 `plugin.json`**：仓库里的 `plugin.json` 需要人写
`name` 和 `url`，而 `url` 依赖发布地址，会写错（旧 `RhenCloud/FanqiePlugin` 的
`plugin.json` 指向的 404 就是这个毛病）。现在人只写**不会随发布变化的元数据**，
`url`/`version`/`abi` 全部由 CI 从 `Cargo.toml` 和构建产物推导。

## 产物布局（CF Pages 站点根）

```
https://<project>.pages.dev/
├── index.html                 插件列表（CI 由 registry.json 渲染）
├── fanqie/index.html          详情页：介绍、安装命令、版本历史
├── registry.json              机器可读索引（宿主读这个）
└── fanqie/
    ├── latest.json            → "1.0.0"
    └── 1.0.0/fanqie.wasm      不可变，永不覆盖
```

`registry.json`：

```json
{
  "schema": 1,
  "plugins": [
    {
      "name": "fanqie",
      "site_id": "fanqie",
      "site_name": "番茄小说",
      "site_domain": "fanqienovel.com",
      "description": "……",
      "abi": "0.1.0",
      "latest": "1.0.0",
      "versions": ["1.0.0"],
      "url": "https://<project>.pages.dev/fanqie/1.0.0/fanqie.wasm",
      "size": 365594,
      "sha256": "…"
    }
  ]
}
```

字段来源：

| 字段 | 来源 |
|---|---|
| `name` | 子目录名 |
| `site_id` / `site_name` / `site_domain` / `description` | `plugin.toml`（人写） |
| `abi` | `plugin.toml`，**必须与宿主支持的 ABI 一致**，CI 校验 |
| `latest` / `versions` | `Cargo.toml` 的 `version`；历史版本从已有目录扫 |
| `url` / `size` / `sha256` | 构建产物 |

## ABI 共享：git 依赖 + tag

插件仓库**不复制** `plugin.wit`，通过 git 依赖指向宿主仓库：

```toml
# plugins/fanqie/Cargo.toml
ilp-plugin-wit = { git = "https://github.com/ILP-Project/ILP", tag = "abi-v0.1.0" }
```

选择理由：

- **零 token**。crates.io 发布需要 `CARGO_REGISTRY_TOKEN`，git 依赖是公开仓库，CI 直接能拉。
- **单一来源**。WIT 只有一份，在 `ILP/crates/ilp-plugin-wit`。
- **版本可钉**。tag `abi-v0.1.0` 对应 `package ilp:plugin@0.1.0`，ABI 变更就换 tag。
- 宿主仓库自己仍用 workspace 内的 path 依赖，不受影响。

代价：插件构建依赖网络拉 ILP 仓库。可接受——构建本来就要拉 crates.io。

**本地开发**：在插件仓库内用 `[patch]` 或直接写 path 覆盖，见下文"本地开发"。

## `install` 入口改造

现在 `parse_source` 强制 `owner/repo`，且拼
`{base}/{owner}/{repo}/refs/heads/{main,master}/plugin.json`。单仓库 + 子目录后
这条路径不存在（会去找 `plugins/refs/heads/main/plugin.json`），**必须改**。

新行为：

```
ilp plugin install fanqie              # 装 latest
ilp plugin install fanqie@1.0.0       # 装指定版本
ilp plugin install fanqie@latest      # 显式 latest
```

解析规则：

1. `source` 按 `@` 拆成 `(name, version)`；无 `@` 等价 `latest`。
2. 取 registry（`ILP_REGISTRY_URL` 可覆盖，同 `ILP_RAW_BASE` 的测试钩子思路）。
3. `name` 必须在 `registry.plugins` 里，否则报"没有名为 X 的插件"并列出可用名字。
4. `version != latest` 时必须在该插件 `versions` 里，否则报错并列出可用版本。
5. **校验 `abi` 与宿主支持的 ABI 一致**（顺带补掉旧方案缺口 ①）。
6. 下载 `url`，校验 wasm magic + `sha256` + `size` 与 registry 声明一致。
7. 写入 `plugins/<name>.wasm`（原子写）。

`sha256` 校验是新增的实质收益：现在只校验魔数，下载到截断或被篡改的产物
只要头部对就能装进去，然后在运行时以莫名其妙的 wasmtime 错误失败。

**向后兼容**：`owner/repo` 形式不再支持。旧 `plugin.json` 安装路径删除。
理由：单仓库后该路径语义已不成立，留着就是两条代码路径两套测试。
`ILP_RAW_BASE` 测试钩子一并移除，换成 `ILP_REGISTRY_URL`。

## CI：构建 → 生成 metadata → 部署

**Cloudflare Pages 用 Git 集成**（用户在 CF 控制台一次性 OAuth 授权），
构建命令由仓库内脚本执行，**不需要 `CLOUDFLARE_API_TOKEN`**：

```
CF Pages 项目设置：
  构建命令: python3 tools/build_site.py
  输出目录: _site
```

`tools/build_site.py` 做四件事：

1. 对每个含 `Cargo.toml` 的子目录 `cargo build --target wasm32-wasip2 --release`。
2. 读 `plugin.toml` + `Cargo.toml`，校验 `abi` 与 `WIT_ABI` 常量一致。
3. 生成 `_site/registry.json`、`_site/<name>/latest.json`、
   `_site/<name>/<version>/<name>.wasm`（内容相同则不动，避免无谓 diff）、`index.html`。
4. 若 `_site/registry.json` 里同名同版本已存在但 sha256 不同，**报错退出**——
   版本号是不可变契约，改代码必须升版本。

同时保留 `.github/workflows/ci.yml` 只做**校验**（build + clippy + 测试，不部署），
因为 CF Pages 的构建失败不会阻止 PR 合并。

## 本地开发

插件仓库里改 WIT 相关代码时，不想每次推 ILP 仓库：

```bash
# 在插件仓库根
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[patch."https://github.com/ILP-Project/ILP"]
ilp-plugin-wit = { path = "../ILP/crates/ilp-plugin-wit" }
EOF
```

`.cargo/config.toml` 进 `.gitignore`。

## 数据源：仍全部编译进 wasm（沿用旧结论）

字符映射表、API 常量、固定盐值等直接编译进 `.wasm`，不做运行时拉取。
理由见 `2026-10-26_plugin-distribution.md`"数据源"一节，结论未变。

## 顺带补掉的旧缺口

| 缺口 | 处理 |
|---|---|
| ① ABI 不校验 | `registry.json` 的 `abi` 字段 + 宿主编译期常量比对，装之前就拒绝 |
| ② 无法钉版本 | `install <name>@<version>` |
| ③ site_id 重复无检测 | CI 校验 `plugin.toml` 的 `site_id` 全局唯一；宿主加载阶段另做重复检测 |
| ④ 无 CI | `tools/build_site.py` + `ci.yml` |

## 迁移步骤

1. `plugin-fanqie` 的全部历史 merge 进 `plugins` 仓库（`git subtree` 或直接
   `git merge --allow-unrelated-histories`，保留提交）。
2. 源码从仓库根移到 `fanqie/` 子目录。
3. 加 `plugin.toml`，删 `plugin.json`（`url` 改由 CI 生成）。
4. 改 `Cargo.toml` 的 `ilp-plugin-wit` 为 git 依赖。
5. ILP 仓库打 tag `abi-v0.1.0`（插件构建依赖它）。

## 落地状态与实测（2026-10-27）

已实施，宿主 `750ad2f`、plugins 仓库 `819eb26`。门禁：宿主 62 测试、插件 27 测试，
两侧 fmt + clippy 全清。

实测确认的几件事，后续改动不要重蹈：

### 1. `git tag` 会挂起（不是卡死代码）

全局 git 配置里 `tag.gpgsign=true`，非交互环境触发 pinentry 等密码 →
永久阻塞。任何自动化脚本打 tag 必须 `git -c tag.gpgsign=false tag ...`。
这条曾经让一条命令挂 20 分钟，并被误判为代码问题。

### 2. `[patch]` / `paths` 救不了不存在的远端

Cargo 总会先解析原始 git 源的 `tag`。`plugins/.cargo/config.toml` 里的
`[patch."https://github.com/ILP-Project/ILP"]` 只能替换**已经解析成功**的源，
所以本地开发要么临时改 path 依赖，要么用 git `insteadOf` 重定向。
在 `abi-v0.1.0` 真正推到 GitHub 之前，干净 clone 的 plugins 仓库**无法构建**。

### 3. Cargo 内置 git 不读 `insteadOf`，要加 `git-fetch-with-cli`

实测把 `url.<local>.insteadOf` 指向本地裸仓库后，`cargo build` 仍报
`reference 'refs/remotes/origin/tags/abi-v0.1.0' not found`：Cargo 自带的
git 实现不认这个配置。加上

```toml
[net]
git-fetch-with-cli = true
```

后立即成功（锁定 `#750ad2f3`），产物组件头 `00 61 73 6d 0d 00 01 00`，
宿主能正常加载。**这是验证 tag 机制的唯一可靠姿势。**

### 4. ABI 校验已双向落地

- 装之前：`registry.json` 的 `abi` 与宿主 `plugin_manager::SUPPORTED_ABI` 比对，
  不匹配直接拒绝，不浪费一次下载。
- 加载时：`PluginMetadata.abi_version` 与同一常量比对。手工拷贝进来的插件
  绕不过这条。实测把插件里的 `0.1.0` 改成 `0.9.9` 后，宿主明确报
  "插件 FanqiePlugin 的 ABI 版本为 0.9.9，本版本 ilp 支持 0.1.0"。

注意：用二进制替换全局改 `0.1.0` 会连 WIT 包名一起改掉，wasmtime 会在
实例化阶段先报 `component imports instance ilp:plugin/host@0.9.9`。
要测宿主自己的 ABI 校验，必须改源码里的 `ABI_VERSION` 常量再重新编译。

### 5. 版本不可变已生效

篡改已发布的 `_site/fanqie/1.0.0/fanqie.wasm` 后重跑 `build_site.py`
会失败并提示升版本，不会覆盖。

### 6. `sha256`/`size` 校验覆盖两种攻击

- 追加字节 → 被 `size` 抓到（365474 → 365475）。
- 等长改中间一个字节 → `size` 相同，被 `sha256` 抓到。

## 遗留

- `abi-v0.1.0` tag 已在本地 ILP 仓库创建，**尚未推送**（该仓库还没有 remote）。
  推送到 GitHub 是 plugins 仓库能干净构建的前置条件。
- Cloudflare Pages 项目尚未在 dashboard 里创建。

## 参考

- 旧方案：`plan/2026-10-26_plugin-distribution.md`
- 现有安装逻辑：`crates/ilp-host/src/plugin_manager.rs`
- ABI 定义：`crates/ilp-plugin-wit/wit/plugin.wit`
- 插件加载：`crates/ilp-host/src/engine.rs::load_plugins`
