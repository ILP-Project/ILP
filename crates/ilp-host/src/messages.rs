//! User-visible messages live here so a future locale can replace this module.

pub const ALL_CHAPTERS_DOWNLOADED: &str = "所有章节已下载";
pub const DOWNLOAD_DONE: &str = "完成";
pub const DOWNLOAD_SUCCESS: &str = "成功";
pub const DOWNLOAD_FAILED: &str = "失败";
pub const CHAPTER_DOWNLOADED: &str = "已下载";
pub const CHAPTER_ADDED: &str = "新增";
pub const UPDATE_REPORT: &str = "更新报告";
pub const PREVIEW: &str = "（预览，未修改本地数据）";
pub const APPLYING: &str = "（将立即下载）";

pub fn update_report(title: &str, added: usize, apply: bool) -> String {
    format!(
        "{UPDATE_REPORT}: {title}，新增 {added} 章{}",
        if apply { APPLYING } else { PREVIEW }
    )
}

pub fn chapter_downloaded(book: &str, chapter: &str) -> String {
    format!("{CHAPTER_DOWNLOADED}: {book} - {chapter}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_stable_and_ready_for_replacement() {
        assert_eq!(
            update_report("书", 2, false),
            "更新报告: 书，新增 2 章（预览，未修改本地数据）"
        );
        assert_eq!(chapter_downloaded("书", "第一章"), "已下载: 书 - 第一章");
    }
}
