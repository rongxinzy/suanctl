//! 端到端冒烟测试：调用真实编译出的 suanctl 二进制，验证 CLI 主路径可运行。
//!
//! cargo 会为集成测试自动构建二进制并通过 `CARGO_BIN_EXE_suanctl` 提供路径。
//! 断言保持宽松（退出码 + 关键输出字段），不依赖真实 GPU/网络/服务，
//! 保证在无硬件 CI 上也可运行。

use std::process::Command;

fn suanctl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_suanctl"))
}

#[test]
fn help_lists_all_subcommands() {
    let output = suanctl().arg("--help").output().expect("run --help");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in ["tui", "doctor", "report", "p2p", "logs", "config"] {
        assert!(
            text.contains(expected),
            "--help 应包含子命令 {expected}，实际输出：{text}"
        );
    }
}

#[test]
fn logs_json_returns_snapshot_with_sources() {
    let output = suanctl()
        .args(["logs", "--json"])
        .output()
        .expect("run logs --json");
    assert!(
        output.status.success(),
        "logs --json 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("\"sources\""), "应包含 sources 字段");
    assert!(text.contains("\"status\""), "应包含 status 字段");
}

#[test]
fn config_without_file_reports_default_behavior() {
    let output = suanctl().arg("config").output().expect("run config");
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("默认配置"),
        "无配置文件时应提示默认配置：{text}"
    );
}

#[test]
fn doctor_runs_to_completion() {
    let output = suanctl().arg("doctor").output().expect("run doctor");
    assert!(
        output.status.success(),
        "doctor 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("suanctl doctor"), "应输出 doctor 摘要");
}

#[test]
fn p2p_subcommand_runs_without_panicking() {
    let output = suanctl().arg("p2p").output().expect("run p2p");
    assert!(
        output.status.success(),
        "p2p 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn report_json_writes_parseable_file() {
    let dir = std::env::temp_dir().join(format!("suanctl-e2e-report-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("report.json");
    let output = suanctl()
        .args([
            "report",
            "--format",
            "json",
            "--output",
            path.to_str().unwrap(),
            "--force",
        ])
        .output()
        .expect("run report");
    assert!(
        output.status.success(),
        "report 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(&path).expect("report 文件应存在");
    let value: serde_json::Value = serde_json::from_str(&text).expect("报告应为合法 JSON");
    assert!(value.get("snapshot").is_some(), "报告应含 snapshot");
    assert!(value.get("status").is_some(), "报告应含 status");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn doctor_with_plugin_config_runs() {
    let dir = std::env::temp_dir().join(format!("suanctl-e2e-config-{}", std::process::id()));
    let plugins_dir = dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("hello.sh"),
        "#!/bin/sh\necho plugin-ok\nexit 0\n",
    )
    .unwrap();
    let config_path = dir.join("suanctl.toml");
    std::fs::write(
        &config_path,
        format!(
            "[plugins]\nenabled = true\ndir = \"{}\"\n",
            plugins_dir.display()
        ),
    )
    .unwrap();

    let output = suanctl()
        .args([
            "doctor",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .expect("run doctor --config");
    assert!(
        output.status.success(),
        "doctor --config 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(&text).expect("doctor --json 应为合法 JSON");
    let plugins = &value["snapshot"]["platform"]["plugins"];
    assert!(plugins.is_array(), "应含 plugins 数组");
    assert!(
        plugins.as_array().is_some_and(|list| list
            .iter()
            .any(|p| { p["name"] == "hello" && p["probe_status"] == "succeeded" })),
        "hello 插件应成功执行：{plugins}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn remote_subcommand_lists_candidates_without_scanning() {
    // 不指定 --host：只展示候选设备，不连接任何主机（行为契约：不自动扫描全部）。
    let output = suanctl().arg("remote").output().expect("run remote");
    assert!(
        output.status.success(),
        "remote 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("候选设备") && text.contains("--host"),
        "应列出候选并提示 --host：{text}"
    );
}

#[test]
fn remote_subcommand_scans_only_explicit_target() {
    // --host 指定某台才扫描该台；未知别名应输出不可达结果且不 panic。
    let output = suanctl()
        .args(["remote", "--host", "no-such-alias-xyz"])
        .output()
        .expect("run remote");
    assert!(
        output.status.success(),
        "remote --host 退出码应为 0：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("no-such-alias-xyz"), "应输出目标结果：{text}");
}

#[test]
fn save_and_history_persist_across_processes() {
    // SurrealDB 嵌入式库通过 --data-dir 指定；save 与 history 是两次独立进程，
    // 验证真实持久化闭环。
    let dir = std::env::temp_dir().join(format!("suanctl-e2e-store-{}", std::process::id()));
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    let saved = suanctl()
        .args(["save", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .expect("run save");
    assert!(
        saved.status.success(),
        "save 退出码应为 0：{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let saved_text = String::from_utf8_lossy(&saved.stdout);
    assert!(
        saved_text.contains("快照已保存"),
        "应输出保存成功：{saved_text}"
    );

    let history = suanctl()
        .args([
            "history",
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run history");
    assert!(history.status.success(), "history 退出码应为 0");
    let list: serde_json::Value =
        serde_json::from_slice(&history.stdout).expect("history 应为 JSON 数组");
    let entries = list.as_array().expect("JSON 数组");
    assert_eq!(entries.len(), 1, "应有一条快照：{list}");
    assert!(entries[0]["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("snapshots:")));
    assert!(entries[0]["hostname"].as_str().is_some());

    std::fs::remove_dir_all(&dir).unwrap();
}
