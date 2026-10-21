//! FTP 下载器（基于 suppaftp v12）。
//!
//! 特性：
//! - 被动模式（PASV），兼容性最好
//! - REST 断点续传（`.part` 文件）
//! - 目录递归下载
//! - 令牌桶限速、暂停/取消
//!
//! 注意：suppaftp v12 的连接 API 接收 `SocketAddr`，
//! 因此这里先用 `to_socket_addrs` 做域名解析。

use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use suppaftp::list::ListParser;
use suppaftp::{FtpError, FtpResult, FtpStream};

use crate::common::limits::TokenBucket;
use crate::common::util::percent_decode;

/// FTP 连接参数。
#[derive(Clone, Debug)]
pub struct FtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub timeout: Duration,
    pub retries: u32,
}

impl FtpConfig {
    /// 解析 `ftp://[user:pass@]host[:port]/path`，返回 (配置, 远端路径)。
    pub fn parse(url: &str, default_user: &str, default_pass: &str) -> Result<(Self, String)> {
        // 只对协议前缀做大小写无关匹配；路径与密码区分大小写，
        // 因此不能对整个 URL 做 to_ascii_lowercase。
        let trimmed = url.trim();
        let lower = trimmed
            .get(..6)
            .unwrap_or("")
            .to_ascii_lowercase();
        let rest = trimmed
            .get(6..)
            .ok_or_else(|| anyhow!("不是合法的 ftp 地址：{url}"))?;
        if lower != "ftp://" {
            return Err(anyhow!("不是合法的 ftp 地址：{url}"));
        }

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (cred, hostport) = match authority.rfind('@') {
            Some(i) => (&authority[..i], &authority[i + 1..]),
            None => ("", authority),
        };
        let (user, password) = match cred.find(':') {
            Some(i) => (
                decode_part(&cred[..i]),
                decode_part(&cred[i + 1..]),
            ),
            None if cred.is_empty() => (
                default_user.to_string(),
                default_pass.to_string(),
            ),
            None => (decode_part(cred), default_pass.to_string()),
        };

        let (host, port) = split_host_port(hostport)?;
        if host.is_empty() {
            bail!("无法从地址中解析主机名：{url}");
        }

        Ok((
            Self {
                host,
                port,
                user,
                password,
                timeout: Duration::from_secs(30),
                retries: 3,
            },
            percent_decode(path),
        ))
    }

    /// 远端路径的最后一段，作为默认文件名。
    pub fn file_name(remote: &str) -> String {
        let p = remote.trim_end_matches('/');
        p.rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(|s| percent_decode(s))
            .unwrap_or_else(|| "download.bin".into())
    }

    /// 是否看起来是一个目录（以 / 结尾，或路径段最后一段没有扩展名）。
    pub fn looks_like_dir(remote: &str) -> bool {
        remote.ends_with('/')
    }
}

fn decode_part(s: &str) -> String {
    percent_decode(s)
}

/// 解析 host[:port]，兼容 IPv6 字面量。
fn split_host_port(hostport: &str) -> Result<(String, u16)> {
    if hostport.starts_with('[') {
        let close = hostport
            .find(']')
            .ok_or_else(|| anyhow!("IPv6 地址格式错误：{hostport}"))?;
        let host = hostport[1..close].to_string();
        let port = match hostport[close + 1..].strip_prefix(':') {
            Some(p) => p.parse().unwrap_or(21),
            None => 21,
        };
        return Ok((host, port));
    }
    match hostport.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Ok((h.to_string(), p.parse().unwrap_or(21)))
        }
        _ => Ok((hostport.to_string(), 21)),
    }
}

/// 运行期控制句柄。
#[derive(Clone, Default)]
pub struct FtpHandle {
    pub pause: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
    pub downloaded: Arc<AtomicU64>,
}

pub type ProgressHook = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// 建立连接。
fn connect(cfg: &FtpConfig) -> Result<FtpStream> {
    let addrs: Vec<SocketAddr> = (cfg.host.as_str(), cfg.port)
        .to_socket_addrs()
        .with_context(|| format!("无法解析 {}:{}", cfg.host, cfg.port))?
        .collect();
    let addr = addrs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("没有解析到 {}:{} 的地址", cfg.host, cfg.port))?;

    let mut stream = FtpStream::connect_timeout(addr, cfg.timeout)
        .map_err(|e| anyhow!("连接 {}:{} 失败：{e}", cfg.host, cfg.port))?;
    stream
        .login(&cfg.user, &cfg.password)
        .map_err(|e| anyhow!("登录失败（用户 {}）：{e}", cfg.user))?;
    Ok(stream)
}

