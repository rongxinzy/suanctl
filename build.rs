//! suanctl 构建脚本：探测 CUDA 工具链并条件编译内置测速器。
//!
//! - 探测到 nvcc 与 libcudart → 把内置 P2P 测速器（CUDA Samples
//!   p2pBandwidthLatencyTest 改造）编译为**独立可执行**并嵌入 OUT_DIR，
//!   设置 cfg `suanctl_builtin_p2p`。主二进制**不链接任何 CUDA 库**：
//!   测速器在运行时解包为临时文件后以子进程执行（动态链系统 libcudart，
//!   GPU 机器自带）。
//! - 探测到 nccl.h 与 libnccl → 内置 NCCL all_reduce 基准同理编译为独立可执行，
//!   设置 cfg `suanctl_builtin_nccl`。
//! - 工具链缺失时静默跳过：构建照常成功，运行时相应能力报告 Unavailable。
//!   因此 suanctl 在无 GPU/CUDA 的机器上依然可以构建与使用（主二进制零 CUDA 依赖）。

use std::path::{Path, PathBuf};
use std::process::Command;

const P2P_DIR: &str = "third_party/p2p_test";
const P2P_SOURCE: &str = "third_party/p2p_test/p2pBandwidthLatencyTest.cu";
const P2P_BIN: &str = "suanctl_p2p_test_bin";
const NCCL_DIR: &str = "third_party/nccl_test";
const NCCL_SOURCE: &str = "third_party/nccl_test/nccl_allreduce_test.cu";
const NCCL_BIN: &str = "suanctl_nccl_test_bin";

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    println!("cargo:rerun-if-changed={P2P_SOURCE}");
    println!("cargo:rerun-if-changed={NCCL_SOURCE}");
    println!("cargo:rerun-if-env-changed=PATH");

    // P2P：需要 nvcc（cudart 头随 toolkit 提供；可执行动态链系统 libcudart）。
    if let Some(nvcc) = find_nvcc() {
        if compile_cuda_binary(&nvcc, P2P_DIR, P2P_SOURCE, P2P_BIN, &out_dir, None) {
            println!("cargo:rustc-cfg=suanctl_builtin_p2p");
        }
    }

    // NCCL：需要 nccl.h + libnccl（动态链系统 libnccl）。
    if find_nccl() {
        if let Some(nvcc) = find_nvcc() {
            let cuda_lib = find_cuda_library_dir(&cuda_root_of(&nvcc));
            if compile_cuda_binary(
                &nvcc,
                NCCL_DIR,
                NCCL_SOURCE,
                NCCL_BIN,
                &out_dir,
                cuda_lib.as_deref(),
            ) {
                println!("cargo:rustc-cfg=suanctl_builtin_nccl");
            }
        }
    }
}

/// 用 nvcc 把单个 .cu 编译为**独立可执行**（动态链系统 CUDA 库），写入 OUT_DIR。
/// 主二进制不链接任何 CUDA 库（测速器运行时解包为子进程执行）。
/// 失败时打印警告并返回 false（构建继续）。
fn compile_cuda_binary(
    nvcc: &Path,
    include_dir: &str,
    source: &str,
    bin_name: &str,
    out_dir: &Path,
    cuda_lib: Option<&Path>,
) -> bool {
    let output_bin = out_dir.join(bin_name);
    let mut command = Command::new(nvcc);
    command
        .args(["-O2", "-arch=native", "-I", include_dir])
        .arg(source)
        .args(["-o"])
        .arg(&output_bin);
    if let Some(cuda_lib) = cuda_lib {
        // NCCL：需要显式 -L libnccl；cudart 由 nvcc 默认 -lcudart 提供。
        command.args(["-L"]).arg(cuda_lib).args(["-lnccl"]);
    }
    let output = command.output();
    match output {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            eprintln!(
                "suanctl: nvcc 编译 {source} 失败（内置测速器不可用）：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            false
        }
        Err(error) => {
            eprintln!("suanctl: 无法运行 nvcc：{error}");
            false
        }
    }
}

/// 在 PATH 与常见安装位置查找 nvcc。
fn find_nvcc() -> Option<PathBuf> {
    if Command::new("nvcc").arg("--version").output().is_ok() {
        // PATH 中有 nvcc：用 `which` 获取真实路径以推导 CUDA 根目录。
        if let Ok(output) = Command::new("sh").args(["-c", "command -v nvcc"]).output() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    for candidate in [
        "/usr/local/cuda/bin/nvcc",
        "/usr/local/cuda-13.2/bin/nvcc",
        "/usr/local/cuda-13.1/bin/nvcc",
        "/usr/local/cuda-13.0/bin/nvcc",
        "/usr/local/cuda-12.4/bin/nvcc",
        "/usr/local/cuda-12.3/bin/nvcc",
        "/usr/local/cuda-12.2/bin/nvcc",
        "/usr/local/cuda-12.1/bin/nvcc",
        "/usr/local/cuda-12.0/bin/nvcc",
        "/usr/local/cuda-11.8/bin/nvcc",
    ] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    // 兜底：扫描 /usr/local 下的 cuda* 目录
    if let Ok(entries) = std::fs::read_dir("/usr/local") {
        let mut matches: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("cuda"))
            })
            .collect();
        matches.sort();
        for root in matches.into_iter().rev() {
            let candidate = root.join("bin/nvcc");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn cuda_root_of(nvcc: &Path) -> PathBuf {
    // nvcc 路径 .../bin/nvcc → CUDA 根目录为 ../..
    nvcc.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/usr/local/cuda"))
}

fn find_cuda_library_dir(cuda_root: &Path) -> Option<PathBuf> {
    [
        cuda_root.join("lib64"),
        cuda_root.join("lib"),
        PathBuf::from("/usr/lib/x86_64-linux-gnu"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("libcudart.so").exists())
}

/// 用 nvcc 编译单个 .cu 为静态库；失败时打印警告并返回 false（构建继续）。
/// 分离编译路径：`nvcc -c`（主机+设备代码）→ `nvcc -dlink`（设备链接）→ ar。
fn find_nccl() -> bool {
    let headers = [
        "/usr/include/nccl.h",
        "/usr/local/include/nccl.h",
        "/usr/include/x86_64-linux-gnu/nccl.h",
    ];
    let libs = [
        "/usr/lib/x86_64-linux-gnu/libnccl.so",
        "/usr/lib/libnccl.so",
        "/usr/local/lib/libnccl.so",
    ];
    headers.iter().any(|path| Path::new(path).is_file())
        && libs.iter().any(|path| Path::new(path).exists())
}
