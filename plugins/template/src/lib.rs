pub mod fixture;

use ilp_plugin_wit::exports::ilp::plugin::plugin::Guest;
use ilp_plugin_wit::ilp::plugin::types::{
    ChapterEntry, ChapterResult, FetchTask, HttpResponse, Permission, PluginMetadata, SearchPage,
};

struct Template;

impl Guest for Template {
    fn metadata() -> PluginMetadata {
        PluginMetadata {
            abi_version: "0.2.0".into(),
            site_name: "@SITE_NAME@".into(),
            site_id: "@SITE_ID@".into(),
            site_domain: "@DOMAIN@".into(),
            plugin_name: "@PLUGIN_NAME@".into(),
            plugin_id: "@PLUGIN_NAME@".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            permissions: vec![Permission::Http, Permission::Clock],
            allowed_domains: vec!["@DOMAIN@".into()],
        }
    }

    fn init(_book_id: String, _force_update: bool) -> Result<(), String> {
        Ok(())
    }
    fn title() -> Result<String, String> {
        Ok("Example title".into())
    }
    fn author() -> Result<String, String> {
        Ok("Example author".into())
    }
    fn cover_url() -> Result<String, String> {
        Ok(String::new())
    }
    fn description() -> Result<String, String> {
        Ok(String::new())
    }
    fn catalog(_force_update: bool) -> Result<Vec<ChapterEntry>, String> {
        Ok(Vec::new())
    }
    fn search(_keyword: String, _cursor: String) -> Result<SearchPage, String> {
        Ok(SearchPage {
            results: Vec::new(),
            cursor: String::new(),
            has_more: false,
        })
    }
    fn plan_fetch(_chapter_ids: Vec<String>) -> Result<Vec<FetchTask>, String> {
        Ok(Vec::new())
    }
    fn consume_fetch(
        _task_id: String,
        _response: HttpResponse,
    ) -> Result<Vec<ChapterResult>, String> {
        Ok(Vec::new())
    }
}

ilp_plugin_wit::export!(Template with_types_in ilp_plugin_wit);
