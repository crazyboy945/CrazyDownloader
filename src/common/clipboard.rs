//! 剪贴板监视：捕获用户复制的下载链接，交给 UI 让用户确认。
//!
//! 设计要点：
//! - 后台线程轮询（默认 700 ms），不阻塞 UI；
//! - 启动时**先把当前剪贴板内容记为"已见过"**，避免程序一打开就弹窗；
//! - 只有"内容变化"才上报，同一条链接不会反复弹；
//! - 识别规则保守：整段像链接才认，或短文本里恰好含一条链接。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// 轮询间隔。太快浪费 CPU，太慢会让用户以为功能没生效。
const POLL_INTERVAL: Duration = Duration::from_millis(700);
/// 整段文本的最大长度，超过就认为不是"一条链接"。
const MAX_TEXT_LEN: usize = 4096;
/// 当链接夹在文字中间时，允许的上下文长度（避免把整篇文章里的链接抓出来）。
const MAX_CONTEXT_LEN: usize = 200;

/// 支持的下载链接协议前缀。
const SCHEMES: &[&str] = &["magnet:", "http://", "https://", "ftp://"];

/// 从一段剪贴板文本里识别下载链接。
///
/// 返回规范化后的链接；不是链接、或一条文本里出现多条链接（语义不明确）则返回 `None`。
pub fn detect_download_link(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_TEXT_LEN || text.contains('\0') {
        return None;
    }

    // 情况 1：整段就是一条链接（允许末尾有换行）
    let flat = text.replace(['\r', '\n', '\t'], " ");
    let flat = flat.trim();
    if !flat.contains(char::is_whitespace) {
        if let Some(link) = as_link(flat) {
            return Some(link);
        }
    }

    // 情况 2：从聊天/网页里连着前后文一起复制，
    // 例如「下载地址：https://example.com/a.zip 谢谢」
    if text.len() > MAX_CONTEXT_LEN {
        return None;
    }
    let mut found: Vec<String> = Vec::new();
    for token in text.split_whitespace() {
        // 夹在正文里的链接要求"看起来确实是个文件"，
        // 否则复制一段提到某网站的文章也会弹窗，太吵。
        if let Some(link) = as_link(token).filter(|l| looks_downloadable(l)) {
            if !found.contains(&link) {
                found.push(link);
            }
        }
    }
    if found.len() == 1 {
        found.pop()
    } else {
        None
    }
}

/// 链接是否"像个可下载的文件"：磁力链、种子、带扩展名的路径、或带查询参数。
fn looks_downloadable(link: &str) -> bool {
    let lower = link.to_ascii_lowercase();
    if lower.starts_with("magnet:") || lower.ends_with(".torrent") {
        return true;
    }
    // 去掉协议和查询串后，看最后一段路径有没有文件名式的扩展名
    let after_scheme = lower.split_once("://").map(|(_, r)| r).unwrap_or(&lower);
    let (path_part, query) = match after_scheme.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (after_scheme, None),
    };
    if query.is_some() {
        return true;
    }
    // 没有路径部分 → 只是网站首页，不算下载链接
    let Some((_host, path)) = path_part.split_once('/') else {
        return false;
    };
    let last = path.rsplit('/').next().unwrap_or("");
    if last.is_empty() {
        return false;
    }
    match last.rsplit_once('.') {
        Some((name, ext)) => {
            !name.is_empty()
                && !ext.is_empty()
                && ext.len() <= 8
                && ext.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}

/// 链接在遇到这些字符时截断（中文标点、空白、各种括号引号）。
fn is_link_stop(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '"' | '\''
                | '“'
                | '”'
                | '‘'
                | '’'
                | '<'
                | '>'
                | '「'
                | '」'
                | '『'
                | '』'
                | '《'
                | '》'
                | '（'
                | '）'
                | '【'
                | '】'
                | '，'
                | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '…'
        )
}

