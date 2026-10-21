//! HTTP/HTTPS 下载器。
//!
//! 特性：
//! - 流式下载，边下边写，不占用大量内存
//! - 多连接分片加速（服务端支持 Range 时）
//! - `.part` 临时文件 + 断点续传元数据，支持重启后继续
//! - 令牌桶限速、暂停/取消、失败重试

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::common::limits::TokenBucket;
use crate::common::util::percent_decode;

/// 一个分片的下载区间。
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    pub index: u32,
    pub start: u64,
    pub end: u64, // 闭区间
}

impl Segment {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

/// 下载过程中的实时统计，回调给引擎更新 UI。
pub type ProgressHook = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// 下载选项。
pub struct HttpOptions {
    pub connections: usize,
    pub retries: u32,
    pub timeout_secs: u64,
    /// 是否尝试断点续传（读 .part 文件）
    pub resume: bool,
    /// 每完成多少字节触发一次限速等待
    pub throttle_chunk: u64,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            connections: 8,
            retries: 5,
            timeout_secs: 60,
            resume: true,
            throttle_chunk: 64 * 1024,
        }
    }
}

/// HTTP 下载器。
pub struct HttpDownloader {
    client: reqwest::Client,
    opts: HttpOptions,
    bucket: Arc<TokenBucket>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    downloaded: Arc<AtomicU64>,
}

