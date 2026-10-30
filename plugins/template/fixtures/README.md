# Fixture 回放

fixture 用于在没有真实网络的情况下验证插件请求和解析逻辑。

## schema v1

每个 JSON 文件包含：

- `schema_version`: 固定为 `1`
- `request.method`
- `request.url`
- `request.headers`
- `request.body_base64`
- `response.status`
- `response.headers`
- `response.body_base64`
- `notes`

请求结构与响应内容分离，响应正文使用 Base64 保存二进制数据。fixture 必须脱敏，不得包含 Cookie、Token、密码、设备 ID 或签名密钥。

## 运行原则

测试应从 fixture 读取响应，验证插件解析结果；不得在单元测试中访问真实站点。新增 fixture 需要人工检查脱敏内容，并通过 secret scanner。
