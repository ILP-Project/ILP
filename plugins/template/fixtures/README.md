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

## 录制

宿主内置录制/回放，环境变量即可切换，插件代码不需要知道自己在录还是在放：

```bash
# 录：放行真实网络，把每个请求/响应写成 cassette
ILP_RECORD_DIR=/tmp/cassettes ilp get all <book_id> -s <site_id>

# 放：只读 cassette，未命中就是错误，绝不回退网络
ILP_REPLAY_DIR=/tmp/cassettes ilp get all <book_id> -s <site_id>
```

回放模式会自动把重试次数设为 0——网络抖动下的重试在回放里没有意义，
只会让"未命中"被重试掩盖。

**未命中是硬错误，不是警告。** 如果回放时提示找不到某个请求，说明录制时
没走到那条分支，或请求体的某个字段（时间戳、随机数）每次不同。后者要在
插件里把它固定下来，而不是放宽匹配规则——放宽匹配等于让测试不再验证任何
东西。

匹配键是「方法 + 脱敏后的 URL + 请求体 SHA-256」。只有一个例外：同一个
URL 只有一条 cassette 时允许忽略请求体匹配（设备注册这类带随机体的请求
需要）。**同一 URL 有多条且请求体不同时会直接报错**，不会猜一条给你。

## 脱敏

录制器会按标记做脱敏，覆盖常见的秘密头（`authorization`、`cookie`、
`x-*-token` 等）和参数（`device_id`、`access_token`、`password` 等）。
标记匹配是**按子串**的，所以 `x-ms-token` 这种没被列进固定名单的头也会中。

但这是"抓大概率"，不是保证：

- 新站点的自定义头可能不带任何常见标记；
- 秘密可能藏在响应正文里，而不在头或 URL 参数里；
- 加密后的设备指纹换个名字就认不出来了。

所以**录完必须自己看一眼**：

```bash
python3 tools/scan_fixtures.py --list          # 列出将扫描的文件
python3 tools/scan_fixtures.py                 # 扫描，命中退出码 1
```

扫描器检查 `.json/.jsonl/.txt/.log/.har/.wit`，跳过 `target/`、`_site/`、
`node_modules/`、`.git/`、`.github/`。它按已知模式和敏感键名匹配，同样
不是保证——真正的保证是"提交前人工看一遍"。

## 提交前自查

1. cassette 里的 URL 有没有暴露内部地址或代理；
2. 响应正文里有没有内嵌的会话标识；
3. 跑一遍 `scan_fixtures.py`；
4. 确认 `ILP_REPLAY_DIR=<cassettes> cargo test` 能全绿（不联网）。
