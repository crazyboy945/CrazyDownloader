//! DHT / tracker 排查工具（不启动 GUI）。
//!
//! 用法：
//! ```text
//! cargo run --example dht_probe -- "magnet:?xt=urn:btih:..."
//! cargo run --example dht_probe -- ./xxx.torrent
//! ```
//!
//! 它会依次：解析出 info hash → 并发向所有 tracker 公告 → 接入 DHT 迭代查找，
//! 把每一步拿到的 peer 数量打印出来，用来判断"找不到 peer"到底是
//! 种子失效、tracker 全挂，还是本机 UDP 被防火墙阻断。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::future::join_all;

use rust_dm::engine::bt::{dht, session, tracker};

#[tokio::main]
async fn main() -> Result<()> {
    let arg = std::env::args()
        .nth(1)
        .context("用法：dht_probe <magnet 链接 | .torrent 路径>")?;

    let info_hash = if arg.trim_start().to_ascii_lowercase().starts_with("magnet:") {
        let start = session::prepare_from_magnet(&arg)?;
        println!("显示名：{:?}", start.display_name);
        println!("自带 tracker：{} 个", start.trackers.len());
        for t in &start.trackers {
            println!("  tr: {t}");
        }
        start.info_hash
    } else {
        let raw = std::fs::read(&arg).with_context(|| format!("读取 {arg} 失败"))?;
        let (start, meta) = session::prepare_from_torrent(&raw)?;
        println!("种子名：{}", meta.name);
        println!("自带 tracker：{} 个", start.trackers.len());
        for t in &start.trackers {
            println!("  tr: {t}");
        }
        start.info_hash
    };
    println!("info hash：{}", hex::encode(info_hash));

    // ---- 1. tracker ----
    println!("\n[1/2] 向 tracker 公告…");
    let trackers = tracker::DEFAULT_TRACKERS
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>();
    let client = tracker::tracker_client()?;
    let peer_id = rust_dm::engine::bt::metainfo::random_peer_id();
    let futs: Vec<_> = trackers
        .iter()
        .map(|t| {
            let t = t.clone();
            let client = client.clone();
            async move {
                let p = tracker::AnnounceParams {
                    info_hash,
                    peer_id,
                    port: 6881,
                    uploaded: 0,
                    downloaded: 0,
                    left: 0,
                    event: "started",
                    numwant: 60,
                };
                let started = Instant::now();
                let r = tokio::time::timeout(
                    Duration::from_secs(30),
                    tracker::announce(&t, &p, &client),
                )
                .await;
                (t, r, started.elapsed())
            }
        })
        .collect();
    let mut total = 0usize;
    for (t, r, cost) in join_all(futs).await {
        match r {
            Ok(Ok(res)) => {
                total += res.peers.len();
                println!(
                    "  [{:>5.1?}] OK   {:<45} peers={}",
                    cost, t, res.peers.len()
                );
            }
            Ok(Err(e)) => println!("  [{:>5.1?}] FAIL {:<45} {}", cost, t, e),
            Err(_) => println!("  [{:>5.1?}] TMO  {:<45} 超时", cost, t),
        }
    }
    println!("tracker 合计：{total} 个 peer");

    // ---- 2. DHT ----
    println!("\n[2/2] 接入 DHT 网络…");
    let node = dht::Dht::start(6881).await.context("启动 DHT 失败")?;
    match node.bootstrap().await {
        Ok(n) => println!("bootstrap 完成，路由表 {n} 个节点"),
        Err(e) => println!("bootstrap 失败：{e}（UDP 可能被防火墙阻断）"),
    }
    let peers = node.get_peers(info_hash, Duration::from_secs(45)).await;
    println!("DHT 找到 {} 个 peer", peers.len());
    for p in peers.iter().take(30) {
        println!("  {p}");
    }
    println!("路由表最终 {} 个节点", node.node_count().await);

    if total == 0 && peers.is_empty() {
        println!("\n结论：tracker 与 DHT 都没找到 peer —— 该种子很可能已失效（无人做种）。");
    } else if total == 0 {
        println!("\n结论：tracker 全部无响应，只有 DHT 可用。");
    }

    // ---- 3. magnet 元数据（BEP 9）----
    if arg.trim_start().to_ascii_lowercase().starts_with("magnet:") {
        let start = session::prepare_from_magnet(&arg)?;
        if start.metainfo.is_some() {
            println!("\n[3/3] magnet 内嵌了完整元数据，无需网络获取");
        } else {
            println!("\n[3/3] 通过 ut_metadata（BEP 9）向 peer 请求元数据…");
            let msg: session::Msg = Arc::new(|s: String| println!("  {s}"));
            match session::fetch_metainfo(&start, &msg).await {
                Ok(m) => println!(
                    "成功：{}（{} 字节，{} 个分片，多文件={}）",
                    m.name,
                    m.total_length,
                    m.piece_count(),
                    m.is_multi
                ),
                Err(e) => println!("失败：{e}"),
            }
        }
    }
    Ok(())
}
