//! 交互式网络配置（netplan）：机房场景免手写 YAML。
//!
//! 采集侧只读 `/sys/class/net` 与 `ip -o addr`；只有 `net set` 在显式确认后
//! 才写 `/etc/netplan/60-suanctl-<iface>.yaml` 并执行 `netplan apply`（需 root）。
//! 写入前备份整个 /etc/netplan，apply 失败自动回滚。

use std::{
    collections::BTreeMap,
    fmt, fs,
    net::IpAddr,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug)]
pub enum NetError {
    Io(std::io::Error),
    Command(String),
    Invalid(String),
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IO 失败：{error}"),
            Self::Command(message) => write!(f, "{message}"),
            Self::Invalid(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for NetError {}

impl From<std::io::Error> for NetError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InterfaceInfo {
    pub name: String,
    pub mac: Option<String>,
    /// sysfs operstate（up/down/unknown）。
    pub state: String,
    /// `ip addr` 读到的地址（CIDR，含 IPv6）。
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StaticConfig {
    /// CIDR，如 192.168.1.10/24。
    pub address: String,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NetplanMode {
    Dhcp,
    Static(StaticConfig),
}

/// 网卡清单：/sys/class/net 提供名称/MAC/状态，`ip -o addr` 提供地址。
/// 跳过 lo。
pub fn list_interfaces() -> Result<Vec<InterfaceInfo>, NetError> {
    list_interfaces_with(Path::new("/sys/class/net"), &ip_addr_output()?)
}

fn ip_addr_output() -> Result<String, NetError> {
    let output = Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
        .map_err(|error| NetError::Command(format!("执行 ip addr 失败：{error}")))?;
    if !output.status.success() {
        return Err(NetError::Command(format!(
            "ip addr 退出码 {}：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn list_interfaces_with(
    sys_class_net: &Path,
    ip_output: &str,
) -> Result<Vec<InterfaceInfo>, NetError> {
    let addresses = parse_ip_addr_output(ip_output);
    let mut interfaces = Vec::new();
    for entry in fs::read_dir(sys_class_net)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let dir = sys_class_net.join(&name);
        let mac = fs::read_to_string(dir.join("address"))
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty() && value != "00:00:00:00:00:00");
        let state = fs::read_to_string(dir.join("operstate"))
            .map(|value| value.trim().to_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        interfaces.push(InterfaceInfo {
            addresses: addresses.get(&name).cloned().unwrap_or_default(),
            name,
            mac,
            state,
        });
    }
    interfaces.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(interfaces)
}

/// 解析 `ip -o addr show`：iface → 地址列表（inet/inet6 都收）。
pub fn parse_ip_addr_output(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // 形如：2: eno1    inet 172.18.6.119/24 brd ... scope global eno1
        if fields.len() < 4 || !fields[0].ends_with(':') {
            continue;
        }
        let name = fields[1].trim_end_matches(':').to_owned();
        if matches!(fields[2], "inet" | "inet6") {
            map.entry(name).or_default().push(fields[3].to_owned());
        }
    }
    map
}

/// 校验并归一化 CIDR（如 192.168.1.10/24）。返回去掉空格的原文。
pub fn validate_cidr(value: &str) -> Result<String, NetError> {
    let value = value.trim();
    let (ip, prefix) = value.split_once('/').ok_or_else(|| {
        NetError::Invalid(format!("地址需要 CIDR 格式（如 192.168.1.10/24）：{value}"))
    })?;
    let ip: IpAddr = ip
        .parse()
        .map_err(|_| NetError::Invalid(format!("IP 地址无效：{ip}")))?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| NetError::Invalid(format!("前缀长度无效：{prefix}")))?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    if prefix > max {
        return Err(NetError::Invalid(format!(
            "前缀长度 /{prefix} 超出 {} 上限 /{max}",
            if ip.is_ipv4() { "IPv4" } else { "IPv6" }
        )));
    }
    Ok(value.to_owned())
}

/// 校验单个 IP（网关 / DNS 用，不带前缀）。
pub fn validate_ip(value: &str) -> Result<String, NetError> {
    let value = value.trim();
    value
        .parse::<IpAddr>()
        .map_err(|_| NetError::Invalid(format!("IP 地址无效：{value}")))?;
    Ok(value.to_owned())
}

/// 渲染 netplan v2 YAML（纯函数，便于测试）。
pub fn render_netplan(iface: &str, mode: &NetplanMode) -> String {
    let mut out = format!(
        "# 由 suanctl net set 生成；手动修改请直接编辑本文件\nnetwork:\n  version: 2\n  ethernets:\n    {iface}:\n"
    );
    match mode {
        NetplanMode::Dhcp => {
            out.push_str("      dhcp4: true\n");
        }
        NetplanMode::Static(config) => {
            out.push_str("      addresses:\n");
            out.push_str(&format!("        - {}\n", config.address));
            if let Some(gateway) = &config.gateway {
                out.push_str("      routes:\n        - to: default\n");
                out.push_str(&format!("          via: {gateway}\n"));
            }
            if !config.dns.is_empty() {
                out.push_str("      nameservers:\n        addresses:\n");
                for server in &config.dns {
                    out.push_str(&format!("          - {server}\n"));
                }
            }
        }
    }
    out
}

/// 列出 /etc/netplan 中同样提到该网卡的其它 YAML（可能与本工具生成的配置合并冲突）。
pub fn find_conflicting_files(netplan_dir: &Path, iface: &str) -> Vec<PathBuf> {
    let marker = format!("{iface}:");
    let mut conflicts = Vec::new();
    let Ok(entries) = fs::read_dir(netplan_dir) else {
        return conflicts;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_own = path
            .file_name()
            .is_some_and(|name| name == format!("60-suanctl-{iface}.yaml").as_str());
        let is_yaml = path.extension().is_some_and(|ext| ext == "yaml");
        if is_yaml
            && !is_own
            && fs::read_to_string(&path)
                .map(|content| content.contains(&marker))
                .unwrap_or(false)
        {
            conflicts.push(path);
        }
    }
    conflicts.sort();
    conflicts
}

/// 当前有效用户是否为 root（写 /etc/netplan 与 netplan apply 的前提）。
pub fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
        .unwrap_or(false)
}

/// 把 /etc/netplan 现有 *.yaml 备份到 backup_dir，写入 suanctl 配置并执行
/// `netplan apply`；apply 失败时自动从备份恢复并再次 apply。
/// 返回 (配置文件路径, 备份目录)。
pub fn apply_netplan(
    netplan_dir: &Path,
    backup_dir: &Path,
    iface: &str,
    yaml: &str,
) -> Result<(PathBuf, PathBuf), NetError> {
    fs::create_dir_all(backup_dir)?;
    for entry in fs::read_dir(netplan_dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "yaml") {
            let file_name = path.file_name().expect("read_dir 条目必有文件名");
            fs::copy(&path, backup_dir.join(file_name))?;
        }
    }

    let target = netplan_dir.join(format!("60-suanctl-{iface}.yaml"));
    fs::write(&target, yaml)?;
    // netplan 对全局可读文件只告警不拒绝，仍收紧到 600。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    }

    if let Err(error) = run_netplan_apply() {
        // 回滚：删掉新文件、恢复备份、再 apply。
        let _ = fs::remove_file(&target);
        let mut restored = false;
        if let Ok(entries) = fs::read_dir(backup_dir) {
            for entry in entries.flatten() {
                let backup = entry.path();
                if backup.extension().is_some_and(|ext| ext == "yaml")
                    && fs::copy(&backup, netplan_dir.join(entry.file_name())).is_ok()
                {
                    restored = true;
                }
            }
        }
        let rollback = if restored {
            run_netplan_apply()
                .map(|_| "已自动回滚到备份配置".to_owned())
                .unwrap_or_else(|e| format!("回滚 apply 也失败：{e}，备份在 {backup_dir:?}"))
        } else {
            format!("回滚失败，请手工从 {backup_dir:?} 恢复")
        };
        return Err(NetError::Command(format!(
            "netplan apply 失败：{error}；{rollback}"
        )));
    }
    Ok((target, backup_dir.to_path_buf()))
}

fn run_netplan_apply() -> Result<(), NetError> {
    let output = Command::new("netplan")
        .arg("apply")
        .output()
        .map_err(|error| NetError::Command(format!("执行 netplan 失败：{error}")))?;
    if output.status.success() {
        return Ok(());
    }
    Err(NetError::Command(format!(
        "退出码 {}：{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_cidr_accepts_and_normalizes() {
        assert_eq!(
            validate_cidr(" 192.168.1.10/24 ").ok().as_deref(),
            Some("192.168.1.10/24")
        );
        assert!(validate_cidr("192.168.1.10").is_err());
        assert!(validate_cidr("192.168.1.300/24").is_err());
        assert!(validate_cidr("192.168.1.10/33").is_err());
        assert!(validate_cidr("fe80::1/129").is_err());
        assert!(validate_cidr("fe80::1/64").is_ok());
    }

    #[test]
    fn validate_ip_rejects_garbage() {
        assert!(validate_ip("172.18.6.1").is_ok());
        assert!(validate_ip("114.114.114.114").is_ok());
        assert!(validate_ip("172.18.6.1/24").is_err());
        assert!(validate_ip("abc").is_err());
    }

    #[test]
    fn render_netplan_dhcp_and_static() {
        let dhcp = render_netplan("eno1", &NetplanMode::Dhcp);
        assert!(dhcp.contains("eno1:\n      dhcp4: true"));

        let static_mode = NetplanMode::Static(StaticConfig {
            address: "172.18.6.119/24".to_owned(),
            gateway: Some("172.18.6.1".to_owned()),
            dns: vec!["114.114.114.114".to_owned(), "8.8.8.8".to_owned()],
        });
        let yaml = render_netplan("eno1", &static_mode);
        let expected = [
            "# 由 suanctl net set 生成；手动修改请直接编辑本文件",
            "network:",
            "  version: 2",
            "  ethernets:",
            "    eno1:",
            "      addresses:",
            "        - 172.18.6.119/24",
            "      routes:",
            "        - to: default",
            "          via: 172.18.6.1",
            "      nameservers:",
            "        addresses:",
            "          - 114.114.114.114",
            "          - 8.8.8.8",
        ]
        .join("\n");
        assert_eq!(yaml, format!("{expected}\n"));
    }

    #[test]
    fn parse_ip_addr_output_groups_by_iface() {
        let text = "1: lo    inet 127.0.0.1/8 scope host lo\n\
                    2: eno1    inet 172.18.6.119/24 brd 172.18.6.255 scope global eno1\n\
                    2: eno1    inet6 fe80::1/64 scope link\n\
                    3: eno2    inet6 fe80::2/64 scope link\n";
        let map = parse_ip_addr_output(text);
        assert_eq!(
            map.get("eno1").map(Vec::as_slice),
            Some(&["172.18.6.119/24".to_owned(), "fe80::1/64".to_owned()][..])
        );
        assert_eq!(
            map.get("eno2").map(Vec::as_slice),
            Some(&["fe80::2/64".to_owned()][..])
        );
    }

    #[test]
    fn find_conflicting_files_matches_iface_key() {
        let dir = std::env::temp_dir().join(format!(
            "suanctl-net-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(
            dir.join("00-installer-config.yaml"),
            "network:\n  ethernets:\n    eno1:\n",
        )
        .expect("write");
        fs::write(
            dir.join("60-suanctl-eno1.yaml"),
            "network:\n  ethernets:\n    eno1:\n",
        )
        .expect("write");
        fs::write(
            dir.join("01-other.yaml"),
            "network:\n  ethernets:\n    eno2:\n",
        )
        .expect("write");
        let conflicts = find_conflicting_files(&dir, "eno1");
        assert_eq!(conflicts, vec![dir.join("00-installer-config.yaml")]);
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
