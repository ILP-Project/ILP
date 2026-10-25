# plugins/

插件产物放在这里，格式为 `<name>.wasm`。

本仓库不再包含插件源码——站点插件在独立仓库 `ILP-Project/plugin-<site>` 里构建，
产物经 GitHub Pages 静态 registry 分发（见 `plan/2026-10-26_plugin-distribution.md`）。

手动安装：

```bash
cargo build --target wasm32-wasip2 --release   # 在 plugin-<site> 仓库里
cp target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm <宿主>/plugins/fanqie.wasm
```
