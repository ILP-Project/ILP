# plugins/

插件产物放在这里，格式为 `<name>.wasm`。

本仓库不再包含插件源码——站点插件在 `ILP-Project/plugins`（一站点一子目录），
产物经 Cloudflare Pages 的静态 registry 分发，默认地址 `https://ilp.elysola.org`。

手动安装：

```bash
cargo build --target wasm32-wasip2 --release   # 在 ILP-Project/plugins 里
cp target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm <宿主>/plugins/fanqie.wasm
```
