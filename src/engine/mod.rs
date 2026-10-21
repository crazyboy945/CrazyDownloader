//! 下载引擎：统一调度 HTTP / FTP / BT 三类任务。
//!
//! 设计要点：
//! - 引擎跑在独立的 tokio 运行时（后台线程），UI 只读快照
//! - 每个任务由 `Arc<TaskControl>` 控制暂停/恢复/取消
//! - 速率统计由后台 tick 任务周期计算，UI 每帧读取

pub mod bt;
pub mod ftp;
pub mod http;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::runtime::Runtime;

use crate::common::limits::TokenBucket;
use crate::common::task::{
    estimate_eta, Config, LogBuffer, LogLevel, Source, Task, TaskId, TaskState,
};

/// 单个任务的控制句柄。
pub struct TaskControl {
    pub pause: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
    /// 上一次统计的下载字节数，用于算瞬时速度
    last_downloaded: AtomicU64,
    last_uploaded: AtomicU64,
    last_tick: Mutex<std::time::Instant>,
}

impl Default for TaskControl {
    fn default() -> Self {
        Self {
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            last_downloaded: AtomicU64::new(0),
            last_uploaded: AtomicU64::new(0),
            last_tick: Mutex::new(std::time::Instant::now()),
        }
    }
}

impl TaskControl {
    /// 基于上一次的采样计算瞬时速度（字节/秒）。
    pub fn compute_speed(&self, downloaded: u64, uploaded: u64) -> (u64, u64) {
        let mut last = self.last_tick.lock().unwrap();
        let now = std::time::Instant::now();
        let dt = now.duration_since(*last).as_secs_f64();
        if dt < 0.05 {
            return (0, 0);
        }
        *last = now;
        let dl = self.last_downloaded.load(Ordering::Relaxed);
        let ul = self.last_uploaded.load(Ordering::Relaxed);
        let down = downloaded.saturating_sub(dl) as f64 / dt;
        let up = uploaded.saturating_sub(ul) as f64 / dt;
        self.last_downloaded.store(downloaded, Ordering::Relaxed);
        self.last_uploaded.store(uploaded, Ordering::Relaxed);
        (down as u64, up as u64)
    }
}

/// 引擎内部共享状态。
pub struct EngineState {
    pub tasks: Vec<Arc<Mutex<Task>>>,
    pub controls: HashMap<TaskId, Arc<TaskControl>>,
    pub logs: LogBuffer,
    pub config: Config,
    pub next_id: TaskId,
    pub next_seq: u64,
    /// 全局限速桶
    pub global_bucket: Arc<TokenBucket>,
}

impl EngineState {
    fn find(&self, id: TaskId) -> Option<Arc<Mutex<Task>>> {
        self.tasks
            .iter()
            .find(|t| t.lock().map(|t| t.id == id).unwrap_or(false))
            .cloned()
    }
}

/// 下载引擎句柄（UI 持有）。
pub struct Engine {
    pub state: Arc<Mutex<EngineState>>,
    runtime: Arc<Runtime>,
}

impl Engine {
    /// 启动引擎，创建后台 tokio 运行时。
    pub fn new(config: Config) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("rust-dm-engine")
            .build()?;

        let bucket = Arc::new(TokenBucket::new_with_rate(config.speed_limit_down));
        let state = Arc::new(Mutex::new(EngineState {
            tasks: Vec::new(),
            controls: HashMap::new(),
            logs: LogBuffer::new(500),
            config: config.clone(),
            next_id: 1,
            next_seq: 1,
            global_bucket: bucket,
        }));

