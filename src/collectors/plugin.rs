//! 用户插件（只读脚本）采集器。
//!
//! 插件是放在插件目录（默认 `~/.suanctl/plugins`，配置可改）中的 `*.sh`
//! 脚本，用于扩展 suanctl 的采集域（如 sensors、ibstat、DPU 状态）。
//! 约束：
//! - 插件必须是只读脚本；文档与校验环节会提醒，suanctl 不承诺沙箱隔离。
//! - 执行走受控 `CommandRunner`（超时 + 输出上限），输出按行截断。
//! - 脚本通过 `bash <path>` 执行，不经过用户的 shell 环境。

use std::path::{Path, PathBuf};

use super::command::{CommandRequest, CommandRunner, ProcessCommandRunner};
use crate::domain::{HealthStatus, LocalProbeStatus, PluginSnapshot};

/// 每个插件最多保留的输出行数。
pub const MAX_PLUGIN_LINES: usize = 50;
/// 插件输出单行最多保留的字符数。
pub const MAX_PLUGIN_LINE_CHARS: usize = 200;

pub trait PluginCollector {
    fn collect_plugins(&self) -> Vec<PluginSnapshot>;
}

/// 显式占位实现：插件未启用时返回空列表。
pub struct UnavailablePluginCollector;

impl PluginCollector for UnavailablePluginCollector {
    fn collect_plugins(&self) -> Vec<PluginSnapshot> {
        Vec::new()
    }
}

pub struct ShellPluginCollector<R = ProcessCommandRunner> {
    runner: R,
    dir: PathBuf,
}

impl ShellPluginCollector<ProcessCommandRunner> {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            runner: ProcessCommandRunner,
            dir,
        }
    }
}

impl<R: CommandRunner> ShellPluginCollector<R> {
    pub fn with_runner(runner: R, dir: PathBuf) -> Self {
        Self { runner, dir }
    }
}

fn plugin_scripts(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut scripts: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().is_some_and(|extension| extension == "sh")
        })
        .collect();
    scripts.sort();
    scripts
}

impl<R: CommandRunner> PluginCollector for ShellPluginCollector<R> {
    fn collect_plugins(&self) -> Vec<PluginSnapshot> {
        if !self.dir.is_dir() {
            return Vec::new();
        }
        plugin_scripts(&self.dir)
            .iter()
            .map(|path| run_plugin(&self.runner, path))
            .collect()
    }
}

fn run_plugin(runner: &dyn CommandRunner, path: &Path) -> PluginSnapshot {
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let request = CommandRequest::new("bash", [path.to_string_lossy().as_ref()]);
    let mut request = request;
    request.timeout = std::time::Duration::from_secs(5);
    let path_text = path.display().to_string();

    match runner.run(&request) {
        Err(_) => PluginSnapshot {
            name,
            path: Some(path_text),
            probe_status: LocalProbeStatus::Unavailable,
            output_tail: Vec::new(),
            truncated: false,
            status: HealthStatus::Unavailable,
        },
        Ok(output) if !output.success => PluginSnapshot {
            name,
            path: Some(path_text),
            probe_status: LocalProbeStatus::Failed,
            output_tail: Vec::new(),
            truncated: false,
            status: HealthStatus::Warning,
        },
        Ok(output) => {
            let lines: Vec<String> = output.stdout.lines().map(truncate_plugin_line).collect();
            let truncated = lines.len() > MAX_PLUGIN_LINES;
            let tail: Vec<String> = lines
                .iter()
                .rev()
                .take(MAX_PLUGIN_LINES)
                .rev()
                .cloned()
                .collect();
            PluginSnapshot {
                name,
                path: Some(path_text),
                probe_status: LocalProbeStatus::Succeeded,
                output_tail: tail,
                truncated,
                status: HealthStatus::Healthy,
            }
        }
    }
}

fn truncate_plugin_line(line: &str) -> String {
    let trimmed = line.trim_end();
    if trimmed.chars().count() > MAX_PLUGIN_LINE_CHARS {
        let mut result: String = trimmed.chars().take(MAX_PLUGIN_LINE_CHARS).collect();
        result.push('…');
        result
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collectors::command::{CommandOutput, CommandRequest, CommandRunner};
    use crate::collectors::CollectorError;

    struct FakeRunner {
        fail_all: bool,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            assert_eq!(request.program, "bash");
            if self.fail_all {
                return Err(CollectorError::unavailable(
                    "command",
                    format!("missing {}", request.program),
                ));
            }
            Ok(CommandOutput {
                success: true,
                exit_code: Some(0),
                stdout: request.args.join(" "),
                stderr: String::new(),
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
            })
        }
    }

    fn fixture_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "suanctl-plugin-test-{suffix}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn collects_sorted_sh_scripts_from_dir() {
        let dir = fixture_dir("sorted");
        std::fs::write(dir.join("z_last.sh"), "#!/bin/sh\necho z\n").unwrap();
        std::fs::write(dir.join("a_first.sh"), "#!/bin/sh\necho a\n").unwrap();
        std::fs::write(dir.join("ignore.txt"), "not a plugin\n").unwrap();

        let snapshots =
            ShellPluginCollector::with_runner(FakeRunner { fail_all: false }, dir.clone())
                .collect_plugins();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].name, "a_first");
        assert_eq!(snapshots[1].name, "z_last");
        assert_eq!(snapshots[0].probe_status, LocalProbeStatus::Succeeded);
        assert_eq!(snapshots[0].status, HealthStatus::Healthy);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_dir_yields_empty_and_failures_are_warning() {
        let missing = PathBuf::from("/nonexistent/suanctl-plugins");
        assert!(
            ShellPluginCollector::with_runner(FakeRunner { fail_all: true }, missing)
                .collect_plugins()
                .is_empty()
        );

        let dir = fixture_dir("fail");
        std::fs::write(dir.join("bad.sh"), "#!/bin/sh\nexit 1\n").unwrap();
        let snapshots =
            ShellPluginCollector::with_runner(FakeRunner { fail_all: true }, dir.clone())
                .collect_plugins();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].probe_status, LocalProbeStatus::Unavailable);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