impl HttpDownloader {
    pub fn new(opts: HttpOptions, bucket: Arc<TokenBucket>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("rust-dm/0.1")
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(opts.timeout_secs))
            .pool_max_idle_per_host(opts.connections.max(2))
            .build()
            .context("创建 HTTP 客户端失败")?;
        Ok(Self {
            client,
            opts,
            bucket,
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            downloaded: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn pause_flag(&self) -> Arc<AtomicBool> {
        self.pause.clone()
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn downloaded(&self) -> Arc<AtomicU64> {
        self.downloaded.clone()
    }

    /// 探测资源：返回 (总大小, 是否支持 Range)
    pub async fn probe(&self, url: &str) -> Result<(u64, bool)> {
        // 先用 Range: bytes=0-0 探测，多数服务器会返回 206 + Content-Range
        let resp = self
            .client
            .get(url)
            .header("Range", "bytes=0-0")
            .send()
            .await
            .with_context(|| format!("请求失败：{url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("服务器返回 {}", status.as_u16());
        }
        let supports_range = status == reqwest::StatusCode::PARTIAL_CONTENT
            || resp
                .headers()
                .get(reqwest::header::ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.contains("bytes"))
                .unwrap_or(false);

        let total = if let Some(range) = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
        {
            // Content-Range: bytes 0-0/12345
            range.rsplit('/').next().and_then(|s| s.trim().parse::<u64>().ok())
        } else {
            resp.content_length()
        };
        Ok((total.unwrap_or(0), supports_range))
    }

    /// 执行下载。`target` 为最终文件路径（内部会使用 `.part`）。
    pub async fn download(
        &self,
        url: &str,
        target: &Path,
        hook: ProgressHook,
        message: impl Fn(String) + Send + Sync + 'static,
    ) -> Result<()> {
        let (total, supports_range) = self.probe(url).await?;

        let part_path = with_part_suffix(target);
        if let Some(dir) = target.parent() {
            tokio::fs::create_dir_all(dir).await.ok();
        }

        // 断点续传：读取上次已完成的字节区间
        let done_ranges = if self.opts.resume && supports_range && total > 0 {
            read_resume_ranges(target, total)
        } else {
            Vec::new()
        };
        // 服务端不支持 Range 时只能单流重下，已有进度作废
        let (segments, already) = if supports_range && total > 0 {
            plan_remaining(total, &done_ranges, self.opts.connections)
        } else {
            (Vec::new(), 0)
        };

        let shared_ranges = Arc::new(Mutex::new(done_ranges));
        let res = if total > 0 && already >= total {
            // 续传元数据显示已全部完成：直接收尾，不再发请求
            Ok(())
        } else if segments.is_empty() {
            // 单流下载（不支持 Range、或总大小未知）
            self.download_single(
                url,
                &part_path,
                total,
                already,
                hook,
                message,
                target.to_path_buf(),
                shared_ranges.clone(),
            )
            .await
        } else {
            self.download_multi(
                url,
                &part_path,
                total,
                segments,
                already,
                hook,
                message,
                target.to_path_buf(),
                shared_ranges.clone(),
            )
            .await
        };

        if let Err(e) = res {
            // 中断：把已完成区间落到磁盘，下次启动从这里继续
            if total > 0 {
                let snapshot = shared_ranges.lock().map(|g| g.clone()).unwrap_or_default();
                write_resume_ranges(target, total, &snapshot);
            }
            // .part 文件保留，供下次续传
            return Err(e);
        }

        // 完成后把 .part 重命名为正式文件，并清理续传元数据
        tokio::fs::rename(&part_path, target)
            .await
            .with_context(|| format!("重命名到 {} 失败", target.display()))?;
        clear_resume_file(target);
        Ok(())
    }

    /// 单连接流式下载。
    #[allow(clippy::too_many_arguments)]
    async fn download_single(
        &self,
        url: &str,
        part: &Path,
        total: u64,
        already: u64,
        hook: ProgressHook,
        message: impl Fn(String) + Send + Sync + 'static,
        target: PathBuf,
        shared_ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    ) -> Result<()> {
        let mut attempt = 0u32;
        let mut start = already;
        let (resp, len) = loop {
            if self.cancelled() {
                bail!("已取消");
            }
            let req = self.client.get(url);
            let req = if start > 0 {
                req.header("Range", format!("bytes={start}-"))
            } else {
                req
            };
            match req.send().await {
                Ok(r) => {
                    if r.status().is_success() {
                        let l = r.content_length().unwrap_or(0);
                        break (r, l);
                    } else {
                        let s = r.status();
                        attempt += 1;
                        if attempt > self.opts.retries {
                            bail!("服务器返回 {}", s.as_u16());
                        }
                        message(format!("服务器返回 {}，第 {} 次重试", s.as_u16(), attempt));
                        tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                    }
                }
                Err(e) => {
                    attempt += 1;
                    if attempt > self.opts.retries {
                        return Err(anyhow!("请求失败：{e}"));
                    }
                    message(format!("请求出错：{e}，第 {attempt} 次重试"));
                    tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
                }
            }
        };

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(part)
            .await?;
        file.set_len(start).await?;

        let mut stream = resp.bytes_stream();
        let mut written = start;
        // 单流下载总是「连续前缀」，所以已完成区间就是 [0, written)
        let mut last_saved = start;
        const SAVE_STEP: u64 = 2 * 1024 * 1024;
        while let Some(chunk) = stream.next().await {
            if self.cancelled() {
                bail!("已取消");
            }
            self.wait_if_paused().await;
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            self.downloaded
                .store(written, Ordering::Relaxed);
            self.bucket.acquire(chunk.len() as u64).await;
            hook(written, if total > 0 { total } else { len });

            // 每 2 MB 落一次盘，避免中途退出丢失全部进度
            if written.saturating_sub(last_saved) >= SAVE_STEP {
                last_saved = written;
                if let Ok(mut g) = shared_ranges.lock() {
                    g.push((0, written.saturating_sub(1)));
                }
                let snapshot = shared_ranges.lock().map(|g| g.clone()).unwrap_or_default();
                write_resume_ranges(&target, total, &snapshot);
            }
        }
        file.flush().await?;
        if let Ok(mut g) = shared_ranges.lock() {
            g.push((0, written.saturating_sub(1)));
        }
        Ok(())
    }

    /// 多连接分片下载。
    #[allow(clippy::too_many_arguments)]
    async fn download_multi(
        &self,
        url: &str,
        part: &Path,
        total: u64,
        segments: Vec<Segment>,
        already: u64,
        hook: ProgressHook,
        message: impl Fn(String) + Send + Sync + 'static,
        target: PathBuf,
        shared_ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    ) -> Result<()> {
        message(format!(
            "多连接下载：{} 个分片，总大小 {}",
            segments.len(),
            crate::common::util::human_size(total)
        ));
        self.downloaded.store(already, Ordering::Relaxed);

        // 预先把文件扩展到总长度
        {
            let f = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(part)
                .await?;
            f.set_len(total).await?;
        }

        let total_shared = Arc::new(AtomicU64::new(already));
        let mut handles = Vec::new();

        for seg in segments {
            let client = self.client.clone();
            let url = url.to_string();
            let part = part.to_path_buf();
            let bucket = self.bucket.clone();
            let pause = self.pause.clone();
            let cancel = self.cancel.clone();
            let downloaded = self.downloaded.clone();
            let total_shared = total_shared.clone();
            let chunk = self.opts.throttle_chunk;
            let hook = hook.clone();
            let retries = self.opts.retries;
            let ranges_c = shared_ranges.clone();
            let target_c = target.clone();

            handles.push(tokio::spawn(async move {
                let r = download_segment(
                    client, url, part, seg, bucket, pause, cancel, downloaded,
                    total_shared, chunk, hook, retries, total, ranges_c.clone(),
                )
                .await;
                // 该分片结束（成功或中断）后立刻把进度落盘，
                // 这样即使后续分片出错，已完成的部分也不会白下。
                let snapshot = ranges_c.lock().map(|g| g.clone()).unwrap_or_default();
                write_resume_ranges(&target_c, total, &snapshot);
                r
            }));
        }

        // 汇总各分片结果
        let mut errors = Vec::new();
        let mut cancelled = false;
        for h in handles {
            match h.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    if e.to_string() == "已取消" {
                        cancelled = true;
                    } else {
                        errors.push(e.to_string());
                    }
                }
                Err(e) => errors.push(format!("分片任务异常：{e}")),
            }
        }

        hook(total_shared.load(Ordering::Relaxed), total);

        // 取消必须如实向上报错。否则残缺的 .part 会被当成成品重命名，
        // 用户拿到的是带空洞的损坏文件。
        if cancelled {
            bail!("已取消");
        }
        if !errors.is_empty() {
            bail!("部分分片失败：{}", errors.join("; "));
        }
        Ok(())
    }

    pub fn set_paused(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    async fn wait_if_paused(&self) {
        while self.pause.load(Ordering::Relaxed) {
            if self.cancelled() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
}

/// 分片进度登记器：离开作用域时把「已真实落盘」的区间写进共享列表。
///
/// 中断（取消/暂停/异常）时分片往往只写了一半，这部分字节确实在磁盘上，
/// 不登记就会在续传时被整个重下，白白浪费已完成的传输。
struct SegProgressGuard {
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    base: u64,
    written: Arc<AtomicU64>,
}

impl Drop for SegProgressGuard {
    fn drop(&mut self) {
        let n = self.written.load(Ordering::Relaxed);
        if n == 0 {
            return;
        }
        if let Ok(mut g) = self.ranges.lock() {
            g.push((self.base, self.base + n - 1));
        }
    }
}

/// 单个分片的下载逻辑（阻塞 I/O + 线程池）。
#[allow(clippy::too_many_arguments)]
async fn download_segment(
    client: reqwest::Client,
    url: String,
    part: PathBuf,
    seg: Segment,
    bucket: Arc<TokenBucket>,
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    downloaded: Arc<AtomicU64>,
    total_shared: Arc<AtomicU64>,
    _chunk: u64,
    hook: ProgressHook,
    retries: u32,
    total: u64,
    shared_ranges: Arc<Mutex<Vec<(u64, u64)>>>,
) -> Result<()> {
    let mut attempt = 0u32;
    // 本分片起始偏移：后续所有续传偏移都以它为基准
    let base = seg.start;
    // 已真实写入磁盘的字节数（连续前缀），中断时用于记录已完成区间
    let written = Arc::new(AtomicU64::new(0));
    // 无论正常结束还是被取消/出错，都把实际落盘的区间登记进去
    let _guard = SegProgressGuard {
        ranges: shared_ranges.clone(),
        base,
        written: written.clone(),
    };
    let mut seg = seg;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("已取消");
        }
        // 等待恢复
        while pause.load(Ordering::Relaxed) {
            if cancel.load(Ordering::Relaxed) {
                bail!("已取消");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        let res: Result<(u64, bool)> = async {
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .read(true)
                .open(&part)
                .await
                .context("打开 .part 文件失败")?;
            file.set_len(total.max(seg.end + 1)).await?;
            {
                use tokio::io::AsyncSeekExt;
                file.seek(std::io::SeekFrom::Start(seg.start)).await?;
            }

            let resp = client
                .get(&url)
                .header("Range", format!("bytes={}-{}", seg.start, seg.end))
                .send()
                .await
                .map_err(|e| anyhow!("分片请求失败：{e}"))?;
            if !resp.status().is_success() {
                bail!("分片服务器返回 {}", resp.status().as_u16());
            }

            let mut stream = resp.bytes_stream();
            let mut seg_done = 0u64;

            while let Some(item) = stream.next().await {
                if cancel.load(Ordering::Relaxed) {
                    bail!("已取消");
                }
                if pause.load(Ordering::Relaxed) {
                    // 暂停：刷新后如实上报中断，由上层决定如何续传
                    file.flush().await?;
                    return Ok((seg_done, false));
                }
                let chunk_bytes = item?;
                if chunk_bytes.is_empty() {
                    continue;
                }
                file.write_all(&chunk_bytes).await?;
                seg_done += chunk_bytes.len() as u64;
                written.fetch_add(chunk_bytes.len() as u64, Ordering::Relaxed);

                let done = downloaded.fetch_add(chunk_bytes.len() as u64, Ordering::Relaxed)
                    + chunk_bytes.len() as u64;
                let _ = total_shared.fetch_add(chunk_bytes.len() as u64, Ordering::Relaxed);
                bucket.acquire(chunk_bytes.len() as u64).await;
                hook(done, total);
            }
            file.flush().await?;
            Ok((seg_done, true))
        }
        .await;

        match res {
            Ok((seg_done, finished)) => {
                if !finished {
                    // 等待恢复，然后从断点继续本分片
                    while pause.load(Ordering::Relaxed) {
                        if cancel.load(Ordering::Relaxed) {
                            bail!("已取消");
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    }
                    if cancel.load(Ordering::Relaxed) {
                        bail!("已取消");
                    }
                    seg.start += seg_done;
                    if seg.start > seg.end {
                        return Ok(());
                    }
                    continue;
                }

                // 校验分片是否真的下满，缺字节不能算成功
                let expected = seg.len();
                if seg_done < expected {
                    seg.start += seg_done;
                    attempt += 1;
                    if attempt > retries {
                        bail!(
                            "分片 {} 未下满：{}/{} 字节",
                            seg.index,
                            seg_done,
                            expected
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
                    continue;
                }
                return Ok(());
            }
            Err(e) => {
                attempt += 1;
                if attempt > retries {
                    return Err(e);
                }
                if cancel.load(Ordering::Relaxed) {
                    bail!("已取消");
                }
                tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            }
        }
    }
}

/// 把完整 URL 拆成若干等长分片。
pub fn split_segments(total: u64, connections: usize) -> Vec<Segment> {
    let n = connections.max(1);
    let per = total / n as u64;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let start = i as u64 * per;
        let end = if i == n - 1 {
            total.saturating_sub(1)
        } else {
            (start + per).saturating_sub(1)
        };
        if start <= end {
            out.push(Segment {
                index: i as u32,
                start,
                end,
            });
        }
    }
    out
}

fn with_part_suffix(p: &Path) -> std::path::PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".part");
    std::path::PathBuf::from(s)
}

fn with_resume_suffix(p: &Path) -> std::path::PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".rust-dm-resume");
    std::path::PathBuf::from(s)
}

/// 续传元数据：记录总大小与所有**已完成字节区间**。
///
/// 多连接下载时各分片进度不同步，「已完成 N 字节」这种前缀式记录会把
/// 中间空洞误当成已下载，续传后产出损坏文件。因此这里按区间记录。
pub fn read_resume_ranges(target: &Path, total: u64) -> Vec<(u64, u64)> {
    let path = with_resume_suffix(target);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    let mut saved_total: Option<u64> = None;
    let mut ranges: Vec<(u64, u64)> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("total=") {
            saved_total = v.trim().parse::<u64>().ok();
            continue;
        }
        // 新格式：每行一个闭区间 "start-end"
        let parsed = line
            .split_once('-')
            .map(|(a, b)| (a.trim().parse::<u64>().ok(), b.trim().parse::<u64>().ok()));
        if let Some((Some(s), Some(e))) = parsed {
            if s <= e {
                ranges.push((s, e));
            }
            continue;
        }
        // 兼容早期格式 "total:done"
        if let Some((t, d)) = line.split_once(':') {
            if let (Ok(t), Ok(d)) = (t.trim().parse::<u64>(), d.trim().parse::<u64>()) {
                saved_total = Some(t);
                if d > 0 && d <= t {
                    ranges.push((0, d - 1));
                }
            }
        }
    }

    // 总大小对不上说明远端资源已变，续传信息作废
    match saved_total {
        Some(t) if t == total => merge_ranges(&ranges, total),
        _ => {
            let _ = std::fs::remove_file(&path);
            Vec::new()
        }
    }
}

/// 把区间裁剪到 [0, total) 后排序合并（相邻区间也会合并）。
fn merge_ranges(ranges: &[(u64, u64)], total: u64) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(s, e) in ranges {
        if total > 0 && s >= total {
            continue;
        }
        let e = if total > 0 { e.min(total - 1) } else { e };
        if s <= e {
            v.push((s, e));
        }
    }
    v.sort_unstable_by_key(|r| r.0);

    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            // 相邻或重叠：直接扩展上一段
            Some(last) if s <= last.1.saturating_add(1) => {
                if e > last.1 {
                    last.1 = e;
                }
            }
            _ => out.push((s, e)),
        }
    }
    out
}

