//! Output rendering: the `-o` format enum, a small padded-column table writer,
//! and deterministic humanizers for bytes and ages. All pure — every command
//! builds its output through these and the dispatcher prints once.
//!
//! The byte humanizer and the empty-cell placeholder live in
//! [`kopiur_ops::format`] and are re-exported here: the shared layer's reports
//! render the same cells, and a table where `-` meant one thing in the CLI and
//! another in the web UI would be two conventions wearing one name.

use chrono::{DateTime, Utc};

/// Placeholder for an absent value in table cells. Re-exported from the shared
/// operations layer so the CLI and the web UI agree on "nothing reported".
pub use kopiur_ops::format::EMPTY_CELL;
/// Humanize a byte count. Re-exported from the shared operations layer so a
/// size reads identically wherever it is rendered.
pub use kopiur_ops::format::human_bytes;

/// Every `-o` value the plugin accepts. Dispatchers `match` this exhaustively,
/// so adding a format forces every command to handle it.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Human-readable columns (the default).
    #[default]
    Table,
    /// Table plus the extra detail columns.
    Wide,
    /// Kubernetes objects as YAML (a `v1/List` for list commands).
    Yaml,
    /// Kubernetes objects as JSON (a `v1/List` for list commands).
    Json,
    /// `<resource>.<group>/<name>` lines, like `kubectl -o name`.
    Name,
}

/// A padded-column table. Cells are plain strings; rendering left-aligns each
/// column to its widest cell with a two-space gap, like `kubectl get`.
pub struct Table {
    headers: Vec<&'static str>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// Start a table with the given column headers.
    pub fn new(headers: Vec<&'static str>) -> Self {
        Self {
            headers,
            rows: Vec::new(),
        }
    }

    /// Append one row. Must have exactly as many cells as there are headers —
    /// a mismatch is a programming error, caught loudly in tests.
    pub fn push(&mut self, row: Vec<String>) {
        assert_eq!(
            row.len(),
            self.headers.len(),
            "table row width must match headers"
        );
        self.rows.push(row);
    }

    /// Number of data rows.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no data rows have been added.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Render with each column padded to its widest cell, two-space gaps, and
    /// no trailing whitespace.
    pub fn render(&self) -> String {
        let cols = self.headers.len();
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.len()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.len());
            }
        }
        let mut out = String::new();
        let write_row = |cells: Vec<&str>, out: &mut String| {
            for (i, cell) in cells.iter().enumerate() {
                if i + 1 == cols {
                    out.push_str(cell);
                } else {
                    out.push_str(&format!("{cell:<width$}  ", width = widths[i]));
                }
            }
            // No trailing spaces even when the last cell is short.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
        };
        write_row(self.headers.to_vec(), &mut out);
        for row in &self.rows {
            write_row(row.iter().map(String::as_str).collect(), &mut out);
        }
        out
    }
}

/// Humanize an age the way `kubectl get` does: the largest single unit
/// (`42s`, `12m`, `5h`, `3d`, `2y`). Takes `now` explicitly so it is
/// deterministic and testable.
pub fn human_age(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds().max(0);
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 60 * 60 => format!("{}m", s / 60),
        s if s < 48 * 60 * 60 => format!("{}h", s / 3600),
        s if s < 365 * 24 * 60 * 60 => format!("{}d", s / 86400),
        s => format!("{}y", s / (365 * 24 * 60 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn table_pads_columns_and_strips_trailing_space() {
        let mut t = Table::new(vec!["NAME", "PHASE"]);
        t.push(vec!["a-very-long-name".into(), "Running".into()]);
        t.push(vec!["b".into(), "Succeeded".into()]);
        let rendered = t.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "NAME              PHASE");
        assert_eq!(lines[1], "a-very-long-name  Running");
        assert_eq!(lines[2], "b                 Succeeded");
        assert!(lines.iter().all(|l| !l.ends_with(' ')));
    }

    #[test]
    fn human_age_matches_kubectl_style() {
        let now = Utc.with_ymd_and_hms(2026, 6, 11, 12, 0, 0).unwrap();
        let at = |secs: i64| now - chrono::Duration::seconds(secs);
        assert_eq!(human_age(at(42), now), "42s");
        assert_eq!(human_age(at(119), now), "119s");
        assert_eq!(human_age(at(120), now), "2m");
        assert_eq!(human_age(at(3 * 3600), now), "3h");
        assert_eq!(human_age(at(40 * 3600), now), "40h");
        assert_eq!(human_age(at(3 * 86400), now), "3d");
        assert_eq!(human_age(at(800 * 86400), now), "2y");
        // A clock-skewed future timestamp clamps to 0s rather than going negative.
        assert_eq!(human_age(now + chrono::Duration::seconds(30), now), "0s");
    }
}
