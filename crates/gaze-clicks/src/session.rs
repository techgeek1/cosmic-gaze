//! Writing the session file.
//!
//! The format is `gaze-provider-et5`'s, unchanged, so `gaze-et5-cli dataset export` and
//! the Phase C harness read a day of clicks the same way they read a recorded session.
//! Three record kinds go in per accepted click:
//!
//! - a `"stop"` with `phase: "click"`, whose window is the second before the press: the
//!   fixation the label is about;
//! - a `"click"` with everything about the press itself, keyed to the stop by `n`;
//! - the `"frame"` records of the gaze window, deduplicated against earlier clicks.
//!
//! The file is flushed after every click. This process runs all day and will be killed
//! rather than stopped, so a session that loses its `meta_end` line has to still be
//! worth loading, and it is: the reader treats a missing end line as an interrupted
//! session rather than a broken one.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gaze_provider_et5::record::{
    CLICK_BACKGROUND, CLICK_PHASE, ClickRecord, SessionEnd, SessionMeta, click_line, frame_line,
    stop_line,
};
use gaze_provider_et5::sweep::{StopWindow, TimedFrame};

use crate::frames::FrameDedup;

/// One open session file.
pub struct ClickSession {
    path         : PathBuf,
    writer       : BufWriter<std::fs::File>,
    /// The meta line's display: the connector the tracker's plane is declared on.
    /// Frame records are filed under it. Stops are filed under the click's own output,
    /// which is wherever the pointer was.
    display      : String,
    /// Body hash of the blob these rows describe. A change means a retrain, and a
    /// retrain means a new file.
    blob_sha256  : String,
    dedup        : FrameDedup,
    clicks       : u64,
    frames       : usize,
    valid_frames : usize,
}

// --- ClickSession ---

impl ClickSession {
    /// Creates the file and writes its meta line.
    ///
    /// `out` overrides the generated path entirely; otherwise the file is
    /// `<dir>/<session_id>.jsonl`, and the session id is already in `meta`.
    pub fn create(dir: &Path, out: Option<&Path>, meta: &SessionMeta) -> Result<ClickSession> {
        let path = {
            match out {
                Some(path) => path.to_path_buf(),
                None       => dir.join(format!("{}.jsonl", meta.session_id)),
            }
        };

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let file      = std::fs::File::create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        let mut writer = BufWriter::new(file);

        writeln!(writer, "{}", serde_json::json!(meta))
            .with_context(|| format!("writing the meta line of {}", path.display()))?;
        writer.flush().context("flushing the meta line")?;

        Ok(ClickSession {
            path         : path,
            writer       : writer,
            display      : meta.display.clone(),
            blob_sha256  : meta.blob_sha256.clone(),
            dedup        : FrameDedup::new(),
            clicks       : 0,
            frames       : 0,
            valid_frames : 0,
        })
    }

    /// Where the session is being written.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Body hash of the blob this session is keyed to.
    pub fn blob_sha256(&self) -> &str {
        &self.blob_sha256
    }

    /// The index the next click will be written under.
    pub fn next_index(&self) -> u64 {
        self.clicks
    }

    /// How many clicks have been written.
    pub fn clicks(&self) -> u64 {
        self.clicks
    }

    /// Writes one accepted click and the gaze frames around it.
    ///
    /// `stop` is `None` in `--no-tracker` mode, where there is no gaze to window and
    /// the click record stands alone as a record of what the recogniser saw.
    pub fn write_click(
        &mut self,
        click  : &ClickRecord,
        stop   : Option<&StopWindow>,
        frames : &[TimedFrame],
    )
        -> Result<()>
    {
        if let Some(stop) = stop {
            let line = stop_line(&click.output, CLICK_PHASE, CLICK_BACKGROUND, stop,
                                 Some(click.n));

            writeln!(self.writer, "{line}")?;
        }

        writeln!(self.writer, "{}", click_line(&click.output, click))?;

        for frame in self.dedup.take(frames) {
            writeln!(self.writer, "{}", frame_line(&self.display, frame))?;

            self.frames += 1;

            if frame.frame.any_valid() {
                self.valid_frames += 1;
            }
        }

        self.writer.flush()
            .with_context(|| format!("flushing {}", self.path.display()))?;

        self.clicks += 1;

        Ok(())
    }