/// 求 [0, total) 中尚未完成的空洞区间。
fn gap_ranges(total: u64, ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let merged = merge_ranges(ranges, total);
    let mut gaps = Vec::new();
    let mut cur = 0u64;
    for (s, e) in merged {
        if s > cur {
            gaps.push((cur, s - 1));
        }
        cur = cur.max(e.saturating_add(1));
    }
    if total > 0 && cur < total {
        gaps.push((cur, total - 1));
    }
    gaps
}

/// 根据已完成区间规划剩余下载任务，返回 (分片列表, 已完成字节数)。
///
/// 连接数按空洞长度比例分配，长空洞拿更多连接；每个空洞至少 1 个连接。
pub fn plan_remaining(total: u64, ranges: &[(u64, u64)], connections: usize) -> (Vec<Segment>, u64) {
    if total == 0 {
        return (Vec::new(), 0);
    }
    let gaps = gap_ranges(total, ranges);
    let remaining: u64 = gaps.iter().map(|(s, e)| e - s + 1).sum();
    if remaining == 0 {
        return (Vec::new(), total);
    }

    let n = connections.max(1).min(remaining as usize);
    let mut out: Vec<Segment> = Vec::new();
    let mut assigned = 0usize;
    for (idx, &(s, e)) in gaps.iter().enumerate() {
        let len = e - s + 1;
        let left = n.saturating_sub(assigned).max(1);
        let k = if idx == gaps.len() - 1 {
            left
        } else {
            let share = (len as u128 * n as u128) / remaining as u128;
            usize::try_from(share.max(1)).unwrap_or(usize::MAX).min(left)
        };
        assigned += k;

        let per = len / k as u64;
        for i in 0..k {
            let st = s + i as u64 * per;
            let en = if i == k - 1 { e } else { st + per - 1 };
            out.push(Segment {
                index: out.len() as u32,
                start: st,
                end: en,
            });
        }
    }
    (out, total - remaining)
}

