//! 下载任务的数据模型。
//!
//! UI 与下载引擎共享同一份任务快照（`EngineState`），引擎在后台更新进度，
//! UI 每帧读取并重绘，因此这里的数据结构需要能被 `Clone`。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// 任务唯一标识。
pub type TaskId = u64;

/// 任务来源协议。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Ftp,
    BitTorrent,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

impl Protocol {
    pub fn label(&self) -> &'static str {
        match self {
            Protocol::Http => "HTTP",
            Protocol::Ftp => "FTP",
            Protocol::BitTorrent => "BT",
        }
    }
}

/// 任务来源描述。
#[derive(Clone, Debug)]
pub enum Source {
    /// http/https 直链
    Http { url: String },
    /// ftp://[user:pass@]host[:port]/path
    Ftp {
        url: String,
        user: String,
        password: String,
    },
    /// magnet:?xt=urn:btih:...
    Magnet(String),
    /// 本地 .torrent 文件路径
    TorrentFile(PathBuf),
}

impl Source {
    /// 从用户输入自动识别协议。
    pub fn parse(input: &str, ftp_user: &str, ftp_pass: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("地址不能为空".into());
        }
        let lower = s.to_ascii_lowercase();
        if lower.starts_with("magnet:") {
            return Ok(Source::Magnet(s.to_string()));
        }
        if lower.starts_with("ftp://") {
            let (url, user, pass) = split_ftp_url(s);
            let user = if user.is_empty() {
                ftp_user.to_string()
            } else {
                user
            };
            let pass = if pass.is_empty() {
                ftp_pass.to_string()
            } else {
                pass
            };
            return Ok(Source::Ftp {
                url,
                user,
                password: pass,
            });
        }
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return Ok(Source::Http {
                url: s.to_string(),
            });
        }
        if lower.ends_with(".torrent") {
            return Ok(Source::TorrentFile(PathBuf::from(s)));
        }
        Err(format!("无法识别的地址：{s}"))
    }

    pub fn protocol(&self) -> Protocol {
        match self {
            Source::Http { .. } => Protocol::Http,
            Source::Ftp { .. } => Protocol::Ftp,
            Source::Magnet(_) | Source::TorrentFile(_) => Protocol::BitTorrent,
        }
    }

    /// 供 UI 展示的一行来源摘要。
    pub fn display(&self) -> String {
        match self {
            Source::Http { url } => url.clone(),
            Source::Ftp { url, .. } => url.clone(),
            Source::Magnet(m) => {
                // magnet 链接通常很长，截断 info hash 之外的内容
                if m.len() > 64 {
                    format!("{}…", &m[..64])
                } else {
                    m.clone()
                }
            }
            Source::TorrentFile(p) => p.display().to_string(),
        }
    }
}

/// 从 ftp URL 中拆出 (完整URL, 用户名, 密码)。
fn split_ftp_url(url: &str) -> (String, String, String) {
    let rest = &url["ftp://".len()..];
    let (authority, _path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (cred, host) = match authority.rfind('@') {
        Some(i) => (&authority[..i], &authority[i + 1..]),
        None => ("", authority),
    };
    let (user, pass) = match cred.find(':') {
        Some(i) => (cred[..i].to_string(), cred[i + 1..].to_string()),
        None => (cred.to_string(), String::new()),
    };
    (url.to_string(), user, pass)
}

/// 任务运行状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskState {
    /// 排队中，等待开始
    Queued,
    /// 正在连接服务器 / 解析元数据
    Connecting,
    /// 下载中
    Downloading,
    /// BT：正在做哈希校验
    Checking,
    /// BT：已下载完成，正在做种
    Seeding,
    /// 已暂停
    Paused,
    /// 完成
    Completed,
    /// 失败
    Failed(String),
}

impl TaskState {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskState::Connecting | TaskState::Downloading | TaskState::Checking | TaskState::Seeding
        )
    }

    pub fn is_finished(&self) -> bool {
        matches!(self, TaskState::Completed | TaskState::Failed(_))
    }

    pub fn label(&self) -> String {
        match self {
            TaskState::Queued => "排队".into(),
            TaskState::Connecting => "连接中".into(),
            TaskState::Downloading => "下载中".into(),
            TaskState::Checking => "校验中".into(),
            TaskState::Seeding => "做种".into(),
            TaskState::Paused => "已暂停".into(),
            TaskState::Completed => "完成".into(),
            TaskState::Failed(e) => format!("失败：{e}"),
        }
    }
}

/// 单个下载任务的完整快照。
#[derive(Clone, Debug)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub source: Source,
    pub protocol: Protocol,
    pub state: TaskState,
    pub save_dir: PathBuf,
    /// 保存到磁盘的最终文件名（BT 为 torrent 根目录名）
    pub file_name: String,

    pub total_bytes: u64,
    pub downloaded: u64,
    pub uploaded: u64,
    /// 瞬时速度（字节/秒），由引擎周期性计算
    pub speed_down: u64,
    pub speed_up: u64,

    /// BT：已连上的 peer / seed / leech 数量
    pub peers: usize,
    pub seeds: usize,
    pub leeches: usize,
    /// BT：已完成的分片数
    pub pieces_done: usize,
    pub pieces_total: usize,

    /// 预计剩余秒数
    pub eta: Option<u64>,
    pub error: Option<String>,
    /// 最近一条日志，UI 中显示
    pub last_message: String,
    pub created_at: SystemTime,
    /// 排序用的稳定序号
    pub seq: u64,
}

