use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static MESSAGE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Percent-encodes everything except RFC 3986 unreserved characters (for URI path segments).
pub fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

pub fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }

    escaped
}

pub fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());

    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if (ch as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            _ => escaped.push(ch),
        }
    }

    escaped
}

pub fn make_uuid() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id() as u128;
    let counter = MESSAGE_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let mut value = now ^ (pid << 64) ^ counter;

    value &= !(0xf_u128 << 76);
    value |= 4_u128 << 76;
    value &= !(0x3_u128 << 62);
    value |= 0x2_u128 << 62;

    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        (value >> 96) as u32,
        (value >> 80) as u16,
        (value >> 64) as u16,
        (value >> 48) as u16,
        value & 0xffff_ffff_ffff_u128
    )
}

pub fn message_id() -> String {
    format!("uuid:{}", make_uuid())
}

pub fn utc_parts() -> (i64, i64, i64, i64, i64, i64) {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    unix_seconds_to_utc(seconds)
}

/// Current UTC time plus `offset_seconds` as an `xs:dateTime`, e.g. `2026-11-10T12:00:00Z`.
pub fn utc_iso(offset_seconds: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (year, month, day, hour, minute, second) = unix_seconds_to_utc(now + offset_seconds);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parses an `xs:duration` of the time-only form `PT#H#M#S` (seconds may be fractional).
pub fn parse_xs_duration(value: &str) -> Option<std::time::Duration> {
    let mut rest = value.trim().strip_prefix("PT")?;
    let mut seconds = 0.0_f64;
    let mut any = false;
    while !rest.is_empty() {
        let end = rest.find(|c: char| c.is_ascii_alphabetic())?;
        let amount: f64 = rest[..end].parse().ok()?;
        seconds += amount
            * match &rest[end..=end] {
                "H" => 3600.0,
                "M" => 60.0,
                "S" => 1.0,
                _ => return None,
            };
        rest = &rest[end + 1..];
        any = true;
    }
    (any && seconds.is_finite() && seconds >= 0.0)
        .then(|| std::time::Duration::from_secs_f64(seconds))
}

fn unix_seconds_to_utc(seconds: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    (year, month, day, hour, minute, second)
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };

    if month <= 2 {
        year += 1;
    }

    (year, month, day)
}

#[cfg(test)]
mod tests {
    #[test]
    fn xs_duration_parsing() {
        use std::time::Duration;
        assert_eq!(
            super::parse_xs_duration("PT5S"),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            super::parse_xs_duration("PT1M30S"),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            super::parse_xs_duration("PT0.5S"),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            super::parse_xs_duration("PT1H"),
            Some(Duration::from_secs(3600))
        );
        for bad in ["", "PT", "5S", "P1D", "PT-5S", "PTxS"] {
            assert_eq!(super::parse_xs_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn utc_iso_has_the_xs_datetime_shape() {
        let value = super::utc_iso(0);
        assert_eq!(value.len(), 20, "{value}");
        assert!(value.ends_with('Z') && value.as_bytes()[10] == b'T');
    }

    use super::{json_escape, make_uuid, percent_encode};

    #[test]
    fn generated_uuid_has_canonical_shape() {
        let uuid = make_uuid();
        let groups: Vec<usize> = uuid.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12], "{uuid}");
    }

    #[test]
    fn percent_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode("FakeCam-1.0_x~"), "FakeCam-1.0_x~");
        assert_eq!(percent_encode("Fake Cam/é"), "Fake%20Cam%2F%C3%A9");
    }

    #[test]
    fn json_escape_handles_quotes_backslashes_and_control_chars() {
        assert_eq!(json_escape("a\"b\\c\nd\u{1}é"), "a\\\"b\\\\c\\nd\\u0001é");
    }
}
