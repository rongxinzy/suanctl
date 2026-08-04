# suanctl 开发入口。默认所有目标均为只读诊断；P2P 压测必须显式调用。

CARGO ?= cargo
CARGO_FLAGS ?= --offline
REPORT_FORMAT ?= markdown

.DEFAULT_GOAL := help

.PHONY: help fmt fmt-check check test clippy verify build tui demo doctor doctor-json p2p p2p-benchmark report clean

help:
	@printf '%s\n' \
		'可用目标：' \
		'  make fmt              格式化 Rust 代码' \
		'  make fmt-check        校验格式' \
		'  make check            快速编译检查（离线）' \
		'  make test             运行单元测试（离线）' \
		'  make clippy           运行严格 Clippy（离线）' \
		'  make verify           fmt-check + test + clippy' \
		'  make build            编译 release 二进制（离线）' \
		'  make tui              启动真实主机中文 TUI' \
		'  make demo             启动演示 TUI' \
		'  make doctor           输出诊断摘要' \
		'  make doctor-json      输出诊断 JSON' \
		'  make p2p              仅采集 P2P 能力/拓扑，不运行负载' \
		'  make p2p-benchmark    显式运行 NVBandwidth GPU 负载' \
		'  make report REPORT_OUTPUT=/path/report.md [REPORT_FORMAT=markdown]' \
		'  make clean            清理 Cargo 构建产物'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

check:
	$(CARGO) check $(CARGO_FLAGS)

test:
	$(CARGO) test $(CARGO_FLAGS)

clippy:
	$(CARGO) clippy --all-targets $(CARGO_FLAGS) -- -D warnings

verify: fmt-check test clippy

build:
	$(CARGO) build --release $(CARGO_FLAGS)

tui:
	$(CARGO) run $(CARGO_FLAGS) -- tui

demo:
	$(CARGO) run $(CARGO_FLAGS) -- tui --demo

doctor:
	$(CARGO) run $(CARGO_FLAGS) -- doctor

doctor-json:
	$(CARGO) run $(CARGO_FLAGS) -- doctor --json

p2p:
	$(CARGO) run $(CARGO_FLAGS) -- p2p

# 此目标会实际占用 GPU 并运行 NVBandwidth；不被 verify、tui 或 report 调用。
p2p-benchmark:
	$(CARGO) run $(CARGO_FLAGS) -- p2p --benchmark

report:
	@test -n "$(REPORT_OUTPUT)" || { echo '请提供 REPORT_OUTPUT，例如：make report REPORT_OUTPUT=/tmp/suanctl.md'; exit 2; }
	$(CARGO) run $(CARGO_FLAGS) -- report --format $(REPORT_FORMAT) --output "$(REPORT_OUTPUT)"

clean:
	$(CARGO) clean
