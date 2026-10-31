# 更新日志

本文件记录 ILP-rs 的用户可见变更。版本遵循 SemVer。

## [Unreleased]

## [0.2.0] - 2026-10-29

### 新增

- 增加 `--quiet`、`--log-file`、Shell completion 和 man page。
- 增加 CLI 参数解析及帮助文档生成测试。

### 兼容性

- 保持已有命令、JSONL 事件和退出码兼容。

### 新增

- 增加 `ilp completion bash|zsh|fish`，用于生成 Shell completion。
- 增加 `ilp completion man`，用于生成 Unix man page。
- 增加全局 `--quiet`，抑制终端进度提示，同时保留日志文件输出。
- 增加全局 `--log-file <PATH>`，可将本次运行日志写入指定文件。
- 增加 CLI 参数解析、completion 和 man page 生成测试。

### 可观测性

- 运行摘要继续通过 `ilp::summary` 日志 target 输出，便于按站点、书籍、成功数和失败数筛选。
- JSONL 输出继续只包含已定义事件，不把日志或普通进度混入标准输出。

### 兼容性

- `--jsonl`、退出码和已有 JSONL 事件字段保持不变。
- 不指定 `--log-file` 时，仍使用配置中的滚动日志文件。

## [0.1.0] - 2026-10-29

### 初始版本

- 提供 Rust/WASM 插件架构和 ABI 0.1/0.2 兼容支持。
- 提供小说获取、搜索、更新、插件管理和诊断命令。
- 提供插件签名、registry 签名、请求权限和 DNS rebinding 防护。
- 提供 JSONL 自动化输出、record/replay 和离线安装测试。
