wit_bindgen::generate!({
    world: "novel-plugin",
    path: "../../crates/ilp-plugin-wit/wit/v1",
    pub_export_macro: true,
});

use exports::ilp::plugin::plugin::Guest;
use ilp::plugin::types::{ChapterContent, ChapterEntry, FetchTask, HttpResponse, PluginMetadata, SearchPage};

struct Fixture;

impl Guest for Fixture {
    fn metadata() -> PluginMetadata {
        PluginMetadata {
            abi_version: "0.1.0".into(), site_name: "v1 fixture".into(),
            site_id: "fixture-v1".into(), site_domain: "fixture.example.com".into(),
            plugin_name: "component-v1".into(), plugin_id: "component-v1".into(),
            version: "0.1.0".into(),
        }
    }
    fn init(_: String, _: bool) -> Result<(), String> { Ok(()) }
    fn title() -> Result<String, String> { Ok("fixture title".into()) }
    fn author() -> Result<String, String> { Ok("fixture author".into()) }
    fn cover_url() -> Result<String, String> { Ok(String::new()) }
    fn description() -> Result<String, String> { Ok(String::new()) }
    fn catalog(_: bool) -> Result<Vec<ChapterEntry>, String> { Ok(Vec::new()) }
    fn search(_: String, _: String) -> Result<SearchPage, String> {
        Ok(SearchPage { results: Vec::new(), cursor: String::new(), has_more: false })
    }
    fn plan_fetch(_: Vec<String>) -> Result<Vec<FetchTask>, String> { Ok(Vec::new()) }
    fn consume_fetch(_: String, _: HttpResponse) -> Result<Vec<ChapterContent>, String> { Ok(Vec::new()) }
}

export!(Fixture with_types_in self);
