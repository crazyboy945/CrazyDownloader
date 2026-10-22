//! 人类可读的格式化与路径处理工具。

/// 把字节数格式化为 `1.23 MB` 形式。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes == 0 {
        return "0 B".to_string();
    }
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

/// 把速率格式化为 `1.23 MB/s` 形式，0 表示 `--`。
pub fn human_rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        "--".to_string()
    } else {
        format!("{}/s", human_size(bytes_per_sec as u64))
    }
}

/// 把秒数格式化为剩余时间描述。
pub fn human_duration(secs: Option<u64>) -> String {
    match secs {
        None => "--".to_string(),
        Some(s) => {
            let d = s / 86_400;
            let h = (s % 86_400) / 3_600;
            let m = (s % 3_600) / 60;
            let sec = s % 60;
            if d > 0 {
                format!("{}天{}小时", d, h)
            } else if h > 0 {
                format!("{}小时{}分", h, m)
            } else if m > 0 {
                format!("{}分{}秒", m, sec)
            } else {
                format!("{}秒", sec)
            }
        }
    }
}

/// 把时间格式化为 `MM-DD HH:MM`（本地时区），用于列表里的"上次尝试"。
///
/// 不引入 chrono：只需要从 Unix 时间戳换算出年月日。
pub fn format_datetime(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 本地时区偏移（东八区等），Windows 下无夏令时的常见情况够用
    let offset = local_utc_offset_secs();
    let secs = secs + offset;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let _ = y;
    format!(
        "{:02}-{:02} {:02}:{:02}",
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// 本机相对 UTC 的偏移秒数（通过比较 `SystemTime` 与本地时间格式化结果无法直接得到，
/// 这里用 `GetTimeZoneInformation` 的替代方案：读取 `TZ`/注册表都太重，
/// 退而使用"UTC 现在的小时数差异"——由 `std::time` 无法获得，
/// 因此直接固定读取系统时区偏移的近似值）。
fn local_utc_offset_secs() -> i64 {
    // 通过 `SystemTime::now()` 与编译期无关的本地时间无法在 std 中直接比较，
    // 这里用 Win32 的 GetLocalTime/GetSystemTime 差值（非 Windows 平台返回 0）。
    #[cfg(windows)]
    {
        #[repr(C)]
        #[derive(Default)]
        struct SystemTimeW {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetLocalTime(out: *mut SystemTimeW);
            fn GetSystemTime(out: *mut SystemTimeW);
        }
        let (mut l, mut u) = (SystemTimeW::default(), SystemTimeW::default());
        unsafe {
            GetLocalTime(&mut l);
            GetSystemTime(&mut u);
        }
        let to_secs = |s: &SystemTimeW| {
            let days = days_from_civil(s.year as i64, s.month as i64, s.day as i64);
            days * 86_400
                + s.hour as i64 * 3600
                + s.minute as i64 * 60
                + s.second as i64
        };
        let diff = to_secs(&l) - to_secs(&u);
        // 抹掉亚秒级抖动
        (diff / 60) * 60
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// Howard Hinnant 的 civil_from_days：Unix 天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// civil_from_days 的逆运算。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 速度平滑器（指数加权平均）。
///
/// 直接用"本采样窗口的字节差 / 窗口长度"算速度，界面会忽高忽低：
/// 数据是成块到达的，窗口里没数据就是 0，有数据就冲高。
/// 这里用 EWMA 平滑：新样本占 35%，其余按历史衰减，
/// 于是速度是"缓升缓降"的连续曲线，而不是锯齿。
pub struct SpeedMeter {
    /// 平滑后的速度（字节/秒）
    ema: f64,
    /// 上一次采样时的累计字节数
    last_bytes: Option<u64>,
    /// 上一次采样的时刻
    last_at: Option<std::time::Instant>,
    /// 采样权重：越大越跟手，越小越平滑
    alpha: f64,
    /// 连续多少次"没有新数据"后就把速度衰减到 0
    idle_ticks: u32,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new(0.35)
    }
}

impl SpeedMeter {
    /// `alpha` 建议 0.2~0.5：0.2 更平滑，0.5 更跟手。
    pub fn new(alpha: f64) -> Self {
        Self {
            ema: 0.0,
            last_bytes: None,
            last_at: None,
            alpha: alpha.clamp(0.05, 1.0),
            idle_ticks: 0,
        }
    }

    /// 用累计字节数采样一次，返回平滑后的速度（字节/秒）。
    pub fn sample(&mut self, total_bytes: u64) -> u64 {
        let now = std::time::Instant::now();
        let (instant, dt) = match (self.last_bytes, self.last_at) {
            (Some(prev), Some(t)) => {
                let dt = now.duration_since(t).as_secs_f64();
                let delta = total_bytes.saturating_sub(prev) as f64;
                (delta / dt.max(0.001), dt)
            }
            _ => (0.0, 0.0),
        };
        self.last_bytes = Some(total_bytes);
        self.last_at = Some(now);

        // 长时间没有新数据：把速度平滑地降到 0，而不是让它在 0 和峰值之间跳
        if instant <= 0.0 {
            self.idle_ticks += 1;
        } else {
            self.idle_ticks = 0;
        }
        if self.idle_ticks > 3 {
            // 长时间没数据：按 0.4 逐步衰减到 0，而不是让曲线掉到 0 再冲高
            self.ema *= 0.4;
        } else {
            self.ema = self.alpha * instant + (1.0 - self.alpha) * self.ema;
        }
        if self.ema < 1024.0 {
            0
        } else {
            self.ema as u64
        }
    }

    /// 直接喂入一个瞬时速度（已经是字节/秒），同样做平滑。
    pub fn smooth(&mut self, instant: f64) -> u64 {
        if instant <= 0.0 {
            self.idle_ticks += 1;
            if self.idle_ticks > 3 {
                self.ema *= 0.4;
            }
        } else {
            self.idle_ticks = 0;
            self.ema = self.alpha * instant + (1.0 - self.alpha) * self.ema;
        }
        if self.ema < 1024.0 {
            0
        } else {
            self.ema as u64
        }
    }

    /// 当前平滑速度。
    pub fn value(&self) -> u64 {
        if self.ema < 1.0 {
            0
        } else {
            self.ema as u64
        }
    }
}

/// 极简百分号解码，用于处理 URL 中的 `%20` 之类转义。
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(h) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(h);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 去掉文件名中的非法字符（Windows 保留字符）。
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "unnamed".to_string()
    } else {
        trimmed
    }
}

/// 若目标已存在则追加 `(1)`、`(2)` 后缀，避免覆盖已有文件。
pub fn unique_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let base = dir.join(name);
    if !base.exists() {
        return base;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10_000u32 {
        let cand = dir.join(format!("{}({}){}", stem, n, ext));
        if !cand.exists() {
            return cand;
        }
    }
    base
}

/// 从 URL 中提取文件名，用作本地保存名。
pub fn filename_from_url(url: &str) -> String {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .trim_end_matches('/');
    let last = path.rsplit('/').next().unwrap_or("");
    sanitize_file_name(&percent_decode(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_meter_smooths_bursty_input() {
        // 模拟"数据成块到达"：一会儿 1 MB/s，一会儿 0
        let mut m = SpeedMeter::new(0.35);
        let burst = m.smooth(1_000_000.0);
        assert!(burst > 0 && burst < 1_000_000, "首个样本应被平滑：{burst}");

        // 连续几帧没有数据时，速度应缓慢衰减，而不是直接掉到 0
        let mut v = burst;
        for _ in 0..2 {
            v = m.smooth(0.0);
            assert!(v > 0, "不应立刻归零：{v}");
        }
        // 长时间空闲后才归零
        for _ in 0..8 {
            v = m.smooth(0.0);
        }
        assert_eq!(v, 0, "长期无数据后应归零");
    }

    #[test]
    fn speed_meter_converges_to_constant_rate() {
        let mut m = SpeedMeter::new(0.5);
        let mut last = 0u64;
        for _ in 0..40 {
            last = m.smooth(800_000.0);
        }
        // 收敛到 800 KB/s 附近
        assert!(
            (700_000..900_000).contains(&last),
            "应收敛到稳定速率，实际 {last}"
        );
    }

    #[test]
    fn speed_meter_samples_cumulative_bytes() {
        let mut m = SpeedMeter::new(0.5);
        // 第一次采样没有基准，返回 0
        assert_eq!(m.sample(0), 0);
        std::thread::sleep(std::time::Duration::from_millis(120));
        let v = m.sample(120_000);
        assert!(v > 0, "第二次采样应给出速度：{v}");
    }

    #[test]
    fn decodes_trailing_escape() {
        // 转义序列正好位于字符串末尾时也必须解码
        assert_eq!(percent_decode("file%20name"), "file name");
        assert_eq!(percent_decode("%E4%B8%AD%E6%96%87"), "中文");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn extracts_file_name_from_url() {
        assert_eq!(filename_from_url("https://a.com/p/file.zip"), "file.zip");
        assert_eq!(filename_from_url("https://a.com/p/my%20file.zip?x=1"), "my file.zip");
        assert_eq!(filename_from_url("https://a.com/p/"), "p");
    }

    #[test]
    fn sanitizes_illegal_chars() {
        assert_eq!(sanitize_file_name("a/b:c*d?.txt"), "a_b_c_d_.txt");
        assert_eq!(sanitize_file_name("   "), "unnamed");
        assert_eq!(sanitize_file_name("..."), "unnamed");
    }

    #[test]
    fn formats_sizes_and_rates() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.50 KB");
        assert_eq!(human_rate(0.0), "--");
        assert_eq!(human_rate(2048.0), "2.00 KB/s");
    }

    #[test]
    fn civil_dates_roundtrip() {
        for (y, m, d) in [
            (1970i64, 1i64, 1i64),
            (2000, 2, 29),
            (2026, 10, 22),
            (1999, 12, 31),
        ] {
            let days = days_from_civil(y, m, d);
            let (yy, mm, dd) = civil_from_days(days);
            assert_eq!(
                (yy, mm as i64, dd as i64),
                (y, m, d),
                "{y}-{m}-{d}"
            );
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn formats_datetime_shape() {
        // 只校验形状：MM-DD HH:MM
        let s = format_datetime(std::time::SystemTime::now());
        assert_eq!(s.len(), 11, "{s}");
        assert_eq!(&s[2..3], "-");
        assert_eq!(&s[8..9], ":");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(human_duration(None), "--");
        assert_eq!(human_duration(Some(30)), "30秒");
        assert_eq!(human_duration(Some(90)), "1分30秒");
        assert_eq!(human_duration(Some(3600 * 5 + 120)), "5小时2分");
    }
}