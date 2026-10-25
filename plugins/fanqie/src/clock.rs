//! 时间源。
//!
//! WASM guest 里没有系统时钟，只能问宿主要；但单元测试跑在原生目标上，
//! 那里没有宿主可问，所以退回标准库。密码学代码要能被原生测试覆盖，
//! 这层间接是必要的。

#[cfg(target_family = "wasm")]
pub fn now_ms() -> u64 {
    crate::ilp::plugin::host::now_ms()
}

#[cfg(not(target_family = "wasm"))]
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn now_ms_is_positive() {
        assert!(super::now_ms() > 0);
    }
}
