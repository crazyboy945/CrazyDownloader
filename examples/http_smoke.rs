//! 走引擎（与界面同一条链路）下载一个 HTTP 资源，周期性打印任务状态。
//!
//! 用法：`cargo run --example http_smoke -- <url> [保存目录] [观察秒数]`
//! 不给 url 时用一个公开的测速文件。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_dm::common::task::{Config, Source, TaskState};
use rust_dm::engine::Engine;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().unwrap_or_else(|| {
        "http://ipv4.download.thinkbroadband.com/10MB.zip".to_string()
    });
    let save = args.next().map(PathBuf::from);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);

    let mut cfg = Config::default();
    if let Some(d) = &save {
        cfg.download_dir = d.clone();
    }
    let dir = cfg.download_dir.clone();
    println!("保存目录：{}", dir.display());
    println!("URL：{url}\n");

    let engine = Arc::new(Engine::new(cfg)?);
    // 界面里由 main.rs 调用；这里也要有，否则速度一直是 0
    rust_dm::engine::spawn_ticker(engine.state.clone());
    let src = Source::parse(&url, "", "").map_err(|e| anyhow::anyhow!(e))?;
    let id = engine.add_task(src, dir);

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(2));
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
        println!(
            "[{:>4}s] 状态={:?} 已下载={:.2}MB/{:.2}MB 速度={} B/s 文件={} 信息={}",
            started.elapsed().as_secs(),
            t.state,
            t.downloaded as f64 / 1e6,
            t.total_bytes as f64 / 1e6,
            t.speed_down,
            t.file_name,
            t.last_message
        );
        match t.state {
            TaskState::Completed => {
                println!("\n>>> 标记为已完成");
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
