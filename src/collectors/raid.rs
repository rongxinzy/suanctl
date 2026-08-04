//! Software RAID and vendor RAID/HBA CLI collection.
//!
//! Every vendor command is represented as a fixed provider specification and
//! executed through `CommandRunner` without a shell or privilege escalation.
//! The collector never stores complete command output, serial numbers, or
//! other vendor payloads in the snapshot.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::domain::{
    CollectionIssue, HealthStatus, LocalProbeStatus, SoftwareRaidSnapshot, VendorCliSnapshot,
};

use super::command::{CommandRequest, CommandRunner, DEFAULT_STDERR_LIMIT, DEFAULT_STDOUT_LIMIT};

pub const DEFAULT_VENDOR_TIMEOUT: Duration = Duration::from_secs(3);
pub const DEFAULT_MAX_VENDOR_CONTROLLERS: usize = 32;
pub const DEFAULT_MAX_VENDOR_DISPLAY_CALLS: usize = 16;

#[derive(Debug, Clone, PartialEq)]
pub struct RaidCollection {
    pub software_raid: Vec<SoftwareRaidSnapshot>,
    pub vendor_clis: Vec<VendorCliSnapshot>,
    pub vendor_observations: Vec<VendorControllerObservation>,
    pub issues: Vec<CollectionIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VendorControllerObservation {
    pub provider: String,
    pub controller_id: Option<String>,
    pub model: Option<String>,
    pub firmware_version: Option<String>,
    pub virtual_drive_count: Option<u32>,
    pub physical_drive_count: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct VendorCliCollector<R> {
    runner: R,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
}

impl<R> VendorCliCollector<R> {
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            timeout: DEFAULT_VENDOR_TIMEOUT,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_output_limits(mut self, stdout_limit: usize, stderr_limit: usize) -> Self {
        self.stdout_limit = stdout_limit;
        self.stderr_limit = stderr_limit;
        self
    }
}

impl<R: CommandRunner> VendorCliCollector<R> {
    pub fn collect(
        &self,
    ) -> (
        Vec<VendorCliSnapshot>,
        Vec<VendorControllerObservation>,
        Vec<CollectionIssue>,
    ) {
        let mut snapshots = Vec::new();
        let mut observations = Vec::new();
        let mut issues = Vec::new();
        for provider in provider_specs() {
            let result = self.probe_provider(provider, &mut issues);
            snapshots.push(result.snapshot);
            observations.extend(result.observations);
        }
        (snapshots, observations, issues)
    }

    fn probe_provider(
        &self,
        provider: VendorProviderSpec,
        issues: &mut Vec<CollectionIssue>,
    ) -> VendorProbeResult {
        let mut selected_executable = None;
        let mut output = None;
        for executable in provider.executables.iter().copied() {
            let mut request = CommandRequest::new(executable, provider.args.iter().copied());
            request.timeout = self.timeout;
            request.stdout_limit = self.stdout_limit;
            request.stderr_limit = self.stderr_limit;
            match self.runner.run(&request) {
                Ok(value) if value.timed_out => {
                    issues.push(raid_issue(
                        "vendor_cli_timeout",
                        HealthStatus::Warning,
                        format!("{} 执行超时，结果保持未知", provider.name),
                    ));
                    return VendorProbeResult {
                        snapshot: vendor_snapshot(
                            provider.name,
                            Some(executable.to_owned()),
                            LocalProbeStatus::Failed,
                            false,
                            None,
                            HealthStatus::Warning,
                        ),
                        observations: Vec::new(),
                    };
                }
                Ok(value) if value.stdout_truncated || value.stderr_truncated => {
                    issues.push(raid_issue(
                        "vendor_cli_output_too_large",
                        HealthStatus::Warning,
                        format!("{} 输出超过安全上限，结果保持未知", provider.name),
                    ));
                    return VendorProbeResult {
                        snapshot: vendor_snapshot(
                            provider.name,
                            Some(executable.to_owned()),
                            LocalProbeStatus::Failed,
                            false,
                            None,
                            HealthStatus::Warning,
                        ),
                        observations: Vec::new(),
                    };
                }
                Ok(value) if value.success => {
                    selected_executable = Some(executable.to_owned());
                    output = Some(value.stdout);
                    break;
                }
                Ok(_) => {
                    // The executable exists, but this fixed read-only command
                    // was rejected. Do not try mutating fallbacks.
                    selected_executable = Some(executable.to_owned());
                    output = Some(String::new());
                    break;
                }
                Err(error) if error.code == "spawn_failed" => continue,
                Err(error) => {
                    issues.push(raid_issue(
                        "vendor_cli_failed",
                        HealthStatus::Warning,
                        format!("{} 执行失败：{}", provider.name, error.message),
                    ));
                    selected_executable = Some(executable.to_owned());
                    output = Some(String::new());
                    break;
                }
            }
        }

        let Some(executable) = selected_executable else {
            return VendorProbeResult {
                snapshot: vendor_snapshot(
                    provider.name,
                    None,
                    LocalProbeStatus::Unavailable,
                    false,
                    None,
                    HealthStatus::Unavailable,
                ),
                observations: Vec::new(),
            };
        };
        let output = output.unwrap_or_default();
        let parsed = (provider.parse)(&output);
        let parsed_ok = !parsed.is_empty() || provider.accepts_empty_success;
        if !parsed_ok {
            issues.push(raid_issue(
                "vendor_cli_unparsed",
                HealthStatus::Warning,
                format!("{} 命令成功但没有可解析的控制器信息", provider.name),
            ));
        }
        VendorProbeResult {
            snapshot: vendor_snapshot(
                provider.name,
                Some(executable.to_owned()),
                if parsed_ok {
                    LocalProbeStatus::Succeeded
                } else {
                    LocalProbeStatus::Failed
                },
                parsed_ok,
                (!parsed.is_empty()).then_some(parsed.len() as u32),
                if parsed_ok {
                    HealthStatus::Healthy
                } else {
                    HealthStatus::Warning
                },
            ),
            observations: parsed,
        }
    }
}

#[derive(Debug, Clone)]
struct VendorProbeResult {
    snapshot: VendorCliSnapshot,
    observations: Vec<VendorControllerObservation>,
}

#[derive(Clone, Copy)]
struct VendorProviderSpec {
    name: &'static str,
    executables: &'static [&'static str],
    args: &'static [&'static str],
    parse: fn(&str) -> Vec<VendorControllerObservation>,
    accepts_empty_success: bool,
}

