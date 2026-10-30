# ILP 插件开发

插件使用 ABI `0.2.0`，共享接口定义位于 `crates/ilp-plugin-wit/wit/v2/plugin.wit`。

## 快速开始

从模板复制一个插件：

```bash
cp -R plugins/template plugins/my-site
```

然后修改 `plugins/my-site/Cargo.toml` 的 package 名称，以及 `src/lib.rs` 中的 metadata 和业务逻辑。

## 构建

```bash
cargo build --manifest-path plugins/my-site/Cargo.toml \
  --target wasm32-wasip2 --release
```

宿主安装前会校验 ABI、WASM component、大小、SHA-256 和签名。

## 元数据治理

每个插件必须在 `plugin.toml` 中声明：

- `license`：代码许可证；
- `data_source`：数据来源；
- `authorization_boundary`：授权边界与不支持的行为；
- `maintenance`：`active`、`limited`、`unmaintained` 或 `archived`；
- `known_limitations`：已知限制列表。

发布脚本会拒绝缺少这些字段或维护状态无效的插件，并将元数据写入 registry 和插件页面。

## 测试与发布门禁

```bash
cargo fmt --check
cargo clippy --manifest-path plugins/my-site/Cargo.toml \
  --target wasm32-wasip2 -- -D warnings
cargo test --manifest-path plugins/my-site/Cargo.toml
```

模板插件位于 `plugins/template`，用于验证最小 ABI 实现和构建路径。
