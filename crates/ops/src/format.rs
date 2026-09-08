//! Shared human-readable formatting for report cells and summary lines.
//!
//! One implementation, because a byte count rendered `1.5 KiB` in `kubectl
//! kopiur snapshots list` and `1536 B` in the web UI would be the same fact
//! told two ways. Everything here is pure and unit-tested.

/// Placeholder for an absent value in table cells, matching `kubectl`'s
/// `<none>`-adjacent convention without the angle brackets.
pub const EMPTY_CELL: &str = "-";

/// Humanize a byte count: `512 B`, `1.5 KiB`, `2.0 GiB`. Binary units, one
/// decimal, matching kopia's own reporting style.
pub fn human_bytes(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "";
    for u in UNITS {
        value /= 1024.0;
        unit = u;
        if value < 1024.0 {
            break;
        }
    }
    format!("{value:.1} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        // The last value that stays in bytes: 1024 is the first KiB.
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
        assert_eq!(human_bytes(5_368_709_120), "5.0 GiB");
    }

    #[test]
    fn empty_cell_is_the_single_placeholder() {
        assert_eq!(EMPTY_CELL, "-");
    }
}
