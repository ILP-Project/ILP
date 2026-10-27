use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ilp", version, about = "小说下载器")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// 获取小说内容
    Get {
        /// 获取类型：chapter/cover/author/title/all
        #[arg(value_name = "kind")]
        kind: String,
        /// 小说 ID
        #[arg(value_name = "id")]
        id: String,
        /// 站点 ID
        #[arg(short = 's', long = "site_id")]
        site_id: String,
        /// 强制更新
        #[arg(short = 'f', long = "force_update")]
        force_update: bool,
    },
    /// 搜索小说
    Search {
        keyword: String,
        #[arg(short = 's', long = "site_id")]
        site_id: String,
    },
    /// 插件管理
    Plugin {
        #[command(subcommand)]
        sub: PluginCommand,
    },
    /// 显示版本与已加载插件信息
    Version,
}

#[derive(Subcommand)]
pub enum PluginCommand {
    /// 列出插件
    List,
    /// 安装插件（name 或 name@version，省略版本即 latest）
    Install { source: String },
    /// 卸载插件
    Uninstall { name: String },
    /// 启用插件
    Enable { name: String },
    /// 禁用插件
    Disable { name: String },
}

pub fn parse() -> Command {
    Cli::parse().command
}