/// 自定义错误类型，用于区分「暂停」这种可恢复情况。
fn paused_err() -> FtpError {
    // suppaftp 的闭包需要返回 FtpError，这里借用其通用连接错误通道
    FtpError::ConnectionError(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "__RUST_DM_PAUSED__",
    ))
}

fn is_paused_marker(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| c.to_string().contains("__RUST_DM_PAUSED__"))
}

/// 单文件下载，支持暂停/续传/重试。阻塞实现，在 `spawn_blocking` 中调用。
pub fn download_file(
    cfg: &FtpConfig,
    remote: &str,
    local: &Path,
    handle: FtpHandle,
    bucket: Arc<TokenBucket>,
    hook: ProgressHook,
) -> Result<()> {
    if let Some(dir) = local.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let part = part_path(local);

    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if handle.cancel.load(Ordering::Relaxed) {
            bail!("已取消");
        }
        match try_download_file(cfg, remote, local, &part, &handle, &bucket, &hook) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if is_paused_marker(&e) {
                    // 暂停：保留 .part，下次从断点继续
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                if handle.cancel.load(Ordering::Relaxed) {
                    bail!("已取消");
                }
                if attempt > cfg.retries {
                    return Err(e);
                }
                std::thread::sleep(Duration::from_millis(800 * attempt as u64));
            }
        }
    }
}

fn try_download_file(
    cfg: &FtpConfig,
    remote: &str,
    local: &Path,
    part: &Path,
    handle: &FtpHandle,
    bucket: &Arc<TokenBucket>,
    hook: &ProgressHook,
) -> Result<()> {
    let mut client = connect(cfg)?;

    // 切到远端目录，只对文件名发 RETR，兼容性更好
    let (dir, fname) = match remote.rfind('/') {
        Some(i) if i > 0 => (remote[..i].to_string(), percent_decode(&remote[i + 1..])),
        _ => ("/".to_string(), percent_decode(remote)),
    };
    if dir != "/" && !dir.is_empty() {
        client
            .cwd(&dir)
            .map_err(|e| anyhow!("切换目录 {dir} 失败：{e}"))?;
    }

    // 断点续传位置
    let mut offset = 0u64;
    if part.exists() {
        offset = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    }
    let total = client
        .size(&fname)
        .map_err(|e| anyhow!("获取文件大小失败：{e}"))? as u64;

    if total > 0 && offset >= total {
        std::fs::rename(part, local)
            .with_context(|| format!("重命名到 {} 失败", local.display()))?;
        hook(total, total);
        return Ok(());
    }

    let mut written = offset;
    handle.downloaded.store(written, Ordering::Relaxed);
    hook(written, total);

    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .context("打开 .part 文件失败")?;
    if offset == 0 {
        f.set_len(0)?;
    }

    if offset > 0 {
        client
            .resume_transfer(offset as usize)
            .map_err(|e| anyhow!("设置续传位置失败：{e}"))?;
    }

    // retr 的闭包签名：FnMut(&mut dyn Read) -> FtpResult<D>
    client
        .retr(&fname, |reader| -> FtpResult<()> {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                if handle.cancel.load(Ordering::Relaxed) {
                    return Err(FtpError::ConnectionError(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "__RUST_DM_CANCELLED__",
                    )));
                }
                if handle.pause.load(Ordering::Relaxed) {
                    return Err(paused_err());
                }
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        return Err(FtpError::ConnectionError(e));
                    }
                };
                f.write_all(&buf[..n])
                    .map_err(FtpError::ConnectionError)?;
                written += n as u64;
                handle.downloaded.store(written, Ordering::Relaxed);
                bucket.blocking_acquire(n as u64);
                hook(written, total);
            }
            f.flush().map_err(FtpError::ConnectionError)?;
            Ok(())
        })
        .map_err(|e| anyhow!("下载失败：{e}"))?;

    std::fs::rename(part, local)
        .with_context(|| format!("重命名到 {} 失败", local.display()))?;
    hook(written, total);
    Ok(())
}

fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// 远端目录条目。
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 列出远端目录内容（用 LIST + ListParser 解析）。
pub fn list_dir(cfg: &FtpConfig, remote_dir: &str) -> Result<Vec<RemoteEntry>> {
    let mut client = connect(cfg)?;
    let lines = client
        .list(Some(remote_dir))
        .map_err(|e| anyhow!("列目录 {remote_dir} 失败：{e}"))?;
    Ok(parse_list(&lines))
}

/// 解析 LIST 输出。优先用 MLSD 风格的 POSIX 解析，失败则跳过。
pub fn parse_list(lines: &[String]) -> Vec<RemoteEntry> {
    let mut out = Vec::new();
    for line in lines {
        let Ok(file) = ListParser::parse_posix(line) else {
            continue;
        };
        let name = file.name().to_string();
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }
        out.push(RemoteEntry {
            name,
            is_dir: file.is_directory(),
            size: file.size() as u64,
        });
    }
    out
}

/// 递归下载整个目录。
pub fn download_dir(
    cfg: &FtpConfig,
    remote_dir: &str,
    local_dir: &Path,
    handle: FtpHandle,
    bucket: Arc<TokenBucket>,
    hook: ProgressHook,
) -> Result<()> {
    std::fs::create_dir_all(local_dir).ok();
    let entries = list_dir(cfg, remote_dir)?;
    for e in entries {
        if handle.cancel.load(Ordering::Relaxed) {
            bail!("已取消");
        }
        let remote = format!("{}/{}", remote_dir.trim_end_matches('/'), e.name);
        if e.is_dir {
            download_dir(
                cfg,
                &remote,
                &local_dir.join(&e.name),
                handle.clone(),
                bucket.clone(),
                hook.clone(),
            )?;
        } else {
            download_file(
                cfg,
                &remote,
                &local_dir.join(&e.name),
                handle.clone(),
                bucket.clone(),
                hook.clone(),
            )
            .with_context(|| format!("下载 {remote} 失败"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_url_with_credentials() {
        let (cfg, path) =
            FtpConfig::parse("ftp://alice:secret@files.example.com:2121/pub/a.txt", "", "")
                .unwrap();
        assert_eq!(cfg.host, "files.example.com");
        assert_eq!(cfg.port, 2121);
        assert_eq!(cfg.user, "alice");
        assert_eq!(cfg.password, "secret");
        assert_eq!(path, "/pub/a.txt");
    }

    #[test]
    fn defaults_port_and_uses_given_creds() {
        let (cfg, _) = FtpConfig::parse("ftp://ftp.gnu.org/pub/x", "anon", "guest@").unwrap();
        assert_eq!(cfg.port, 21);
        assert_eq!(cfg.user, "anon");
        assert_eq!(cfg.password, "guest@");
    }

    #[test]
    fn parses_ipv6_literal() {
        let (cfg, _) = FtpConfig::parse("ftp://[::1]:2121/x", "u", "p").unwrap();
        assert_eq!(cfg.host, "::1");
        assert_eq!(cfg.port, 2121);
    }

    #[test]
    fn preserves_case_in_path_and_password() {
        // 远端路径与密码区分大小写，解析时不能被小写化
        let (cfg, path) = FtpConfig::parse("ftp://Bob:S3cret@FTP.Example.COM:2121/Pub/File.TXT", "", "")
            .unwrap();
        assert_eq!(cfg.user, "Bob");
        assert_eq!(cfg.password, "S3cret");
        assert_eq!(path, "/Pub/File.TXT");
        // 主机名本身不区分大小写，保留原样即可
        assert_eq!(cfg.host, "FTP.Example.COM");
    }

    #[test]
    fn accepts_uppercase_scheme() {
        let (cfg, path) = FtpConfig::parse("FTP://files.example.com/a.txt", "anon", "").unwrap();
        assert_eq!(cfg.host, "files.example.com");
        assert_eq!(path, "/a.txt");
    }

    #[test]
    fn parses_posix_list_lines() {
        let lines = vec![
            "drwxr-xr-x 2 user group 4096 Nov 5 13:46 subdir".to_string(),
            "-rw-r--r-- 1 user group 1234 Nov 5 13:46 example.txt".to_string(),
        ];
        let entries = parse_list(&lines);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_dir);
        assert!(!entries[1].is_dir);
        assert_eq!(entries[1].name, "example.txt");
        assert_eq!(entries[1].size, 1234);
    }
}