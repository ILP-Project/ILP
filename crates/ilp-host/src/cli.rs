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
    /// 检查运行环境，并可选地打出诊断包
    Doctor {
        /// 生成诊断包到指定路径（不指定则只检查不打包）
        #[arg(long, value_name = "PATH")]
        bundle: Option<std::path::PathBuf>,
        /// 诊断包里附带 manifest 摘要（含书名与章节标题，默认不含）
        #[arg(long)]
        with_manifest: bool,
        /// 跳过打包前的确认，直接生成
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// 显示版本与已加载插件信息
    Version,
}

#[derive(Subcommand)]
pub enum PluginCommand {
    /// 从模板生成新插件骨架
    New {
        /// 插件名（按 Cargo 包名规则，如 my-site），决定目录名
        name: String,
        /// 站点显示名，如「某某小说」
        #[arg(long)]
        site_name: Option<String>,
        /// 站点 ID（与 `ilp get -s` 用的值一致）
        #[arg(long)]
        site_id: Option<String>,
        /// 站点主域名，同时写进 allowed_domains
        #[arg(long)]
        domain: Option<String>,
        /// 父目录，默认当前目录
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// 列出插件
    List,
    /// 显示插件元数据与完整性信息
    Info { name: String },
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