        Ok(Self {
            state,
            runtime: Arc::new(runtime),
        })
    }

    pub fn config(&self) -> Config {
        self.state.lock().unwrap().config.clone()
    }

    pub fn update_config(&self, cfg: Config) {
        let mut s = self.state.lock().unwrap();
        s.global_bucket.set_rate(cfg.speed_limit_down);
        s.config = cfg;
    }

    pub fn set_global_limit(&self, bytes_per_sec: u64) {
        self.state
            .lock()
            .unwrap()
            .global_bucket
            .set_rate(bytes_per_sec);
    }

    fn log(&self, level: LogLevel, task_id: Option<TaskId>, msg: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .logs
            .push(level, task_id, msg.into());
    }

    pub fn log_info(&self, msg: impl Into<String>) {
        self.log(LogLevel::Info, None, msg);
    }

    pub fn log_error(&self, msg: impl Into<String>) {
        self.log(LogLevel::Error, None, msg);
    }

    /// 添加任务。
    pub fn add_task(&self, source: Source, save_dir: PathBuf) -> TaskId {
        let (id, seq, name, protocol, task) = {
            let mut s = self.state.lock().unwrap();
            let id = s.next_id;
            s.next_id += 1;
            let seq = s.next_seq;
            s.next_seq += 1;

            let name = match &source {
                Source::Http { url } => crate::common::util::filename_from_url(url),
                Source::Ftp { url, .. } => ftp::FtpConfig::file_name(url),
                Source::Magnet(m) => {
                    let m2 = bt::session::prepare_from_magnet(m);
                    match m2 {
                        Ok(start) => start
                            .display_name
                            .clone()
                            .unwrap_or_else(|| format!("magnet_{}", hex::encode(&start.info_hash[..6]))),
                        Err(_) => "magnet 下载".to_string(),
                    }
                }
                Source::TorrentFile(p) => p
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "torrent".into()),
            };

            let protocol = source.protocol();
            let mut task = Task::new(id, name.clone(), source, save_dir, seq);
            task.protocol = protocol;
            (id, seq, name, protocol, task)
        };

        let control = Arc::new(TaskControl::default());
        let task_arc = Arc::new(Mutex::new(task));
        {
            let mut s = self.state.lock().unwrap();
            s.tasks.push(task_arc.clone());
            s.controls.insert(id, control.clone());
        }

        self.log(
            LogLevel::Info,
            Some(id),
            format!("已添加任务：{name}（{protocol}）"),
        );

        self.spawn(id, task_arc, control);
        id
    }

    /// 根据协议分派到对应的下载实现。
    fn spawn(&self, id: TaskId, task: Arc<Mutex<Task>>, control: Arc<TaskControl>) {
        let (source, save_dir, config, bucket) = {
            let t = task.lock().unwrap();
            let s = self.state.lock().unwrap();
            (
                t.source.clone(),
                t.save_dir.clone(),
                s.config.clone(),
                s.global_bucket.clone(),
            )
        };

        let runtime = self.runtime.clone();
        let task2 = task.clone();
        let control2 = control.clone();

        runtime.spawn(async move {
            let result: Result<()> = match &source {
                Source::Http { url } => {
                    run_http(id, url, save_dir, config, bucket, task2.clone(), control2.clone()).await
                }
                Source::Ftp {
                    url,
                    user,
                    password,
                } => {
                    run_ftp(
                        id,
                        url,
                        user,
                        password,
                        save_dir,
                        config,
                        bucket,
                        task2.clone(),
                        control2.clone(),
                    )
                    .await
                }
                Source::Magnet(_) | Source::TorrentFile(_) => {
                    run_bt(
                        id,
                        &source,
                        save_dir,
                        config,
                        bucket,
                        task2.clone(),
                        control2.clone(),
                    )
                    .await
                }
            };

            if let Err(e) = result {
                let msg = format!("{e}");
                if let Ok(mut t) = task2.lock() {
                    // 取消/暂停不算失败
                    if control2.cancel.load(Ordering::Relaxed) {
                        t.state = TaskState::Failed("已取消".into());
                    } else if t.state != TaskState::Completed {
                        t.error = Some(msg.clone());
                        t.state = TaskState::Failed(msg.clone());
                    }
                    t.last_message = msg.clone();
                }
            }
        });
    }

    /// 暂停任务。
    pub fn pause_task(&self, id: TaskId) {
        if let Some(c) = self.state.lock().unwrap().controls.get(&id) {
            c.pause.store(true, Ordering::Relaxed);
            if let Some(t) = self.state.lock().unwrap().find(id) {
                if let Ok(mut t) = t.lock() {
                    if t.state.is_active() {
                        t.state = TaskState::Paused;
                        t.speed_down = 0;
                        t.speed_up = 0;
                    }
                }
            }
            self.log(LogLevel::Info, Some(id), "任务已暂停");
        }
    }

    /// 恢复任务。
    pub fn resume_task(&self, id: TaskId) {
        if let Some(c) = self.state.lock().unwrap().controls.get(&id) {
            c.pause.store(false, Ordering::Relaxed);
            self.log(LogLevel::Info, Some(id), "任务已恢复");
        }
    }

    /// 取消并移除任务。
    pub fn remove_task(&self, id: TaskId) {
        {
            let mut s = self.state.lock().unwrap();
            if let Some(c) = s.controls.remove(&id) {
                c.cancel.store(true, Ordering::Relaxed);
                c.pause.store(false, Ordering::Relaxed);
            }
            s.tasks.retain(|t| t.lock().map(|t| t.id != id).unwrap_or(true));
        }
        self.log(LogLevel::Info, Some(id), "任务已移除");
    }

    /// 暂停全部。
    pub fn pause_all(&self) {
        let ids: Vec<TaskId> = self.state.lock().unwrap().tasks.iter().filter_map(|t| t.lock().ok().map(|t| t.id)).collect();
        for id in ids {
            self.pause_task(id);
        }
    }

    /// 恢复全部。
    pub fn resume_all(&self) {
        let ids: Vec<TaskId> = self.state.lock().unwrap().tasks.iter().filter_map(|t| t.lock().ok().map(|t| t.id)).collect();
        for id in ids {
            self.resume_task(id);
        }
    }

    /// 移除所有已完成任务。
    pub fn clear_finished(&self) {
        let mut s = self.state.lock().unwrap();
        let done: Vec<TaskId> = s
            .tasks
            .iter()
            .filter_map(|t| t.lock().ok().map(|t| (t.id, t.state.is_finished())))
            .filter(|(_, f)| *f)
            .map(|(i, _)| i)
            .collect();
        for id in &done {
            if let Some(c) = s.controls.remove(id) {
                c.cancel.store(true, Ordering::Relaxed);
            }
        }
        s.tasks.retain(|t| t.lock().map(|t| !done.contains(&t.id)).unwrap_or(true));
    }

    /// 全局统计。
    pub fn global_stats(&self) -> (u64, u64, usize, usize) {
        let s = self.state.lock().unwrap();
        let mut down = 0u64;
        let mut up = 0u64;
        let mut active = 0usize;
        let mut peers = 0usize;
        for t in &s.tasks {
            if let Ok(t) = t.lock() {
                down += t.speed_down;
                up += t.speed_up;
                peers += t.peers;
                if t.state.is_active() {
                    active += 1;
                }
            }
        }
        (down, up, active, peers)
    }
}

