use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PluginMetadata {
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
