# suanctl

面向 Linux 智算服务器的中文优先诊断、监控与证据导出工具。终端界面基于 Ratatui。

## 能力范围

- 主机、NVIDIA GPU、燧原 Enflame GCU、PCIe 链路、IOMMU、ACS、驱动与 CUDA 状态。
  若 `nvidia-smi` 不可用，自动尝试其改名体 `querygpu`，再尝试燧原 `efsmi`
  （存在即使用，并在 GPU 快照 `smi_tool` / `vendor` 字段记录实际工具与厂商）；
  远程设备 GPU 摘要同样支持该回退链。Enflame 侧额外采集驱动版本、设备 SN、
  ECC 开关、分类错误计数（SIP/Bus/FW/DTE/DRAM HBM/PCIE/GCU-LARE 等）与
  累计复位次数，并兼容新旧两版 efsmi 输出格式。
- GPU 厂商画像接口（`src/vendors.rs`）：电源状态术语（P 态 / DPM）、GPU 表
  备注列、「平台与健康」厂商段与厂商诊断规则全部收敛到 `VendorProfile`
  trait；NVIDIA / Enflame / Generic 已实现，接入新厂商只需实现该 trait 并注册。
- PCIe 加速器拓扑树：按 sysfs 父链合并共享分支，渲染 GPU/GCU 到根端口的
  上行路径（Markdown 报告 + TUI 总览页），端点标注 NUMA 节点，厂商无关。
- PCIe Switch、RAID/HBA、SAS PHY、mdraid 采集。
- llama.cpp、vLLM、SGLang 的宿主机进程、Docker、Podman、nerdctl 与显式端点发现。
- 系统日志采集与异常检测：`dmesg`、`journalctl`、`/var/log` 常见文件尾部，
  检测 Xid / NVRM / PCIe AER / ECC / NVLink 等异常模式（源自 llama-test-matrix blackbox）。
- 配置化扩展：`suanctl.toml`（远程主机清单、自定义日志异常模式、插件采集）。
- 远程设备扫描：解析 `~/.ssh/config` 免密主机，权限预检测后降级采集（TUI 远程页 / `suanctl remote`）。
- 本地数据存储：SurrealDB 嵌入式历史快照库（`suanctl save` / `suanctl history`）。
- GPU P2P 驱动能力矩阵、拓扑路径与实测速率：**内置 CUDA Samples 的
  p2pBandwidthLatencyTest**（构建时检测到 nvcc + libcudart 即编译进二进制，
  无外部依赖）；未内置时回退 NVBandwidth。Enflame GCU 回退 `efsmi --topo -m`
  拓扑路径矩阵（仅路径，能力字段为未知）。每条链路标注两端 GPU 在 PCIe 树上的
  上行汇聚点（最近公共上游桥），并对跨 NUMA 的 P2P 链路给出诊断告警。
- 内置 NCCL all_reduce 基准（`suanctl nccl`，构建时检测到 nccl.h + libnccl 才编译）。
- JSON、JSONL、Markdown 证据报告。
- 推理端点压测：`suanctl bench` 对 OpenAI 兼容端点（llama.cpp / vLLM / SGLang）
  发并发 /v1/chat/completions 负载，报告吞吐（tok/s）与延迟分位数；端点支持
  配置文件 `[[endpoints]]` 显式声明与自动发现（进程/容器 + 探活）。
- 出厂检测（TUI 出厂检测页）：验收清单逐项打勾，覆盖操作系统 / 内核版本 /
  CPU / 内存（含内存条规格）/ 硬盘（数量、总容量、系统盘、数据盘干净度）/
  RAID（硬 RAID 控制器与 mdraid 降级）/ 网络配置（IP 与 static/DHCP）/
  GPU 数量与健康 / ECC / PCIe 链路宽度 / 驱动一致性 / P2P 能力与实测 /
  IOMMU / ACS / 系统日志 / 诊断发现。

默认采集路径使用 Linux sysfs、procfs、固定只读命令和 GET 探针。P2P/NCCL 实测由
显式操作触发（`p2p --benchmark` / `nccl` / TUI `b` 键）。

### 内置 CUDA 测速器（可选编译）

`build.rs` 构建时探测 CUDA 工具链，条件编译内置测速器，**无 GPU/CUDA 环境构建
不受影响**（相应能力运行时报告不可用）：

- 检测到 `nvcc` + `libcudart` → 编译 `third_party/p2p_test/`（CUDA Samples 13.2
  `p2pBandwidthLatencyTest` 改造）进二进制，`p2p --benchmark` 直接执行内置
  带宽/延迟矩阵测试，不再依赖外部 nvbandwidth。
- 检测到 `nccl.h` + `libnccl` → 编译 `third_party/nccl_test/`（最小 all_reduce
  基准）进二进制，`suanctl nccl` 执行。

### 打包：完整版与轻量版

