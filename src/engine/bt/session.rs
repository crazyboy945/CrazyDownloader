//! BitTorrent 下载会话：调度 piece/block、连接 peer、校验与落盘。
//!
//! 并发模型：
//! - 主循环负责选块（rarest-first）与状态维护
//! - 每个 peer 一个读取任务，把收到的数据经由 channel 汇总到主循环
//! - 所有文件写入在主循环串行进行，避免多任务同时写同一文件

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::common::limits::TokenBucket;

use super::bencode::Bencode;
use super::dht;
use super::metainfo::{parse_info_dict, parse_magnet, parse_torrent, Metainfo, TorrentFile};
use super::peer::{self, PeerConnection, PeerMessage};
use super::tracker::{announce, tracker_client, AnnounceParams, DEFAULT_TRACKERS};

/// 标准 block 大小 16 KiB（BEP 3）
pub const BLOCK_LEN: u32 = 16 * 1024;
/// 元数据分片大小（BEP 9）
pub const METADATA_PIECE_LEN: usize = 16 * 1024;
/// 获取元数据时同时连接的 peer 数
const META_CONCURRENCY: usize = 16;

/// 分片内的一个块。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId {
    pub piece: u32,
    pub index: u32,
}

impl BlockId {
    pub fn begin(&self) -> u32 {
        self.index * BLOCK_LEN
    }

    /// 该块的实际长度（分片末尾可能不足 16 KiB）。
    pub fn length(&self, meta: &Metainfo) -> u32 {
        let piece_len = meta.piece_len(self.piece as usize);
        let begin = self.begin();
        if begin >= piece_len {
            0
        } else {
            (piece_len - begin).min(BLOCK_LEN)
        }
    }
}

/// 分片级状态。
#[derive(Default)]
struct PieceState {
    /// 校验通过、已完成的分片
    completed: HashSet<u32>,
    /// 已发出请求、等待回包的块
    pending: HashSet<BlockId>,
    /// 已收到并写入磁盘的块
    received: HashMap<u32, HashSet<u32>>,
}

impl PieceState {
    fn mark_received(&mut self, id: BlockId) -> bool {
        self.pending.remove(&id);
        let s = self.received.entry(id.piece).or_default();
        if s.insert(id.index) {
            true
        } else {
            false
        }
    }

    /// 该分片是否所有块都已收到。
    fn is_complete(&self, meta: &Metainfo, piece: u32) -> bool {
        let piece_len = meta.piece_len(piece as usize);
        if piece_len == 0 {
            return true;
        }
        let total = piece_len.div_ceil(BLOCK_LEN);
        let got = self.received.get(&piece).map(|s| s.len() as u32).unwrap_or(0);
        got >= total
    }
}

/// 单个 peer 的连接状态（只保留写侧，读取在独立任务中）。
struct PeerState {
    addr: SocketAddr,
    /// 写侧。发送请求期间会被临时取出（`take`），发完放回，
    /// 以避免持锁跨 `.await`。
    write: Option<OwnedWriteHalf>,
    bitfield: Option<Vec<bool>>,
    choking: bool,
    /// 已发出但未收回的块数
    outstanding: u32,
}

type OwnedWriteHalf = tokio::net::tcp::OwnedWriteHalf;
type OwnedReadHalf = tokio::net::tcp::OwnedReadHalf;

/// 运行期统计，回传给 UI。
#[derive(Debug, Clone, Default)]
pub struct BtStats {
    pub downloaded: u64,
    pub uploaded: u64,
    pub peers: usize,
    pub seeds: usize,
    pub leeches: usize,
    pub pieces_done: usize,
    pub pieces_total: usize,
    pub speed_down: u64,
    pub speed_up: u64,
}

/// 运行期控制。
#[derive(Clone)]
pub struct BtHandle {
    pub pause: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
}

