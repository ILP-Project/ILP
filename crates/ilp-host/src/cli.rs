use clap::{Parser, Subcommand};

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum HelpFormat {
    Bash,
    Zsh,
    Fish,
    Man,
}

#[derive(Parser)]
#[command(name = "ilp", version, about = "小说下载器")]
pub struct Cli {
    /// 输出结构化 JSONL 事件，不混入普通进度文本
    #[arg(long, global = true)]
    pub jsonl: bool,
    /// 输出更详细的运行日志
    #[arg(short, long, global = true)]
    pub verbose: bool,
    /// 禁止标准错误上的进度和普通提示（日志文件仍保留）
    #[arg(long, global = true)]
    pub quiet: bool,
    /// 将日志写入指定文件（覆盖配置中的滚动日志文件）
    #[arg(long, global = true, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
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
    /// 检查目录更新；默认只生成变更报告
    Update {
        /// 小说 ID
        #[arg(value_name = "id")]
        id: String,
        /// 站点 ID
        #[arg(short = 's', long = "site_id")]
        site_id: String,
        /// 应用目录变更并下载新增章节
        #[arg(long)]
        apply: bool,
    },
    /// 恢复指定的中断下载任务
    Resume {
        /// 任务 UUID
        #[arg(value_name = "TASK_ID")]
        task_id: uuid::Uuid,
    },
    /// 恢复全部具有持久化请求的中断下载任务
    ResumeAll,
    /// 只读预览旧数据迁移，不创建备份或修改数据
    MigrationPreview,
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
    /// 生成 Shell completion 或 Unix man page
    Completion {
        #[arg(value_enum)]
        shell: HelpFormat,
        #[arg(long, value_name = "PATH")]
        output: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
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
    /// 仅校验本地 WASM 与 metadata，不联网、不安装、不写入信任记录
    Validate {
        /// 本地 WASM 文件路径
        path: std::path::PathBuf,
    },
    /// 安装插件（name 或 name@version，省略版本即 latest）
    Install { source: String },
    /// 检查插件更新；默认只生成报告
    Update {
        /// 插件名
        name: String,
        /// 应用更新并替换本地插件
        #[arg(long)]
        apply: bool,
    },
    /// 卸载插件
    Uninstall { name: String },
    /// 启用插件
    Enable { name: String },
    /// 禁用插件
    Disable { name: String },
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, PluginCommand};
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("命令行参数应能解析")
    }

    #[test]
    fn parses_get_with_short_and_long_options() {
        let cli = parse(&[
            "ilp",
            "--jsonl",
            "--verbose",
            "--quiet",
            "--log-file",
            "/tmp/ilp.log",
            "get",
            "all",
            "book-1",
            "-s",
            "fanqie",
            "-f",
        ]);
        assert!(cli.jsonl);
        assert!(cli.verbose);
        assert!(cli.quiet);
        assert_eq!(
            cli.log_file.as_deref().and_then(|p| p.to_str()),
            Some("/tmp/ilp.log")
        );
        match cli.command {
            Command::Get {
                kind,
                id,
                site_id,
                force_update,
            } => {
                assert_eq!(kind, "all");
                assert_eq!(id, "book-1");
                assert_eq!(site_id, "fanqie");
                assert!(force_update);
            }
            other => panic!("应解析为 get，实际为 {other:?}"),
        }
    }

    #[test]
    fn parses_update_without_apply_by_default() {
        let cli = parse(&["ilp", "update", "book-1", "--site_id", "fanqie"]);
        match cli.command {
            Command::Update { id, site_id, apply } => {
                assert_eq!(id, "book-1");
                assert_eq!(site_id, "fanqie");
                assert!(!apply);
            }
            other => panic!("应解析为 update，实际为 {other:?}"),
        }
    }

    #[test]
    fn parses_plugin_lifecycle_commands() {
        for (args, expected) in [
            (&["ilp", "plugin", "list"][..], "list"),
            (&["ilp", "plugin", "info", "fanqie"][..], "info"),
            (
                &["ilp", "plugin", "validate", "./plugin.wasm"][..],
                "validate",
            ),
            (&["ilp", "plugin", "install", "fanqie@1.2.3"][..], "install"),
            (&["ilp", "plugin", "uninstall", "fanqie"][..], "uninstall"),
            (&["ilp", "plugin", "enable", "fanqie"][..], "enable"),
            (&["ilp", "plugin", "disable", "fanqie"][..], "disable"),
        ] {
            let cli = parse(args);
            let actual = match cli.command {
                Command::Plugin {
                    sub: PluginCommand::List,
                } => "list",
                Command::Plugin {
                    sub: PluginCommand::Info { .. },
                } => "info",
                Command::Plugin {
                    sub: PluginCommand::Validate { .. },
                } => "validate",
                Command::Plugin {
                    sub: PluginCommand::Install { .. },
                } => "install",
                Command::Plugin {
                    sub: PluginCommand::Uninstall { .. },
                } => "uninstall",
                Command::Plugin {
                    sub: PluginCommand::Enable { .. },
                } => "enable",
                Command::Plugin {
                    sub: PluginCommand::Disable { .. },
                } => "disable",
                _ => "other",
            };
            assert_eq!(actual, expected, "参数: {args:?}");
        }
    }

    #[test]
    fn parses_doctor_flags_and_plugin_new_options() {
        let doctor = parse(&[
            "ilp",
            "doctor",
            "--bundle",
            "/tmp/report.tar.gz",
            "--with-manifest",
            "-y",
        ]);
        match doctor.command {
            Command::Doctor {
                bundle,
                with_manifest,
                yes,
            } => {
                assert_eq!(bundle.unwrap().to_str(), Some("/tmp/report.tar.gz"));
                assert!(with_manifest);
                assert!(yes);
            }
            other => panic!("应解析为 doctor，实际为 {other:?}"),
        }

        let plugin = parse(&[
            "ilp",
            "plugin",
            "new",
            "my-site",
            "--site-name",
            "示例站",
            "--site-id",
            "example",
            "--domain",
            "example.com",
            "--dir",
            "/tmp/plugins",
        ]);
        match plugin.command {
            Command::Plugin {
                sub:
                    PluginCommand::New {
                        name,
                        site_name,
                        site_id,
                        domain,
                        dir,
                    },
            } => {
                assert_eq!(name, "my-site");
                assert_eq!(site_name.as_deref(), Some("示例站"));
                assert_eq!(site_id.as_deref(), Some("example"));
                assert_eq!(domain.as_deref(), Some("example.com"));
                assert_eq!(dir.unwrap().to_str(), Some("/tmp/plugins"));
            }
            other => panic!("应解析为 plugin new，实际为 {other:?}"),
        }
    }

    #[test]
    fn parses_completion_command() {
        let cli = parse(&["ilp", "completion", "bash", "--output", "/tmp/ilp.bash"]);
        match cli.command {
            Command::Completion {
                shell: super::HelpFormat::Bash,
                output,
            } => {
                assert_eq!(output.unwrap().to_str(), Some("/tmp/ilp.bash"));
            }
            other => panic!("应解析为 completion，实际为 {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_required_arguments() {
        assert!(Cli::try_parse_from(["ilp", "get", "all", "book-1"]).is_err());
        assert!(Cli::try_parse_from(["ilp", "search", "关键词"]).is_err());
        assert!(Cli::try_parse_from(["ilp", "plugin", "info"]).is_err());
        assert!(Cli::try_parse_from(["ilp", "unknown"]).is_err());
    }
}
