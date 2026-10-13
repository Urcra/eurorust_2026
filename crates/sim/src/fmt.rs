//! Exact decimal formatting of the integer units, for reports.

use crate::types::{Cash, Price, Qty};

/// Cash as USD with all 7 decimals, e.g. `-12.3456789`.
pub fn usd(cash: Cash) -> String {
    let (sign, abs) = (if cash < 0 { "-" } else { "" }, cash.unsigned_abs());
    format!("{sign}{}.{:07}", abs / 10_000_000, abs % 10_000_000)
}

/// Price in ticks as USD, e.g. `83021.42`.
pub fn price(ticks: Price) -> String {
    let (sign, abs) = (if ticks < 0 { "-" } else { "" }, ticks.unsigned_abs());
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// Quantity in lots as BTC, e.g. `0.01000`.
pub fn btc(lots: Qty) -> String {
    let (sign, abs) = (if lots < 0 { "-" } else { "" }, lots.unsigned_abs());
    format!("{sign}{}.{:05}", abs / 100_000, abs % 100_000)
}

/// Integer with thousands separators, e.g. `863,999`.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Milliseconds since the Unix epoch as `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
pub fn utc_ms(ms: u64) -> String {
    let (secs, ms) = (ms / 1_000, ms % 1_000);
    let (days, sod) = ((secs / 86_400) as i64, secs % 86_400);
    // Civil-from-days, Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{ms:03}", sod / 3_600, sod / 60 % 60, sod % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_units() {
        assert_eq!(usd(-123_456_789), "-12.3456789");
        assert_eq!(usd(5), "0.0000005");
        assert_eq!(usd(0), "0.0000000");
        assert_eq!(price(8_302_142), "83021.42");
        assert_eq!(price(5), "0.05");
        assert_eq!(btc(1_000), "0.01000");
        assert_eq!(btc(-1_000_000), "-10.00000");
    }

    #[test]
    fn formats_counts() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(863_999), "863,999");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn formats_utc() {
        assert_eq!(utc_ms(0), "1970-01-01 00:00:00.000");
        assert_eq!(utc_ms(1_790_607_733_602), "2026-09-28 15:02:13.602");
        assert_eq!(utc_ms(951_782_400_000), "2000-02-29 00:00:00.000");
    }
}
