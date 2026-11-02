# JSONL 事件

使用 `--jsonl` 后，标准输出每行一个 JSON 对象；日志写入标准错误。

```bash
ilp --jsonl get all <book-id> -s <site-id>
ilp --jsonl update <book-id> -s <site-id>
ilp --jsonl update <book-id> -s <site-id> --apply
ilp --jsonl resume <task-id>
ilp --jsonl resume-all
ilp --jsonl migration-preview
ilp --jsonl migration-apply --yes
```

## 事件

### `update_report`

```json
{"event":"update_report","book":"书名","added":3,"apply":false}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `event` | string | 固定为 `update_report` |
| `book` | string | 书名 |
| `added` | number | 新增章节数 |
| `apply` | boolean | 是否使用 `--apply` |

### `chapter_added`

```json
{"event":"chapter_added","id":"c1","title":"第一章"}
```

### `chapter_downloaded`

```json
{"event":"chapter_downloaded","book":"书名","chapter":"第一章","id":"c1","status":"ok"}
```

`chapter_downloaded.status` 当前成功值为 `ok`。下载失败不会伪造成功事件；失败详情写入日志和最终统计。

### `download_report`

```json
{"event":"download_report","task_id":"...","book_id":"book-1","completed":2,"failed":0,"total":3}
```

恢复命令在每个任务完成后输出。`completed`、`failed` 和 `total` 是持久化任务的章节统计。

### `resume_error`

```json
{"event":"resume_error","task_id":"...","message":"..."}
```

`resume-all` 对单个任务恢复失败时输出，随后继续处理其他中断任务。

### `resume_summary`

```json
{"event":"resume_summary","attempted":2,"succeeded":1,"failed":1}
```

`resume-all` 最后输出批量恢复汇总。

### `migration_preview`

```json
{"event":"migration_preview","data_dir":"...","format":"Unknown","artifacts":[]}
```

只读检测旧数据，不创建备份、不修改数据库。

### `migration_applied`

```json
{"event":"migration_applied","status":"NotNeeded","records_migrated":0,"backup":null}
```

显式迁移成功后输出。`migration-apply` 在非交互环境必须使用 `--yes`。

### `migration_error`

```json
{"event":"migration_error","message":"..."}
```

迁移失败时输出；命令仍以致命错误退出，不应根据此事件推断迁移成功。

### `plugin_list`

```json
{"event":"plugin_list","plugins":[{"plugin_id":"plugin-example","plugin_name":"示例插件","site_id":"example","site_name":"示例站","version":"1.2.3","abi_version":"0.2.0","source":"registry:test","signature_key_id":"release-2026"}]}
```

`plugin list` 只从 Core 的 `PluginService::registered` 读取已注册记录；不会扫描插件
目录，也不会列出或信任未注册的 WASM。空列表仍输出 `plugins: []`。

## 兼容规则

- `event` 是事件类型，未知类型必须忽略而不是中止。
- 新字段可以增加，现有字段不会在同一主版本中改名或改变类型。
- 不要解析人类可读文本来判断状态。
- 事件顺序是产生顺序，不保证跨进程或跨任务排序。
- 退出码仍然有效：`0` 全部成功，`1` 部分失败，`2` 致命错误。
