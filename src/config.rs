//! `suanctl.toml` 配置文件解析与校验。
//!
//! 配置是可选的：不提供文件时全部走内置默认，行为与旧版本一致。CLI 参数
//! 的优先级高于配置文件。配置文件只包含清单/规则类数据，不保存私钥等
//! 敏感内容（SSH 私钥只允许引用路径）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::collectors::logs::ConfiguredPattern;
use crate::domain::HealthStatus;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuanctlConfig {
    /// 远程主机清单（为远程巡检预留；当前版本仅校验与展示）。
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
    /// 日志采集扩展。
    #[serde(default)]
    pub logs: LogsConfig,
    /// 插件采集。
    #[serde(default)]
    pub plugins: PluginsConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostConfig {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub user: Option<String>,
    /// SSH 私钥路径；只引用路径，不保存私钥内容。
    #[serde(default)]
    pub ssh_key: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsConfig {
    /// 追加的异常日志模式，与内置 7 个模式（Xid/NVRM/AER/ECC 等）合并。
    #[serde(default)]
    pub extra_patterns: Vec<PatternConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatternConfig {
    /// 稳定模式标识，例如 "my_app_error"。
    pub name: String,
    pub regex: String,
    /// "critical" / "warning" / "healthy" / "unknown"。
    #[serde(default = "default_pattern_severity")]
    pub severity: String,
}

fn default_pattern_severity() -> String {
    "warning".to_owned()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// 是否启用插件采集。
    #[serde(default)]
    pub enabled: bool,
    /// 插件目录；默认 `~/.suanctl/plugins`（存在才扫描）。
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// 配置文件解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub message: String,
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl SuanctlConfig {
    /// 加载配置；`None` 表示不读取文件，返回内置默认。
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let text = std::fs::read_to_string(path).map_err(|error| ConfigError {
            message: format!("无法读取配置文件 {}：{error}", path.display()),
        })?;
        let config: SuanctlConfig = toml::from_str(&text).map_err(|error| ConfigError {
            message: format!("配置文件 {} 解析失败：{error}", path.display()),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// 校验配置内容：主机清单字段、日志模式正则与严重级别。
    pub fn validate(&self) -> Result<(), ConfigError> {
        for host in &self.hosts {
            if host.name.trim().is_empty() {
                return Err(ConfigError {
                    message: "hosts[].name 不能为空".to_owned(),
                });
            }
            if host.address.trim().is_empty() {
                return Err(ConfigError {
                    message: format!("hosts[].address 不能为空（host={}）", host.name),
                });
            }
        }
        for pattern in &self.logs.extra_patterns {
            if pattern.name.trim().is_empty() {
                return Err(ConfigError {
                    message: "logs.extra_patterns[].name 不能为空".to_owned(),
                });
            }
            regex::Regex::new(&pattern.regex).map_err(|error| ConfigError {
                message: format!("logs.extra_patterns[{}] 正则非法：{error}", pattern.name),
            })?;
            parse_severity(&pattern.severity).map_err(|error| ConfigError {
                message: format!("logs.extra_patterns[{}] {}", pattern.name, error.message),
            })?;
        }
        if let Some(dir) = &self.plugins.dir {
            if !dir.is_dir() {
                return Err(ConfigError {
                    message: format!("plugins.dir 不是有效目录：{}", dir.display()),
                });
            }
        }
        Ok(())
    }

    /// 转换为日志采集器可用的附加模式列表。
    pub fn to_log_patterns(&self) -> Result<Vec<ConfiguredPattern>, ConfigError> {
        self.logs
            .extra_patterns
            .iter()
            .map(|pattern| {
                Ok(ConfiguredPattern {
                    name: pattern.name.clone(),
                    regex: pattern.regex.clone(),
                    severity: parse_severity(&pattern.severity)?,
                })
            })
            .collect()
    }
}

fn parse_severity(value: &str) -> Result<HealthStatus, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "critical" => Ok(HealthStatus::Critical),
        "warning" | "warn" => Ok(HealthStatus::Warning),
        "healthy" | "ok" => Ok(HealthStatus::Healthy),
        "unknown" => Ok(HealthStatus::Unknown),
        _ => Err(ConfigError {
            message: format!("未知严重级别：{value}（可选 critical/warning/healthy/unknown）"),
        }),
    }
}

/// 默认插件目录：`~/.suanctl/plugins`。
pub fn default_plugins_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".suanctl").join("plugins"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_loads_without_file() {
        let config = SuanctlConfig::load(None).expect("default config");
        assert!(config.hosts.is_empty());
        assert!(config.logs.extra_patterns.is_empty());
        assert!(!config.plugins.enabled);
    }

    #[test]
    fn parses_full_config_file() {
        let dir =
            std::env::temp_dir().join(format!("suanctl-config-test-full-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("suanctl.toml");
        std::fs::write(
            &path,
            r#"
[logs]
extra_patterns = [
  { name = "my_error", regex = "my-app.*failed", severity = "critical" },
  { name = "noisy", regex = "noisy-line" },
]

[plugins]
enabled = true

[[hosts]]
name = "k1"
address = "172.18.5.123"
user = "root"
"#,
        )
        .unwrap();

        let config = SuanctlConfig::load(Some(&path)).expect("parsed config");
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].name, "k1");
        assert_eq!(config.logs.extra_patterns.len(), 2);
        assert_eq!(config.logs.extra_patterns[0].severity, "critical");
        assert_eq!(config.logs.extra_patterns[1].severity, "warning");
        assert!(config.plugins.enabled);

        let patterns = config.to_log_patterns().expect("log patterns");
        assert_eq!(patterns[0].severity, HealthStatus::Critical);
        assert_eq!(patterns[1].severity, HealthStatus::Warning);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bad_regex_and_severity() {
        let dir =
            std::env::temp_dir().join(format!("suanctl-config-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.toml");
        std::fs::write(
            &path,
            "[logs]\nextra_patterns = [{ name = \"bad\", regex = \"(\" }]\n",
        )
        .unwrap();
        let error = SuanctlConfig::load(Some(&path)).expect_err("bad regex");
        assert!(error.message.contains("正则非法"));

        std::fs::write(
            &path,
            "[logs]\nextra_patterns = [{ name = \"bad\", regex = \"x\", severity = \"fatal\" }]\n",
        )
        .unwrap();
        let error = SuanctlConfig::load(Some(&path)).expect_err("bad severity");
        assert!(error.message.contains("未知严重级别"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
