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

## 测试与发布门禁

```bash
cargo fmt --check
cargo clippy --manifest-path plugins/my-site/Cargo.toml \
  --target wasm32-wasip2 -- -D warnings
cargo test --manifest-path plugins/my-site/Cargo.toml
```

模板插件位于 `plugins/template`，用于验证最小 ABI 实现和构建路径。
