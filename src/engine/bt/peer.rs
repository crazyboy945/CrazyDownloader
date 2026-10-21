//! BitTorrent peer wire protocol（BEP 3 / BEP 5 扩展）。
//!
//! 覆盖：握手、bitfield、have、request/piece/cancel、
//! choke/unchoke/interested，以及扩展消息（BEP 10）中的 ut_metadata（BEP 9）与 ut_pex（BEP 11）。

use anyhow::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 协议握手前缀 `\x13BitTorrent protocol`（1 字节长度 + 19 字节字符串 = 20 字节）
pub const PROTOCOL: &[u8; 20] = b"\x13BitTorrent protocol";

/// 握手包总长度：19 + 8 + 20 + 20 = 68
pub const HANDSHAKE_LEN: usize = 68;

/// 消息 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgId {
    Choke = 0,
    Unchoke = 1,
    Interested = 2,
    NotInterested = 3,
    Have = 4,
    Bitfield = 5,
    Request = 6,
    Piece = 7,
    Cancel = 8,
    Port = 9,
    Extended = 20,
}

impl MsgId {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => MsgId::Choke,
            1 => MsgId::Unchoke,
            2 => MsgId::Interested,
            3 => MsgId::NotInterested,
            4 => MsgId::Have,
            5 => MsgId::Bitfield,
            6 => MsgId::Request,
            7 => MsgId::Piece,
            8 => MsgId::Cancel,
            9 => MsgId::Port,
            20 => MsgId::Extended,
            _ => return None,
        })
    }
}

/// 一条已解析的消息。
#[derive(Debug, Clone)]
pub enum PeerMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    /// (index, bitvec)，bitvec[i] 为 true 表示拥有第 i 片
    Bitfield(Vec<bool>),
    /// (index, begin, length)
    Request(u32, u32, u32),
    /// (index, begin, data)
    Piece(u32, u32, Vec<u8>),
    /// (index, begin, length)
    Cancel(u32, u32, u32),
    Port(u16),
    /// (扩展消息 ID, bencode 头, 剩余原始数据)
    Extended {
        ext_id: u8,
        payload: Vec<u8>,
    },
}

/// 对端连接。
pub struct PeerConnection<S> {
    pub stream: S,
    /// 对方是否有分片（未收到 bitfield 前为 None）
    pub peer_bitfield: Option<Vec<bool>>,
    /// 对方是否处于 choking 状态
    pub choking: bool,
    /// 对方是否对我们 interested
    pub peer_interested: bool,
    /// 扩展消息 ID 映射：本地 ID -> 对端 ID
    pub ext_local: Option<u8>,
    pub ext_remote: Option<u8>,
    /// 对端是否在握手保留位里声明支持扩展协议（BEP 10）。
    /// 没声明的客户端不会回扩展握手，也就拿不到 ut_metadata（BEP 9）元数据。
    pub ext_supported: bool,
    pub peer_id: [u8; 20],
}

impl<S> PeerConnection<S> {
    pub fn new(stream: S, peer_id: [u8; 20]) -> Self {
        Self {
            stream,
            peer_bitfield: None,
            choking: true, // 初始为 choked
            peer_interested: false,
            ext_local: None,
            ext_remote: None,
            ext_supported: false,
            peer_id,
        }
    }
}

/// 构造握手包。
///
/// 保留位第 5 字节的 0x10 是 BEP 10（扩展协议）支持标志：
/// 不在握手里声明，主流客户端就不会回扩展握手，ut_metadata 也就无从谈起。
pub fn build_handshake(info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Vec<u8> {
    let mut reserved = [0u8; 8];
    reserved[5] |= 0x10; // 扩展协议（BEP 10）
    let mut buf = Vec::with_capacity(HANDSHAKE_LEN);
    buf.extend_from_slice(PROTOCOL);
    buf.extend_from_slice(&reserved);
    buf.extend_from_slice(info_hash);
    buf.extend_from_slice(peer_id);
    buf
}

/// 发送握手包。
pub async fn send_handshake<S>(conn: &mut PeerConnection<S>, info_hash: &[u8; 20], peer_id: &[u8; 20]) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let buf = build_handshake(info_hash, peer_id);
    conn.stream.write_all(&buf).await?;
    conn.stream.flush().await?;
    Ok(())
}

