# suanctl 扩展规划（Roadmap）

> 目标：在"中文优先、只读诊断、证据导出"的定位上，把 suanctl 从"单机一次性诊断工具"
> 演进为"智算服务器日常巡检 + 持续监控 + 告警 + 多机对比"的运维主入口，同时保持
> 只读安全边界与可审计的证据导向。
>
> 状态：规划文档（草案）。每项标注优先级（P0 优先）、工作量（S≤2 人日 / M≤1 周 / L>1 周）。

---

## 0. 现状盘点

### 0.1 已有能力矩阵

| 域 | 能力 | 入口 | 说明 |
|---|---|---|---|
| 主机 | hostname/OS/内核/CPU/内存/负载 | doctor/report/TUI 总览 | LinuxHostCollector |
| GPU | 温度/利用率/显存/功耗/P态/Xid/复位 | doctor/report/TUI GPU 页 | NvidiaSmiCollector |
| PCIe | 链路代际/宽度/带宽上限/ACS/IOMMU | 总览摘要 + 诊断 | LinuxPcieCollector |
| 驱动/CUDA | 模块/版本匹配/toolkit/库存在性 | 总览摘要 + 诊断 | platform.rs |
| 存储 | RAID/HBA/SAS PHY/mdraid/storcli | 诊断 | storage.rs |
| 服务发现 | llama.cpp/vLLM/SGLang 进程/容器/显式端点 + HTTP 探针 | TUI 服务页 | engines/ |
| P2P | 驱动能力矩阵 + NVBandwidth 实测（显式触发） | GPU 页 + p2p CLI | p2p.rs |
| 日志 | dmesg/journalctl/syslog 尾部 + 异常模式检测（Xid/NVRM/AER/ECC） | TUI 日志页 + logs CLI | logs.rs（已炼化） |
| 诊断 | 确定性规则引擎 → DiagnosisFinding | 诊断页 | diagnosis.rs |
| 报告 | JSON / JSONL / Markdown 证据报告 | report CLI / TUI e | storage.rs |
| TUI | 6 页 + 刷新/导出/P2P 操作状态机 | tui | app.rs / ui.rs |

### 0.2 架构分层（现状）

```
CLI (main.rs) ──┬── TUI (app.rs 状态机 / ui.rs 页面)
                ├── doctor / report / logs / p2p
                └── RuntimeCollector (runtime.rs) 装配：
                      host · gpu · platform · logs · engines 发现
                      → DashboardSnapshot → diagnose() → findings
                      → EvidenceReport → JSON/JSONL/Markdown
```

关键约束（现有设计原则，扩展时不得违背）：
1. 只读采集：命令不经 shell、固定白名单、超时与输出上限（`collectors/command.rs`）。
2. 证据导向：快照数据有界（截断/上限），诊断规则确定性、可离线重放。
3. 向后兼容：`DashboardSnapshot` 新字段一律 `#[serde(default)]` 可选。
4. 测试隔离：collector 可注入 fake runner / fixture，测试不读真实系统。

---

## 1. 扩展方向总览

| 方向 | 一句话目标 | 核心价值 |
|---|---|---|
| A 监控深化 | 从"一次性快照"到"持续监控 + 异常触发快照"（继承 llama-test-matrix blackbox 剩余能力） | 捕获偶发故障现场 |
| B 远程多机 | 从"本机"到"批量巡检服务器网段"（SSH 只读） | 覆盖用户真实场景（172.18.5.x 多机） |
| C 历史积累 | 监控数据落盘/入库，支持趋势与基线对比 | 故障前因分析 |
| D 告警通知 | 异常即时通知（webhook/邮件/文件） | 从"人看"到"系统推" |
| E 交互体验 | 配置化、滚动/搜索、插件化采集 | 好用与可定制 |
| F 工程质量 | e2e 测试、fixtures 扩充、CI、打包发布 | 可持续交付 |

依赖关系：A、B 是地基（P0）；C 依赖 A 的数据通道；D 依赖 A 的触发与 C 的上下文；E 是横切；F 全程。

---

## 2. 路线图（三阶段）