/// 写入续传元数据（先写临时文件再原子改名，避免中途崩溃留下半截文件）。
pub fn write_resume_ranges(target: &Path, total: u64, ranges: &[(u64, u64)]) {
    let path = with_resume_suffix(target);
    let merged = merge_ranges(ranges, total);
    if merged.is_empty() || total == 0 {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let mut content = format!("total={total}\n");
    for (s, e) in &merged {
        content.push_str(&format!("{s}-{e}\n"));
    }
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, content).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// 删除续传元数据（下载完成后调用）。
pub fn clear_resume_file(target: &Path) {
    let _ = std::fs::remove_file(with_resume_suffix(target));
}

/// 从 Content-Disposition 解析文件名。
pub fn name_from_headers(headers: &reqwest::header::HeaderMap, url: &str) -> String {
    if let Some(cd) = headers.get(reqwest::header::CONTENT_DISPOSITION) {
        if let Ok(s) = cd.to_str() {
            if let Some(idx) = s.find("filename=") {
                let raw = &s[idx + 9..];
                let raw = raw.trim_start_matches('"').trim_end_matches('"');
                let raw = raw.trim_end_matches(';');
                if !raw.is_empty() {
                    return crate::common::util::sanitize_file_name(&percent_decode(raw));
                }
            }
        }
    }
    crate::common::util::filename_from_url(url)
}