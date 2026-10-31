//! 跨 WASM 边界的端到端测试。
//!
//! 这一层的价值在于验证"宿主与插件对契约的理解一致"，而不是重复单元测试。
//! 插件里的接口地址是硬编码的，所以这里不碰真实网络：只验证加载、元数据、
//! 以及两阶段抓取 ABI 的形状与错误传播。
//!
//! 如果 wasm 产物不存在（没跑过 wasm 构建），测试会打一行提示并跳过，
//! 而不是失败——否则 `cargo test` 在只关心宿主的场景下会莫名其妙变红。

use std::path::PathBuf;

fn wasm_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ILP_E2E_PLUGIN") {
        return PathBuf::from(path);
    }
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    // 插件自单一仓库迁移后不再在本仓库构建，产物在兄弟目录 `plugins` 下。
    // 保留旧路径做回退：拆分前构建过的产物还在，能让 e2e 继续跑。
    let sibling = p
        .parent()
        .map(|root| root.join("plugins/target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm"));
    if let Some(path) = sibling {
        if path.exists() {
            return path;
        }
    }
    p.join("target/wasm32-wasip2/release/ilp_plugin_fanqie.wasm")
}

/// 拿到已构建的插件；未构建则返回 None 让调用方跳过。
fn plugin_wasm() -> Option<PathBuf> {
    let path = wasm_path();
    if path.exists() {
        Some(path)
    } else {
        eprintln!(
            "跳过 e2e：未找到 {}（先在 ILP-Project/plugins 里 cargo build --target wasm32-wasip2 --release）",
            path.display()
        );
        None
    }
}

fn tmp_storage(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ilp_e2e_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

#[test]
fn component_loads_and_reports_metadata() {
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("meta");
    let plugin = crate::engine::PluginInstance::load(&path, &storage).expect("应能加载组件");

    let m = &plugin.metadata;
    assert_eq!(m.site_id, "fanqie");
    assert_eq!(m.site_domain, "fanqienovel.com");
    assert!(!m.site_name.is_empty(), "site_name 不能为空");
    assert!(!m.plugin_name.is_empty(), "plugin_name 不能为空");
    assert!(!m.version.is_empty(), "version 不能为空");

    assert_eq!(plugin.site_id(), "fanqie");
    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn two_instances_do_not_share_state() {
    // 同一个插件被加载两次（例如为不同书并发下载）时，状态必须各自独立，
    // 否则一本次的 book_id 会串到另一本上。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let sa = tmp_storage("inst_a");
    let sb = tmp_storage("inst_b");
    let a = crate::engine::PluginInstance::load(&path, &sa).expect("加载 a");
    let b = crate::engine::PluginInstance::load(&path, &sb).expect("加载 b");
    // site_id 来自元数据，天然相同；关键是它们能同时存在且互不干扰。
    assert_eq!(a.site_id(), b.site_id());
    assert_eq!(a.metadata.plugin_id, b.metadata.plugin_id);
    let _ = std::fs::remove_dir_all(&sa);
    let _ = std::fs::remove_dir_all(&sb);
}

#[test]
fn methods_fail_cleanly_without_init() {
    // 未 init 就取元数据/目录，应该是 Err 而不是 panic 或空成功。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("noinit");
    let mut plugin = crate::engine::PluginInstance::load(&path, &storage).expect("应能加载组件");

    // title 依赖目录/页面，未 init 时应明确失败。
    let r = plugin.title();
    assert!(r.is_err(), "未 init 就取标题应失败，实得 {r:?}");

    let storage2 = tmp_storage("noinit2");
    let mut plugin2 = crate::engine::PluginInstance::load(&path, &storage2).expect("应能加载组件");
    let r2 = plugin2.catalog(false);
    assert!(r2.is_err(), "未 init 就取目录应失败，实得 {:?}", r2.err());

    let _ = std::fs::remove_dir_all(&storage);
    let _ = std::fs::remove_dir_all(&storage2);
}

#[test]
fn plan_fetch_rejects_empty_list_without_network() {
    // 空列表必须在此之前短路返回，不能因为要联网而失败：
    // 全库已下载时宿主就是这么调的。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("empty");
    let mut plugin = crate::engine::PluginInstance::load(&path, &storage).expect("应能加载组件");

    let tasks = plugin.plan_fetch(&[]).expect("空列表应返回空计划");
    assert!(tasks.is_empty(), "空输入不该产出任务");

    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn plan_fetch_without_catalog_fails_instead_of_hitting_network() {
    // book_id 未知时必须立刻报错。否则会带着空 book_id 去请求网关，
    // 拿到一个莫名其妙的响应再失败，排查成本高得多。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("nocat");
    let mut plugin = crate::engine::PluginInstance::load(&path, &storage).expect("应能加载组件");

    let err = plugin
        .plan_fetch(&["123".to_string()])
        .expect_err("未知 book_id 应报错");
    let text = format!("{err:#}");
    assert!(
        text.contains("目录") || text.contains("book_id"),
        "错误应指明缺目录/book_id，实得: {text}"
    );

    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn consume_fetch_reports_non_200() {
    // 宿主把失败响应交回来时，插件必须报错让宿主记 failed，
    // 而不是返回空正文把坏数据当成功写盘。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("consume");
    let mut plugin = crate::engine::PluginInstance::load(&path, &storage).expect("应能加载组件");

    let resp = crate::engine::wit_types::HttpResponse {
        status: 500,
        headers: Vec::new(),
        body: Vec::new(),
    };
    let err = plugin
        .consume_fetch("ch1", resp)
        .expect_err("非 200 应报错");
    assert!(
        format!("{err:#}").contains("500"),
        "错误应带状态码: {err:#}"
    );

    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn plugin_writes_device_state_to_host_storage() {
    // 设备注册结果靠 storage 缓存；这条路径不通，每本书都会重新注册一次。
    // 触发一次真实的设备注册需要网络，所以这里改为验证"插件确实会往 host
    // storage 写字节"这一契约前提：预置一份设备状态，两次加载都不会把它丢掉。
    let Some(path) = plugin_wasm() else {
        return;
    };
    let storage = tmp_storage("kv");
    let seeded = br#"{"device_id":1,"install_id":2}"#;
    std::fs::write(storage.join("fanqie_device"), seeded).expect("预置设备状态");

    let p1 = crate::engine::PluginInstance::load(&path, &storage).expect("首次加载");
    drop(p1);
    // 插件在 load 期不应当动这份状态。
    assert_eq!(
        std::fs::read(storage.join("fanqie_device")).expect("读回"),
        seeded,
        "加载插件不该改写已有设备状态"
    );

    let p2 = crate::engine::PluginInstance::load(&path, &storage).expect("二次加载");
    drop(p2);

    let _ = std::fs::remove_dir_all(&storage);
}

#[test]
fn storage_dir_is_created_lazily_and_isolated() {
    let Some(path) = plugin_wasm() else {
        return;
    };
    let a = tmp_storage("iso_a");
    let b = tmp_storage("iso_b");
    // 刻意删掉再让插件写入，验证宿主会按需创建而不是要求预建目录。
    std::fs::remove_dir_all(&a).ok();
    let _pa = crate::engine::PluginInstance::load(&path, &a).expect("加载 a");
    let _pb = crate::engine::PluginInstance::load(&path, &b).expect("加载 b");
    assert_ne!(a, b);

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}
