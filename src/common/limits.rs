//! 令牌桶限速器。速率设为 0 表示不限速。
//!
//! 同时服务于 HTTP 任务、BT 任务以及全局限速，通过共享实例实现。

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// 线程/任务安全的令牌桶。桶容量取 1 秒的流量，允许短时突发。
pub struct TokenBucket {
    inner: Mutex<Bucket>,
    rate: Mutex<f64>,
}

impl TokenBucket {
    /// 不限速（速率 0）。
    pub fn new() -> Self {
        Self::new_with_rate(0)
    }

    pub fn new_with_rate(bytes_per_sec: u64) -> Self {
        Self {
            inner: Mutex::new(Bucket {
                tokens: 0.0,
                last: Instant::now(),
            }),
            rate: Mutex::new(bytes_per_sec as f64),
        }
    }

    /// 设置速率（字节/秒），0 表示不限速。
    pub fn set_rate(&self, bytes_per_sec: u64) {
        *self.rate.lock().unwrap() = bytes_per_sec as f64;
    }

    pub fn rate(&self) -> u64 {
        *self.rate.lock().unwrap() as u64
    }

    /// 尝试取 `n` 字节配额，返回需要等待的时长。
    /// 返回 `None` 表示已成功扣减。
    fn try_take(&self, n: f64) -> Option<Duration> {
        let rate = *self.rate.lock().unwrap();
        if rate <= 0.0 {
            return None;
        }
        let mut b = self.inner.lock().unwrap();
        let now = Instant::now();
        let elapsed = (now - b.last).as_secs_f64();
        // 桶容量 = 1 秒额度，允许短时突发
        b.tokens = (b.tokens + elapsed * rate).min(rate);
        b.last = now;
        if b.tokens >= n {
            b.tokens -= n;
            None
        } else {
            Some(Duration::from_secs_f64((n - b.tokens) / rate))
        }
    }

    /// 异步获取配额（用于 tokio 任务）。
    pub async fn acquire(&self, n: u64) {
        if n == 0 {
            return;
        }
        let n = n as f64;
        loop {
            match self.try_take(n) {
                None => return,
                // 分片等待，保证外部的暂停/取消信号能及时被响应
                Some(d) => tokio::time::sleep(d.min(Duration::from_millis(200))).await,
            }
        }
    }

    /// 同步获取配额（用于 `spawn_blocking` 内部的下载线程）。
    pub fn blocking_acquire(&self, n: u64) {
        if n == 0 {
            return;
        }
        let n = n as f64;
        loop {
            match self.try_take(n) {
                None => return,
                Some(d) => std::thread::sleep(d.min(Duration::from_millis(200))),
            }
        }
    }
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_bucket_never_waits() {
        let b = TokenBucket::new();
        assert!(b.try_take(1_000_000.0).is_none());
    }

    #[test]
    fn limited_bucket_asks_to_wait() {
        let b = TokenBucket::new_with_rate(1_000);
        // 桶为空，取 1000 字节需要等待约 1 秒
        let d = b.try_take(1_000.0).expect("应需要等待");
        assert!(d.as_secs_f64() > 0.5, "等待时长应接近 1s，实际 {d:?}");
    }
}