//! Where the labels so far have landed, and where the next window should put its
//! furniture.
//!
//! Passive clicks pile up where the user's applications put their controls: a left
//! rail, a tab strip, a diff column. The trainer exists to fill the rest, so it keeps
//! a histogram of accepted labels over the display and biases each new window's
//! sidebar side and toolbar edge toward the emptier half. Positions are stored as
//! fractions of the window so the histogram survives a resize and a different panel.

use gaze_core::GlobalPx;

/// Bins across the window.
pub const COLS: usize = 8;

/// Bins down the window.
pub const ROWS: usize = 4;

/// The histogram.
#[derive(Clone, Debug)]
pub struct Coverage {
    counts : [u32; COLS * ROWS],
}

// --- Coverage ---

impl Coverage {
    /// An empty histogram.
    pub fn new() -> Coverage {
        Coverage { counts: [0; COLS * ROWS] }
    }

    /// Counts a label at fractions `(fx, fy)` of the window.
    pub fn add(&mut self, fx: f64, fy: f64) {
        self.counts[bin(fx, fy)] += 1;
    }

    /// Labels in the bin containing `(fx, fy)`.
    pub fn count(&self, fx: f64, fy: f64) -> u32 {
        self.counts[bin(fx, fy)]
    }

    /// Total labels counted.
    pub fn total(&self) -> u32 {
        self.counts.iter().sum()
    }

    /// The emptiest bin's count and the fullest's.
    pub fn range(&self) -> (u32, u32) {
        let min = self.counts.iter().copied().min().unwrap_or(0);
        let max = self.counts.iter().copied().max().unwrap_or(0);

        (min, max)
    }

    /// The share of labels in the left half of the window and the share in the top
    /// half, each in `[0, 1]`.
    ///
    /// Both are 0.5 when nothing has been counted, so an empty histogram asks for
    /// nothing in particular.
    pub fn halves(&self) -> (f64, f64) {
        let total = self.total();

        if total == 0 {
            return (0.5, 0.5);
        }

        let mut left = 0u32;
        let mut top  = 0u32;

        for row in 0..ROWS {
            for col in 0..COLS {
                let n = self.counts[row * COLS + col];

                if col < COLS / 2 {
                    left += n;
                }

                if row < ROWS / 2 {
                    top += n;
                }
            }
        }

        (f64::from(left) / f64::from(total), f64::from(top) / f64::from(total))
    }

    /// Loads every `click` record in the session files under `dir` that landed on
    /// `output`, whose logical rectangle is `origin` and `size` in global pixels.
    ///
    /// The trainer runs full screen on that output, so its window is the output and a
    /// global click maps to a window fraction by subtracting the origin.
    pub fn load_sessions(
        &mut self,
        dir    : &std::path::Path,
        output : &str,
        origin : GlobalPx,
        size   : (f64, f64),
    )
        -> std::io::Result<usize>
    {
        let mut loaded = 0;

        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(0);
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let text = std::fs::read_to_string(&path)?;

            for line in text.lines() {
                if !line.contains("\"kind\":\"click\"") {
                    continue;
                }

                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };

                let Some(click) = value.get("click") else {
                    continue;
                };

                if click.get("output").and_then(|o| o.as_str()) != Some(output) {
                    continue;
                }

                let (Some(x), Some(y)) = (
                    click.pointer("/px/x").and_then(|v| v.as_f64()),
                    click.pointer("/px/y").and_then(|v| v.as_f64()),
                ) else {
                    continue;
                };

                self.add((x - origin.x) / size.0, (y - origin.y) / size.1);

                loaded += 1;
            }
        }

        Ok(loaded)
    }
}

impl Default for Coverage {
    fn default() -> Self {
        Self::new()
    }
}

/// The bin index for fractions `(fx, fy)`, clamped to the grid.
fn bin(fx: f64, fy: f64) -> usize {
    let col = ((fx * COLS as f64).floor() as isize).clamp(0, COLS as isize - 1) as usize;
    let row = ((fy * ROWS as f64).floor() as isize).clamp(0, ROWS as isize - 1) as usize;

    row * COLS + col
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_cover_the_unit_square_and_clamp_outside_it() {
        assert_eq!(bin(0.0, 0.0), 0);
        assert_eq!(bin(0.99, 0.99), COLS * ROWS - 1);
        assert_eq!(bin(-1.0, 2.0), (ROWS - 1) * COLS);
        assert_eq!(bin(0.5, 0.0), COLS / 2);
    }

    #[test]
    fn halves_are_even_when_empty_and_follow_the_labels_otherwise() {
        let mut coverage = Coverage::new();

        assert_eq!(coverage.halves(), (0.5, 0.5));

        // Three in the top left, one in the bottom right.
        for _ in 0..3 {
            coverage.add(0.05, 0.05);
        }

        coverage.add(0.95, 0.95);

        let (left, top) = coverage.halves();

        assert!((left - 0.75).abs() < 1e-9, "left share {left}");
        assert!((top - 0.75).abs() < 1e-9, "top share {top}");
    }

    #[test]
    fn session_files_are_loaded_by_output_and_shifted_by_the_origin() {
        let dir = std::env::temp_dir().join("gaze-trainer-coverage-test");
        let _   = std::fs::create_dir_all(&dir);

        let path = dir.join("s.jsonl");
        std::fs::write(&path, concat!(
            "{\"kind\":\"meta\"}\n",
            "{\"kind\":\"click\",\"click\":{\"output\":\"DP-1\",\"px\":{\"x\":2659.0,\"y\":100.0}}}\n",
            "{\"kind\":\"click\",\"click\":{\"output\":\"DP-2\",\"px\":{\"x\":10.0,\"y\":10.0}}}\n",
        )).unwrap();

        let mut coverage = Coverage::new();
        let loaded = coverage
            .load_sessions(&dir, "DP-1", GlobalPx { x: 2559.0, y: 0.0 }, (3840.0, 1600.0))
            .unwrap();

        assert_eq!(loaded, 1);
        assert_eq!(coverage.count(100.0 / 3840.0, 100.0 / 1600.0), 1);
        assert_eq!(coverage.total(), 1);

        let _ = std::fs::remove_file(&path);
    }
}
