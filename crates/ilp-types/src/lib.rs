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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChapterEntry {
    pub id: String,
    pub title: String,
    pub url: String,
    pub fetch_url: String,
    pub update_time: i64,
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

/// 插件解析出的一章正文。
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChapterContent {
    pub id: String,
    pub title: String,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ParseInput {
    pub title: String,
    pub content: String,
}