    /// Writes the end line and closes the file.
    ///
    /// `blob_sha256` is the hash retrieved at the end, which differing from the meta
    /// line's means the firmware mutated its own model mid-session.
    pub fn finish(mut self, blob_sha256: &str) -> Result<()> {
        let end = SessionEnd {
            kind         : "meta_end".into(),
            blob_sha256  : blob_sha256.to_string(),
            frames       : self.frames,
            valid_frames : self.valid_frames,
        };

        writeln!(self.writer, "{}", serde_json::json!(end))?;

        self.writer.flush()
            .with_context(|| format!("flushing {}", self.path.display()))?;

        Ok(())
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{GlobalPx, Rect};
    use gaze_provider_et5::Et5Frame;
    use gaze_provider_et5::record::{ClickElement, SESSION_FORMAT};
    use gaze_provider_et5::ttp::{DisplayArea, DisplayRect};

    use super::*;

    /// A meta line for a synthetic click session.
    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            kind              : "meta".into(),
            format            : SESSION_FORMAT,
            session_id        : id.into(),
            created_unix_s    : 0.0,
            blob_sha256       : "0".repeat(64),
            blob_bytes        : 0,
            display           : "DP-1".into(),
            display_area      : DisplayArea::from_rect(DisplayRect {
                w_mm  : 600.0,
                h_mm  : 340.0,
                ox_mm : -300.0,
                oy_mm : 20.0,
                z_mm  : 0.0,
            }),
            desk_sha256       : "0".repeat(64),
            tracker_pitch_deg : 0.0,
            glasses           : false,
            note              : "clicks".into(),
        }
    }

    /// A click record on `output` at index `n`.
    fn click(n: u64, output: &str) -> ClickRecord {
        ClickRecord {
            n           : n,
            button      : "left".into(),
            output      : output.into(),
            px          : GlobalPx { x: 100.0, y: 200.0 },
            t_press     : 10.0 + n as f64,
            t_release   : 10.08 + n as f64,
            moved_px    : 0.5,
            multi       : 1,
            element     : ClickElement {
                kind  : "button".into(),
                bbox  : Rect { x: 80.0, y: 190.0, w: 60.0, h: 24.0 },
                text  : Some("Save".into()),
                score : 0.87,
            },
            crop_luma   : 0.21,
            frame_age_s : Some(0.031),
            cursor      : Some("hand".into()),
            source      : Some("vision".into()),
            trainer     : None,
        }
    }

    /// A device frame at `t_s`.
    fn frame(t_s: f64) -> TimedFrame {
        TimedFrame {
            t_s   : t_s,
            frame : Et5Frame {
                validity_l   : Some(0),
                validity_r   : Some(0),
                gaze_2d_norm : Some([0.5, 0.5]),
                ..Et5Frame::default()
            },
        }
    }

    #[test]
    fn a_session_writes_meta_records_and_end_and_never_repeats_a_frame() {
        let dir  = std::env::temp_dir().join("gaze-clicks-session-test");
        let path = dir.join("written.jsonl");

        let _ = std::fs::remove_file(&path);

        let meta = meta("1234-abcdef12-clicks");
        let mut session = ClickSession::create(&dir, Some(&path), &meta)
            .expect("the session file is created");

        let window: Vec<TimedFrame> = (0..10).map(|i| frame(9.0 + i as f64 * 0.1)).collect();

        let stop = StopWindow {
            u        : 0.25,
            v        : 0.5,
            px       : GlobalPx { x: 100.0, y: 200.0 },
            t_start  : 9.4,
            t_end    : 10.1,
            parallax : false,
        };

        session.write_click(&click(0, "DP-2"), Some(&stop), &window)
            .expect("the first click is written");

        // A second click whose window overlaps the first one's tail.
        let overlap: Vec<TimedFrame> = (5..16).map(|i| frame(9.0 + i as f64 * 0.1)).collect();

        session.write_click(&click(1, "DP-1"), Some(&stop), &overlap)
            .expect("the second click is written");

        assert_eq!(session.next_index(), 2);

        session.finish(&"0".repeat(64)).expect("the end line is written");

        let text  = std::fs::read_to_string(&path).expect("the session reads back");
        let lines : Vec<&str> = text.lines().collect();

        let kinds = |want: &str| {
            lines.iter()
                .filter(|l| l.contains(&format!("\"kind\":\"{want}\"")))
                .count()
        };

        assert_eq!(kinds("meta")    , 1);
        assert_eq!(kinds("meta_end"), 1);
        assert_eq!(kinds("stop")    , 2);
        assert_eq!(kinds("click")   , 2);

        // Ten frames in the first window, six new ones in the second.
        assert_eq!(kinds("frame"), 16);

        // The stop is filed under the click's own output, not the meta display.
        assert!(lines.iter().any(|l| l.contains("\"kind\":\"stop\"")
                                  && l.contains("\"display\":\"DP-2\"")));

        let _ = std::fs::remove_file(&path);
    }
}
