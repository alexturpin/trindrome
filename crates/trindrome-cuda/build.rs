//! Compiles kernels/search.cu into a fatbin embedded in the crate.
//!
//! nvcc is found through `NVCC`, `CUDA_PATH` or the default Windows install
//! location. On Windows nvcc needs `cl.exe`, which is located through the `cc`
//! crate, so no Developer Prompt is needed. Without a CUDA toolkit the crate
//! still builds and reports the backend as unavailable at run time.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Real machine code for Ampere (RTX 30), Ada (RTX 40) and Blackwell (RTX 50),
/// plus Ampere PTX that newer drivers can JIT for later architectures.
const GENCODE: [&str; 4] = [
    "arch=compute_86,code=sm_86",
    "arch=compute_89,code=sm_89",
    "arch=compute_120,code=sm_120",
    "arch=compute_86,code=compute_86",
];

fn main() {
    println!("cargo::rerun-if-changed=kernels/search.cu");
    println!("cargo::rerun-if-env-changed=NVCC");
    println!("cargo::rerun-if-env-changed=CUDA_PATH");
    println!("cargo::rustc-check-cfg=cfg(trindrome_kernels)");

    let Some(nvcc) = find_nvcc() else {
        println!("cargo::warning=nvcc not found; building without CUDA kernels (set NVCC or CUDA_PATH)");
        return;
    };
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("search.fatbin");

    let mut cmd = Command::new(&nvcc);
    cmd.args(["--fatbin", "-O3", "-std=c++17", "--threads", "0"]);
    for g in GENCODE {
        cmd.args(["-gencode", g]);
    }
    cmd.arg("-o").arg(&out).arg("kernels/search.cu");

    if cfg!(windows) {
        let cl = cc::windows_registry::find_tool("x86_64-pc-windows-msvc", "cl.exe")
            .expect("nvcc on Windows needs MSVC; install the Visual Studio C++ build tools");
        cmd.arg("-ccbin").arg(cl.path().parent().unwrap());
        for (key, value) in cl.get_envs() {
            cmd.env(key, value);
        }
    }

    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("failed to run {}: {e}", nvcc.display()));
    assert!(status.success(), "nvcc failed to compile kernels/search.cu");
    println!("cargo::rustc-cfg=trindrome_kernels");
}

fn find_nvcc() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "nvcc.exe" } else { "nvcc" };
    if let Ok(path) = std::env::var("NVCC") {
        return Some(PathBuf::from(path));
    }
    if let Ok(root) = std::env::var("CUDA_PATH") {
        let candidate = Path::new(&root).join("bin").join(exe);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let defaults: &[&str] = if cfg!(windows) {
        &[r"C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA"]
    } else {
        &["/usr/local"]
    };
    for base in defaults {
        let Ok(entries) = std::fs::read_dir(base) else { continue };
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join("bin").join(exe).exists())
            .collect();
        versions.sort();
        if let Some(latest) = versions.pop() {
            return Some(latest.join("bin").join(exe));
        }
    }
    None
}
