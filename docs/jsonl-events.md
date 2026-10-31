# JSONL 事件

使用 `--jsonl` 后，标准输出每行一个 JSON 对象；日志写入标准错误。

```bash
ilp --jsonl get all <book-id> -s <site-id>
ilp --jsonl update <book-id> -s <site-id>
ilp --jsonl update <book-id> -s <site-id> --apply
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

## 兼容规则

- `event` 是事件类型，未知类型必须忽略而不是中止。
- 新字段可以增加，现有字段不会在同一主版本中改名或改变类型。
- 不要解析人类可读文本来判断状态。
- 事件顺序是产生顺序，不保证跨进程或跨任务排序。
- 退出码仍然有效：`0` 全部成功，`1` 部分失败，`2` 致命错误。
