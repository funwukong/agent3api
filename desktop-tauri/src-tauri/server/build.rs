//! 生成**内嵌界面资源表**（`--features embed-ui` 的单文件分发形态）。
//!
//! ── 它做什么 ────────────────────────────────────────────────
//! 走读 `desktop-tauri/ui/`，把每个文件写成一行
//! `("相对路径", include_bytes!("绝对路径"))`，产出到 `$OUT_DIR/embedded_ui.rs`，
//! 由 `server/src/server/embedded_ui.rs` 用 `include!` 装进来。
//!
//! ── 为什么不用 include_dir / rust-embed ─────────────────────
//! 这里要的只是「一个静态数组 + 一次查表」，几十行就够；引一个第三方 crate
//! 只为这件事，会把它自己的版本兼容面、feature 组合、以及「它怎么处理
//! 隐藏文件」的行为一起带进来。生成的代码是死的，出了问题看一眼就知道。
//!
//! ── 不开 feature 时 ─────────────────────────────────────────
//! 只写一张空表（`&[]`）。这是刻意的：`include!` 在两种构建下都要能编译，
//! 而 Docker 的桩依赖层只 COPY 了 `server/`，那时 `../../ui` 根本不存在 ——
//! 走读它会让构建直接失败。空表让「没有界面」成为正常状态。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! 构建脚本 panic 会把「只是少了个 ui 目录」放大成「编不过」，所以这里
//! 一律用 `let ... else` / `unwrap_or_default` 兜住，失败就退化成空表。

use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap_or_default());
    let target = out_dir.join("embedded_ui.rs");

    // 没开 feature：写空表走人（绝大多数构建走这条）
    if env::var_os("CARGO_FEATURE_EMBED_UI").is_none() {
        let _ = std::fs::write(&target, EMPTY_TABLE);
        return;
    }

    // build.rs 的工作目录是 crate 根（server/），ui 在 ../../ui
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let ui_root = manifest.join("../../ui");
    println!("cargo:rerun-if-changed={}", ui_root.display());
    if !ui_root.is_dir() {
        println!(
            "cargo:warning=embed-ui 已开启，但找不到界面目录 {} —— 内嵌表为空，面板将不可用",
            ui_root.display()
        );
        let _ = std::fs::write(&target, EMPTY_TABLE);
        return;
    }

    let mut files: Vec<String> = Vec::new();
    collect(&ui_root, &ui_root, &mut files);
    files.sort();
    if files.is_empty() {
        let _ = std::fs::write(&target, EMPTY_TABLE);
        return;
    }

    let mut source = String::with_capacity(files.len() * 96 + 128);
    source.push_str("// 由 build.rs 生成，不要手改（见 server/build.rs）。\n");
    source.push_str("pub static FILES: &[(&str, &[u8])] = &[\n");
    for relative in &files {
        // include_bytes! 的路径按字面解析：统一成正斜杠，Windows 反斜杠会被
        // 当成转义序列吃掉
        let absolute = ui_root.join(relative).to_string_lossy().replace('\\', "/");
        let _ = writeln!(
            source,
            "    (\"{}\", include_bytes!(\"{}\")),",
            relative, absolute
        );
    }
    source.push_str("];\n");
    let _ = std::fs::write(&target, source);
}

/// 空表（未开启 feature / 目录缺失 / 目录为空三种情况共用）
const EMPTY_TABLE: &str = "// 由 build.rs 生成（未内嵌界面）。\npub static FILES: &[(&str, &[u8])] = &[];\n";

/// 递归收集 `dir` 下的文件，写成相对 `root` 的正斜杠路径。
fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            if let Some(text) = relative.to_str() {
                out.push(text.replace('\\', "/"));
            }
        }
    }
}
