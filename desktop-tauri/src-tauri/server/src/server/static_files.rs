//! headless 形态的管理界面托管：把 `ui/` 静态目录随网关一并出出去。
//!
//! ── 两种来源：磁盘优先，内嵌兜底 ────────────────────────────
//! 同一个入口服务两种部署形态：
//!   · **磁盘**（Docker / 面板自更新）：`ui/` 目录随包分发，可热替换；
//!   · **内嵌**（`--features embed-ui` 的单文件分发）：整份界面编进可执行
//!     文件，见 [`crate::server::embedded_ui`]。
//!
//! 顺序是**先磁盘后内嵌**，不能反：面板自更新落地的是磁盘上的 `ui/`，
//! 优先内嵌会让「更新界面」失效。旁边的 `ui/` 存在时，行为与只有磁盘
//! 那一套时逐字相同。
//!
//! ── 为什么不用 tower-http 的 ServeDir ───────────────────────
//! 界面是一组固定扩展名的扁平文件，需要的只是「读文件 + content-type +
//! 路径安全」三件事，百来行自持实现足够；引 tower-http 只为这一件事，
//! 还得对齐它的 404 / 过滤器行为 —— 不划算（CORS 不用它也是同一理由）。
//!
//! ── 安全边界 ────────────────────────────────────────────────
//!   · 只服务 `ui/` 目录内的文件：路径逐段归一化，`..` 与绝不允许逃出根；
//!   · `/api`、`/v1`、`/auth`、`/health` 前缀不进静态服务 —— 静态 fallback
//!     只接「未注册路由」，但显式挡一道，避免将来某条管理路由改名后
//!     意外落进静态分支；
//!   · 注入的桥接脚本只在 index.html 上发生，其它原样透出。
//!
//! ── 缓存 ────────────────────────────────────────────────────
//! 一律 `no-store`：面板是小流量场景，协商缓存的复杂度不值得 ——
//! 资源换了名字（部署新 ui/）后浏览器立刻拿到新版，不会出现
//! 「发新版后页面引用了旧 js」的中间态。

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{header, Request, Response, StatusCode};
use axum::response::Response as AxumResponse;

use crate::server::embedded_ui;
use crate::server::errors;

/// 判定路径是否属于「接口保留前缀」：静态服务绝不碰它们，
/// 未注册的接口路径继续走原来的 404 文案（Node 版行为）。
fn is_api_path(path: &str) -> bool {
    path == "/health"
        || path.starts_with("/api/")
        || path.starts_with("/v1/")
        || path.starts_with("/auth/")
        || path == "/api"
        || path == "/v1"
        || path == "/auth"
}

/// 静态服务入口：磁盘目录 → 内嵌表 → 404 兜底。
pub async fn serve(request: Request<Body>, ui_dir: PathBuf) -> AxumResponse {
    let path = request.uri().path().to_string();
    if is_api_path(&path) {
        return not_found(request).await;
    }
    // 归一化失败（试图逃出根）与「文件不存在」走同一个 404 —— 不区分两者
    // 是有意的：回一个 403 等于告诉探测者「这个路径形态被识别了」。
    let Some(segments) = normalize(&path) else {
        return not_found(request).await;
    };
    // ① 磁盘：可热替换，也是面板自更新的落点
    if let Some(file_path) = resolve_disk(&ui_dir, &segments) {
        return read_response(&file_path).await;
    }
    // ② 内嵌：单文件分发形态（未开启 feature 时表是空的，直接落到 404）
    if let Some((name, bytes)) = resolve_embedded(&segments) {
        return respond(name, bytes.to_vec());
    }
    not_found(request).await
}