/// 读取下载器已写入的字节数。
fn dl_bytes_of(dl: &http::HttpDownloader) -> u64 {
    dl.downloaded().load(Ordering::Relaxed)
}

/// HTTP 任务实现。
async fn run_http(
    id: TaskId,
    url: &str,
    save_dir: PathBuf,
    config: Config,
    bucket: Arc<TokenBucket>,
    task: Arc<Mutex<Task>>,
    control: Arc<TaskControl>,
) -> Result<()> {
    // 探测
    {
        let mut t = task.lock().unwrap();
        t.state = TaskState::Connecting;
        t.last_message = "正在连接服务器…".into();
    }

    let opts = http::HttpOptions {
        connections: config.http_connections,
        ..Default::default()
    };
    let dl = http::HttpDownloader::new(opts, bucket)?;
    dl.pause_flag().store(control.pause.load(Ordering::Relaxed), Ordering::Relaxed);

    let (total, supports_range) = dl.probe(url).await?;
    {
        let mut t = task.lock().unwrap();
        t.total_bytes = total;
        t.state = TaskState::Downloading;
        t.last_message = if supports_range {
            format!("支持多连接，已开启 {} 连接", config.http_connections)
        } else {
            "服务器不支持断点续传，单连接下载".into()
        };
    }

    // 目标文件名
    let target = {
        let t = task.lock().unwrap();
        if t.file_name.is_empty() {
            save_dir.join(crate::common::util::filename_from_url(url))
        } else {
            t.local_path()
        }
    };
    {
        let mut t = task.lock().unwrap();
        t.file_name = target
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
    }

    // 进度回调
    let task_c = task.clone();
    let hook: http::ProgressHook = Arc::new(move |done, total| {
        if let Ok(mut t) = task_c.lock() {
            t.downloaded = done;
            if total > 0 {
                t.total_bytes = total;
            }
        }
    });

    let msg_task = task.clone();
    let message = move |m: String| {
        if let Ok(mut t) = msg_task.lock() {
            t.last_message = m;
        }
    };

    dl.download(url, &target, hook, message).await?;

    {
        let mut t = task.lock().unwrap();
        // 以实际写入的字节数为准，避免虚报进度
        let done = t.downloaded.max(dl_bytes_of(&dl));
        t.downloaded = done;
        if t.total_bytes == 0 {
            t.total_bytes = done;
        }
        t.state = TaskState::Completed;
        t.speed_down = 0;
        t.eta = None;
        t.last_message = format!("完成：{}", target.display());
    }
    let _ = (id, control);
    Ok(())
}