impl Task {
    pub fn new(id: TaskId, name: String, source: Source, save_dir: PathBuf, seq: u64) -> Self {
        let protocol = source.protocol();
        let file_name = name.clone();
        Self {
            id,
            name,
            source,
            protocol,
            state: TaskState::Queued,
            save_dir,
            file_name,
            total_bytes: 0,
            downloaded: 0,
            uploaded: 0,
            speed_down: 0,
            speed_up: 0,
            peers: 0,
            seeds: 0,
            leeches: 0,
            pieces_done: 0,
            pieces_total: 0,
            eta: None,
            error: None,
            last_message: String::new(),
            created_at: SystemTime::now(),
            seq,
        }
    }

    /// 完成百分比 0..=1
    pub fn progress(&self) -> f32 {
        if self.total_bytes == 0 {
            // 总大小未知时用状态表示，避免除零
            return if self.state == TaskState::Completed {
                1.0
            } else {
                0.0
            };
        }
        (self.downloaded as f64 / self.total_bytes as f64).clamp(0.0, 1.0) as f32
    }

    /// 分享率（BT 做种相关）
    pub fn share_ratio(&self) -> f32 {
        if self.downloaded == 0 {
            0.0
        } else {
            self.uploaded as f32 / self.downloaded as f32
        }
    }

    pub fn local_path(&self) -> PathBuf {
        self.save_dir.join(&self.file_name)
    }
}

/// 全局配置。
#[derive(Clone, Debug)]
pub struct Config {
    /// 默认保存目录
    pub download_dir: PathBuf,
    /// 同时进行的 HTTP/FTP 任务数上限
    pub max_http_tasks: usize,
    /// 同时进行的 BT 任务数上限
    pub max_bt_tasks: usize,
    /// 全局下载限速（字节/秒，0 = 不限）
    pub speed_limit_down: u64,
    /// 全局上传限速（BT）
    pub speed_limit_up: u64,
    /// HTTP 单任务连接数
    pub http_connections: usize,
    /// BT 单任务最大 peer 数
    pub bt_max_peers: usize,
    /// BT 监听端口
    pub bt_port: u16,
    /// 是否在任务完成后自动开始做种
    pub bt_seed_after_complete: bool,
    pub ftp_user: String,
    pub ftp_password: String,
}

impl Default for Config {
    fn default() -> Self {
        let dir = dirs_download_dir();
        Self {
            download_dir: dir,
            max_http_tasks: 5,
            max_bt_tasks: 3,
            speed_limit_down: 0,
            speed_limit_up: 0,
            http_connections: 8,
            bt_max_peers: 50,
            bt_port: 6881,
            bt_seed_after_complete: true,
            ftp_user: "anonymous".into(),
            ftp_password: "".into(),
        }
    }
}

/// 取用户主目录下的 Downloads，失败则退回当前目录。
fn dirs_download_dir() -> PathBuf {
    if let Ok(home) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
        let p = PathBuf::from(home).join("Downloads");
        if p.is_dir() {
            return p;
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 计算剩余时间：按当前速度估算。
pub fn estimate_eta(done: u64, total: u64, speed: u64) -> Option<u64> {
    if speed == 0 || total == 0 || done >= total {
        return None;
    }
    Some(Duration::from_secs((total - done) / speed).as_secs())
}

/// 日志级别
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(&self) -> &'static str {
        match self {
            LogLevel::Info => "信息",
            LogLevel::Warn => "警告",
            LogLevel::Error => "错误",
        }
    }
}

/// 一条日志。
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub time: SystemTime,
    pub level: LogLevel,
    pub task_id: Option<TaskId>,
    pub message: String,
}

/// 简单滚动日志表，只保留最近 N 条。
pub struct LogBuffer {
    pub entries: Vec<LogEntry>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
        }
    }

    pub fn push(&mut self, level: LogLevel, task_id: Option<TaskId>, message: String) {
        if self.entries.len() >= self.capacity {
            let overflow = self.entries.len() + 1 - self.capacity;
            self.entries.drain(0..overflow);
        }
        self.entries.push(LogEntry {
            time: SystemTime::now(),
            level,
            task_id,
            message,
        });
    }
}

/// 任务排序方式，对应 UI 表头点击。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Progress,
    SpeedDown,
    SpeedUp,
    Status,
    /// 剩余时间
    Eta,
    /// 添加时间（列表里显示为"上次尝试"）
    LastTry,
}

impl SortKey {
    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Name => "名称",
            SortKey::Size => "大小",
            SortKey::Progress => "进度",
            SortKey::SpeedDown => "下载速度",
            SortKey::SpeedUp => "上传速度",
            SortKey::Status => "状态",
            SortKey::Eta => "剩余时间",
            SortKey::LastTry => "上次尝试",
        }
    }

    pub fn cmp(&self, a: &Task, b: &Task) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self {
            SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            SortKey::Size => a.total_bytes.cmp(&b.total_bytes),
            SortKey::Progress => a
                .progress()
                .partial_cmp(&b.progress())
                .unwrap_or(Ordering::Equal),
            SortKey::SpeedDown => a.speed_down.cmp(&b.speed_down),
            SortKey::SpeedUp => a.speed_up.cmp(&b.speed_up),
            SortKey::Status => b.state.is_active().cmp(&a.state.is_active()).then_with(|| {
                a.state
                    .label()
                    .cmp(&b.state.label())
            }),
            // 没有 ETA 的（已完成/未知）排到最后
            SortKey::Eta => a.eta.unwrap_or(u64::MAX).cmp(&b.eta.unwrap_or(u64::MAX)),
            SortKey::LastTry => a
                .created_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .cmp(&b.created_at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default()),
        }
    }
}

/// 选项列表类型，UI 中用于下拉框。
pub type StringMap = BTreeMap<String, String>;