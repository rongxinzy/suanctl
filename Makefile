# suanctl 开发入口。默认所有目标均为只读诊断；P2P 压测必须显式调用。

CARGO ?= cargo
CARGO_FLAGS ?= --offline
REPORT_FORMAT ?= markdown

.DEFAULT_GOAL := help

.PHONY: help fmt fmt-check check test clippy verify build build-full build-lite tui demo doctor doctor-json p2p p2p-benchmark report dist dist-full dist-lite dist-musl cuda-testers clean

DIST_VERSION ?= 0.1.0
DIST_NAME := suanctl-$(DIST_VERSION)-$(shell uname -s)-$(shell uname -m)
P2P_TESTER_NAME := suanctl-p2p-test

help:
	@printf '%s\n' \
		'可用目标：' \
		'  make fmt              格式化 Rust 代码' \
		'  make fmt-check        校验格式' \
		'  make check            快速编译检查（离线）' \
		'  make test             运行单元测试（离线）' \
		'  make clippy           运行严格 Clippy（离线）' \
		'  make verify           fmt-check + test + clippy' \
		'  make build            编译 release 二进制（离线，CUDA 测速器自动探测）' \
		'  make build-full       编译完整版（强制内置 CUDA P2P/NCCL 测速器，需 nvcc）' \
		'  make build-lite       编译轻量版（不内置任何 CUDA 测速器）' \
		'  make tui              启动真实主机中文 TUI' \
		'  make demo             启动演示 TUI' \
		'  make doctor           输出诊断摘要' \
		'  make doctor-json      输出诊断 JSON' \
		'  make p2p              仅采集 P2P 能力/拓扑，不运行负载' \
		'  make p2p-benchmark    显式运行 NVBandwidth GPU 负载' \
		'  make report REPORT_OUTPUT=/path/report.md [REPORT_FORMAT=markdown]' \
		'  make dist             打包（测速器自动探测）dist/$(DIST_NAME).tar.gz' \
		'  make dist-full        打包完整版 dist/$(DIST_NAME)-full.tar.gz（测速器已嵌入二进制）' \
		'  make dist-lite        打包轻量版 dist/$(DIST_NAME)-lite.tar.gz（仅本体）' \
		'  make dist-musl        打包 musl 静态版（部署到 glibc 较老的机器）' \
		'  make cuda-testers     单独编译 CUDA 测速器到 dist/cuda-testers/（需 nvcc）' \
		'  make clean            清理 Cargo 构建产物' \
		'' \
		'轻量版补齐 P2P 实测能力：把 make cuda-testers 的产物 $(P2P_TESTER_NAME)' \
		'放到以下任一位置，suanctl 运行时自动发现并调用：' \
		'  1. 环境变量 SUANCTL_P2P_TEST_BIN=/path/to/$(P2P_TESTER_NAME)' \
		'  2. suanctl 主程序同目录' \
		'  3. ~/.suanctl/bin/$(P2P_TESTER_NAME)'

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

check:
	$(CARGO) check $(CARGO_FLAGS)

test:
	$(CARGO) test $(CARGO_FLAGS) -- --test-threads=4

clippy:
	$(CARGO) clippy --all-targets $(CARGO_FLAGS) -- -D warnings

verify: fmt-check test clippy

build:
	$(CARGO) build --release $(CARGO_FLAGS)

# 完整版：强制内置 CUDA 测速器；构建机无 nvcc 时直接失败（不静默降级）。
build-full:
	SUANCTL_BUILTIN_TESTERS=1 $(CARGO) build --release $(CARGO_FLAGS)

# 轻量版：不内置任何 CUDA 测速器；运行时可发现外部测速器补齐（见 make help 尾部）。
build-lite:
	SUANCTL_BUILTIN_TESTERS=0 $(CARGO) build --release $(CARGO_FLAGS)

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

# $(1)=发行目录名 $(2)=二进制路径
define package_dist
	rm -rf dist/$(1) dist/$(1).tar.gz
	mkdir -p dist/$(1)
	cp $(2) dist/$(1)/
	cp README.md LICENSE dist/$(1)/
	cp docs/ROADMAP.md dist/$(1)/ 2>/dev/null || true
	cp -r examples dist/$(1)/examples
	cd dist && tar -czf $(1).tar.gz $(1)
	@echo "打包完成：dist/$(1).tar.gz"
endef

# 打包发布：release 二进制 + 文档 + 示例插件，输出 dist/$(DIST_NAME).tar.gz
dist:
	$(CARGO) build --release $(CARGO_FLAGS)
	$(call package_dist,$(DIST_NAME),target/release/suanctl)

# 完整版：CUDA P2P/NCCL 测速器已嵌入二进制，目标机无需任何附带文件。
dist-full:
	SUANCTL_BUILTIN_TESTERS=1 $(CARGO) build --release $(CARGO_FLAGS)
	$(call package_dist,$(DIST_NAME)-full,target/release/suanctl)

# 轻量版：仅本体。补齐 P2P 实测：make cuda-testers 后把 suanctl-p2p-test 放到
# 主程序同目录 / ~/.suanctl/bin/，或设置 SUANCTL_P2P_TEST_BIN（见 make help）。
dist-lite:
	SUANCTL_BUILTIN_TESTERS=0 $(CARGO) build --release $(CARGO_FLAGS)
	$(call package_dist,$(DIST_NAME)-lite,target/release/suanctl)

# 静态链接 musl 构建（适合部署到 glibc 较老的机器）
dist-musl:
	@rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-musl || { echo 'musl target 未安装：rustup target add x86_64-unknown-linux-musl'; exit 2; }
	$(CARGO) build --release --target x86_64-unknown-linux-musl $(CARGO_FLAGS)
	$(call package_dist,$(DIST_NAME)-musl,target/x86_64-unknown-linux-musl/release/suanctl)

# 单独编译 CUDA P2P 测速器（不嵌入主二进制），供轻量版按需分发。
cuda-testers:
	@command -v nvcc >/dev/null 2>&1 || { echo '未找到 nvcc：需要 CUDA 工具链（或设置 PATH 指向 CUDA bin）'; exit 2; }
	mkdir -p dist/cuda-testers
	nvcc -O2 -arch=native -I third_party/p2p_test \
		third_party/p2p_test/p2pBandwidthLatencyTest.cu \
		-o dist/cuda-testers/$(P2P_TESTER_NAME)
	@echo "测速器已编译：dist/cuda-testers/$(P2P_TESTER_NAME)"
	@echo "放置到 suanctl 同目录或 ~/.suanctl/bin/ 即可被轻量版自动发现"
