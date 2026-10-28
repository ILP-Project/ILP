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
https://ilp.elysola.org/
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
      "url": "https://ilp.elysola.org/fanqie/1.0.0/fanqie.wasm",
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

### 7. Cloudflare Pages 构建镜像不含 Rust

官方文档把 Rust 和 Swift 列在"默认不安装"里（`cloudflare/cloudflare-docs`
issue #849 明确写了这件事）。原计划"CF 构建命令直接跑
`python3 tools/build_site.py`"在 CF 上必然失败——它需要 `cargo` 和
`wasm32-wasip2`。

改成 **GitHub Actions 构建、CF 只托管 `site` 分支**：

- `publish.yml` 装 wasm target → 回填上次产物 → `build_site.py` →
  校验 → 推 `site` 分支；
- CF Pages 监听 `site` 分支，构建命令留空，输出目录 `/`。

这同时保住了"不需要 `CLOUDFLARE_API_TOKEN`"这个性质：只推分支，不调 API。

### 8. 回填 `_site` 是版本不可变守卫的前置条件

`publish.yml` 最不能省的一步是"把 `site` 分支的旧产物先 checkout 进
`_site`，再构建"。原因：守卫靠比较 `_site` 里已有产物的 sha256，而 CI
每次都是干净目录——不回填的话守卫永远不触发，改代码不升版本就会静默
覆盖别人已装好的同名版本。

本地实测（裸仓库模拟整个流程）：

- 回填后改代码不升版本 → `错误: fanqie 1.0.0 已发布过，但产物内容不同
  （afbe1a159674… → 5be401ba1b25…）`，退出码 1；
- 升到 1.1.0 → 两个版本并存，`latest` 指向 1.1.0，1.0.0 仍可下载。

### 9. 版本排序曾是字符串排序

`versions` 用字符串倒序，`1.10.0` 会排在 `1.9.0` 前面；`latest` 也只是
"这次构建的版本"，用旧版本号重新构建会把站点上的 latest 降回去。

现在 `version_key()` 按数字分段比较（预发布版本排在同号正式版之前），
`latest` 取所有已发布版本的最大值。CI 的校验步骤里也断言了这两点。

### 10. `_site` 里 `git init` 的仓库没有 remote

第一次真实运行死在 `'origin' does not appear to be a git repository`——
`_site` 是被 `git init` 出来的全新仓库，从未配置远端。改成显式带 token
推送完整 URL，不依赖 `actions/checkout` 留在外层仓库的 credential helper。

## 落地状态（2026-10-28）

- **已推送**：ILP `main` 18 个 commit + tag `abi-v0.1.0` 到
  `https://github.com/ILP-Project/ILP.git`；plugins `main` 到
  `https://github.com/ILP-Project/plugins.git`。
- **干净环境验证通过**：删掉 `Cargo.lock` 和 `target`、无 `.cargo` 补丁
  克隆 plugins，全量解析并锁定 `#750ad2f3`，组件头
  `00 61 73 6d 0d 00 01 00`，宿主可加载 `FanqiePlugin v1.0.0`。
- **CI 绿灯**：`ci` run 40714849372、`publish` run 40714849397 均 success。
- **`site` 分支已产出**：6 个文件，`registry.json` 的 sha256 与分支内
  产物一致（`541dac990bc951b8…`）。
- **351 章 E2E 通过**：网络恢复后，用从 `site` 分支安装的插件跑完整
  `get all`，**成功 351 章、失败 0 章**；PUA 残留 0、HTML 残留 0、空章节 0；
  封面 `data/covers/部族荣光_7069948840148732967.jpg` 17.7K / 225×300；
  重跑 `所有章节已下载`，退出码 0。

### 11. 自定义域名 `ilp.elysola.org` 已上线

域名解析到 Cloudflare，但 Pages 项目绑定前一直返回 **522**（源站不可达）。
绑定后立即 200，`registry.json` 内容正确。

宿主默认地址随之从占位的 `ilp-plugins.pages.dev` 改为该域名，抽成
`plugin_manager::DEFAULT_REGISTRY` 常量并加了断言字面量的测试——这个地址
同时出现在 README、计划文档和默认值里，三处不一致时失败方式最隐蔽：
安装照样成功，只是打到旧站点。

用**不带任何环境变量**的默认配置实测：

| 场景 | 结果 |
|---|---|
| `plugin install fanqie` | 退出码 0，装的产物与站点 sha256 一致 |
| `plugin install fanqie@1.0.0` | 退出码 0 |
| `plugin install fanqie@9.9.9` | 退出码 2，列出可用版本 |
| `plugin install nope` | 退出码 2，列出可用插件 |
| `plugin install ../evil` | 退出码 2，名字非法 |
| 351 章 `get all` | **351 成功 / 0 失败**，PUA 0、HTML 0、空章 0 |
| 幂等重跑 | `所有章节已下载`，退出码 0 |

## 遗留

- ~~Cloudflare Pages 项目尚未创建~~ — 已创建并绑定 `ilp.elysola.org`。
- `ILP-Project/plugin-fanqie` 已被 `ILP-Project/plugins` 取代，尚未决定
  归档还是删除。

## Cloudflare Pages 操作清单

命令行无法代做 dashboard 里的 Git 授权，需要手动点。全部只做一次。

### 1. 创建项目

Cloudflare Dashboard → Workers & Pages → Create → Pages → **Connect to Git**。

- 授权 GitHub，选择仓库 **`ILP-Project/plugins`**。
- 分支：**`site`**（不是 `main`）。

### 2. 构建设置

| 字段 | 值 |
|---|---|
| Framework preset | `None` |
| Build command | **留空** |
| Build output directory | **`/`** |
| Root directory | 留空 |

产物已经在 `site` 分支的根目录，CF 不需要再构建任何东西。

### 3. 环境变量

不需要。特别**不要**设 `CLOUDFLARE_API_TOKEN`——那条路径是给 wrangler
直传用的，本方案走的是分支托管。

### 4. 自定义域名（可选）

域名已定为 **`ilp.elysola.org`**，它就是宿主 `DEFAULT_REGISTRY`
（`crates/ilp-host/src/plugin_manager.rs`）的值，装插件时默认打这个地址。

在 Pages 项目里 Add custom domain 填 `ilp.elysola.org`。若是该 zone 下的
子域，CF 会自动建 CNAME 并签证书；`ilp.elysola.org` 的 DNS 目前已有记录
（解析到 CF IP），但源站返回 **522**，说明 Pages 项目还没绑定上去。

### 5. 验证

项目建好后跑：

```bash
curl -s https://ilp.elysola.org/registry.json | head -c 200
```

应能看到 `{"schema": 1, ...}`。然后：

```bash
ilp plugin install fanqie        # 装 latest
ilp plugin install fanqie@1.0.0  # 装指定版本
```

### 注意事项

- **分支必须选 `site`**。选 `main` 会去构建源码仓库，而 CF 镜像没有
  Rust，构建必然失败。
- 每次 push `main` 后 `publish.yml` 会更新 `site` 分支，CF 自动重新部署。
- `site` 分支由 CI 强推（`push -f`），**不要在上面手动提交**，会被覆盖。

## 参考

- 旧方案：`plan/2026-10-26_plugin-distribution.md`
- 现有安装逻辑：`crates/ilp-host/src/plugin_manager.rs`
- ABI 定义：`crates/ilp-plugin-wit/wit/plugin.wit`
- 插件加载：`crates/ilp-host/src/engine.rs::load_plugins`