```bash
make dist-full     # 完整版：测速器强制嵌入二进制（构建机需 nvcc，缺失即报错）
make dist-lite     # 轻量版：仅本体，不内置任何 CUDA 测速器
make cuda-testers  # 单独编译 CUDA 测速器到 dist/cuda-testers/（需 nvcc）
```

轻量版可在事后补齐 P2P 实测能力：把 `make cuda-testers` 的产物
`suanctl-p2p-test` 放到以下任一位置，运行时自动发现（优先级从高到低）：

1. 环境变量 `SUANCTL_P2P_TEST_BIN=/path/to/suanctl-p2p-test`
2. `suanctl` 主程序同目录
3. `~/.suanctl/bin/suanctl-p2p-test`

测速器动态链接系统 `libcudart`，需与目标机的 CUDA runtime 匹配。

## 快速开始

```bash
# 首次构建需联网拉取依赖（含 SurrealDB）；之后可用 --offline
cargo build --release
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
| `1`-`8`、方向键 | 页面导航（总览 / GPU / 服务 / 诊断 / 出厂检测 / 报告 / 日志 / 远程） |
| `↑`/`↓`/`PgUp`/`PgDn`、`Home`/`End`、鼠标滚轮 | 页面滚动（每页独立） |
| `/` | 过滤（日志 / 诊断 / 出厂检测页，Esc 清除后退出） |
| `r` | 采集更新（后台执行，Esc 可取消，UI 不冻结） |
| `b` | P2P 测速 |
| `s` | 远程扫描：从候选列表选择设备后回车（远程页） |
| `m` | 导出格式：JSON / JSONL / Markdown |
| `e` | 报告导出 |
| `Enter`/`Esc` | 关闭操作结果弹窗（5 秒后自动消失） |
| `?` | 帮助 |
| `q` | 退出 |

## 命令行

```bash
suanctl doctor
suanctl doctor --json
suanctl logs
suanctl logs --json
suanctl p2p
suanctl p2p --benchmark
suanctl report --format markdown --output suanctl-report.md
suanctl config
suanctl doctor --config suanctl.toml
suanctl save
suanctl history [--limit 10] [--show <id>] [--json]
suanctl net show                     # 网卡清单 + netplan 配置（只读）
suanctl net set                      # 交互式配置 IP（选网卡 → DHCP/静态 → 预览 → 确认）
suanctl net set eno1 --address 192.168.1.10/24 --gateway 192.168.1.1 --dns 114.114.114.114
suanctl bench --list                 # 列出可压测的推理端点（探活后）
suanctl bench                        # 自动发现第一个可达端点并压测
suanctl bench --endpoint http://127.0.0.1:8080 --prompts 32 --concurrency 8
```

`net set` 生成 `/etc/netplan/60-suanctl-<iface>.yaml` 并执行 `netplan apply`（需 root）；
写入前备份 `/etc/netplan` 到 `~/.suanctl/netplan-backup/<时间戳>/`，apply 失败自动回滚。
`--dry-run` 只打印 YAML，`--yes` 跳过确认（脚本化用）。

`p2p --benchmark`、`doctor --p2p-benchmark` 和 `report --p2p-benchmark` 执行 NVBandwidth GPU 负载。

## 本地数据存储（SurrealDB）

`save` 将一次完整采集快照持久化到本地 SurrealDB（嵌入式 surrealkv 引擎），
`history` 查询历史快照并支持按 id 回放——为趋势/基线对比打基础。

```bash
suanctl save                      # 采集并保存，输出记录 id（如 snapshots:xxx）
suanctl history                   # 最近 10 条（时间/状态/主机/GPU 数）
suanctl history --limit 50        # 最近 50 条
suanctl history --status warning  # 按采集状态过滤（healthy/warning/critical/unavailable/unknown）
suanctl history --host <name>     # 主机名子串匹配
suanctl history --pattern xid     # 日志异常模式过滤（xid/nvrm/pcie_bus_error/…）
suanctl history --log-source dmesg        # 日志来源过滤（dmesg/kern.log/syslog/…）
suanctl history --log-status critical     # 日志异常级别过滤
suanctl history --search OOM     # 日志尾部行内容关键字搜索（大小写不敏感）
suanctl history --gpus 8          # GPU 数量 ≥ 8
suanctl history --since 7d --until 2026-08-01  # 时间范围（7d/24h/30m、2026-08-01、RFC3339、毫秒）
suanctl history --offset 10       # 分页跳过前 N 条
suanctl history --show <id>       # 查看指定快照元信息
suanctl history --show <id> --json  # 导出完整快照 JSON
suanctl log-events                # 跨快照日志异常事件（save 时展开）
suanctl log-events --pattern xid  # 按异常模式过滤
suanctl log-events --stats        # 异常模式统计（出现快照次数/命中合计）
suanctl remote                    # 仅列出 ~/.ssh/config 候选设备（不扫描、不连接）
suanctl remote --host k1          # 只扫描指定某台设备
suanctl remote --host k1 --json   # 指定单台 + 机器可读输出
```

所有过滤条件可组合（AND 语义），值经参数绑定防注入。日志异常匹配在每次 `save` 时展开为
`log_events` 事件表，支持跨快照检索与统计。

## 远程设备扫描

`remote` **不自动扫描全部设备**：不带 `--host` 时仅列出 `~/.ssh/config` 中的候选设备
（只读展示，不建立任何连接）；用户用 `--host <别名>` 显式指定某台，才扫描该台。

指定单台后流程为：**先做权限预检测**（`sudo -n true`），再执行系统级只读采集：
主机/负载信息、nvidia-smi GPU 摘要、内核日志尾部。当前用户无 sudo 权限时提示并
降级（跳过 dmesg 等系统级采集项，结果标记 `[已降级]`），不会采集到一半才报错。

### 远程 Agent（worker 模式）

`remote` 只能执行固定白名单命令；需要**完整本地能力**（内置 CUDA P2P 测速、NCCL、
日志异常检测、本地存储等）时，用 `suanctl agent` 把 suanctl 自身部署到免密主机并
作为 worker 执行：

```bash
suanctl agent wfk8smaster3                     # 部署自身（版本匹配则跳过上传）
suanctl agent wfk8smaster3 --check-sudo        # 部署 + sudo 预检测
suanctl agent wfk8smaster3 doctor --json       # 远程本地模式完整采集（JSON 管道干净）
suanctl agent wfk8smaster3 p2p --benchmark     # 远程内置 CUDA Samples P2P 测速
suanctl agent wfk8smaster3 nccl                # 远程 NCCL all_reduce 基准
suanctl agent wfk8smaster3 --redeploy doctor   # 忽略版本，强制重新上传
```

#### 角色协商（控制端 / 被控端）

控制端与被控端角色**显式指定**（`--role`），不依赖 ssh 免密探测；被控端无决策权、
只执行命令：

```bash
suanctl agent wfk8smaster3 --role controller handshake   # 本端声明控制端（默认）
suanctl agent wfk8smaster3 --role worker handshake        # 本端声明被控端
suanctl identity [--json]                                # 本机身份（握手交换用）
```

握手交换双方身份（hostname / 用户 / 版本 / 系统 / 内置能力）并输出协商结果：
本端为控制端时对端为 worker（无决策权）；本端声明 worker 时提示对方机器如何反向控制。
`--redeploy` 可强制刷新远程 agent（版本号相同但代码更新时使用）。

安全约束：ssh 参数固定（BatchMode + ConnectTimeout + accept-new）；**传输优先走
rsync**（`-az --partial --inplace`，增量/断点续传，大二进制更稳），rsync 不可用时
回退 ssh stdin 管道（均不经远端 shell 拼接）；远程路径固定为
`~/.suanctl/agent/suanctl`；远程命令由本进程 argv 直传。构建时若静态链接 CUDA
runtime（`build.rs` 自动选择），agent 客户端在无 CUDA 机器上也能运行，远程 GPU
机器直接具备内置测速能力。

TUI 的「远程」页（`7` 或方向键进入）展示候选设备列表，按 `s` 进入选择模式
（↑/↓ 或数字 1-9 移动光标），回车后只扫描选中的那台（Esc 取消），异步执行，展示状态、
sudo 可用性、降级标记与采集摘要。

数据目录缺省 `~/.suanctl/data`，可用全局参数 `--data-dir <path>` 覆盖。

## 配置与插件

`--config <path>`（全局参数）加载 `suanctl.toml`；不提供时行为与旧版一致。

```toml
# 追加自定义日志异常模式（与内置 Xid/NVRM/AER/ECC 等合并）
[logs]
extra_patterns = [
  { name = "my_app_error", regex = "my-app.*failed", severity = "critical" },
]

# 启用插件采集：扫描目录下 *.sh 只读脚本（输出 ≤50 行/行 ≤200 字符）
[plugins]
enabled = true
dir = "/etc/suanctl/plugins"   # 缺省 ~/.suanctl/plugins

# 远程主机清单（为远程巡检预留）
[[hosts]]
name = "k1"
address = "172.18.5.123"
user = "root"

# 显式声明推理服务端点（服务发现与 suanctl bench 共用）
[[endpoints]]
name = "本地 llama.cpp"
engine = "llama_cpp"          # llama_cpp / vllm / sglang
url = "http://127.0.0.1:8080"
model = "qwen"                # 可选；缺省查 /v1/models
```

示例插件见 `examples/plugins/sensors.sh`；`suanctl config --config <path>` 可校验并展示配置。

## 验证

```bash
make verify
```

## 许可证

[MIT](LICENSE)