/// FTP 任务实现。
#[allow(clippy::too_many_arguments)]
async fn run_ftp(
    id: TaskId,
    url: &str,
    user: &str,
    password: &str,
    save_dir: PathBuf,
    config: Config,
    bucket: Arc<TokenBucket>,
    task: Arc<Mutex<Task>>,
    control: Arc<TaskControl>,
) -> Result<()> {
    let (cfg, remote) = ftp::FtpConfig::parse(url, user, password)?;
    {
        let mut t = task.lock().unwrap();
        t.state = TaskState::Connecting;
        t.last_message = format!("正在连接 {}:{} …", cfg.host, cfg.port);
    }

    let handle = ftp::FtpHandle {
        pause: control.pause.clone(),
        cancel: control.cancel.clone(),
        downloaded: Arc::new(AtomicU64::new(0)),
    };

    let task_c = task.clone();
    let hook: ftp::ProgressHook = Arc::new(move |done, total| {
        if let Ok(mut t) = task_c.lock() {
            t.downloaded = done;
            if total > 0 {
                t.total_bytes = total;
            }
        }
    });

    let is_dir = ftp::FtpConfig::looks_like_dir(&remote);
    let name = ftp::FtpConfig::file_name(&remote);
    {
        let mut t = task.lock().unwrap();
        t.file_name = if is_dir {
            name.clone()
        } else {
            t.file_name.clone()
        };
    }

    let cfg2 = cfg.clone();
    let handle2 = handle.clone();
    let bucket2 = bucket.clone();
    let hook2 = hook.clone();
    let remote2 = remote.clone();

    // 阻塞 FTP API 放到阻塞线程池
    let result = if is_dir {
        let local = save_dir.join(crate::common::util::sanitize_file_name(&name));
        tokio::task::spawn_blocking(move || {
            ftp::download_dir(&cfg2, &remote2, &local, handle2, bucket2, hook2)
        })
        .await?
    } else {
        let local = save_dir.join(crate::common::util::sanitize_file_name(&name));
        tokio::task::spawn_blocking(move || {
            ftp::download_file(&cfg2, &remote2, &local, handle2, bucket2, hook2)
        })
        .await?
    };

    result?;

    {
        let mut t = task.lock().unwrap();
        t.state = TaskState::Completed;
        t.speed_down = 0;
        t.eta = None;
        t.last_message = format!("完成：{}", name);
    }
    let _ = (id, config);
    Ok(())
}