/// 读取并校验对端的握手包。
pub async fn read_handshake<S>(conn: &mut PeerConnection<S>) -> Result<[u8; 20]>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = vec![0u8; HANDSHAKE_LEN];
    conn.stream.read_exact(&mut buf).await?;
    // 布局：1 字节长度 + 19 字节协议串（共 PROTOCOL 的 20 字节）
    //      + 8 字节保留位 + 20 字节 info hash + 20 字节 peer id = 68。
    // 曾按 19 字节协议串取偏移，整体错位 1 字节，导致每次握手都被判为不匹配。
    if &buf[0..20] != PROTOCOL {
        bail!("对端协议标识不匹配（可能是非 BT 流量）");
    }
    // 保留位第 5 字节的 0x10：对端是否支持扩展协议（BEP 10）
    conn.ext_supported = buf[20 + 5] & 0x10 != 0;
    let mut info_hash = [0u8; 20];
    info_hash.copy_from_slice(&buf[28..48]);
    let mut peer_id = [0u8; 20];
    peer_id.copy_from_slice(&buf[48..68]);
    conn.peer_id = peer_id;
    Ok(info_hash)
}

/// 读取一条消息。返回 `None` 表示连接已关闭。
pub async fn read_message<S>(conn: &mut PeerConnection<S>) -> Result<Option<PeerMessage>>
where
    S: tokio::io::AsyncRead + Unpin,
{
    // 4 字节长度前缀，0 表示 keep-alive
    let mut len_buf = [0u8; 4];
    if conn.stream.read_exact(&mut len_buf).await.is_err() {
        return Ok(None);
    }
    let len = u32::from_be_bytes(len_buf);
    if len == 0 {
        return Ok(Some(PeerMessage::KeepAlive));
    }
    if len > 2 * 1024 * 1024 {
        bail!("消息过长（{len} 字节），疑似协议异常");
    }

    let mut body = vec![0u8; len as usize];
    conn.stream.read_exact(&mut body).await?;

    let id = body[0];
    let args = &body[1..];
    let msg = match MsgId::from_u8(id) {
        Some(MsgId::Choke) => {
            conn.choking = true;
            PeerMessage::Choke
        }
        Some(MsgId::Unchoke) => {
            conn.choking = false;
            PeerMessage::Unchoke
        }
        Some(MsgId::Interested) => {
            conn.peer_interested = true;
            PeerMessage::Interested
        }
        Some(MsgId::NotInterested) => {
            conn.peer_interested = false;
            PeerMessage::NotInterested
        }
        Some(MsgId::Have) => {
            if args.len() < 4 {
                bail!("have 消息长度不足");
            }
            let idx = u32::from_be_bytes(args[0..4].try_into().unwrap());
            if let Some(bf) = conn.peer_bitfield.as_mut() {
                if let Some(slot) = bf.get_mut(idx as usize) {
                    *slot = true;
                }
            }
            PeerMessage::Have(idx)
        }
        Some(MsgId::Bitfield) => {
            let n = args.len() * 8;
            let bf: Vec<bool> = (0..n).map(|i| args[i / 8] & (0x80 >> (i % 8)) != 0).collect();
            conn.peer_bitfield = Some(bf.clone());
            PeerMessage::Bitfield(bf)
        }
        Some(MsgId::Request) => {
            if args.len() < 12 {
                bail!("request 消息长度不足");
            }
            PeerMessage::Request(
                u32::from_be_bytes(args[0..4].try_into().unwrap()),
                u32::from_be_bytes(args[4..8].try_into().unwrap()),
                u32::from_be_bytes(args[8..12].try_into().unwrap()),
            )
        }
        Some(MsgId::Piece) => {
            if args.len() < 8 {
                bail!("piece 消息长度不足");
            }
            PeerMessage::Piece(
                u32::from_be_bytes(args[0..4].try_into().unwrap()),
                u32::from_be_bytes(args[4..8].try_into().unwrap()),
                args[8..].to_vec(),
            )
        }
        Some(MsgId::Cancel) => {
            if args.len() < 12 {
                bail!("cancel 消息长度不足");
            }
            PeerMessage::Cancel(
                u32::from_be_bytes(args[0..4].try_into().unwrap()),
                u32::from_be_bytes(args[4..8].try_into().unwrap()),
                u32::from_be_bytes(args[8..12].try_into().unwrap()),
            )
        }
        Some(MsgId::Port) => {
            if args.len() < 2 {
                bail!("port 消息长度不足");
            }
            PeerMessage::Port(u16::from_be_bytes(args[0..2].try_into().unwrap()))
        }
        Some(MsgId::Extended) => {
            if args.is_empty() {
                bail!("extended 消息缺少扩展 ID");
            }
            PeerMessage::Extended {
                ext_id: args[0],
                payload: args[1..].to_vec(),
            }
        }
        None => PeerMessage::KeepAlive, // 未知 ID，忽略
    };
    Ok(Some(msg))
}

