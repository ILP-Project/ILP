/// 进程退出码。
///
/// 和原版不同，这里把"部分章节失败"与"整体跑不起来"分开：
/// 前者用户重跑即可续传，后者需要先修配置或网络。
pub const SUCCESS: i32 = 0;
pub const PARTIAL: i32 = 1;
pub const FATAL: i32 = 2;

/// 标记"部分成功"的错误：错误信息照常打印，但退出码是 1 而不是 2。
#[derive(Debug)]
pub struct PartialFailure(pub String);

impl std::fmt::Display for PartialFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PartialFailure {}

pub fn code_for(err: &anyhow::Error) -> i32 {
    if err.chain().any(|e| e.is::<PartialFailure>()) {
        PARTIAL
    } else {
        FATAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn plain_error_is_fatal() {
        assert_eq!(code_for(&anyhow!("配置读取失败")), FATAL);
    }

    #[test]
    fn partial_failure_maps_to_one() {
        let err = anyhow::Error::new(PartialFailure("有 3 章下载失败".to_string()));
        assert_eq!(code_for(&err), PARTIAL);
    }

    #[test]
    fn partial_failure_is_detected_when_wrapped() {
        let err = anyhow::Error::new(PartialFailure("x".to_string())).context("外层上下文");
        assert_eq!(code_for(&err), PARTIAL);
    }
}