/// 判断单个 token 是否是下载链接，并去掉尾部标点、截断粘连的上下文。
fn as_link(token: &str) -> Option<String> {
    let token = token.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '“' | '”' | '‘' | '’' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
        )
    });
    if token.is_empty() || token.len() > MAX_TEXT_LEN {
        return None;
    }

    // 本地种子文件路径
    let lower_token = token.to_ascii_lowercase();
    if lower_token.ends_with(".torrent") && std::path::Path::new(token).is_file() {
        return Some(token.to_string());
    }

    let scheme = SCHEMES
        .iter()
        .find(|s| lower_token.starts_with(*s))?;

    // 从协议往后扫，遇到非法字符就截断（处理「https://x/a.zip，谢谢」这种粘连）
    let end = token
        .char_indices()
        .find(|(_, c)| is_link_stop(*c))
        .map(|(i, _)| i)
        .unwrap_or(token.len());
    let link = &token[..end];
    // 再剥掉常见的 ASCII 尾随标点
    let link = link.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
    if link.len() <= scheme.len() {
        return None;
    }

    if *scheme == "magnet:" {
        // magnet 必须带 xt=urn:btih / btmh，否则不是可下载的磁力链
        let lower = link.to_ascii_lowercase();
        return if lower.contains("xt=urn:bt") {
            Some(link.to_string())
        } else {
            None
        };
    }

    // 去掉协议后必须还有主机名
    let rest = &link[scheme.len()..];
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(link.to_string())
}

/// 读取系统剪贴板（纯文本）。
///
/// Windows 直接调 user32/kernel32：比引入第三方剪贴板 crate 更轻，
/// 而且本项目用的 GNU 工具链在编译 `windows-sys 0.61`（raw-dylib）时会因
/// dlltool 失败，绕开它反而更稳。其他平台暂不支持，返回 `None`。
mod platform {
    pub fn available() -> bool {
        cfg!(windows)
    }

    #[cfg(windows)]
    pub fn read_text() -> Option<String> {
        win::read_clipboard_text()
    }

    #[cfg(not(windows))]
    pub fn read_text() -> Option<String> {
        None
    }

    #[cfg(windows)]
    mod win {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::ptr;

        type Bool = i32;
        type Handle = *mut core::ffi::c_void;
        /// 文本格式（UTF-16）
        const CF_UNICODETEXT: u32 = 13;

        #[link(name = "user32")]
        extern "system" {
            fn OpenClipboard(hwnd: Handle) -> Bool;
            fn CloseClipboard() -> Bool;
            fn GetClipboardData(format: u32) -> Handle;
            fn IsClipboardFormatAvailable(format: u32) -> Bool;
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn GlobalLock(handle: Handle) -> *mut core::ffi::c_void;
            fn GlobalUnlock(handle: Handle) -> Bool;
            fn GlobalSize(handle: Handle) -> usize;
        }