/// 发送一条消息。
pub async fn send_message<S>(conn: &mut PeerConnection<S>, msg: &[u8]) -> Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    conn.stream.write_all(msg).await?;
    conn.stream.flush().await?;
    Ok(())
}

/// 构造一个普通消息帧。
pub fn frame(id: MsgId, args: &[u8]) -> Vec<u8> {
    let len = (args.len() + 1) as u32;
    let mut buf = Vec::with_capacity(4 + args.len() + 1);
    buf.extend_from_slice(&len.to_be_bytes());
    buf.push(id as u8);
    buf.extend_from_slice(args);
    buf
}

/// 构造 request 消息。
pub fn build_request(index: u32, begin: u32, length: u32) -> Vec<u8> {
    let mut args = Vec::with_capacity(12);
    args.extend_from_slice(&index.to_be_bytes());
    args.extend_from_slice(&begin.to_be_bytes());
    args.extend_from_slice(&length.to_be_bytes());
    frame(MsgId::Request, &args)
}

/// 构造 piece 消息。
pub fn build_piece(index: u32, begin: u32, data: &[u8]) -> Vec<u8> {
    let mut args = Vec::with_capacity(8 + data.len());
    args.extend_from_slice(&index.to_be_bytes());
    args.extend_from_slice(&begin.to_be_bytes());
    args.extend_from_slice(data);
    frame(MsgId::Piece, &args)
}

