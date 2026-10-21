//! 单 peer 的 ut_metadata 调试器（不启动 GUI）。
//!
//! 用法：`cargo run --example meta_debug -- <ip:port> <40 位十六进制 info hash>`
//!
//! 逐条打印对端发来的消息，用来判断元数据拿不到是"对端没回扩展握手"、
//! "不支持 ut_metadata" 还是"回了请求但不给数据"。

use std::time::Duration;

use anyhow::{Context, Result};
use rust_dm::engine::bt::peer::{self, PeerConnection, PeerMessage};
use tokio::net::TcpStream;

#[tokio::main]
async fn main() -> Result<()> {
    let addr = std::env::args().nth(1).context("缺少 peer 地址")?;
    let hash_hex = std::env::args().nth(2).context("缺少 info hash")?;
    let bytes = hex::decode(hash_hex.trim())?;
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&bytes);

    let peer_id = rust_dm::engine::bt::metainfo::random_peer_id();
    let sock = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
        .await
        .context("连接超时")??;
    println!("TCP 已连接 {addr}");
    let _ = sock.set_nodelay(true);

    let mut conn = PeerConnection::new(sock, peer_id);
    peer::send_handshake(&mut conn, &info_hash, &peer_id).await?;
    let remote = peer::read_handshake(&mut conn).await?;
    println!("握手完成：info hash 匹配={}", remote == info_hash);
    println!("对端 peer id = {}", String::from_utf8_lossy(&conn.peer_id));
    println!("对端支持扩展协议（BEP 10）= {}", conn.ext_supported);
    if !conn.ext_supported {
        println!("对端没声明扩展协议，不会回扩展握手");
        return Ok(());
    }

    // 宣告 ut_metadata 并表明兴趣
    let hs = peer::build_ut_metadata_handshake(None);
    peer::send_message(&mut conn, &peer::build_extended(peer::EXT_ID_UT_METADATA, &hs)).await?;
    peer::send_message(&mut conn, &peer::frame(peer::MsgId::Interested, &[])).await?;
    peer::send_message(&mut conn, &peer::frame(peer::MsgId::Unchoke, &[])).await?;
    println!("已发送扩展握手 + interested + unchoke");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let t0 = tokio::time::Instant::now();
    let mut remote_id: Option<u8> = None;
    let mut chunks: Vec<Option<Vec<u8>>> = Vec::new();
    let mut total: usize = 0;
    let mut inflight = 0usize;
    let mut requested: std::collections::HashSet<usize> = std::collections::HashSet::new();
    requested.insert(0);
    while tokio::time::Instant::now() < deadline {
        let wait = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match tokio::time::timeout(wait, peer::read_message(&mut conn)).await {
            Ok(Ok(Some(m))) => m,
            Ok(Ok(None)) => {
                println!("连接被对端关闭");
                break;
            }
            Ok(Err(e)) => {
                println!("读取出错：{e}");
                break;
            }
            Err(_) => {
                println!("等待超时");
                break;
            }
        };
        match msg {
            PeerMessage::Extended { ext_id, payload } => {
                println!(
                    "扩展消息 ext_id={ext_id} payload={}",
                    String::from_utf8_lossy(&payload[..payload.len().min(160)])
                );
                if ext_id == 0 {
                    // 解析 m 字典
                    if let Ok((v, _)) = rust_dm::engine::bt::bencode::Bencode::decode(&payload) {
                        if let Some(m) = v.get("m").and_then(|x| x.as_dict()) {
                            let entry = m.get(b"ut_metadata".as_slice());
                            println!("  m.ut_metadata = {entry:?}");
                            match entry {
                                Some(rust_dm::engine::bt::bencode::Bencode::Int(i)) => remote_id = Some(*i as u8),
                                Some(rust_dm::engine::bt::bencode::Bencode::Dict(d)) => {
                                    remote_id = d
                                        .get(b"v".as_slice())
                                        .and_then(|x| x.as_int())
                                        .map(|x| x as u8)
                                }
                                _ => {}
                            }
                        }
                        if let Some(sz) = v.get("metadata_size").and_then(|x| x.as_int()) {
                            println!("  metadata_size = {sz}");
                        }
                    }
                    if let Some(rid) = remote_id {
                        println!("  → 请求 piece 0（对端 ut_metadata id = {rid}）");
                        peer::send_message(
                            &mut conn,
                            &peer::build_extended(rid, &peer::build_ut_metadata_request(0)),
                        )
                        .await?;
                        inflight += 1;
                    } else {
                        println!("  → 对端不支持 ut_metadata");
                        break;
                    }
                } else {
                    match peer::parse_ut_metadata_data(&payload) {
                        Ok((piece, size, used)) => {
                            inflight = inflight.saturating_sub(1);
                            if chunks.len() < piece + 1 {
                                chunks.resize(piece + 1, None);
                            }
                            chunks[piece] = Some(payload[used..].to_vec());
                            if size > 0 {
                                total = size;
                                let need = size.div_ceil(16 * 1024);
                                if chunks.len() < need {
                                    chunks.resize(need, None);
                                }
                            }
                            let done = chunks.iter().filter(|c| c.is_some()).count();
                            let need = if total > 0 { total.div_ceil(16 * 1024) } else { 0 };
                            println!(
                                "  [{:?}] 收到 piece={piece} total={total} 进度 {done}/{need}",
                                t0.elapsed()
                            );
                            if total > 0 && done == need {
                                println!("  全部收齐，用时 {:?}", t0.elapsed());
                                return Ok(());
                            }
                            // 补齐在途请求（跳过已请求过的分片）
                            while inflight < 4 {
                                let Some(next) = (0..need).find(|i| {
                                    chunks.get(*i).map(|c| c.is_none()).unwrap_or(true)
                                        && !requested.contains(i)
                                }) else {
                                    break;
                                };
                                requested.insert(next);
                                peer::send_message(
                                    &mut conn,
                                    &peer::build_extended(
                                        remote_id.unwrap(),
                                        &peer::build_ut_metadata_request(next),
                                    ),
                                )
                                .await?;
                                inflight += 1;
                            }
                        }
                        Err(e) => println!("  非 data 消息：{e}"),
                    }
                }
            }
            other => println!("普通消息：{other:?}"),
        }
    }
    Ok(())
}
