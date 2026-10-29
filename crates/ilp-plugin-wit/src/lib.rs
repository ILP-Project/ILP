//! ILP 插件 ABI 0.2.0。
//!
//! 0.1.0 的 WIT 定义仍保存在 `wit/v1`，供宿主兼容加载历史组件；
//! 但这里不能同时生成两套 guest `export!` 绑定。Component Model 链接器
//! 会把两个 world 的导出要求合并，导致新插件被错误要求同时实现 0.1.0。
//! 因此：插件 crate 只依赖当前 ABI，宿主在自己的模块里单独 bindgen 0.1.0。

wit_bindgen::generate!({
    world: "novel-plugin",
    path: "wit/v2",
    pub_export_macro: true,
});