        pub fn read_clipboard_text() -> Option<String> {
            unsafe {
                if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                    return None;
                }
                // 剪贴板可能被其他程序短暂占用，重试几次
                let mut opened = false;
                for _ in 0..5 {
                    if OpenClipboard(ptr::null_mut()) != 0 {
                        opened = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if !opened {
                    return None;
                }

                let mut out = None;
                let handle = GetClipboardData(CF_UNICODETEXT);
                if !handle.is_null() {
                    let ptr = GlobalLock(handle) as *const u16;
                    if !ptr.is_null() {
                        let max_len = GlobalSize(handle) / 2;
                        let mut len = 0usize;
                        while len < max_len && *ptr.add(len) != 0 {
                            len += 1;
                        }
                        let slice = std::slice::from_raw_parts(ptr, len);
                        out = Some(OsString::from_wide(slice).to_string_lossy().into_owned());
                        GlobalUnlock(handle);
                    }
                }
                CloseClipboard();
                out
            }
        }
    }
}

/// 立即读一次剪贴板文本（用于「从剪贴板添加」菜单项）。
pub fn read_now() -> Option<String> {
    platform::read_text()
}

/// 剪贴板监视器。
///
/// `try_recv()` 拿走检测到的链接；`set_enabled()` 可随时开关。
pub struct ClipboardWatcher {
    rx: Receiver<String>,
    enabled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// 轮询线程是否成功启动（拿不到剪贴板时为 false，UI 可提示用户）
    available: Arc<AtomicBool>,
}

impl ClipboardWatcher {
    /// 启动监视线程。`enabled` 为初始开关状态。
    pub fn spawn(enabled: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let enabled = Arc::new(AtomicBool::new(enabled));
        let stop = Arc::new(AtomicBool::new(false));
        let available = Arc::new(AtomicBool::new(false));

        let flag = enabled.clone();
        let stop_flag = stop.clone();
        let avail = available.clone();
        let spawned = thread::Builder::new()
            .name("rust-dm-clipboard".into())
            .spawn(move || {
                if !platform::available() {
                    eprintln!("[rust-dm] 当前平台暂不支持剪贴板监视");
                    return;
                }
                avail.store(true, Ordering::Relaxed);

                // 程序启动时剪贴板里已有的内容不算"新复制"，记为已见过，
                // 避免程序一启动就弹窗打扰。
                let mut last_seen =
                    platform::read_text().and_then(|t| detect_download_link(&t));

                while !stop_flag.load(Ordering::Relaxed) {
                    if flag.load(Ordering::Relaxed) {
                        if let Some(text) = platform::read_text() {
                            if let Some(link) = detect_download_link(&text) {
                                if last_seen.as_deref() != Some(link.as_str()) {
                                    last_seen = Some(link.clone());
                                    if tx.send(link).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    thread::sleep(POLL_INTERVAL);
                }
            })
            .is_ok();

        if !spawned {
            eprintln!("[rust-dm] 剪贴板监视线程启动失败");
        }

        Self {
            rx,
            enabled,
            stop,
            available,
        }
    }

    /// 取走一条待确认的链接（非阻塞）。
    pub fn try_recv(&self) -> Option<String> {
        self.rx.try_recv().ok()
    }

    /// 当前是否开启监视。
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// 开关监视。
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// 剪贴板是否可用（个别受限环境拿不到剪贴板句柄）。
    pub fn available(&self) -> bool {
        self.available.load(Ordering::Relaxed)
    }
}

impl Drop for ClipboardWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_urls() {
        for link in [
            "https://example.com/file.zip",
            "http://example.com/a/b.tar.gz",
            "ftp://user:pass@example.com/x.iso",
        ] {
            assert_eq!(detect_download_link(link).as_deref(), Some(link));
        }
    }

    #[test]
    fn accepts_magnet_with_info_hash() {
        let m = "magnet:?xt=urn:btih:d160b8d8ea35a5b4e52837468fc8f03d55cef1f7&dn=ubuntu";
        assert_eq!(detect_download_link(m).as_deref(), Some(m));
        // 缺 xt 的 magnet 不算
        assert!(detect_download_link("magnet:?dn=onlyname").is_none());
    }

    #[test]
    fn trims_wrapping_quotes_and_punctuation() {
        assert_eq!(
            detect_download_link("\"https://example.com/a.zip\"").as_deref(),
            Some("https://example.com/a.zip")
        );
        assert_eq!(
            detect_download_link("https://example.com/a.zip。").as_deref(),
            Some("https://example.com/a.zip")
        );
        assert_eq!(
            detect_download_link("见 https://example.com/a.zip，谢谢").as_deref(),
            Some("https://example.com/a.zip")
        );
    }

    #[test]
    fn ignores_ordinary_text() {
        for text in [
            "",
            "这是一段普通文字",
            "hello world",
            "https://",
            "看这篇文章 https://example.com 里面还有更多内容，后面跟了一大堆说明文字用来超过上下文长度限制所以不应该被当成链接",
        ] {
            assert!(
                detect_download_link(text).is_none(),
                "不应把 {text:?} 当成下载链接"
            );
        }
    }

    #[test]
    fn ignores_multiline_article() {
        let article = "第一行有 https://example.com/a.zip\n第二行 https://example.com/b.zip\n第三行";
        // 段落里有两条链接，语义不明确，不弹窗
        assert!(detect_download_link(article).is_none());
    }

    #[test]
    fn accepts_torrent_path_when_file_exists() {
        let dir = std::env::temp_dir();
        let path = dir.join("rust_dm_clip_test.torrent");
        std::fs::write(&path, b"d4:infod4:name1:xee").unwrap();
        let text = path.to_string_lossy().to_string();
        assert_eq!(detect_download_link(&text).as_deref(), Some(text.as_str()));
        // 不存在的路径不认
        let missing = dir.join("rust_dm_clip_missing.torrent");
        assert!(detect_download_link(&missing.to_string_lossy()).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
