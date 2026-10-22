//! 走引擎（和界面同一条链路）跑一个 BT 任务，周期性打印任务状态。
//!
//! 用来复现"界面提示已完成但实际没下完"这类状态问题。
//!
//! 用法：`cargo run --example engine_bt -- <种子路径或 magnet> [保存目录] [观察秒数]`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_dm::common::task::{Config, Source, TaskState};
use rust_dm::engine::Engine;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let src_text = args.next().ok_or_else(|| {
        anyhow::anyhow!("用法：engine_bt <种子路径或 magnet> [保存目录] [观察秒数]")
    })?;
    let save = args.next().map(PathBuf::from);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(180);

    let mut cfg = Config::default();
    if let Some(d) = &save {
        cfg.download_dir = d.clone();
    }
    let dir = cfg.download_dir.clone();
    println!("保存目录：{}", dir.display());

    let engine = Arc::new(Engine::new(cfg)?);
    let src = Source::parse(&src_text, "", "").map_err(|e| anyhow::anyhow!(e))?;
    let id = engine.add_task(src, dir);

    let started = Instant::now();
    let mut last = String::new();
    while started.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(3));
        let snapshot = {
            let s = engine.state.lock().unwrap();
            s.tasks
                .iter()
                .find_map(|t| t.lock().ok().map(|t| t.clone()))
                .filter(|t| t.id == id)
        };
        let Some(t) = snapshot else {
            println!("任务已不存在");
            break;
        };
        let line = format!(
            "[{:>4}s] 状态={:?} 已下载={:.2}MB/{:.2}MB 分片={}/{} 速度={} B/s 信息={}",
            started.elapsed().as_secs(),
            t.state,
            t.downloaded as f64 / 1e6,
            t.total_bytes as f64 / 1e6,
            t.pieces_done,
            t.pieces_total,
            t.speed_down,
            t.last_message
        );
        // 只在状态/分片数变化时打印，避免刷屏
        let key = format!("{:?}-{}", t.state, t.pieces_done);
        if key != last {
            println!("{line}");
            last = key;
        }
        match t.state {
            TaskState::Completed => {
                println!("\n>>> 任务被标记为「已完成」，但已下载只有 {:.2} MB / {:.2} MB，分片 {}/{}",
                    t.downloaded as f64 / 1e6, t.total_bytes as f64 / 1e6, t.pieces_done, t.pieces_total);
                break;
            }
            TaskState::Failed(e) => {
                println!("\n>>> 任务失败：{e}");
                break;
            }
            _ => {}
        }
    }
    println!("观察结束");
    Ok(())
}