/// BT 任务实现。
async fn run_bt(
    id: TaskId,
    source: &Source,
    save_dir: PathBuf,
    config: Config,
    bucket: Arc<TokenBucket>,
    task: Arc<Mutex<Task>>,
    control: Arc<TaskControl>,
) -> Result<()> {
    {
        let mut t = task.lock().unwrap();
        t.state = TaskState::Connecting;
        t.last_message = "正在解析种子元数据…".into();
    }

    let msg_task = task.clone();
    let msg: bt::session::Msg = Arc::new(move |m: String| {
        if let Ok(mut t) = msg_task.lock() {
            t.last_message = m;
        }
    });

    let opts = bt::BtOptions {
        port: config.bt_port,
        max_peers: config.bt_max_peers,
        seed_after_complete: config.bt_seed_after_complete,
        ..Default::default()
    };
    let handle = bt::BtHandle {
        pause: control.pause.clone(),
        cancel: control.cancel.clone(),
    };

    let prog_task = task.clone();
    let on_progress: Arc<dyn Fn(bt::BtStats) + Send + Sync> = Arc::new(move |s: bt::BtStats| {
        if let Ok(mut t) = prog_task.lock() {
            t.downloaded = s.downloaded;
            t.uploaded = s.uploaded;
            t.peers = s.peers;
            t.seeds = s.seeds;
            t.leeches = s.leeches;
            t.pieces_done = s.pieces_done;
            t.pieces_total = s.pieces_total;
            t.total_bytes = t.total_bytes.max(s.downloaded);
            if t.state == TaskState::Connecting && t.total_bytes > 0 {
                t.state = TaskState::Downloading;
            }
        }
    });

    let (link, target) = match source {
        Source::Magnet(m) => (m.clone(), save_dir),
        Source::TorrentFile(p) => {
            let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            (
                canon.to_string_lossy().to_string(),
                save_dir,
            )
        }
        _ => unreachable!(),
    };

    bt::session::run_task(&link, &target, opts, handle, bucket, msg, on_progress).await?;

    {
        let mut t = task.lock().unwrap();
        t.state = TaskState::Completed;
        t.speed_down = 0;
        t.eta = None;
        t.last_message = "下载完成".into();
    }
    let _ = (id, control);
    Ok(())
}

/// 后台 tick：周期刷新速度与剩余时间。
///
/// 必须通过引擎自建的运行时启动：调用时（`main` 里）还没有任何
/// tokio runtime 上下文，直接 `tokio::spawn` 会 panic。
pub fn spawn_ticker(engine: Arc<Mutex<EngineState>>) {
    let runtime = Runtime::new().expect("创建 ticker 运行时失败");
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            let snapshot: Vec<(Arc<Mutex<Task>>, Option<Arc<TaskControl>>)> = {
                let s = engine.lock().unwrap();
                s.tasks
                    .iter()
                    .map(|t| {
                        let id = t.lock().map(|t| t.id).unwrap_or(0);
                        (t.clone(), s.controls.get(&id).cloned())
                    })
                    .collect()
            };
            for (t, c) in snapshot {
                if let (Ok(mut task), Some(ctl)) = (t.lock(), c) {
                    let (down, up) = ctl.compute_speed(task.downloaded, task.uploaded);
                    task.speed_down = down;
                    task.speed_up = up;
                    task.eta = estimate_eta(task.downloaded, task.total_bytes, down);
                }
            }
        }
    });
}

/// 确保目录存在。
pub async fn ensure_dir(p: &Path) -> Result<()> {
    tokio::fs::create_dir_all(p).await?;
    Ok(())
}