# suanctl

面向 Linux 智算服务器的中文优先诊断、监控与证据导出工具。终端界面基于 Ratatui。

## 能力范围

- 主机、NVIDIA GPU、PCIe 链路、IOMMU、ACS、驱动与 CUDA 状态。
- PCIe Switch、RAID/HBA、SAS PHY、mdraid 采集。
- llama.cpp、vLLM、SGLang 的宿主机进程、Docker、Podman、nerdctl 与显式端点发现。
- GPU P2P 驱动能力矩阵、拓扑路径与 NVBandwidth 实测速率。
- JSON、JSONL、Markdown 证据报告。

默认采集路径使用 Linux sysfs、procfs、固定只读命令和 GET 探针。P2P 实测由显式操作触发。

## 快速开始

```bash
cargo build --release --offline
./target/release/suanctl tui
```

开发环境可使用 Makefile：

```bash
make demo
make doctor
make p2p
make verify
```

## TUI 操作

| 快捷键 | 操作 |
| --- | --- |
| `1`-`5`、方向键 | 页面导航 |
| `r` | 采集更新 |
| `b` | P2P 测速 |
| `m` | 导出格式：JSON / JSONL / Markdown |
| `e` | 报告导出 |
| `?` | 帮助 |
| `q` | 退出 |

## 命令行

```bash
suanctl doctor
suanctl doctor --json
suanctl p2p
suanctl p2p --benchmark
suanctl report --format markdown --output suanctl-report.md
```

`p2p --benchmark`、`doctor --p2p-benchmark` 和 `report --p2p-benchmark` 执行 NVBandwidth GPU 负载。

## 验证

```bash
make verify
```

## 许可证

[MIT](LICENSE)
