//! 只跑 magnet 元数据获取（BEP 9），不做下载，用来快速验证磁力链能否解析出种子。
//!
//! 用法：`cargo run --example meta_fetch -- "magnet:?xt=urn:btih:..."`

use std::sync::Arc;

use anyhow::{Context, Result};
use rust_dm::engine::bt::session;

#[tokio::main]
async fn main() -> Result<()> {
    let arg = std::env::args()
        .nth(1)
        .context("用法：meta_fetch <magnet 链接>")?;
    let start = session::prepare_from_magnet(&arg)?;
    println!("info hash：{}", hex::encode(start.info_hash));
    println!("显示名：{:?}", start.display_name);
    if start.metainfo.is_some() {
        println!("magnet 内嵌了完整元数据，无需网络获取");
        return Ok(());
    }
    let msg: session::Msg = Arc::new(|s: String| println!("  {s}"));
    let meta = session::fetch_metainfo(&start, &msg).await?;
    println!(
        "成功：{}（{} 字节，{} 个分片，多文件={}）",
        meta.name,
        meta.total_length,
        meta.piece_count(),
        meta.is_multi
    );
    Ok(())
}
