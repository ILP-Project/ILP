//! ILP 插件 ABI。
//!
//! 这里同时生成两个 ABI 版本的绑定，分别放在 [`v0_1`] 和 [`v0_2`] 模块里。
//!
//! WIT 层面它们是同一个包的两个版本（`ilp:plugin@0.1.0` /
//! `ilp:plugin@0.2.0`）。生成到不同 Rust 模块后互不冲突——同一 crate 内
//! 多个 `bindgen!` 只要模块不同就能共存，这是宿主同时支持新旧插件的前提。
//!
//! 根导出目前仍是 0.1.0，等插件侧迁移完成后再切换。

mod v0_1 {
    wit_bindgen::generate!({
        world: "novel-plugin",
        path: "wit/v1",
        pub_export_macro: true,
    });
}

/// 0.2.0 ABI：权限声明 + capability handle + 结构化章节结果。
pub mod v0_2 {
    wit_bindgen::generate!({
        world: "novel-plugin",
        path: "wit/v2",
    });
}

pub use v0_1::*;