impl Default for BtHandle {
    fn default() -> Self {
        Self {
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 进度消息回调。用 `Arc` 包装以便在多处`clone()` 后共用同一个回调。
pub type Msg = Arc<dyn Fn(String) + Send + Sync>;

/// BT 任务选项。
pub struct BtOptions {
    pub port: u16,
    pub max_peers: usize,
    pub seed_after_complete: bool,
    /// 单个 peer 的块流水线深度
    pub max_inflight_per_peer: u32,
}

impl Default for BtOptions {
    fn default() -> Self {
        Self {
            port: 6881,
            max_peers: 50,
            seed_after_complete: true,
            max_inflight_per_peer: 8,
        }
    }
}

/// magnet / torrent 的启动信息。
pub struct BtStart {
    pub info_hash: [u8; 20],
    pub trackers: Vec<String>,
    /// magnet 的 `x.pe=` 直连 peer（"host:port" 字符串，异步解析）
    pub peers: Vec<String>,
    pub metainfo: Option<Metainfo>,
    pub display_name: Option<String>,
}

/// 解析 magnet 链接。
pub fn prepare_from_magnet(link: &str) -> Result<BtStart> {
    let m = parse_magnet(link)?;
    Ok(BtStart {
        info_hash: m.info_hash,
        trackers: m.trackers,
        peers: m.peers,
        metainfo: m.meta.as_ref().and_then(|b| parse_torrent(&b.encode()).ok()),
        display_name: m.name,
    })
}

/// 解析 .torrent 内容。
pub fn prepare_from_torrent(raw: &[u8]) -> Result<(BtStart, Metainfo)> {
    let meta = parse_torrent(raw)?;
    let trackers = super::metainfo::trackers_of(&meta);
        Ok((
        BtStart {
            info_hash: meta.info_hash,
            trackers,
            peers: Vec::new(),
            metainfo: Some(meta.clone()),
            display_name: Some(meta.name.clone()),
        },
        meta,
    ))
}

/// 下载会话。
pub struct BtSession {
    meta: Metainfo,
    save_path: PathBuf,
    opts: BtOptions,
    handle: BtHandle,
    bucket: Arc<TokenBucket>,
    stats: Arc<Mutex<BtStats>>,
    dl_bytes: Arc<AtomicU64>,
}

impl BtSession {
    pub fn new(
        meta: Metainfo,
        save_path: PathBuf,
        opts: BtOptions,
        handle: BtHandle,
        bucket: Arc<TokenBucket>,
    ) -> Self {
        Self {
            meta,
            save_path,
            opts,
            handle,
            bucket,
            stats: Arc::new(Mutex::new(BtStats::default())),
            dl_bytes: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn meta(&self) -> &Metainfo {
        &self.meta
    }

    pub fn stats_handle(&self) -> Arc<Mutex<BtStats>> {
        self.stats.clone()
    }

    fn cancelled(&self) -> bool {
        self.handle.cancel.load(Ordering::Relaxed)
    }

    /// 预分配磁盘空间。
    async fn allocate_files(&self, msg: &Msg) -> Result<()> {
        for f in &self.meta.files {
            let full = self.file_path_for(f);
            if let Some(dir) = full.parent() {
                tokio::fs::create_dir_all(dir).await.ok();
            }
            let need = match tokio::fs::metadata(&full).await {
                Ok(md) => md.len() != f.length,
                Err(_) => true,
            };
            if need {
                let file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&full)
                    .await
                    .with_context(|| format!("创建 {} 失败", full.display()))?;
                file.set_len(f.length).await?;
            }
        }
        msg(format!(
            "已预分配 {} 个文件，共 {}",
            self.meta.files.len(),
            crate::common::util::human_size(self.meta.total_length)
        ));
        Ok(())
    }

    /// 校验本地已有数据，返回已完成的分片集合。
    async fn verify_existing(&self, msg: Msg) -> Result<HashSet<u32>> {
        let mut done = HashSet::new();
        let total = self.meta.piece_count();
        if total == 0 {
            return Ok(done);
        }
        msg(format!("开始校验已有数据（{total} 个分片）"));

        let mut buf = vec![0u8; self.meta.piece_len(0) as usize];
        for i in 0..total {
            if self.cancelled() {
                bail!("已取消");
            }
            let want = self.meta.piece_len(i);
            buf.resize(want as usize, 0);
            if self.read_piece(i as u32, &mut buf).await? != want as usize {
                break;
            }
            let mut h = Sha1::new();
            h.update(&buf);
            let got: [u8; 20] = h.finalize().into();
            if Some(got) == self.meta.piece_hash(i) {
                done.insert(i as u32);
            }
            if i % 16 == 0 {
                msg(format!("校验进度 {}/{}", i, total));
            }
        }
        msg(format!("校验完成，{} 个分片已有效", done.len()));
        Ok(done)
    }

    /// 分片索引 -> 文件路径。
    fn file_path_for(&self, f: &TorrentFile) -> PathBuf {
        if self.meta.is_multi {
            let mut p = self.save_path.clone();
            for seg in &f.path {
                p = p.join(seg);
            }
            p
        } else {
            self.save_path.join(f.path.first().map(|s| s.as_str()).unwrap_or("data"))
        }
    }

    /// 读回整个分片（可能跨文件）。
    async fn read_piece(&self, piece: u32, buf: &mut [u8]) -> Result<usize> {
        let mut written = 0usize;
        let mut cursor = piece as u64 * self.meta.piece_length as u64;
        let end_piece = cursor + self.meta.piece_len(piece as usize) as u64;

        for f in &self.meta.files {
            if written >= buf.len() || cursor >= end_piece {
                break;
            }
            let f_start = cursor;
            let f_end = f_start + f.length;
            if cursor >= f_end {
                continue;
            }
            let inner = cursor - f_start;
            let avail = f.length - inner;
            let want = ((buf.len() - written) as u64).min(avail);
            let want = want.min(end_piece - cursor) as usize;
            if want == 0 {
                break;
            }
            let path = self.file_path_for(f);
            let mut fh = tokio::fs::File::open(&path).await?;
            {
                use tokio::io::AsyncSeekExt;
                fh.seek(std::io::SeekFrom::Start(inner)).await?;
            }
            let mut tmp = vec![0u8; want];
            {
                use tokio::io::AsyncReadExt;
                fh.read_exact(&mut tmp).await?;
            }
            buf[written..written + want].copy_from_slice(&tmp);
            written += want;
            cursor += want as u64;
        }
        Ok(written)
    }

    /// 写入一个块。
    async fn write_block(&self, byte_offset: u64, data: &[u8]) -> Result<()> {
        let mut cursor = byte_offset;
        let end = byte_offset + data.len() as u64;
        let mut slice = data;

        for f in &self.meta.files {
            if slice.is_empty() || cursor >= end {
                break;
            }
            let f_start = cursor;
            let f_end = f_start + f.length;
            if cursor >= f_end {
                continue;
            }
            let inner = cursor - f_start;
            let avail = (f.length - inner) as usize;
            let take = avail.min(slice.len());
            let path = self.file_path_for(f);
            let mut fh = tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&path)
                .await?;
            {
                use tokio::io::AsyncSeekExt;
                fh.seek(std::io::SeekFrom::Start(inner)).await?;
            }
            fh.write_all(&slice[..take]).await?;
            fh.flush().await?;
            slice = &slice[take..];
            cursor += take as u64;
        }
        Ok(())
    }

    /// 读回并校验一个分片。
    async fn verify_piece(&self, piece: u32) -> Result<bool> {
        let len = self.meta.piece_len(piece as usize) as usize;
        if len == 0 {
            return Ok(false);
        }
        let mut buf = vec![0u8; len];
        if self.read_piece(piece, &mut buf).await? != len {
            return Ok(false);
        }
        let mut h = Sha1::new();
        h.update(&buf);
        let got: [u8; 20] = h.finalize().into();
        Ok(self.meta.piece_hash(piece as usize) == Some(got))
    }

    /// 已校验完成的分片对应的字节数。
    fn bytes_of_completed(&self, state: &PieceState) -> u64 {
        let mut n = 0u64;
        for p in &state.completed {
            n += self.meta.piece_len(*p as usize) as u64;
        }
        n.min(self.meta.total_length)
    }

    /// 主下载循环。
    pub async fn run(
        &self,
        msg: Msg,
        on_progress: Arc<dyn Fn(BtStats) + Send + Sync>,
    ) -> Result<()> {
        tokio::fs::create_dir_all(&self.save_path).await.ok();
        self.allocate_files(&msg).await?;

        let existing = self.verify_existing(msg.clone()).await?;
        let piece_count = self.meta.piece_count();
        if existing.len() == piece_count && piece_count > 0 {
            msg("本地数据已完整，无需下载".into());
            {
                let mut s = self.stats.lock().unwrap();
                s.downloaded = self.meta.total_length;
                s.pieces_done = piece_count;
                s.pieces_total = piece_count;
            }
            on_progress(self.stats.lock().unwrap().clone());
            return Ok(());
        }

        let mut state = PieceState {
            completed: existing,
            pending: HashSet::new(),
            received: HashMap::new(),
        };
        // 已完成的分片无需再收块
        for p in state.completed.clone() {
            state.received.insert(p, HashSet::new());
        }

        let (addr_tx, mut addr_rx) = mpsc::unbounded_channel::<SocketAddr>();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<BtEvent>();
        let peer_id = super::metainfo::random_peer_id();

        {
            let mut s = self.stats.lock().unwrap();
            s.downloaded = self.bytes_of_completed(&state);
            s.pieces_done = state.completed.len();
            s.pieces_total = piece_count;
        }
        on_progress(self.stats.lock().unwrap().clone());

        // tracker 循环
        let trackers = super::metainfo::trackers_of(&self.meta);
        let tracker_task = tokio::spawn(tracker_loop(
            trackers,
            self.meta.info_hash,
            self.opts.port,
            self.opts.max_peers,
            addr_tx.clone(),
            msg.clone(),
        ));

        let peers: Arc<Mutex<Vec<PeerState>>> = Arc::new(Mutex::new(Vec::new()));
        let mut peer_counts = PeerCounts::default();
        let mut last_report = std::time::Instant::now();
        let mut last_dl = self.bytes_of_completed(&state);

        loop {
            if self.cancelled() {
                break;
            }
            while self.handle.pause.load(Ordering::Relaxed) && !self.cancelled() {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            if self.cancelled() {
                break;
            }

            // ---- 连接新 peer ----
            let live = peers.lock().unwrap().len();
            if live < self.opts.max_peers {
                let mut budget = self.opts.max_peers - live;
                while budget > 0 {
                    let Ok(addr) = addr_rx.try_recv() else { break };
                    if addr.ip().is_unspecified() {
                        break;
                    }
                    if peers.lock().unwrap().iter().any(|p| p.addr == addr) {
                        continue;
                    }
                    budget -= 1;
                    let peers_c = peers.clone();
                    let ev_tx = ev_tx.clone();
                    let ih = self.meta.info_hash;
                    let pid = peer_id;
                    tokio::spawn(async move {
                        match connect_peer(addr, ih, pid).await {
                            Ok((conn, _remote_id)) => {
                                // 用 into_split 拿到 owned 两半：
                                // 写侧要存进 PeerState 长期持有，读侧交给独立任务。
                                let (r, mut w) = conn.stream.into_split();
                                // 发送 interested 表明需求
                                {
                                    let mut tmp = PeerConnection {
                                        stream: &mut w,
                                        peer_bitfield: None,
                                        choking: true,
                                        peer_interested: false,
                                        ext_local: None,
                                        ext_remote: None,
                                        ext_supported: false,
                                        peer_id: pid,
                                    };
                                    let _ = peer::send_message(
                                        &mut tmp,
                                        &peer::frame(super::peer::MsgId::Interested, &[]),
                                    )
                                    .await;
                                }
                                peers_c.lock().unwrap().push(PeerState {
                                    addr,
                                    write: Some(w),
                                    bitfield: None,
                                    choking: true,
                                    outstanding: 0,
                                });
                                let _ = ev_tx.send(BtEvent::PeerAdded(addr));
                                spawn_peer_reader(addr, r, ev_tx);
                            }
                            Err(_) => {
                                let _ = ev_tx.send(BtEvent::PeerFailed(addr));
                            }
                        }
                    });
                }
            }

            // ---- 处理 peer 事件 ----
            let mut dirty = false;
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    BtEvent::PeerAdded(_) => {}
                    BtEvent::PeerFailed(_) => {}
                    BtEvent::Disconnected(addr) => {
                        let mut list = peers.lock().unwrap();
                        if let Some(pos) = list.iter().position(|p| p.addr == addr) {
                            list.remove(pos);
                        }
                        drop(list);
                        dirty = true;
                    }
                    BtEvent::Choking(addr, choking) => {
                        if let Some(p) = peers.lock().unwrap().iter_mut().find(|p| p.addr == addr) {
                            p.choking = choking;
                        }
                    }
                    BtEvent::Have(addr, index) => {
                        if let Some(p) = peers.lock().unwrap().iter_mut().find(|p| p.addr == addr) {
                            if let Some(bf) = p.bitfield.as_mut() {
                                if let Some(slot) = bf.get_mut(index as usize) {
                                    *slot = true;
                                }
                            }
                        }
                        dirty = true;
                    }
                    BtEvent::Bitfield(addr, bf) => {
                        if let Some(p) = peers.lock().unwrap().iter_mut().find(|p| p.addr == addr) {
                            p.bitfield = Some(bf);
                        }
                        dirty = true;
                    }
                    BtEvent::Data {
                        addr,
                        piece,
                        begin,
                        data,
                    } => {
                        dirty = true;
                        if let Some(p) = peers.lock().unwrap().iter_mut().find(|p| p.addr == addr) {
                            if p.outstanding > 0 {
                                p.outstanding -= 1;
                            }
                        }
                        let block_index = begin / BLOCK_LEN;
                        let id = BlockId {
                            piece,
                            index: block_index,
                        };
                        let expect = id.length(&self.meta);
                        if expect == 0 || data.len() as u32 != expect {
                            continue;
                        }
                        let byte_offset = piece as u64 * self.meta.piece_length as u64 + begin as u64;
                        if self.write_block(byte_offset, &data).await.is_err() {
                            continue;
                        }
                        self.dl_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                        self.bucket.acquire(data.len() as u64).await;
                        state.mark_received(id);

                        // 分片收满则校验
                        if state.is_complete(&self.meta, piece)
                            && !state.completed.contains(&piece)
                        {
                            match self.verify_piece(piece).await {
                                Ok(true) => {
                                    state.completed.insert(piece);
                                    state.received.remove(&piece);
                                    let done = state.completed.len();
                                    msg(format!(
                                        "分片校验通过 {done}/{piece_count}，进度 {:.1}%",
                                        self.bytes_of_completed(&state) as f64
                                            / self.meta.total_length.max(1) as f64
                                            * 100.0
                                    ));
                                }
                                Ok(false) => {
                                    // 校验失败：清空该分片的块，重下
                                    msg(format!("分片 {piece} 校验失败，将重新下载"));
                                    state.received.insert(piece, HashSet::new());
                                    state.pending.retain(|b| b.piece != piece);
                                    // 重置各 peer 的在途计数，否则会一直卡在
                                    // outstanding 已满而不再请求新块。
                                    for p in peers.lock().unwrap().iter_mut() {
                                        p.outstanding = 0;
                                    }
                                }
                                Err(e) => {
                                    msg(format!("分片 {piece} 校验出错：{e}"));
                                }
                            }
                        }
                    }
                }
            }

            // ---- 选块并发出请求 ----
            {
                let avail = {
                    let list = peers.lock().unwrap();
                    list.iter()
                        .filter_map(|p| p.bitfield.clone())
                        .collect::<Vec<_>>()
                };
                let mut avail_count: HashMap<u32, u32> = HashMap::new();
                for bf in &avail {
                    for (i, has) in bf.iter().enumerate() {
                        if *has {
                            *avail_count.entry(i as u32).or_insert(0) += 1;
                        }
                    }
                }
                peer_counts.seeds = avail_count.get(&0).copied().unwrap_or(0);
                peer_counts.leeches = avail.len().saturating_sub(peer_counts.seeds as usize);

                let total_inflight = self.opts.max_inflight_per_peer as usize * self.opts.max_peers;
                let picks = select_blocks(
                    &state,
                    &self.meta,
                    &avail_count,
                    total_inflight.min(512) as u32,
                );

                if !picks.is_empty() {
                    // 规划阶段（持锁） + 发送阶段（不持锁）。
                    // 若在持锁时 .await，MutexGuard 会跨 await 使 future 失去 Send。
                    let mut plan: Vec<(usize, BlockId, u32)> = Vec::new();
                    // 把要发送的 peer 的写侧临时取出，避免跨 await 借用
                    let mut taken: std::collections::BTreeMap<usize, OwnedWriteHalf> =
                        Default::default();
                    {
                        let mut list = peers.lock().unwrap();
                        let mut qi = 0usize;
                        for (idx, p) in list.iter_mut().enumerate() {
                            if p.choking || p.outstanding >= self.opts.max_inflight_per_peer {
                                continue;
                            }
                            let Some(bf) = p.bitfield.clone() else { continue };
                            while p.outstanding < self.opts.max_inflight_per_peer
                                && qi < picks.len()
                            {
                                let block = picks[qi];
                                qi += 1;
                                let has = bf.get(block.piece as usize).copied().unwrap_or(false);
                                if !has {
                                    continue;
                                }
                                let len = block.length(&self.meta);
                                if len == 0 {
                                    continue;
                                }
                                plan.push((idx, block, len));
                                p.outstanding += 1;
                            }
                            if qi >= picks.len() {
                                break;
                            }
                        }
                        for idx in plan.iter().map(|(i, _, _)| *i) {
                            if let Some(w) = list[idx].write.take() {
                                taken.insert(idx, w);
                            }
                        }
                    }

                    let mut failed: Option<usize> = None;
                    for (idx, block, len) in plan {
                        let Some(w) = taken.get_mut(&idx) else { break };
                        let mut tmp = PeerConnection {
                            stream: w,
                            peer_bitfield: None,
                            choking: false,
                            peer_interested: false,
                            ext_local: None,
                            ext_remote: None,
                            ext_supported: false,
                            peer_id: [0u8; 20],
                        };
                        let ok = peer::send_message(
                            &mut tmp,
                            &peer::build_request(block.piece, block.begin(), len),
                        )
                        .await
                        .is_ok();
                        if !ok {
                            failed = Some(idx);
                            break;
                        }
                        state.pending.insert(block);
                    }

                    // 统一把写侧放回，并回滚失败 peer 的在途计数
                    {
                        let mut list = peers.lock().unwrap();
                        for (idx, w) in taken {
                            if let Some(p) = list.get_mut(idx) {
                                p.write = Some(w);
                            }
                        }
                        if let Some(idx) = failed {
                            if let Some(p) = list.get_mut(idx) {
                                p.outstanding = p.outstanding.saturating_sub(1);
                            }
                        }
                    }
                }
            }

            // ---- 更新统计 ----
            if last_report.elapsed() >= Duration::from_millis(500) {
                let now_bytes = self.bytes_of_completed(&state);
                let dt = last_report.elapsed().as_secs_f64().max(0.001);
                let speed = now_bytes.saturating_sub(last_dl) as f64 / dt;
                let done = state.completed.len();
                let plen = self.meta.piece_length as f64;
                let mut s = self.stats.lock().unwrap();
                s.downloaded = now_bytes;
                s.speed_down = speed as u64;
                s.pieces_done = done;
                s.pieces_total = piece_count;
                s.peers = peers.lock().unwrap().len();
                s.seeds = peer_counts.seeds as usize;
                s.leeches = peer_counts.leeches;
                s.uploaded = 0;
                let _ = plen;
                drop(s);
                last_dl = now_bytes;
                last_report = std::time::Instant::now();
                on_progress(self.stats.lock().unwrap().clone());
            }

            if state.completed.len() >= piece_count {
                break;
            }

            // 没有数据流动时稍作等待
            if !dirty {
                tokio::time::sleep(Duration::from_millis(120)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        tracker_task.abort();

        {
            let mut s = self.stats.lock().unwrap();
            s.downloaded = self.meta.total_length;
            s.pieces_done = piece_count;
            s.speed_down = 0;
        }
        Ok(())
    }
}

/// peer 池的统计。
#[derive(Default)]
struct PeerCounts {
    seeds: u32,
    leeches: usize,
}

/// peer 读取任务把消息转成事件。
enum BtEvent {
    PeerAdded(SocketAddr),
    PeerFailed(SocketAddr),
    Disconnected(SocketAddr),
    Choking(SocketAddr, bool),
    Have(SocketAddr, u32),
    Bitfield(SocketAddr, Vec<bool>),
    Data {
        addr: SocketAddr,
        piece: u32,
        begin: u32,
        data: Vec<u8>,
    },
}

/// 为单个 peer 启动持续读取的任务。
fn spawn_peer_reader(addr: SocketAddr, mut read: OwnedReadHalf, ev_tx: mpsc::UnboundedSender<BtEvent>) {
    tokio::spawn(async move {
        let mut conn = PeerConnection {
            stream: &mut read,
            peer_bitfield: None,
            choking: true,
            peer_interested: false,
            ext_local: None,
            ext_remote: None,
            ext_supported: false,
            peer_id: [0u8; 20],
        };
        loop {
            match peer::read_message(&mut conn).await {
                Ok(Some(PeerMessage::Piece(piece, begin, data))) => {
                    if ev_tx
                        .send(BtEvent::Data {
                            addr,
                            piece,
                            begin,
                            data,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Some(PeerMessage::Bitfield(bf))) => {
                    let _ = ev_tx.send(BtEvent::Bitfield(addr, bf));
                }
                Ok(Some(PeerMessage::Have(idx))) => {
                    let _ = ev_tx.send(BtEvent::Have(addr, idx));
                }
                Ok(Some(PeerMessage::Unchoke)) => {
                    let _ = ev_tx.send(BtEvent::Choking(addr, false));
                }
                Ok(Some(PeerMessage::Choke)) => {
                    let _ = ev_tx.send(BtEvent::Choking(addr, true));
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let _ = ev_tx.send(BtEvent::Disconnected(addr));
    });
}

/// 连接单个 peer 并完成握手，返回连接与其 peer id。
async fn connect_peer(
    addr: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
) -> Result<(PeerConnection<TcpStream>, [u8; 20])> {
    let stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(addr))
        .await
        .context("连接 peer 超时")??;
    let _ = stream.set_nodelay(true);
    let mut conn = PeerConnection::new(stream, peer_id);
    peer::send_handshake(&mut conn, &info_hash, &peer_id).await?;
    let remote_hash = peer::read_handshake(&mut conn).await?;
    if remote_hash != info_hash {
        bail!("info hash 不匹配");
    }
    let rid = conn.peer_id;
    Ok((conn, rid))
}

/// 周期性向 tracker 公告并把 peer 送入队列。
async fn tracker_loop(
    trackers: Vec<String>,
    info_hash: [u8; 20],
    port: u16,
    numwant: usize,
    addr_tx: mpsc::UnboundedSender<SocketAddr>,
    msg: Msg,
) {
    let peer_id = super::metainfo::random_peer_id();
    let client = tracker_client().unwrap_or_else(|_| reqwest::Client::new());
    let mut interval = 1800u64;
    // tracker 全部失效时的兜底：DHT 网络（BEP 5）
    let mut dht: Option<dht::Dht> = None;

    loop {
        let mut got_any = false;
        for t in trackers.iter() {
            let p = AnnounceParams {
                info_hash,
                peer_id,
                port,
                uploaded: 0,
                downloaded: 0,
                left: 0,
                event: "started",
                numwant: numwant as u32,
            };
            match announce(t, &p, &client).await {
                Ok(res) => {
                    let got = res.peers.len();
                    msg(format!("tracker 返回 {got} 个 peer（{t}）"));
                    for peer in res.peers {
                        if peer.port > 0 {
                            let _ = addr_tx.send(SocketAddr::new(peer.ip, peer.port));
                        }
                    }
                    if got > 0 {
                        got_any = true;
                    }
                    interval = res.interval.clamp(30, 1800);
                    break;
                }
                Err(e) => msg(format!("tracker 失败（{t}）：{e}")),
            }
        }

        if !got_any {
            // 所有 tracker 都没给 peer，转到 DHT 找（只在首次失败时初始化）
            if dht.is_none() {
                match dht::Dht::start(port).await {
                    Ok(node) => {
                        if let Err(e) = node.bootstrap().await {
                            msg(format!("DHT 接入失败：{e}"));
                        }
                        dht = Some(node);
                    }
                    Err(e) => msg(format!("DHT 启动失败：{e}")),
                }
            }
            if let Some(node) = &dht {
                let found = node.get_peers(info_hash, Duration::from_secs(25)).await;
                msg(format!("DHT 返回 {} 个 peer", found.len()));
                for p in found {
                    if p.port() > 0 {
                        let _ = addr_tx.send(p);
                        got_any = true;
                    }
                }
            }
        }

        if got_any {
            // 拿到 peer 后先等一段时间再重新公告
            tokio::time::sleep(Duration::from_secs(interval.max(30))).await;
        } else {
            // 没拿到就 30 秒后重试
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

/// rarest-first 选块：稀有分片优先，同稀有度按序号推进。
fn select_blocks(
    state: &PieceState,
    meta: &Metainfo,
    avail_count: &HashMap<u32, u32>,
    max_total: u32,
) -> Vec<BlockId> {
    let mut out = Vec::new();
    if max_total == 0 {
        return out;
    }
    let piece_count = meta.piece_count();
    let mut candidates: Vec<u32> = (0..piece_count as u32)
        .filter(|p| !state.completed.contains(p))
        .collect();
    candidates.sort_by_key(|p| (avail_count.get(p).copied().unwrap_or(0), *p));

    for piece in candidates {
        if out.len() >= max_total as usize {
            break;
        }
        let piece_len = meta.piece_len(piece as usize);
        if piece_len == 0 {
            continue;
        }
        let blocks = piece_len.div_ceil(BLOCK_LEN);
        let received = state.received.get(&piece);
        for b in 0..blocks {
            let id = BlockId {
                piece,
                index: b,
            };
            if state.pending.contains(&id) {
                continue;
            }
            if received.map(|s| s.contains(&b)).unwrap_or(false) {
                continue;
            }
            out.push(id);
            if out.len() >= max_total as usize {
                break;
            }
        }
    }
    out
}

/// 顶层入口：运行一个 BT 任务直到完成。
pub async fn run_task(
    source: &str,
    save_dir: &Path,
    opts: BtOptions,
    handle: BtHandle,
    bucket: Arc<TokenBucket>,
    msg: Msg,
    on_progress: Arc<dyn Fn(BtStats) + Send + Sync>,
) -> Result<()> {
    let (meta, name) = if source.trim_start().to_ascii_lowercase().starts_with("magnet:") {
        let start = prepare_from_magnet(source)?;
        msg("magnet 链接已解析，正在获取元数据…".to_string());
        let meta = fetch_metainfo(&start, &msg).await?;
        let name = meta.name.clone();
        (meta, name)
    } else {
        let raw = tokio::fs::read(source)
            .await
            .with_context(|| format!("读取 {source} 失败"))?;
        let (_s, meta) = prepare_from_torrent(&raw)?;
        let name = meta.name.clone();
        (meta, name)
    };

    let dir = save_dir.join(crate::common::util::sanitize_file_name(&name));
    let session = BtSession::new(meta, dir, opts, handle, bucket);
    session.run(msg, on_progress).await
}

/// 通过 ut_metadata（BEP 9）从 peer 获取 magnet 元数据。
///
/// 查找 peer 的顺序：
/// 1. 并发向 magnet 自带的 tracker + 内置默认 tracker 公告
/// 2. peer 不足时接入 DHT 网络（BEP 5）继续找
/// 3. 分批并发向 peer 请求元数据分片并校验 info hash
pub async fn fetch_metainfo(start: &BtStart, msg: &Msg) -> Result<Metainfo> {
    if let Some(m) = &start.metainfo {
        return Ok(m.clone());
    }
    let trackers = merge_trackers(&start.trackers);
    let peer_id = super::metainfo::random_peer_id();
    let port = 6881u16;

    // magnet 自带的 x.pe（BEP 9）优先：这些地址是发布者直接给的，通常最可靠
    let mut peers = resolve_peers(&start.peers).await;
    if !peers.is_empty() {
        msg(format!("magnet 直接给出了 {} 个 peer", peers.len()));
    }

    msg(format!("正在向 {} 个 tracker 查询 peer…", trackers.len()));
    let from_trackers = announce_all(&trackers, start.info_hash, peer_id, port, msg).await;
    msg(format!("tracker 共返回 {} 个 peer", from_trackers.len()));
    let mut seen: HashSet<SocketAddr> = peers.iter().copied().collect();
    for p in from_trackers {
        if seen.insert(p) {
            peers.push(p);
        }
    }

    // magnet 里的 tracker 十有八九已经失效，peer 不够就转向 DHT
    if peers.len() < 8 {
        msg(format!("peer 数量不足（{}），正在接入 DHT 网络查找…", peers.len()));
        match dht_find_peers(start.info_hash, port, Duration::from_secs(45), msg).await {
            Ok(found) => {
                msg(format!("DHT 返回 {} 个 peer", found.len()));
                for p in found {
                    if seen.insert(p) {
                        peers.push(p);
                    }
                }
            }
            Err(e) => msg(format!("DHT 查找失败：{e}")),
        }
    }

    if peers.is_empty() {
        bail!(
            "没有找到任何 peer：已尝试 {} 个 tracker 与 DHT 网络。\
常见原因：种子已无人做种、magnet 自带 tracker 全部失效且 DHT 被防火墙/NAT 阻断（UDP 需放行）",
            trackers.len()
        );
    }

    msg(format!("共 {} 个 peer，开始并发获取元数据", peers.len()));
    peers.truncate(150);
    // 主流客户端对 ut_metadata 有节流（实测约 2 秒 1 片），30 片可能要 1 分钟以上，
    // 因此单个 peer 的耐心要足够，总时长也要给足。
    let deadline = std::time::Instant::now() + Duration::from_secs(300);

    // 滚动并发：一个失败了立刻补一个，避免整批被最慢的 peer 拖住
    let mut running = FuturesUnordered::new();
    let mut next = 0usize;
    let mut failures = 0usize;
    let mut sample: Option<String> = None;
    let mut reported = 0usize;

    while std::time::Instant::now() < deadline && (next < peers.len() || !running.is_empty()) {
        while running.len() < META_CONCURRENCY && next < peers.len() {
            let addr = peers[next];
            next += 1;
            let budget = deadline
                .saturating_duration_since(std::time::Instant::now())
                .min(Duration::from_secs(90));
            running.push(fetch_meta_from_peer(
                addr,
                start.info_hash,
                peer_id,
                budget,
            ));
        }
        let remain = deadline.saturating_duration_since(std::time::Instant::now());
        if remain.is_zero() || running.is_empty() {
            break;
        }
        let outcome = tokio::time::timeout(remain, running.next()).await;
        match outcome {
            Ok(Some(Ok(data))) => match parse_info_dict(&data) {
                Ok(m) => {
                    if m.info_hash == start.info_hash {
                        msg(format!("元数据获取完成（已尝试 {} 个 peer）", next));
                        return Ok(m);
                    }
                    msg("元数据 info hash 不匹配，继续尝试其他 peer".to_string());
                }
                Err(e) => msg(format!("元数据解析失败：{e}")),
            },
            // 单个 peer 连不上/不支持扩展是常态，只做汇总，不逐条刷屏
            Ok(Some(Err(e))) => {
                failures += 1;
                if sample.is_none() {
                    sample = Some(e.to_string());
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
        if failures >= reported + 10 {
            reported = failures;
            if let Some(e) = &sample {
                msg(format!("已尝试 {next} 个 peer，失败 {failures} 个（示例：{e}）"));
            }
        }
    }
    bail!("元数据获取超时：共尝试 {next} 个 peer，均未返回完整元数据")
}

/// 合并 magnet 自带的 tracker 与内置默认 tracker（去重、去空）。
fn merge_trackers(extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = extra
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    for t in DEFAULT_TRACKERS {
        if !out.iter().any(|x| x == t) {
            out.push((*t).to_string());
        }
    }
    out
}

/// 并发向所有 tracker 公告，汇总去重后的 peer 列表。
async fn announce_all(
    trackers: &[String],
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    port: u16,
    msg: &Msg,
) -> Vec<SocketAddr> {
    let client = tracker_client().unwrap_or_else(|_| reqwest::Client::new());
    let futs: Vec<_> = trackers
        .iter()
        .map(|t| {
            let t = t.clone();
            let client = client.clone();
            async move {
                let p = AnnounceParams {
                    info_hash,
                    peer_id,
                    port,
                    uploaded: 0,
                    downloaded: 0,
                    left: 0,
                    event: "started",
                    numwant: 60,
                };
                (
                    t.clone(),
                    tokio::time::timeout(Duration::from_secs(30), announce(&t, &p, &client)).await,
                )
            }
        })
        .collect();

    let mut peers: Vec<SocketAddr> = Vec::new();
    let mut seen: HashSet<SocketAddr> = HashSet::new();
    for (t, res) in join_all(futs).await {
        match res {
            Ok(Ok(r)) => {
                let mut n = 0usize;
                for p in r.peers {
                    if p.port == 0 {
                        continue;
                    }
                    let addr = SocketAddr::new(p.ip, p.port);
                    if seen.insert(addr) {
                        peers.push(addr);
                        n += 1;
                    }
                }
                msg(format!("tracker 返回 {n} 个 peer（{t}）"));
            }
            Ok(Err(e)) => msg(format!("tracker 失败（{t}）：{e}")),
            Err(_) => msg(format!("tracker 超时（{t}）")),
        }
    }
    peers
}

/// 解析 magnet 的 `x.pe=` 直连 peer 地址（host:port 或 ip:port）。
async fn resolve_peers(raw: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for item in raw {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        // 先按 IP:port 直接解析，失败再走 DNS
        if let Ok(addr) = item.parse::<SocketAddr>() {
            out.push(addr);
            continue;
        }
        if let Ok(addrs) =
            tokio::time::timeout(Duration::from_secs(4), tokio::net::lookup_host(item)).await
        {
            if let Ok(addrs) = addrs {
                out.extend(addrs);
            }
        }
    }
    out
}

/// 通过 DHT 网络查找 peer（tracker 无果时的兜底）。
async fn dht_find_peers(
    info_hash: [u8; 20],
    port: u16,
    timeout: Duration,
    msg: &Msg,
) -> Result<Vec<SocketAddr>> {
    let node = dht::Dht::start(port).await.context("启动 DHT 失败")?;
    match node.bootstrap().await {
        Ok(n) => msg(format!("DHT 已接入，路由表 {} 个节点", n)),
        Err(e) => msg(format!("DHT bootstrap 失败：{e}")),
    }
    Ok(node.get_peers(info_hash, timeout).await)
}

/// 向单个 peer 请求全部元数据分片，返回拼接好的 info 字典原始字节。
async fn fetch_meta_from_peer(
    addr: SocketAddr,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = std::time::Instant::now() + timeout;
    let (conn, _) = tokio::time::timeout(timeout.min(Duration::from_secs(8)), connect_peer(addr, info_hash, peer_id))
        .await
        .map_err(|_| anyhow!("连接 peer 超时"))??;
    // 没声明扩展协议的对端不会回扩展握手，继续等下去只是白耗时间
    if !conn.ext_supported {
        bail!("对端不支持扩展协议（BEP 10）");
    }
    let (r, w) = tokio::io::split(conn.stream);
    let ext_supported = conn.ext_supported;
    let mut writer = PeerConnection {
        stream: w,
        peer_bitfield: None,
        choking: true,
        peer_interested: false,
        ext_local: None,
        ext_remote: None,
        ext_supported,
        peer_id,
    };
    let mut reader = PeerConnection {
        stream: r,
        peer_bitfield: None,
        choking: true,
        peer_interested: false,
        ext_local: None,
        ext_remote: None,
        ext_supported,
        peer_id,
    };

    // 宣告本地扩展（BEP 9 的 ut_metadata）
    let hs = peer::build_ut_metadata_handshake(None);
    peer::send_message(
        &mut writer,
        &peer::build_extended(peer::EXT_ID_UT_METADATA, &hs),
    )
    .await?;
    // 顺手表明兴趣并解除对对端的 choke：部分客户端只在收到 interested 后才理会请求
    peer::send_message(&mut writer, &peer::frame(peer::MsgId::Interested, &[])).await?;
    peer::send_message(&mut writer, &peer::frame(peer::MsgId::Unchoke, &[])).await?;

    let mut chunks: Vec<Option<Vec<u8>>> = Vec::new();
    let mut total_size = 0usize;
    let mut remote_ext_id: Option<u8> = None;
    // 在途的元数据请求数：串行一问一答太慢，允许少量并发
    let mut inflight = 0usize;
    // 已发出请求的分片：避免同一片被重复请求（否则对端会把同一片回好几遍）
    let mut requested: HashSet<usize> = HashSet::new();
    const MAX_INFLIGHT: usize = 4;
    // 排查手段：设置 RUST_DM_BT_DEBUG=1 后打印与单个 peer 的协议交互
    let verbose = std::env::var("RUST_DM_BT_DEBUG").is_ok();
    if verbose {
        eprintln!("[bt] {addr} 已连接，对端支持扩展={ext_supported}");
    }

    while std::time::Instant::now() < deadline {
        // 每次读取都带剩余时间上限，避免对端不回扩展握手时整个任务被拖死
        let wait = deadline.saturating_duration_since(std::time::Instant::now());
        let Some(msg) = tokio::time::timeout(wait, peer::read_message(&mut reader))
            .await
            .map_err(|_| anyhow!("等待 peer 消息超时"))??
        else {
            break;
        };
        if verbose {
            match &msg {
                PeerMessage::Extended { ext_id, payload } => {
                    eprintln!("[bt] {addr} 扩展消息 ext_id={ext_id} len={}", payload.len())
                }
                other => eprintln!("[bt] {addr} 消息 {other:?}"),
            }
        }
        let PeerMessage::Extended { ext_id, payload } = msg else {
            continue;
        };

        if ext_id == 0 {
            // 对端扩展握手：找出它的 ut_metadata 本地 ID。
            // BEP 10 里 `m` 把扩展名映射到**整数 ID**；个别实现会再包一层 {"v": id}。
            if let Ok((v, _)) = Bencode::decode(&payload) {
                if remote_ext_id.is_none() {
                    remote_ext_id = extract_ut_metadata_id(&v);
                }
            }
            // 对端不支持 ut_metadata 就不用干等超时了
            if remote_ext_id.is_none() {
                bail!("对端不支持 ut_metadata（BEP 9）");
            }
            let Some(rid) = remote_ext_id else { continue };
            if inflight == 0 && chunks.is_empty() {
                // 先请求第 0 片，从响应里拿 total_size
                let req = peer::build_ut_metadata_request(0);
                if verbose {
                    eprintln!("[bt] {addr} 请求 piece 0（rid={rid}）");
                }
                peer::send_message(&mut writer, &peer::build_extended(rid, &req)).await?;
                inflight += 1;
                requested.insert(0);
            }
        } else if ext_id == peer::UT_METADATA_LOCAL_ID as u8 || Some(ext_id) == remote_ext_id {
            // 对端回数据时用的是**我方**在扩展握手里声明的 ID（BEP 10：双方各自用自己的 ID 收、用对端的 ID 发）。
            // 曾经只比对对端的 ID，qBittorrent（ut_metadata=2）发来的数据（ext_id=1）被整条丢弃。
            inflight = inflight.saturating_sub(1);
            // 对端明确拒绝（msg_type=2）就不用再等超时了
            if let Ok((v, _)) = Bencode::decode(&payload) {
                if v.get("msg_type").and_then(|x| x.as_int()) == Some(peer::UT_METADATA_REJECT) {
                    bail!("对端拒绝提供元数据分片");
                }
            }
            let Ok((piece, total, used)) = peer::parse_ut_metadata_data(&payload) else {
                continue;
            };
            let data = payload[used..].to_vec();
            if chunks.len() < piece + 1 {
                chunks.resize(piece + 1, None);
            }
            chunks[piece] = Some(data);
            if total > 0 {
                total_size = total;
                let needed = total.div_ceil(METADATA_PIECE_LEN);
                if chunks.len() < needed {
                    chunks.resize(needed, None);
                }
            }
            if total_size > 0 {
                if let Some(data) = assemble(&chunks, total_size) {
                    return Ok(data);
                }
            }
            // 补齐后续缺失分片，保持少量在途以加快获取
            let Some(rid) = remote_ext_id else { continue };
            while inflight < MAX_INFLIGHT {
                let need = if total_size > 0 {
                    total_size.div_ceil(METADATA_PIECE_LEN)
                } else {
                    chunks.len()
                };
                // 只挑"还没收到、也没发出请求"的分片
                let Some(next) = (0..need).find(|i| {
                    chunks.get(*i).map(|c| c.is_none()).unwrap_or(true) && !requested.contains(i)
                }) else {
                    break;
                };
                if verbose {
                    eprintln!("[bt] {addr} 请求 piece {next}");
                }
                let req = peer::build_ut_metadata_request(next);
                peer::send_message(&mut writer, &peer::build_extended(rid, &req)).await?;
                inflight += 1;
                requested.insert(next);
            }
        }
    }
    Err(anyhow!("获取元数据超时"))
}

/// 从对端扩展握手里取出 ut_metadata 的本地扩展 ID。
///
/// 大多数客户端直接给整数（BEP 10 的标准写法），少数会包一层 {"v": id}。
fn extract_ut_metadata_id(v: &Bencode) -> Option<u8> {
    let m = v.get("m")?.as_dict()?;
    let entry = m.get(b"ut_metadata".as_slice())?;
    match entry {
        Bencode::Int(id) if *id > 0 && *id <= u8::MAX as i64 => Some(*id as u8),
        Bencode::Dict(inner) => inner
            .get(b"v".as_slice())
            .and_then(|x| x.as_int())
            .filter(|i| *i > 0 && *i <= u8::MAX as i64)
            .map(|i| i as u8),
        _ => None,
    }
}

/// 拼接元数据分片。
fn assemble(chunks: &[Option<Vec<u8>>], total: usize) -> Option<Vec<u8>> {
    if total == 0 || chunks.is_empty() {
        return None;
    }
    let need = total.div_ceil(METADATA_PIECE_LEN);
    if chunks.len() < need || chunks[..need].iter().any(|c| c.is_none()) {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    for c in chunks.iter().take(need) {
        out.extend_from_slice(c.as_ref()?);
    }
    out.truncate(total);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_meta(piece_len: u32, total: u64) -> Metainfo {
        let count = total.div_ceil(piece_len as u64) as usize;
        Metainfo {
            info_hash: [0u8; 20],
            name: "t".into(),
            piece_length: piece_len,
            pieces: vec![0u8; count * 20],
            total_length: total,
            files: vec![],
            is_multi: false,
            announce: vec![],
            announce_list: vec![],
            comment: None,
            created_by: None,
            creation_date: None,
            web_seeds: vec![],
            is_private: false,
            node: None,
        }
    }

    #[test]
    fn ut_metadata_id_is_plain_int() {
        // BEP 10 的标准写法：m 里直接是整数 ID
        let hs = super::super::bencode::dict(vec![(
            "m",
            super::super::bencode::dict(vec![
                ("ut_metadata", Bencode::Int(3)),
                ("ut_pex", Bencode::Int(1)),
            ]),
        )]);
        assert_eq!(extract_ut_metadata_id(&hs), Some(3));
    }

    #[test]
    fn ut_metadata_id_may_be_nested_dict() {
        let mut inner = std::collections::BTreeMap::new();
        inner.insert(b"v".to_vec(), Bencode::Int(7));
        let mut m = std::collections::BTreeMap::new();
        m.insert(b"ut_metadata".to_vec(), Bencode::Dict(inner));
        let hs = Bencode::Dict({
            let mut d = std::collections::BTreeMap::new();
            d.insert(b"m".to_vec(), Bencode::Dict(m));
            d
        });
        assert_eq!(extract_ut_metadata_id(&hs), Some(7));
    }

    #[test]
    fn ut_metadata_id_absent_when_unsupported() {
        let hs = super::super::bencode::dict(vec![(
            "m",
            super::super::bencode::dict(vec![("ut_pex", Bencode::Int(1))]),
        )]);
        assert_eq!(extract_ut_metadata_id(&hs), None);
    }

    #[test]
    fn block_length_handles_last_block() {
        let meta = fake_meta(32768, 40000);
        let last = BlockId {
            piece: 1,
            index: 0,
        };
        assert_eq!(last.length(&meta), 7232);
        let full = BlockId {
            piece: 0,
            index: 0,
        };
        assert_eq!(full.length(&meta), BLOCK_LEN);
    }

    #[test]
    fn piece_completion_requires_all_blocks() {
        let meta = fake_meta(32768, 32768 * 2);
        let mut st = PieceState::default();
        // piece 0 有 2 块
        st.mark_received(BlockId {
            piece: 0,
            index: 0,
        });
        assert!(!st.is_complete(&meta, 0));
        st.mark_received(BlockId {
            piece: 0,
            index: 1,
        });
        assert!(st.is_complete(&meta, 0));
    }

    #[test]
    fn duplicate_block_is_ignored() {
        let mut st = PieceState::default();
        let id = BlockId {
            piece: 3,
            index: 0,
        };
        assert!(st.mark_received(id));
        assert!(!st.mark_received(id));
    }

    #[test]
    fn select_blocks_prefers_rare_pieces() {
        let meta = fake_meta(16384, 16384 * 4);
        let state = PieceState::default();
        let mut avail = HashMap::new();
        avail.insert(0u32, 3);
        avail.insert(1u32, 2);
        avail.insert(3u32, 1);
        let picks = select_blocks(&state, &meta, &avail, 10);
        assert_eq!(picks[0].piece, 2, "最稀有的分片应优先");
    }

    #[test]
    fn select_blocks_skips_received_and_pending() {
        let meta = fake_meta(16384, 16384 * 2);
        let mut state = PieceState::default();
        state.mark_received(BlockId {
            piece: 0,
            index: 0,
        });
        state.pending.insert(BlockId {
            piece: 0,
            index: 1,
        });
        let picks = select_blocks(&state, &meta, &HashMap::new(), 100);
        assert!(
            picks.iter().all(|b| !(b.piece == 0 && (b.index == 0 || b.index == 1))),
            "已收到/已请求的块不应重复下发：{picks:?}"
        );
    }

    #[test]
    fn select_blocks_skips_completed_pieces() {
        let meta = fake_meta(16384, 16384 * 2);
        let mut state = PieceState::default();
        state.completed.insert(0);
        let picks = select_blocks(&state, &meta, &HashMap::new(), 100);
        assert!(picks.iter().all(|b| b.piece == 1));
    }

    #[test]
    fn assembles_metadata_chunks() {
        // 真实元数据按 16 KiB 分片，这里构造跨两个分片的场景
        let p = METADATA_PIECE_LEN;
        let chunks = vec![Some(vec![1u8; p]), Some(vec![2u8; p])];
        let out = assemble(&chunks, p * 2).unwrap();
        assert_eq!(out.len(), p * 2);
        assert_eq!(out[0], 1);
        assert_eq!(out[p], 2);

        // 缺任一分片时应返回 None
        let missing = vec![Some(vec![1u8; p]), None];
        assert!(assemble(&missing, p * 2).is_none());
    }

    #[test]
    fn piece_len_truncates_last_piece() {
        let meta = fake_meta(32768, 40000);
        assert_eq!(meta.piece_len(0), 32768);
        assert_eq!(meta.piece_len(1), 7232);
    }
}