use anyhow::Result;

/// 统一的可导出文档模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub title: String,
    pub author: String,
    pub chapters: Vec<DocumentChapter>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentChapter {
    pub title: String,
    pub text: String,
}

/// 将统一文档模型转换为一种用户可保存的格式。
pub trait Exporter {
    fn export(&self, document: &Document) -> Result<String>;
}

/// 当前默认的纯文本导出器，保持原有章节文件格式。
#[derive(Debug, Default, Clone, Copy)]
pub struct TxtExporter;

impl Exporter for TxtExporter {
    fn export(&self, document: &Document) -> Result<String> {
        let mut output = String::new();
        for (index, chapter) in document.chapters.iter().enumerate() {
            if index > 0 {
                output.push_str("\n\n");
            }
            output.push_str(&chapter.title);
            output.push('\n');
            output.push_str(&document.author);
            output.push_str("\n---\n");
            output.push_str(&chapter.text);
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn txt_export_preserves_the_existing_single_chapter_layout() {
        let document = Document {
            title: "书".into(),
            author: "作者".into(),
            chapters: vec![DocumentChapter {
                title: "第一章".into(),
                text: "正文".into(),
            }],
        };
        assert_eq!(
            TxtExporter.export(&document).unwrap(),
            "第一章\n作者\n---\n正文"
        );
    }

    #[test]
    fn txt_export_separates_multiple_chapters() {
        let document = Document {
            title: "书".into(),
            author: "作者".into(),
            chapters: vec![
                DocumentChapter {
                    title: "第一章".into(),
                    text: "一".into(),
                },
                DocumentChapter {
                    title: "第二章".into(),
                    text: "二".into(),
                },
            ],
        };
        assert_eq!(
            TxtExporter.export(&document).unwrap(),
            "第一章\n作者\n---\n一\n\n第二章\n作者\n---\n二"
        );
    }
}