/// 构造 bitfield 消息。
pub fn build_bitfield(bf: &[bool]) -> Vec<u8> {
    let bytes = (bf.len() + 7) / 8;
    let mut out = vec![0u8; bytes];
    for (i, has) in bf.iter().enumerate() {
        if *has {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    frame(MsgId::Bitfield, &out)
}

/// 构造 have 消息。
pub fn build_have(index: u32) -> Vec<u8> {
    frame(MsgId::Have, &index.to_be_bytes())
}

/// 构造 keep-alive。
pub fn build_keepalive() -> Vec<u8> {
    vec![0, 0, 0, 0]
}

/// 构造扩展消息帧。
pub fn build_extended(ext_id: u8, payload: &[u8]) -> Vec<u8> {
    let mut args = Vec::with_capacity(1 + payload.len());
    args.push(ext_id);
    args.extend_from_slice(payload);
    frame(MsgId::Extended, &args)
}

/// 本地扩展 ID 分配。
/// 扩展消息的第一个字节：0 固定表示扩展握手本身。
pub const EXT_ID_UT_METADATA: u8 = 0;
pub const EXT_ID_UT_PEX: u8 = 1;

/// 我们为 ut_metadata 分配的本地扩展 ID。
///
/// 必须非 0：0 被保留给扩展握手消息。对端给我们发 ut_metadata 数据时，
/// 会用我们在这里声明的这个 ID。
pub const UT_METADATA_LOCAL_ID: i64 = 1;

/// 构造 ut_metadata 的扩展握手 payload（BEP 9 / BEP 10）。
///
/// `m` 字段声明本地支持，**值必须是整数 ID**（BEP 10 要求）。
/// 曾经写成 `{"ut_metadata": {"v": 1}}`，主流客户端解析失败后直接断开连接，
/// 表现为"连上了但立刻被关掉"。
pub fn build_ut_metadata_handshake(metadata_size: Option<usize>) -> Vec<u8> {
    let mut d = std::collections::BTreeMap::new();
    let mut inner = std::collections::BTreeMap::new();
    inner.insert(
        b"ut_metadata".to_vec(),
        super::bencode::Bencode::Int(UT_METADATA_LOCAL_ID),
    );
    d.insert(b"m".to_vec(), super::bencode::Bencode::Dict(inner));
    if let Some(sz) = metadata_size {
        d.insert(
            b"metadata_size".to_vec(),
            super::bencode::Bencode::Int(sz as i64),
        );
    }
    d.insert(b"v".to_vec(), super::bencode::bstr("rust-dm"));
    super::bencode::Bencode::Dict(d).encode()
}

/// ut_metadata 的消息类型（BEP 9）：0 = request，1 = data，2 = reject。
///
/// 曾经把 request 写成 1、data 判成 2，两类消息全部错位：
/// 对端收到"data 类型"的请求会直接忽略（甚至断开），我们收到真正的 data 也会当成非法。
pub const UT_METADATA_REQUEST: i64 = 0;
pub const UT_METADATA_DATA: i64 = 1;
pub const UT_METADATA_REJECT: i64 = 2;

/// 构造 ut_metadata 的请求 payload。
pub fn build_ut_metadata_request(piece: usize) -> Vec<u8> {
    super::bencode::dict(vec![
        ("msg_type", super::bencode::Bencode::Int(UT_METADATA_REQUEST)),
        ("piece", super::bencode::Bencode::Int(piece as i64)),
    ])
    .encode()
}

/// 解析 ut_metadata 的 data 响应头部，返回 (piece, total_size) 与剩余数据偏移。
pub fn parse_ut_metadata_data(payload: &[u8]) -> Result<(usize, usize, usize)> {
    let (v, _) = super::bencode::Bencode::decode(payload)?;
    let piece = v.get("piece").and_then(|x| x.as_int()).unwrap_or(-1);
    let msg_type = v.get("msg_type").and_then(|x| x.as_int()).unwrap_or(-1);
    if msg_type == UT_METADATA_REJECT {
        bail!("对端拒绝了元数据分片请求（msg_type=2）");
    }
    if msg_type != UT_METADATA_DATA {
        bail!("不是 ut_metadata data 消息（msg_type={msg_type}）");
    }
    // 头部之后跟原始元数据片段
    let (_, used) = super::bencode::Bencode::decode(payload)?;
    let total_size = v
        .get("total_size")
        .and_then(|x| x.as_int())
        .unwrap_or(0) as usize;
    if piece < 0 {
        bail!("ut_metadata 响应缺少 piece");
    }
    Ok((piece as usize, total_size, used))
}

/// 构造 ut_pex 的 announce payload（BEP 11）。
pub fn build_ut_pex_announce(added: &[(std::net::SocketAddr, u8)]) -> Vec<u8> {
    let mut added_field: std::collections::BTreeMap<Vec<u8>, super::bencode::Bencode> =
        std::collections::BTreeMap::new();
    for (addr, flags) in added {
        // compact peer 编码：4 字节 IP + 2 字节端口
        let mut key = match addr.ip() {
            std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
            std::net::IpAddr::V6(v6) => v6.octets().to_vec(),
        };
        key.extend_from_slice(&addr.port().to_be_bytes());
        added_field.insert(key, super::bencode::Bencode::Int(*flags as i64));
    }
    let mut d = std::collections::BTreeMap::new();
    d.insert(b"added".to_vec(), super::bencode::Bencode::Dict(added_field));
    d.insert(
        b"added.f".to_vec(),
        super::bencode::Bencode::Bytes(vec![0u8; 4]),
    );
    super::bencode::Bencode::Dict(d).encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_layout_is_68_bytes() {
        let h = build_handshake(&[0u8; 20], &[1u8; 20]);
        assert_eq!(h.len(), HANDSHAKE_LEN);
        assert_eq!(&h[0..20], &PROTOCOL[..]);
        // 保留位第 5 字节的 0x10 = 支持扩展协议（BEP 10）
        assert_eq!(h[25] & 0x10, 0x10, "必须声明扩展协议支持，否则拿不到 ut_metadata");
        assert_eq!(&h[28..48], &[0u8; 20][..]);
        assert_eq!(&h[48..68], &[1u8; 20][..]);
    }

    #[tokio::test]
    async fn read_handshake_parses_same_layout() {
        // 回归测试：曾经 read_handshake 按 19 字节协议串取偏移（整体错位 1 字节），
        // 结果每个 peer 都判定为"协议标识不匹配"，BT 下载完全不可用。
        let info_hash = [7u8; 20];
        let peer_id = [9u8; 20];
        let buf = build_handshake(&info_hash, &peer_id);
        let mut conn = PeerConnection {
            stream: &buf[..],
            peer_bitfield: None,
            choking: true,
            peer_interested: false,
            ext_local: None,
            ext_remote: None,
            ext_supported: false,
            peer_id: [0u8; 20],
        };
        let got = read_handshake(&mut conn).await.unwrap();
        assert_eq!(got, info_hash);
        assert_eq!(conn.peer_id, peer_id);
        assert!(conn.ext_supported);
    }

    #[tokio::test]
    async fn read_handshake_detects_missing_extension_bit() {
        let mut buf = build_handshake(&[1u8; 20], &[2u8; 20]);
        buf[25] &= !0x10; // 对端不支持扩展协议
        let mut conn = PeerConnection {
            stream: &buf[..],
            peer_bitfield: None,
            choking: true,
            peer_interested: false,
            ext_local: None,
            ext_remote: None,
            ext_supported: false,
            peer_id: [0u8; 20],
        };
        read_handshake(&mut conn).await.unwrap();
        assert!(!conn.ext_supported);
    }

    #[test]
    fn request_frame_layout() {
        let f = build_request(7, 16384, 16384);
        // 长度前缀 = id(1) + index(4) + begin(4) + length(4) = 13
        assert_eq!(&f[0..4], &13u32.to_be_bytes());
        assert_eq!(f[4], MsgId::Request as u8);
        assert_eq!(u32::from_be_bytes(f[5..9].try_into().unwrap()), 7);
        assert_eq!(u32::from_be_bytes(f[9..13].try_into().unwrap()), 16384);
        assert_eq!(u32::from_be_bytes(f[13..17].try_into().unwrap()), 16384);
    }

    #[test]
    fn piece_frame_carries_payload() {
        let data = vec![9u8; 32];
        let f = build_piece(1, 0, &data);
        assert_eq!(&f[0..4], &41u32.to_be_bytes());
        assert_eq!(&f[13..], &data[..]);
    }

    #[test]
    fn bitfield_packs_bits() {
        // BEP 3：第 i 片对应第 i/8 字节的 0x80 >> (i%8) 位（高位在前）
        let bf = vec![true, false, true, true, false, false, false, false, true];
        let f = build_bitfield(&bf);
        // 帧头占 5 字节：4 字节长度 + 1 字节 id
        let payload = &f[5..];
        assert_eq!(payload[0], 0b1011_0000, "第 0、2、3 片置位");
        assert_eq!(payload[1], 0b1000_0000, "第 8 片落在第二字节的最高位");
    }

    #[test]
    fn keepalive_is_zero_length() {
        assert_eq!(build_keepalive(), vec![0, 0, 0, 0]);
    }

    #[test]
    fn parses_ut_metadata_data_header() {
        // BEP 9：data 消息的 msg_type 是 1（0=request，2=reject）
        let header = super::super::bencode::dict(vec![
            ("msg_type", super::super::bencode::Bencode::Int(UT_METADATA_DATA)),
            ("piece", super::super::bencode::Bencode::Int(1)),
            ("total_size", super::super::bencode::Bencode::Int(300)),
        ])
        .encode();
        let mut payload = header;
        payload.extend_from_slice(&[7u8; 16]);
        let (piece, total, used) = parse_ut_metadata_data(&payload).unwrap();
        assert_eq!(piece, 1);
        assert_eq!(total, 300);
        assert_eq!(payload.len() - used, 16);
    }

    #[test]
    fn extension_handshake_uses_integer_ids() {
        // BEP 10：m 里必须是"扩展名 -> 整数 ID"。
        // 曾经写成 {"ut_metadata": {"v": 1}}，对端解析失败后直接断开。
        let payload = build_ut_metadata_handshake(None);
        let (v, _) = super::super::bencode::Bencode::decode(&payload).unwrap();
        let m = v.get("m").and_then(|x| x.as_dict()).unwrap();
        let id = m.get(b"ut_metadata".as_slice()).unwrap();
        assert!(
            matches!(id, super::super::bencode::Bencode::Int(i) if *i > 0),
            "ut_metadata 的 ID 必须是正整数，实际：{id:?}"
        );

        let payload = build_ut_metadata_handshake(Some(1234));
        let (v, _) = super::super::bencode::Bencode::decode(&payload).unwrap();
        assert_eq!(
            v.get("metadata_size").and_then(|x| x.as_int()),
            Some(1234)
        );
    }

    #[test]
    fn metadata_request_uses_msg_type_zero() {
        let encoded = build_ut_metadata_request(3);
        let (v, _) = super::super::bencode::Bencode::decode(&encoded).unwrap();
        assert_eq!(
            v.get("msg_type").and_then(|x| x.as_int()),
            Some(UT_METADATA_REQUEST),
            "request 必须是 msg_type=0，否则对端会忽略请求"
        );
        assert_eq!(v.get("piece").and_then(|x| x.as_int()), Some(3));
    }

    #[test]
    fn rejects_metadata_reject_message() {
        let payload = super::super::bencode::dict(vec![
            ("msg_type", super::super::bencode::Bencode::Int(UT_METADATA_REJECT)),
            ("piece", super::super::bencode::Bencode::Int(4)),
        ])
        .encode();
        let err = parse_ut_metadata_data(&payload).unwrap_err();
        assert!(err.to_string().contains("拒绝"));
    }
}