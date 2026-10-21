//! BT 下载冒烟测试：跑 `run_task` 若干秒，打印 peer 数与下载量后取消。
//!
//! 用来验证"tracker 能否拿到 peer、peer 能否连上、数据能否真的下下来"，
//! 又不用等整个种子下完。
//!
//! 用法：
//! ```text
//! cargo run --example bt_smoke -- "<magnet 或 .torrent 路径>" [运行秒数]
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rust_dm::common::limits::TokenBucket;
use rust_dm::engine::bt::session::{self, BtHandle, BtOptions, BtStats, Msg};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let source = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("用法：bt_smoke <magnet 或 .torrent 路径> [秒数]"))?;
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);

    let dir = std::env::temp_dir().join("rust-dm-bt-smoke");
    std::fs::create_dir_all(&dir)?;
    println!("保存目录：{}", dir.display());
    println!("最多运行 {secs} 秒\n");

    let opts = BtOptions {
        port: 6881,
        max_peers: 50,
        seed_after_complete: false,
        ..Default::default()
    };
    let handle = BtHandle {
        pause: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let cancel_flag = handle.cancel.clone();
    let bucket = Arc::new(TokenBucket::new());

    let msg: Msg = Arc::new(|m: String| println!("  [log] {m}"));

    // 只在状态变化时打印，避免刷屏
    let last_print = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));
    let peak = Arc::new(Mutex::new((0usize, 0u64)));
    let last_print2 = last_print.clone();
    let peak2 = peak.clone();
    let on_progress = Arc::new(move |s: BtStats| {
        {
            let mut p = peak2.lock().unwrap();
            p.0 = p.0.max(s.peers);
            p.1 = p.1.max(s.downloaded);
        }
        let mut t = last_print2.lock().unwrap();
        if t.elapsed() >= Duration::from_secs(3) {
            *t = Instant::now();
            println!(
                "  [进度] peered={} seeds={} 已下载={} 速度={}/s 分片={}/{}",
                s.peers, s.seeds, s.downloaded, s.speed_down, s.pieces_done, s.pieces_total
            );
        }
    });

    // 到点取消
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        println!("\n到时间了，取消任务…");
        cancel_flag.store(true, Ordering::Relaxed);
    });

    let started = Instant::now();
    let r = session::run_task(
        &source,
        &dir,
        opts,
        handle,
        bucket,
        msg,
        on_progress,
    )
    .await;

    let (peers, downloaded) = *peak.lock().unwrap();
    println!("\n运行 {} 秒结束", started.elapsed().as_secs());
    println!("峰值 peer 数：{peers}");
    println!("峰值已下载：{downloaded} 字节");
    match r {
        Ok(()) => println!("结果：正常结束"),
        Err(e) => println!("结果：{e}"),
    }
    Ok(())
}