```
Phase 1（地基，P0）        Phase 2（监控，P1）          Phase 3（规模化，P2）
─────────────────────    ─────────────────────       ─────────────────────
E1 配置文件 suanctl.toml  A2 持续日志跟随(tail -f)     B2 多机批量对比报告
A1 命令可用性探测矩阵     A3 周期指标循环+落盘        C2 历史趋势查询(TUI)
B1 远程主机只读执行层     C1 事件/快照存储            D2 告警路由/抑制
F1 e2e 骨架 + CI          A4 触发快照(incident)       E3 插件化采集器
                         D1 基础告警(webhook/文件)    F3 打包发布(deb/rpm/静态)
                         E2 TUI 滚动/搜索
                         F2 fixtures 扩充
```

---

## 3. 扩展项详情

### Phase 1（P0：地基）

#### E1 配置文件 `suanctl.toml`（S）✅ 已完成
- **目标**：集中管理远程主机清单、告警规则、采集开关、日志触发正则，CLI 参数可覆盖。
- **落点**：新增 `src/config.rs`；`RuntimeCollector::with_configured_endpoints` 已接受显式端点，顺延为 `with_config`。
- **内容**：`hosts[]`（name/address/user/ssh_key）、`logs.trigger_regex`、`monitor.interval`、`alerts.webhooks[]`。
- **验收**：空配置 = 现状行为；配置可被 CLI 覆盖；`suanctl doctor --config x.toml` 生效。
- **风险**：低。注意配置文件包含 SSH 凭据路径，只引用路径不存私钥。

#### F1 e2e 骨架 + CI（S）✅ 已完成
- **目标**：集成测试真实调用编译出的二进制验证 CLI 主路径；GitHub Actions 全量验证。
- **落点**：`tests/e2e.rs`（`CARGO_BIN_EXE_suanctl` 自动构建）、`.github/workflows/ci.yml`（fmt + clippy -D warnings + test + release）。
- **验收**：无 GPU/无网络的 CI 上 e2e 全部通过；`cargo test` 自动覆盖。

#### A1 命令可用性探测矩阵（S）
- **目标**：doctor 输出一份"本机已具备哪些只读工具"矩阵（nvidia-smi/lspci/storcli/journalctl/dmesg/dcgmi/nvbandwidth…），作为能力基线进入报告。
- **落点**：`collectors/platform.rs` 或新 `collectors/capability.rs`；`PlatformSnapshot` 加 `capabilities`。
- **验收**：报告含工具矩阵；TUI 总览一行摘要。
- **风险**：低。这是后续 A2/A4 条件启用的基础。

#### B1 远程主机只读执行层（M）
- **目标**：把现有"本机只读命令"抽象为可对远程主机执行，统一 `CommandRunner` 接口。
- **设计**：`CommandRunner` trait 现有 `ProcessCommandRunner`；新增 `RemoteCommandRunner`（`ssh -o BatchMode=yes <host> -- <cmd>`，命令仍走白名单+参数固定，禁止 shell 拼接；超时/输出上限复用 `command.rs`）。采集器泛型参数 `R: CommandRunner` 已支持（`LinuxLogCollector<R>`、`LinuxStorageCollector<R>` 等均如此），改造成本低。
- **落点**：`collectors/command.rs` 新增 runner；`config.rs` hosts；CLI `suanctl remote doctor --host k1`。
- **验收**：对 ssh config 中一台可达主机执行 doctor 得到同构报告；不可达主机输出明确 unavailable 而非挂起。
- **风险**：中。SSH 会话复用（ControlMaster）、known_hosts 校验、命令白名单防注入是重点。只读原则：远程执行禁止写命令。

### Phase 2（P1：监控）

#### A2 持续日志跟随（M）
- **目标**：`suanctl monitor` 子命令：`dmesg -wT` / `journalctl -f` / `tail -F` 跟随写入本地证据目录（沿用 llama-test-matrix `start_log_followers` 设计），行级匹配触发正则。
- **落点**：新 `src/monitor/`（logs 跟随 + 事件通道），复用 `collectors/logs.rs` 的 `patterns()` 与截断规则；输出目录 `evidence/<run-id>/logs/*.log`。
- **验收**：`suanctl monitor --duration 10s` 能落盘并报告触发；SIGINT 优雅停止。
- **风险**：低-中。日志文件轮转（logrotate）需处理 `tail -F`；权限（dmesg 需 CAP_SYSLOG）降级为 journalctl/文件源。

