# ILP 插件分发与数据源方案

> 状态：**已被 `2026-10-27_plugin-single-repo.md` 取代**。
>
> 本文档的**组织结论**（一站点一仓库 `plugin-<site>` + GitHub Pages）已废弃：
> 改为单个 `plugins` 仓库 + Cloudflare Pages。**数据源结论**（全部编译进 wasm）
> 以及"待补的代码漏洞"清单仍然有效，新文档继续沿用。
>
> 保留本文档是为了记录当时的取舍理由，不要按它实施。

## 组织边界

GitHub 组织 `ILP-Project`，职责划分：

| 仓库 | 内容 | 说明 |
|---|---|---|
| `ILP` | 宿主本体（CLI、存储、HTTP、wasmtime、插件管理） | 当前仓库 |
| `plugin-<site>` | 单个站点插件，一站点一仓库 | 如 `plugin-fanqie` |
| `plugins` | **静态 registry**（GitHub Pages 源） | 只有 JSON 与产物，无源码 |

插件用 `plugin-<站点>` 前缀命名（已定），一眼区分"本体 / 插件 / registry"。

## 插件分发：GitHub Pages 静态 registry

```
https://ilp-project.github.io/plugins/
├── registry.json                 全部插件索引：发现、搜索、ABI 声明
├── fanqie/
│   ├── latest.json               → "1.2.0"
│   ├── 1.2.0/fanqie.wasm         不可变，永不覆盖
│   └── 1.1.0/fanqie.wasm
└── qidian/
    └── ...
```

`registry.json` 形状：

```json
{
  "schema": 1,
  "plugins": [
    {
      "name": "fanqie",
      "site_id": "fanqie",
      "site_name": "番茄小说",
      "abi": "0.1.0",
      "latest": "1.2.0",
      "versions": ["1.2.0", "1.1.0"]
    }
  ]
}
```

选择它的理由：

- **版本可 pin**：`1.2.0/` 路径不可变，`install fanqie@1.1.0` 有意义；当前的
  `refs/heads/main` 方案永远只能装 latest，无法回滚。
- **可携带 ABI 声明**：`abi` 字段让宿主在下载前就能判断兼容性（见下文漏洞 ①）。
- **一次请求完成发现**：`registry list` 只需拉一个 JSON，不必逐个仓库探测。
- **产物与源码解耦**：registry 仓库里没有源码，纯静态托管，CI 只负责生成。

代价：偏离原版 `install <owner/repo>` 的心智模型，且需要写 CI（方案阶段不实现）。

### 发布流程（后续实现）

1. `plugin-fanqie` 打 tag `v1.2.0` → CI 构建 wasm32-wasip2 → 上传到 Releases。
2. CI 推送到 `plugins` 仓库：写 `fanqie/1.2.0/fanqie.wasm`、更新 `fanqie/latest.json`
   和顶层 `registry.json`。
3. `1.2.0/` 一旦写入不再修改；修 bug 发 `1.2.1`，绝不覆盖旧路径。

## 数据源：全部编译进 wasm

字符映射表、API 常量、固定盐值等**直接编译进 `.wasm`**，不做运行时拉取、不做构建时外部依赖。

理由：

- 插件本就沙箱化，运行时只能经宿主 `http` 接口取数据；把表做成远程依赖会让
  **启动依赖网络**，失败面凭空增大。
- 表格是协议的组成部分，不是配置。协议变了就该发新版本插件，而不是偷偷换表。
- 单产物、无外部依赖，`plugin install` 的语义保持简单。
- 来源可溯已由 README 的 fanqie-assistant 署名承担，不需要靠独立数据仓库体现。

## 待补的代码漏洞（方案阶段不改码）

按优先级排列。这些都是现状里真实存在的缺口，不是假想：

### ① ABI 版本不校验（最要紧）

`wit/plugin.wit` 声明了 `package ilp:plugin@0.1.0`，但 `engine.rs::PluginInstance::load`
只调 `metadata()`，**从不比对版本**。后果：ABI 变更后旧插件会被正常加载，然后在调用
某个函数时以莫名其妙的 wasmtime 错误失败，排查成本极高。

修法：`PluginMetadata` 增加 `abi_version` 字段，`load` 时与宿主编译期常量比对，
不匹配直接拒绝并提示"插件要求 0.1.0，宿主支持 0.2.0，请升级插件"。

### ② install 无法固定版本

`plugin_manager.rs:156` 只读 `refs/heads/{main,master}`，永远拿 latest。
改为 registry 方案后，需要支持 `install fanqie@1.1.0`。

### ③ site_id 重复无检测

两个插件声明同一个 `site_id` 时，`find_plugin` 取第一个匹配，
行为取决于 `plugins/` 目录里的文件排序（`load_plugins` 里 `paths.sort()`）。
应在加载阶段检测重复并告警/拒绝。

### ④ 无 CI

项目当前没有任何 `.github/workflows`，插件构建、产物校验、registry 更新全靠手工。
方案落地时需要三条流水线：宿主跨平台构建、插件 wasm 构建 + 校验、registry 发布。

## 参考

- 现有安装逻辑：`crates/ilp-host/src/plugin_manager.rs`
- ABI 定义：`wit/plugin.wit`（`package ilp:plugin@0.1.0`）
- 插件加载：`crates/ilp-host/src/engine.rs::load_plugins`
