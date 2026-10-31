use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::{
    generate,
    shells::{Bash, Fish, Zsh},
};
use std::io::Write;
use std::path::Path;

use crate::cli::{Cli, HelpFormat};

pub fn write_completion(shell: HelpFormat, output: Option<&Path>) -> Result<()> {
    let mut command = Cli::command();
    let mut buffer = Vec::new();
    match shell {
        HelpFormat::Bash => generate(Bash, &mut command, "ilp", &mut buffer),
        HelpFormat::Zsh => generate(Zsh, &mut command, "ilp", &mut buffer),
        HelpFormat::Fish => generate(Fish, &mut command, "ilp", &mut buffer),
        HelpFormat::Man => {
            let man = clap_mangen::Man::new(command);
            man.render(&mut buffer).context("生成 man page 失败")?;
        }
    }
    write_output(output, &buffer)
}

fn write_output(output: Option<&Path>, data: &[u8]) -> Result<()> {
    match output {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("创建输出目录失败: {}", parent.display()))?;
            }
            std::fs::write(path, data).with_context(|| format!("写入失败: {}", path.display()))?;
        }
        None => std::io::stdout()
            .write_all(data)
            .context("写入标准输出失败")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn generated_bash_completion_contains_commands() {
        let path = std::env::temp_dir().join(format!(
            "ilp-completion-{}.bash",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        write_completion(HelpFormat::Bash, Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("plugin"));
        assert!(text.contains("completion"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generated_man_page_has_name_and_options() {
        let path = std::env::temp_dir().join(format!("ilp-{}.1", std::process::id()));
        write_completion(HelpFormat::Man, Some(&path)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ilp"));
        assert!(text.contains("OPTIONS"));
        let _ = std::fs::remove_file(path);
    }
}