#### A3 周期指标循环 + 落盘（M）
- **目标**：nvidia-smi 查询（query-gpu / compute-apps / dmon）、PCIe AER 计数器、`nvidia-smi topo -m` diff 周期采集为 CSV/JSONL（llama-test-matrix `collector.rs` 模式）。
- **落点**：`src/monitor/collectors.rs`；复用 `collectors/gpu.rs`、`logs.rs` 的查询构造。
- **验收**：1s 间隔跑 10s，输出 CSV 行数与字段正确；AER 计数器跨轮累积正确。
- **风险**：中。nvidia-smi 偶发失败/超时不能中断循环（已有错误分支模式）。

#### C1 事件与快照存储（M）🟡 快照部分已完成（suanctl save/history，SurrealDB 嵌入式），事件存储待 monitor 接入
- **目标**：把 A2 触发、A3 异常、诊断降级统一为"事件"（Event {ts, kind, severity, object, message, context}），写入 `evidence/<run-id>/events/events.jsonl`；含触发前后上下文（日志 tail + 指标窗口）。
- **落点**：`src/monitor/events.rs`；`EvidenceReport` 增加事件导入能力（`from_events`）。
- **验收**：monitor 运行后事件 JSONL 可回放；TUI 报告页可见事件数。
- **风险**：中。事件去重/抖动（同模式 60s 冷却，沿用 blackbox cooldown 概念）。

#### A4 触发快照 incident（M）
- **目标**：事件触发时抓取证据包：dmesg tail、nvidia-smi 全家桶、ps/free/vmstat、lspci、日志 tail，打包 tar.gz（llama-test-matrix `snapshot.rs` 炼化）。
- **落点**：`src/monitor/snapshot.rs`；产物 `evidence/<run-id>/incidents/<ts>/` + `.tar.gz`。
- **验收**：模拟触发（注入测试日志行）能产出完整证据包；无 nvidia-smi 的主机自动降级。
- **风险**：中。证据包大小需上限（tail 行数、命令白名单）；与"只读"哲学一致（只读命令，写证据目录）。

#### D1 基础告警（S-M）
- **目标**：事件 → 通知：`webhook`（HTTP POST JSON）/ 本地文件（告警追加）；规则可配置（severity 阈值、冷却）。
- **落点**：`src/monitor/alerts.rs`；复用 `engines/http.rs` 的 HTTP 传输。
- **验收**：monitor 触发 Xid 后 webhook 收到结构化告警；`--alerts-off` 可关闭。
- **风险**：低-中。HTTP 超时/失败不阻塞监控主循环。

#### E2 TUI 滚动 / 搜索（M）✅ 已完成
- **目标**：日志页、诊断页支持 ↑↓/PgUp/PgDn 滚动与 `/` 过滤；解决页面内容超出终端的问题。
- **落点**：`AppState` 加每页 `scroll_offset` 与 `filter`；`ui.rs` 渲染截取窗口。
- **验收**：日志 200 行可滚动查看；过滤关键字高亮匹配行。
- **风险**：低-中。状态机与现有操作弹窗需共存（滚动键在操作非 Idle 时不生效）。

#### F2 fixtures 扩充（S）✅ 已完成
- **目标**：为 monitor/告警/远程路径补充 fixture（日志样例、nvidia-smi 变体、异常触发线），保持"测试不读真实系统"。
- **落点**：`src/collectors/fixtures/`、`src/monitor/fixtures/`。
- **验收**：新增路径均有单测覆盖；`make verify` 无网络/无 GPU 可跑。

### Phase 3（P2：规模化）

#### B2 多机批量对比报告（L）
- **目标**：`suanctl fleet doctor --hosts a,b,c` 并行采集（B1），产出对比矩阵报告（同字段跨机对齐：GPU 数/驱动版本/日志异常/Xid/存储状态），Markdown 表格。
- **落点**：`src/fleet.rs`；`storage.rs` 增加 `render_fleet_markdown`。
- **验收**：5 台机器 <30s 完成；单机失败不阻塞整体；报告含机间差异高亮。
- **风险**：中-高。并行度控制、结果归一化（不同硬件机型不能硬比）。