fn provider_specs() -> [VendorProviderSpec; 5] {
    [
        VendorProviderSpec {
            name: "storcli",
            executables: &["storcli64", "storcli"],
            args: &["/call", "show", "J"],
            parse: parse_storcli_json,
            accepts_empty_success: false,
        },
        VendorProviderSpec {
            name: "perccli",
            executables: &["perccli64", "perccli"],
            args: &["/call", "show", "J"],
            parse: parse_storcli_json,
            accepts_empty_success: false,
        },
        VendorProviderSpec {
            name: "sas3ircu",
            executables: &["sas3ircu"],
            args: &["LIST"],
            parse: parse_sas3ircu_list,
            accepts_empty_success: false,
        },
        VendorProviderSpec {
            name: "ssacli",
            executables: &["ssacli"],
            args: &["ctrl", "all", "show", "config", "detail"],
            parse: parse_ssacli_text,
            accepts_empty_success: false,
        },
        VendorProviderSpec {
            name: "arcconf",
            executables: &["arcconf"],
            args: &["getconfig", "1"],
            parse: parse_arcconf_text,
            accepts_empty_success: false,
        },
    ]
}

fn vendor_snapshot(
    provider: &str,
    executable: Option<String>,
    probe_status: LocalProbeStatus,
    parsed: bool,
    controllers_observed: Option<u32>,
    status: HealthStatus,
) -> VendorCliSnapshot {
    VendorCliSnapshot {
        provider: provider.to_owned(),
        executable,
        probe_status,
        parsed,
        controllers_observed,
        status,
    }
}

