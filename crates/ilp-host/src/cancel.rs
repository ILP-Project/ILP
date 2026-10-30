//! 取消信号。
//!
//! 用一个进程级标志把「用户按了 Ctrl-C」广播给所有可能长时间阻塞的地方：
//! 排队中的 HTTP 任务、重试退避、以及正在沙箱里执行的插件。
//!
//! 为什么不用 channel / guard 之类更"漂亮"的东西：取消是单向、一次性、
//! 全局的语义，而且要从信号处理器（只能做异步信号安全的极少数事）里触发。
//! 一个 `AtomicBool` 恰好够用，也恰好安全。

use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// 请求取消。可以被重复调用：第二次调用由调用方决定要更强的手段。
///
/// 返回本次调用之前是否已经请求过取消，用于实现「一次优雅、二次强制」。
pub fn request() -> bool {
    REQUESTED.swap(true, Ordering::SeqCst)
}

pub fn requested() -> bool {
    REQUESTED.load(Ordering::SeqCst)
}

/// 仅在测试里用：把标志复位，避免同一进程内的用例相互污染。
#[cfg(test)]
pub fn reset() {
    REQUESTED.store(false, Ordering::SeqCst);
}

#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 测试里共用全局取消标志的互斥锁。
///
/// 取消是**进程级**状态，而 `cargo test` 是并行跑的：一个用例把标志置位，
/// 别的用例的请求就会被“取消”掉。把所有会真的发出请求、或会置位的用例
/// 串行化，并在拿锁时复位。
#[cfg(test)]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reset();
    guard
}

/// 统一的取消错误，方便上层按类型判断，而不是去匹配中文字符串。
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "已取消")
    }
}

impl std::error::Error for Cancelled {}

/// 已经请求取消就返回错误，否则 Ok。
pub fn check() -> anyhow::Result<()> {
    if requested() {
        Err(anyhow::Error::new(Cancelled))
    } else {
        Ok(())
    }
}

/// 可被取消打断的睡眠：把长退避切成小片，避免用户按了 Ctrl-C 还要干等几秒。
/// 返回是否被取消。
pub fn sleep(duration: std::time::Duration) -> bool {
    const SLICE: std::time::Duration = std::time::Duration::from_millis(20);
    let mut left = duration;
    while left > std::time::Duration::ZERO {
        if requested() {
            return true;
        }
        let step = left.min(SLICE);
        std::thread::sleep(step);
        left -= step;
    }
    requested()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_sticky_and_idempotent() {
        let _guard = test_guard();
        reset();
        assert!(!requested());
        assert!(check().is_ok());
        assert!(!request(), "第一次请求应返回「此前未取消」");
        assert!(request(), "第二次请求应返回「此前已取消」");
        assert!(requested());
        assert!(check().is_err());
        reset();
    }

    #[test]
    fn sleep_wakes_up_early_when_cancelled() {
        // 取消是进程级全局状态。不拿守卫的话，并行跑的其它测试
        // 一旦调用 request()，这里的第一次断言就会莫名其妙地失败。
        let _guard = test_guard();
        reset();
        let started = std::time::Instant::now();
        assert!(!sleep(std::time::Duration::from_millis(5)));
        assert!(started.elapsed() >= std::time::Duration::from_millis(5));

        request();
        let started = std::time::Instant::now();
        assert!(sleep(std::time::Duration::from_secs(5)));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "取消后不应继续睡满"
        );
        reset();
    }
}
