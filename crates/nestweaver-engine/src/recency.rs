/// Parse an ISO 8601 string (subset) to Unix epoch seconds (f64).
///
/// Accepts:
/// - `YYYY-MM-DD`
/// - `YYYY-MM-DDTHH:MM:SS`
/// - `YYYY-MM-DDTHH:MM:SSZ` (UTC, Z suffix stripped)
/// - `YYYY-MM-DDTHH:MM:SS+00:00` / `…-00:00` (UTC)
/// - `YYYY-MM-DDTHH:MM:SS±HH:MM` (non-UTC offset applied correctly)
///
/// Returns 0.0 on any parse failure so the node gets no boost.
pub fn parse_iso8601_to_epoch(s: &str) -> f64 {
    let s = s.trim();

    // Split into date/time/offset parts.
    let (date_part, time_part, tz_offset_secs) = if let Some(t_pos) = s.find('T') {
        let date = &s[..t_pos];
        let after_t = &s[t_pos + 1..];

        // Find timezone offset in the time portion.
        // The offset starts at +/- after the seconds; a leading '-' in the time
        // itself is not an offset, so skip the first character of after_t.
        let (time_str, offset_secs) = extract_tz(after_t);
        (date, Some(time_str), offset_secs)
    } else {
        (s, None, 0i64)
    };

    let dp: Vec<&str> = date_part.split('-').collect();
    if dp.len() != 3 {
        return 0.0;
    }
    let year: i64 = dp[0].parse().unwrap_or(0);
    let month: u32 = dp[1].parse().unwrap_or(0);
    let day: u32 = dp[2].parse().unwrap_or(0);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0.0;
    }

    let (hour, minute, second) = if let Some(t) = time_part {
        let tp: Vec<&str> = t.split(':').collect();
        if tp.len() < 2 {
            (0u32, 0u32, 0u32)
        } else {
            let h = tp[0].parse().unwrap_or(0);
            let m = tp[1].parse().unwrap_or(0);
            let sec: u32 = if tp.len() >= 3 {
                tp[2]
                    .split('.')
                    .next()
                    .unwrap_or("0")
                    .parse()
                    .unwrap_or(0)
            } else {
                0
            };
            (h, m, sec)
        }
    } else {
        (0, 0, 0)
    };

    let m_adj = if month <= 2 { month + 9 } else { month - 3 };
    let y_adj = if month <= 2 { year - 1 } else { year };
    let era = if y_adj >= 0 {
        y_adj / 400
    } else {
        (y_adj - 399) / 400
    };
    let yoe = (y_adj - era * 400) as u64;
    let doy = (153 * m_adj as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = (era * 146_097 + doe as i64) - 719_468;
    let secs =
        days * 86_400 + hour as i64 * 3600 + minute as i64 * 60 + second as i64 - tz_offset_secs;
    if secs < 0 { 0.0 } else { secs as f64 }
}

/// Split a time string like `"14:30:00+05:30"` or `"14:30:00Z"` into
/// the bare time part (without tz) and the offset expressed in seconds.
///
/// A positive offset means the local clock is ahead of UTC, so to convert
/// to UTC we subtract it: `utc = local - offset`.
fn extract_tz(after_t: &str) -> (&str, i64) {
    // 'Z' suffix → UTC.
    if let Some(base) = after_t.strip_suffix('Z') {
        return (base, 0);
    }
    // `+00:00` / `-00:00` → UTC.
    if let Some(base) = after_t.strip_suffix("+00:00").or_else(|| after_t.strip_suffix("-00:00")) {
        return (base, 0);
    }
    // Look for a sign character after position 5 (past "HH:MM") to find the
    // start of a timezone offset.
    let search_start = after_t.len().min(5);
    let offset_pos = after_t[search_start..]
        .find(['+', '-'])
        .map(|p| search_start + p);
    if let Some(pos) = offset_pos {
        let time_str = &after_t[..pos];
        let offset_str = &after_t[pos..];
        let sign: i64 = if offset_str.starts_with('-') { -1 } else { 1 };
        let digits = &offset_str[1..];
        let parts: Vec<&str> = digits.split(':').collect();
        let oh: i64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let om: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let offset_secs = sign * (oh * 3600 + om * 60);
        (time_str, offset_secs)
    } else {
        (after_t, 0)
    }
}
