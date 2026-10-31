use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PluginMetadata {
    /// 插件编译依据的 ABI 版本，加载时与宿主支持的版本比对。
    pub abi_version: String,
    pub site_name: String,
    pub site_id: String,
    pub site_domain: String,
    pub plugin_name: String,
    pub plugin_id: String,
    pub version: String,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChapterEntry {
    pub id: String,
    pub title: String,
    pub url: String,
    pub fetch_url: String,
    pub update_time: i64,
    /// 站点声明的稳定展示顺序；为空时保留插件返回的目录位置。
    #[serde(default)]
    pub order_key: String,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ChapterOrderConflict {
    pub value: String,
    pub first_id: String,
    pub second_id: String,
}

/// 检查目录能否无歧义地映射到统一文档顺序。
///
/// 空 `order_key` 表示插件没有声明排序键，不参与冲突检查；重复的章节
/// id 始终是错误，因为它会让 manifest 和落盘状态无法唯一定位章节。
pub fn validate_chapter_order(chapters: &[ChapterEntry]) -> Result<(), String> {
    let mut ids = std::collections::HashMap::new();
    let mut order_keys = std::collections::HashMap::new();
    for chapter in chapters {
        if let Some(first_id) = ids.insert(chapter.id.clone(), chapter.id.clone()) {
            return Err(format!("章节 id 重复: {}（{}）", chapter.id, first_id));
        }
        if chapter.order_key.is_empty() {
            continue;
        }
        if let Some(first_id) = order_keys.insert(chapter.order_key.clone(), chapter.id.clone()) {
            return Err(format!(
                "章节 order_key 冲突: {}（{} 与 {}）",
                chapter.order_key, first_id, chapter.id
            ));
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SearchResult {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover_url: String,
    pub description: String,
}

/// 一页搜索结果。
///
/// 分页是游标式的：只有插件知道服务端下一页的游标（番茄用的是
/// passback + search_id，且会忽略 offset），宿主无法自己推算，
/// 所以游标必须和结果一起往返。
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    /// 下一页游标；`has_more` 为 false 时无意义
    pub cursor: String,
    /// 服务端声明是否还有下一页
    pub has_more: bool,
}

/// 插件解析出的一章结果状态。
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChapterStatus {
    Ok,
    Locked,
    Captcha,
    AuthRequired,
    NotFound,
    Invalid,
    Error,
}

/// 插件解析出的一章结果。
///
/// `Ok` 才会携带正文；失败状态仍然保留在宿主侧，供 manifest、诊断和
/// 重试策略使用，而不是被 `filter_map` 静默丢掉。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChapterResult {
    pub id: String,
    pub title: String,
    pub status: ChapterStatus,
    pub text: Option<String>,
    pub error_code: String,
    pub retryable: bool,
    pub message: Option<String>,
}

impl ChapterResult {
    pub fn ok(id: String, title: String, text: String) -> Self {
        Self {
            id,
            title,
            status: ChapterStatus::Ok,
            text: Some(text),
            error_code: String::new(),
            retryable: false,
            message: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ParseInput {
    pub title: String,
    pub content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_result_carries_text_and_no_error() {
        let result = ChapterResult::ok("ch-1".into(), "第一章".into(), "正文".into());
        assert_eq!(result.status, ChapterStatus::Ok);
        assert_eq!(result.text.as_deref(), Some("正文"));
        assert!(result.error_code.is_empty());
        assert!(!result.retryable);
    }

    #[test]
    fn order_validation_accepts_unique_declared_keys() {
        let chapters = vec![
            ChapterEntry {
                id: "a".into(),
                title: "A".into(),
                url: String::new(),
                fetch_url: String::new(),
                update_time: 0,
                order_key: "001".into(),
            },
            ChapterEntry {
                id: "b".into(),
                title: "B".into(),
                url: String::new(),
                fetch_url: String::new(),
                update_time: 0,
                order_key: "002".into(),
            },
        ];
        assert!(validate_chapter_order(&chapters).is_ok());
    }

    #[test]
    fn order_validation_rejects_duplicate_keys_and_ids() {
        let base = ChapterEntry {
            id: "a".into(),
            title: "A".into(),
            url: String::new(),
            fetch_url: String::new(),
            update_time: 0,
            order_key: "001".into(),
        };
        let mut duplicate_key = base.clone();
        duplicate_key.id = "b".into();
        assert!(validate_chapter_order(&[base.clone(), duplicate_key]).is_err());
        assert!(validate_chapter_order(&[base.clone(), base]).is_err());
    }

    #[test]
    fn failed_result_can_be_serialized_without_a_body() {
        let result = ChapterResult {
            id: "ch-2".into(),
            title: "第二章".into(),
            status: ChapterStatus::AuthRequired,
            text: None,
            error_code: "auth".into(),
            retryable: false,
            message: Some("需要登录".into()),
        };
        let json = serde_json::to_string(&result).expect("serialize result");
        assert!(json.contains("auth"));
        assert!(!json.contains("正文"));
        let round_trip: ChapterResult = serde_json::from_str(&json).expect("deserialize result");
        assert_eq!(round_trip.status, ChapterStatus::AuthRequired);
        assert!(round_trip.text.is_none());
    }
}