pub fn collect_software_raid(
    proc_root: &Path,
    sysfs_root: &Path,
) -> (Vec<SoftwareRaidSnapshot>, Vec<CollectionIssue>) {
    let mut issues = Vec::new();
    let mut arrays = parse_mdstat(&match fs::read_to_string(proc_root.join("mdstat")) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            issues.push(raid_issue(
                "mdstat_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {}/mdstat 失败：{error}", proc_root.display()),
            ));
            String::new()
        }
    });

    let block_root = sysfs_root.join("block");
    let entries = match fs::read_dir(&block_root) {
        Ok(entries) => Some(entries),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            issues.push(raid_issue(
                "md_sysfs_unavailable",
                HealthStatus::Unavailable,
                format!("读取 {} 失败：{error}", block_root.display()),
            ));
            None
        }
    };
    if let Some(entries) = entries {
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if !name.starts_with("md") || name.len() <= 2 {
                continue;
            }
            let item = arrays
                .entry(name.clone())
                .or_insert_with(|| SoftwareRaidSnapshot {
                    name: name.clone(),
                    level: None,
                    raid_disks: None,
                    degraded: None,
                    sync_action: None,
                    array_state: None,
                    status: HealthStatus::Unknown,
                });
            item.level = read_md_text(&entry.path(), "level").or(item.level.clone());
            item.raid_disks = read_md_u32(&entry.path(), "raid_disks").or(item.raid_disks);
            item.degraded = read_md_u32(&entry.path(), "degraded").or(item.degraded);
            item.sync_action =
                read_md_text(&entry.path(), "sync_action").or(item.sync_action.clone());
            item.array_state =
                read_md_text(&entry.path(), "array_state").or(item.array_state.clone());
            item.status = software_raid_status(item);
        }
    }
    let mut result = arrays.into_values().collect::<Vec<_>>();
    result.sort_by(|left, right| left.name.cmp(&right.name));
    (result, issues)
}

pub fn parse_mdstat(text: &str) -> BTreeMap<String, SoftwareRaidSnapshot> {
    let mut arrays = BTreeMap::new();
    let lines = text.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else {
            continue;
        };
        if !name.starts_with("md")
            || name.len() <= 2
            || !name[2..].chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        let level = tokens
            .iter()
            .position(|token| *token == "active")
            .and_then(|position| tokens.get(position + 1))
            .map(|value| (*value).to_owned());
        let mut raid_disks = None;
        let mut degraded = None;
        let mut array_state = None;
        for token in tokens.iter().chain(
            lines
                .get(index + 1)
                .into_iter()
                .flat_map(|line| line.split_whitespace())
                .collect::<Vec<_>>()
                .iter(),
        ) {
            if let Some(value) = token
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
            {
                if let Some((total, active)) = value.split_once('/') {
                    raid_disks = total.parse::<u32>().ok();
                    degraded = total
                        .parse::<u32>()
                        .ok()
                        .zip(active.parse::<u32>().ok())
                        .map(|(total, active)| total.saturating_sub(active));
                } else if value
                    .chars()
                    .all(|character| character == 'U' || character == '_')
                {
                    let missing =
                        value.chars().filter(|character| *character == '_').count() as u32;
                    degraded = Some(missing);
                    raid_disks = Some(value.chars().count() as u32);
                    array_state = Some(if missing == 0 { "clean" } else { "degraded" }.to_owned());
                }
            }
        }
        arrays.insert(
            name.to_owned(),
            SoftwareRaidSnapshot {
                name: name.to_owned(),
                level,
                raid_disks,
                degraded,
                sync_action: None,
                array_state,
                status: HealthStatus::Unknown,
            },
        );
    }
    for array in arrays.values_mut() {
        array.status = software_raid_status(array);
    }
    arrays
}

fn read_md_text(path: &Path, name: &str) -> Option<String> {
    fs::read_to_string(path.join("md").join(name))
        .ok()
        .and_then(|value| scalar(&value))
}

fn read_md_u32(path: &Path, name: &str) -> Option<u32> {
    read_md_text(path, name).and_then(|value| value.parse().ok())
}

fn software_raid_status(array: &SoftwareRaidSnapshot) -> HealthStatus {
    if array.degraded.is_some_and(|value| value > 0)
        || array
            .sync_action
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("idle"))
        || array.array_state.as_deref().is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("degraded") || value.contains("fault") || value.contains("recover")
        })
    {
        HealthStatus::Warning
    } else if array.level.is_some() || array.raid_disks.is_some() || array.array_state.is_some() {
        HealthStatus::Healthy
    } else {
        HealthStatus::Unknown
    }
}