/// URL 路径 → 归一化的路径段（`None` = 试图逃出根，按 404 处理）。
///
/// 逐段归一化而不是字符串拼接：`..`、内嵌点段、重复斜杠都被消化掉，
/// 归一化结果仍带 `..` 前缀的（试图逃出根）一律拒绝。
fn normalize(url_path: &str) -> Option<Vec<String>> {
    // 只取路径段，忽略查询串（axum 的 uri().path() 已不含 query，双保险）
    let url_path = url_path.split('?').next().unwrap_or("");
    let mut segments: Vec<String> = Vec::new();
    for segment in url_path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            other => {
                // 分段后不可能再含 `/`，但反斜杠（Windows 分隔符）可能被塞进来
                if other.contains('\\') {
                    return None;
                }
                segments.push(other.to_string());
            }
        }
    }
    Some(segments)
}

/// 归一化路径段 → `ui/` 内的具体文件（`None` = 不存在）。
fn resolve_disk(ui_dir: &Path, segments: &[String]) -> Option<PathBuf> {
    let mut file_path = ui_dir.to_path_buf();
    if segments.is_empty() {
        file_path.push("index.html");
        return file_path.is_file().then_some(file_path);
    }
    for segment in segments {
        file_path.push(segment);
    }
    if file_path.is_file() {
        return Some(file_path);
    }
    // 无扩展名的路径按页面补 .html（/login → login.html）：
    // 面板的独立登录页就是这种形态；带扩展名的请求不补（/app.js 照字面找）
    if has_no_extension(segments) {
        file_path.set_extension("html");
        return file_path.is_file().then_some(file_path);
    }
    None
}

/// 归一化路径段 → 内嵌资源（`None` = 没编进来）。
fn resolve_embedded(segments: &[String]) -> Option<(&'static str, &'static [u8])> {
    let key = if segments.is_empty() {
        "index.html".to_string()
    } else {
        segments.join("/")
    };
    if let Some(hit) = embedded_ui::lookup(&key) {
        return Some(hit);
    }
    // 与磁盘同一条规则：无扩展名按页面补 .html
    if has_no_extension(segments) {
        return embedded_ui::lookup(&format!("{key}.html"));
    }
    None
}

/// 末段是否「没有扩展名」（空路径不算 —— 它已经映射到 index.html 了）
fn has_no_extension(segments: &[String]) -> bool {
    segments
        .last()
        .is_some_and(|segment| !segment.contains('.'))
}

/// 读磁盘文件并组装响应；读失败（权限等）按 404 处理（不区分 404/403，
/// 面板资源缺失的唯一正确动作就是「当它不存在」）。
async fn read_response(file_path: &Path) -> AxumResponse {
    let bytes = match tokio::fs::read(file_path).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return errors::not_found_response("GET", "static");
        }
    };
    let name = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    respond(name, bytes)
}

/// 组装响应（磁盘与内嵌共用）：index.html 注入桥接、按扩展名定 content-type。
fn respond(name: &str, bytes: Vec<u8>) -> AxumResponse {
    let body = if name == "index.html" {
        inject_web_shim(&bytes)
    } else {
        bytes
    };
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_of(name))
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(body));
    response.unwrap_or_else(|_| errors::not_found_response("GET", "static"))
}

/// 把网页端桥接注入 index.html：`<head>` 开标签后插一段内联脚本，
/// 使 `window.workbuddyDesktop` 在所有界面脚本之前就绪
/// （与桌面壳 bridge.rs 的「页面脚本执行前注入」同一时序保证）。
fn inject_web_shim(html: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(html) else {
        return html.to_vec();
    };
    let shim = format!("<script>{}</script>", crate::web_shim::shim_js());
    match text.replacen("<head>", &format!("<head>{shim}"), 1) {
        replaced if replaced != text => replaced.into_bytes(),
        // 没有 <head>（结构被改坏）也别让面板整页失效：追加到末尾兜底
        _ => format!("{text}<script>{}</script>", crate::web_shim::shim_js()).into_bytes(),
    }
}

