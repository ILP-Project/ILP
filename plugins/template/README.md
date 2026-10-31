# ILP 插件模板

这是 `ilp plugin new` 嵌入到二进制里的骨架源头。**不要直接编辑这个目录再构建**：
里面是 `@PLUGIN_NAME@`、`@SITE_ID@`、`@SITE_NAME@`、`@DOMAIN@`、`@CRATE_NAME@`
这类占位符，占位符即非法包名，单独 build 必然失败。

正确用法是生成一份：

```bash
ilp plugin new my-site --site-name "我的站点" --domain mysite.com
```

## 构建

在生成的目录里执行：

```bash
cargo build --manifest-path my-site/Cargo.toml --target wasm32-wasip2 --release
```

产物位于：

```text
my-site/target/wasm32-wasip2/release/ilp_plugin_my_site.wasm
```

`plugin new` 结束时会把这条真实路径打出来，直接照着复制即可。

## 改模板本身

改动 `plugins/template/` 后必须：

1. 保留每个占位符至少出现一次，否则 `template_files_are_embedded_and_placeholder_driven`
   会失败（它守的就是"别把模板值写死回字面量"）；
2. 在宿主的 `TEMPLATE_FILES` 里登记新文件，否则不会被生成出去；
3. 跑 `cargo test -p ilp-host new_plugin`。

## 开发约束

- `metadata()` 中的权限是申请，不是宿主授权；每次 host 调用仍会被宿主策略复查。
- 插件不应实现账号密码登录，只处理用户已授权的 Cookie/Token。
- 所有章节都应返回结构化 `ChapterResult`，不要静默丢失失败章节。
- 提交前执行 `cargo fmt --check`、`cargo clippy --target wasm32-wasip2 -- -D warnings` 和 WASM release build。
- 解析逻辑应使用 `fixtures/` 中的脱敏 JSON 回放数据，不在测试中访问真实站点。
- fixture schema v1 将请求结构与响应内容分离，正文以 Base64 保存；不得提交 Cookie、Token 或设备密钥。