pub fn parse_storcli_json(output: &str) -> Vec<VendorControllerObservation> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return Vec::new();
    };
    let mut observations = Vec::new();
    walk_storcli(&value, None, &mut observations);
    dedupe_observations(observations, "storcli")
}

fn walk_storcli(
    value: &Value,
    inherited_id: Option<String>,
    observations: &mut Vec<VendorControllerObservation>,
) {
    match value {
        Value::Object(object) => {
            let id =
                object_string(object, &["Controller", "Controller ID", "Ctl"]).or(inherited_id);
            let model = object_string(object, &["Product Name", "Model", "Model Name", "Name"]);
            let firmware_version = object_string(
                object,
                &["Firmware Version", "FW Version", "Firmware", "Version"],
            );
            let virtual_drive_count =
                object_array_len(object, &["VD LIST", "Virtual Drives", "VD"]);
            let physical_drive_count =
                object_array_len(object, &["PD LIST", "Physical Drives", "PD"]);
            if model.is_some()
                || firmware_version.is_some()
                || virtual_drive_count.is_some()
                || physical_drive_count.is_some()
            {
                observations.push(VendorControllerObservation {
                    provider: "storcli".to_owned(),
                    controller_id: id.clone(),
                    model,
                    firmware_version,
                    virtual_drive_count,
                    physical_drive_count,
                });
            }
            for (key, child) in object {
                if key.eq_ignore_ascii_case("Serial Number") || key.eq_ignore_ascii_case("Serial") {
                    continue;
                }
                walk_storcli(child, id.clone(), observations);
            }
        }
        Value::Array(values) => {
            for child in values {
                walk_storcli(child, inherited_id.clone(), observations);
            }
        }
        _ => {}
    }
}

fn object_string(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        object.iter().find_map(|(key, value)| {
            key.eq_ignore_ascii_case(name).then(|| match value {
                Value::String(value) => scalar(value),
                Value::Number(value) => Some(value.to_string()),
                _ => None,
            })?
        })
    })
}

fn object_array_len(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<u32> {
    names.iter().find_map(|name| {
        object.iter().find_map(|(key, value)| {
            if !key.eq_ignore_ascii_case(name) {
                return None;
            }
            match value {
                Value::Array(values) => Some(values.len().min(u32::MAX as usize) as u32),
                Value::Object(values) => Some(values.len().min(u32::MAX as usize) as u32),
                _ => None,
            }
        })
    })
}

pub fn parse_sas3ircu_list(output: &str) -> Vec<VendorControllerObservation> {
    output
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let id = fields.iter().find(|field| field.parse::<u32>().is_ok())?;
            Some(VendorControllerObservation {
                provider: "sas3ircu".to_owned(),
                controller_id: Some((*id).to_owned()),
                model: fields.get(1).map(|value| (*value).to_owned()),
                firmware_version: None,
                virtual_drive_count: None,
                physical_drive_count: None,
            })
        })
        .take(DEFAULT_MAX_VENDOR_CONTROLLERS)
        .collect()
}

pub fn parse_ssacli_text(output: &str) -> Vec<VendorControllerObservation> {
    parse_controller_text(output, "ssacli")
}

pub fn parse_arcconf_text(output: &str) -> Vec<VendorControllerObservation> {
    parse_controller_text(output, "arcconf")
}

fn parse_controller_text(output: &str, provider: &str) -> Vec<VendorControllerObservation> {
    let mut model = None;
    let mut firmware_version = None;
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if model.is_none() && (lower.contains("controller model") || lower.contains("product name"))
        {
            model = line.split_once(':').and_then(|(_, value)| scalar(value));
        }
        if firmware_version.is_none()
            && (lower.contains("firmware") || lower.contains("firmware version"))
        {
            firmware_version = line.split_once(':').and_then(|(_, value)| scalar(value));
        }
    }
    (model.is_some() || firmware_version.is_some())
        .then(|| VendorControllerObservation {
            provider: provider.to_owned(),
            controller_id: None,
            model,
            firmware_version,
            virtual_drive_count: None,
            physical_drive_count: None,
        })
        .into_iter()
        .collect()
}