/// 扩展名 → content-type（界面用到的固定集合，其余按二进制流处理）。
///
/// 入参是**文件名**而不是路径：磁盘与内嵌两条来源都只拿得到名字
/// （内嵌表里存的就是相对路径），用路径的 `extension()` 会把两条来源
/// 分成两套取法。
fn mime_of(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        // PWA 清单（`ui/manifest.webmanifest`）。规范类型是 `application/manifest+json`，
        // 浏览器对 `application/json` 也宽松接受；但**落到下面 `_` 那支的
        // `application/octet-stream` 不是 JSON 类型** —— 清单被判解析失败，
        // 「添加到主屏幕」驱动的显示名 / 主题色 / 图标清单整套读不出来
        // （iOS 那条路走 `apple-touch-icon` + `apple-mobile-web-app-*`，不受影响）。
        "webmanifest" => "application/manifest+json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
    .to_string()
}

async fn not_found(request: Request<Body>) -> AxumResponse {
    let method = request.method().as_str().to_string();
    let path = request.uri().path().to_string();
    errors::not_found_response(&method, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments_of(path: &str) -> Option<Vec<String>> {
        normalize(path)
    }

    #[test]
    fn normalize_flattens_noise_but_keeps_real_segments() {
        assert_eq!(
            segments_of("/a/./b//c.js").unwrap_or_default(),
            vec!["a".to_string(), "b".to_string(), "c.js".to_string()]
        );
        // 查询串不进路径段（双保险：axum 的 path() 本来也不含）
        assert_eq!(
            segments_of("/app.js?v=3").unwrap_or_default(),
            vec!["app.js".to_string()]
        );
        // 根路径 → 空段（由调用方映射到 index.html）
        assert_eq!(segments_of("/").unwrap_or_default(), Vec::<String>::new());
    }

    #[test]
    fn normalize_rejects_escapes() {
        // 逐层退到根再退一层 = 试图逃出根，必须拒绝（不是「当它不存在」那么温和）
        assert!(segments_of("/../Cargo.toml").is_none());
        assert!(segments_of("/a/../../b").is_none());
        // 反斜杠是 Windows 的分隔符，不许借它绕过检查
        assert!(segments_of("/a\\b").is_none());
        // 中间段退回仍在根内是合法的（/a/../b → b）
        assert_eq!(
            segments_of("/a/../b").unwrap_or_default(),
            vec!["b".to_string()]
        );
    }

    #[test]
    fn extensionless_paths_get_the_html_fallback() {
        assert!(has_no_extension(&["login".to_string()]));
        assert!(!has_no_extension(&["app.js".to_string()]));
        // 空段已经映射到 index.html，不该再被当成「无扩展名页面」
        assert!(!has_no_extension(&[]));
    }

    #[test]
    fn mime_table_covers_the_panel_asset_kinds() {
        assert_eq!(mime_of("index.html"), "text/html; charset=utf-8");
        assert_eq!(mime_of("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(mime_of("style.css"), "text/css; charset=utf-8");
        assert_eq!(mime_of("manifest.webmanifest"), "application/manifest+json; charset=utf-8");
        assert_eq!(mime_of("icons/logo.png"), "image/png");
        // 无扩展名 / 未知扩展名按二进制流（不能瞎猜成 text，会让浏览器乱解析）
        assert_eq!(mime_of("README"), "application/octet-stream");
        assert_eq!(mime_of("data.bin"), "application/octet-stream");
    }

    #[test]
    fn embedded_lookup_matches_the_build_feature() {
        let count = embedded_ui::count();
        if cfg!(feature = "embed-ui") {
            // 开了 feature 且界面目录在，表必然非空；为空说明构建时没找到 ui/
            assert!(count > 0, "embed-ui 已开启但内嵌表为空：构建时找不到 desktop-tauri/ui");
        } else {
            // 未开 feature：表为空，静态服务照旧只看磁盘
            assert_eq!(count, 0, "未开启 embed-ui 时内嵌表必须是空的");
            assert!(embedded_ui::lookup("index.html").is_none());
            assert!(resolve_embedded(&["index.html".to_string()]).is_none());
        }
    }
}