#### C2 历史趋势查询（M-L）🟡 历史快照列表/回放已完成，趋势曲线待做
- **目标**：`suanctl history`：读取历史事件/指标目录，趋势曲线（温度/利用率/日志异常频次）与基线对比。
- **落点**：`src/history.rs`；TUI 新增"趋势"页（ASCII 曲线）或输出 Markdown/CSV。
- **验收**：对同一 run 目录两次采样能画出趋势；缺失数据不伪造。
- **风险**：中。存储格式需在 C1 定稿（JSONL 目录树优先于引入数据库依赖）。

#### D2 告警路由 / 抑制（M）
- **目标**：多 webhook 路由、按主机/模式抑制（维护窗口）、告警聚合（同源去重升级）。
- **落点**：`src/monitor/alerts.rs` 扩展；配置化。
- **验收**：同一 Xid 模式在冷却窗口内只发一条；维护窗口静默。
- **风险**：中。规则引擎复杂度控制。

#### E3 插件化采集器（L）✅ 已完成（基础版）
- **目标**：`suanctl collect --plugin xxx` 或目录约定 `~/.suanctl/plugins/*.sh`（只读脚本白名单），扩展采集域（IB/RDMA、DPU、电源、网络）。
- **落点**：`collectors/plugin.rs`；输出并入 `DashboardSnapshot.platform.plugins`。
- **验收**：示例插件（sensors、ibstat）开箱即用；插件输出受长度/白名单约束。
- **风险**：中-高。安全边界：插件只读、超时、输出上限；这是"只读哲学"的扩展而非破例。

#### F3 打包发布（S-M）✅ 已完成
- **目标**：`make dist`：release 静态二进制（musl）+ deb/rpm + man 页；CI 产物。
- **落点**：`Makefile`、`.github/workflows/`。
- **验收**：容器内 `cargo build --release --target x86_64-unknown-linux-musl` 产出可运行二进制。

---

## 4. 风险与约束

| 风险 | 说明 | 缓解 |
|---|---|---|
| 只读哲学 vs 持续监控 | monitor 需要写证据目录、保持跟随进程 | 明确"写"仅限 `evidence/<run-id>/`；命令永远只读；文档声明 |
| 远程执行安全 | SSH 凭据、命令注入、不可信主机 | 白名单命令 + 固定参数（复用 command.rs）；BatchMode + known_hosts；禁止 shell；凭据只引用路径 |
| 资源占用 | 跟随进程、指标循环、证据包 | 所有循环可停（stop_flag）；输出有上限；interval 可配置 |
| 权限降级 | dmesg/journalctl 需权限 | 多源回退（A2 已有）；probe_status 如实上报 |
| 数据格式漂移 | C1/C2 依赖事件格式 | Phase 2 先定 schema（JSONL 字段），再写消费者 |
| 范围膨胀 | 插件化/多机易失控 | 每个方向有明确验收；插件白名单而非自由执行 |

---

## 5. 验证策略

- 每阶段结束跑 `make verify`（fmt + test + clippy -D warnings）保持全绿。
- 新增路径全部走 fixture/注入测试，不依赖真实 GPU/网络（现有测试纪律）。
- monitor/fleet 提供 `--duration`/`--dry-run` 便于 CI 冒烟。
- 安全敏感路径（B1 远程、E3 插件）加 `security_review`。

---

## 6. 建议执行顺序（前 4 步）

1. **E1 配置 + A1 能力矩阵**（S+S）：为一切铺路，收益立即可见。
2. **B1 远程执行层**（M）：复用现有 `CommandRunner` 泛型，改造面最小。
3. **A2 日志跟随 + C1 事件存储**（M+M）：把"日志采集能力"从快照升级为监控闭环。
4. **A3 指标循环 + A4 触发快照**（M+M）：形成完整 blackbox 能力（对齐 llama-test-matrix）。