fn dedupe_observations(
    observations: Vec<VendorControllerObservation>,
    provider: &str,
) -> Vec<VendorControllerObservation> {
    let mut deduped = Vec::new();
    for mut observation in observations {
        observation.provider = provider.to_owned();
        if !deduped
            .iter()
            .any(|existing: &VendorControllerObservation| {
                existing.controller_id == observation.controller_id
                    && existing.model == observation.model
                    && existing.firmware_version == observation.firmware_version
                    && existing.virtual_drive_count == observation.virtual_drive_count
                    && existing.physical_drive_count == observation.physical_drive_count
            })
        {
            deduped.push(observation);
        }
        if deduped.len() >= DEFAULT_MAX_VENDOR_CONTROLLERS {
            break;
        }
    }
    deduped
}

fn scalar(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn raid_issue(
    code: impl Into<String>,
    status: HealthStatus,
    message: impl Into<String>,
) -> CollectionIssue {
    CollectionIssue {
        collector: "raid".to_owned(),
        code: code.into(),
        status,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_mdstat, parse_storcli_json, VendorCliCollector};
    use crate::collectors::command::{CommandOutput, CommandRequest, CommandRunner};
    use crate::collectors::CollectorError;
    use crate::domain::HealthStatus;

    const HEALTHY_MDSTAT: &str = include_str!("fixtures/mdstat_healthy.txt");
    const DEGRADED_MDSTAT: &str = include_str!("fixtures/mdstat_degraded.txt");
    const STORCLI_BROADCOM: &str = include_str!("fixtures/storcli_broadcom.json");

    #[derive(Clone)]
    struct MissingRunner;

    impl CommandRunner for MissingRunner {
        fn run(&self, _request: &CommandRequest) -> Result<CommandOutput, CollectorError> {
            Err(CollectorError::new(
                "command",
                "spawn_failed",
                "tool missing",
            ))
        }
    }

    #[test]
    fn mdstat_parser_identifies_degraded_array() {
        let arrays = parse_mdstat(DEGRADED_MDSTAT);
        let array = arrays.get("md0").expect("md0");
        assert_eq!(array.level.as_deref(), Some("raid1"));
        assert_eq!(array.raid_disks, Some(2));
        assert_eq!(array.degraded, Some(1));
        assert_eq!(array.status, HealthStatus::Warning);
    }

    #[test]
    fn mdstat_parser_identifies_healthy_array_without_inventing_disk_health() {
        let arrays = parse_mdstat(HEALTHY_MDSTAT);
        let array = arrays.get("md0").expect("md0");
        assert_eq!(array.degraded, Some(0));
        assert_eq!(array.status, HealthStatus::Healthy);
    }

    #[test]
    fn storcli_json_parser_keeps_inventory_but_drops_serial() {
        let observations = parse_storcli_json(STORCLI_BROADCOM);
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].model.as_deref(),
            Some("Broadcom MegaRAID 9560-16i")
        );
        assert_eq!(observations[0].virtual_drive_count, Some(1));
        assert_eq!(observations[0].physical_drive_count, Some(2));
        assert_eq!(
            observations[0].firmware_version.as_deref(),
            Some("5.230.00-1234")
        );
        let serialized = format!("{observations:?}");
        assert!(!serialized.contains("DO-NOT-RETAIN"));
        assert!(!serialized.to_ascii_lowercase().contains("serial"));
    }

    #[test]
    fn missing_vendor_tools_are_capability_unavailable_not_collector_failure() {
        let (snapshots, observations, issues) = VendorCliCollector::new(MissingRunner).collect();
        assert!(observations.is_empty());
        assert!(issues.is_empty());
        assert_eq!(snapshots.len(), 5);
        assert!(snapshots.iter().all(|snapshot| {
            snapshot.probe_status == crate::domain::LocalProbeStatus::Unavailable
                && snapshot.status == HealthStatus::Unavailable
                && snapshot.executable.is_none()
        }));
    }
}
