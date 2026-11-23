/// 当前 Unix 秒（最近游玩时间戳）。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

/// 最近游玩的相对时间标签（首页实例列表）。
///
/// 只按天粒度换算，不依赖时区——避免为了「昨天 22:16」这种精度引入 chrono。
pub fn last_played_label(last_played: Option<i64>) -> String {
    let Some(timestamp) = last_played else {
        return "尚未游玩".to_string();
    };
    let days = (now_unix() - timestamp).max(0) / 86_400;
    match days {
        0 => "今天".to_string(),
        1 => "昨天".to_string(),
        2..=29 => format!("{days} 天前"),
        30..=364 => format!("{} 个月前", days / 30),
        _ => format!("{} 年前", days / 365),
    }
}

/// ISO 时间戳 → `YYYY-MM-DD`（不是合法日期时原样返回前 10 位）
pub fn date_only(iso: &str) -> String {
    iso.chars().take(10).collect()
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 的 days_from_civil）
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// ISO 日期 → 「今天 / 昨天 / N 天前 / N 个月前 / N 年前」。
///
pub fn iso_relative_label(iso: &str) -> String {
    let date = date_only(iso);
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return String::new();
    };
    let (Ok(year), Ok(month), Ok(day)) = (
        year.parse::<i64>(),
        month.parse::<i64>(),
        day.parse::<i64>(),
    ) else {
        return String::new();
    };
    let days = (now_unix() / 86_400 - days_from_civil(year, month, day)).max(0);
    match days {
        0 => "今天".to_string(),
        1 => "昨天".to_string(),
        2..=29 => format!("{days} 天前"),
        30..=364 => format!("{} 个月前", days / 30),
        _ => format!("{} 年前", days / 365),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_unix_is_after_2020() {
        assert!(now_unix() > 1_577_836_800);
    }

    #[test]
    fn iso_label_buckets() {
        assert_eq!(iso_relative_label(""), "");
        assert!(iso_relative_label("2024-08-16T00:00:00Z").ends_with("年前"));
        assert_eq!(date_only("2024-08-16T12:00:00Z"), "2024-08-16");
    }

    #[test]
    fn label_without_timestamp() {
        assert_eq!(last_played_label(None), "尚未游玩");
    }

    #[test]
    fn label_for_recent_days() {
        let now = now_unix();
        assert_eq!(last_played_label(Some(now)), "今天");
        assert_eq!(last_played_label(Some(now - 86_400)), "昨天");
        assert_eq!(last_played_label(Some(now - 3 * 86_400)), "3 天前");
    }

    #[test]
    fn label_for_months_and_years() {
        let now = now_unix();
        assert_eq!(last_played_label(Some(now - 45 * 86_400)), "1 个月前");
        assert_eq!(last_played_label(Some(now - 400 * 86_400)), "1 年前");
    }

    #[test]
    fn future_timestamp_stays_today() {
        // 时钟回拨时不应出现「-3 天前」
        assert_eq!(last_played_label(Some(now_unix() + 86_400)), "今天");
    }
}
