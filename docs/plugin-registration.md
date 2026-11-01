# 本地插件 trusted registration 边界

`ilp-host` 的 `plugin_registration` 适配器只负责把**已经存在于本地**的插件
metadata、WASM 原始字节和调用方明确取得的 trust inputs 转换成 Core 的
`PluginRegistrationRequest`，再委托给 `PluginService`。它不执行网络安装、不读取
registry、不验证或生成签名，也不把文件路径自动当作信任来源。

## 输入边界

- `metadata`：必须来自同一个本地插件实例；Core 继续负责 metadata、ABI 能力和
  host policy 校验。
- `wasm`：同一个本地产物的原始字节。适配器重新计算 SHA-256，并拒绝调用方提供
  的 `content_hash` 与实际字节不匹配。
- `user_confirmed`：必须由上层用户确认流程明确设置为 `true`。缺省、`false` 或
  未取得确认时拒绝。
- `source`：必须由调用方明确提供的非空来源标识，例如本地导入记录。适配器不会
  从路径、registry 或网络地址推断来源。
- `signature_key_id`：只有上层已经有可靠的签名验证结果和对应 key ID 时才填写。
  `None` 只表示没有可传递的 key ID，并不表示签名已验证；适配器不会伪造 key ID
  或签名。

没有可靠 trust inputs 时传入 `None`，适配器会在调用 Core 前拒绝。这是有意的
**default-deny** 行为：`plugin info` 和其他 metadata-only 读取不能升级为 trusted
registration。成功请求仍必须通过 Core `PluginService::register`，由 Core 负责
策略校验、冲突检查和持久化。

当前适配器只覆盖已有本地 metadata 和明确 trust inputs；网络下载、registry
签名、产物签名验证和用户交互不属于该 API 的职责。

## CLI 边界

`ilp plugin validate <path>` 是本地插件的显式 dry-run 入口。它只读取给定的
WASM，校验组件格式并在 deny-all 策略下读取和校验 metadata，然后打印摘要。该命令
不会访问 registry 或下载网络产物，不会安装、启用插件，也不会调用
`plugin_registration` 的 trusted registration 或写入 Core 的信任记录。

因此，`plugin info` 和 `plugin validate` 都是 inspection/validation，不是信任授予。
只有上层取得可靠的用户确认、来源和签名验证结果后，才能调用
`build_local_registration_request` / `register_local_plugin`；没有这样的交互式
确认与签名流程时，不应把 `plugin_manager` 的安装结果自动接入 Core registration。
