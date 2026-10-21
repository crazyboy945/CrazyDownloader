//! rust-dm —— 多协议下载工具的核心库。
//!
//! 拆分为 lib + bin 后，引擎（HTTP / FTP / BitTorrent）可以被集成测试直接调用，
//! 从而对分片下载、断点续传等真实 IO 逻辑做端到端验证。

pub mod common;
pub mod engine;
pub mod ui;
