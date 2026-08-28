//! End to end on the file format: what `ClickSession` writes, `dataset` reads.
//!
//! The two halves live in different crates and only agree through the JSONL on disk,
//! so this is the test that would catch a field renamed on one side. It writes a
//! session the way a collector run would, then loads it with the same reader
//! `gaze-et5-cli dataset export` uses and checks the rows that come out.

use gaze_clicks::session::ClickSession;
use gaze_core::{DesktopGeometry, GlobalPx, Rect};
use gaze_provider_et5::Et5Frame;
use gaze_provider_et5::dataset::{CSV_COLUMNS, Session, rows};
use gaze_provider_et5::record::{ClickElement, ClickRecord, SESSION_FORMAT, SessionMeta};
use gaze_provider_et5::sweep::{StopWindow, TimedFrame};
use gaze_provider_et5::ttp::{DisplayArea, DisplayRect};

/// The connector the tracker's plane is declared on, and the meta line's display.
const DEVICE_DISPLAY: &str = "DP-1";

/// Where the click lands. A different panel from the device's, which is the case the
/// per-record `display` field exists for.
const CLICK_DISPLAY: &str = "DP-2";

/// The desk config every session's rows are resolved against.
fn desk() -> DesktopGeometry {
    DesktopGeometry::from_toml(
        &std::fs::read_to_string("../../config/desk.toml").expect("desk config"),
    ).expect("desk config parses")
}

/// A device frame at `t_s` reporting both eyes and a combined gaze at `(nx, ny)`.
fn frame(t_s: f64, nx: f64, ny: f64) -> TimedFrame {
    TimedFrame {
        t_s   : t_s,
        frame : Et5Frame {
            timestamp_us        : Some((t_s * 1e6) as i64),
            frame_counter       : Some((t_s * 90.0) as u32),
            validity_l          : Some(0),
            validity_r          : Some(0),
            pupil_l_mm          : Some(3.5),
            pupil_r_mm          : Some(3.6),
            gaze_2d_norm        : Some([nx, ny]),
            gaze_2d_unfiltered  : Some([nx, ny]),
            gaze_2d_l_norm      : Some([nx, ny]),
            gaze_2d_r_norm      : Some([nx, ny]),
            eye_origin_l_mm     : Some([-32.0, 100.0, 600.0]),
            eye_origin_r_mm     : Some([ 32.0, 100.0, 600.0]),
            gaze_3d_l_mm        : Some([0.0, 0.0, -100.0]),
            gaze_3d_r_mm        : Some([0.0, 0.0, -100.0]),
            eye_origin_raw_l_mm : Some([-32.0, 100.0, 600.0]),
            eye_origin_raw_r_mm : Some([ 32.0, 100.0, 600.0]),
        },
    }
}

/// The meta line a collector run writes.
fn meta() -> SessionMeta {
    SessionMeta {
        kind              : "meta".into(),
        format            : SESSION_FORMAT,
        session_id        : "0-00000000-clicks".into(),
        created_unix_s    : 0.0,
        blob_sha256       : "0".repeat(64),
        blob_bytes        : 0,
        display           : DEVICE_DISPLAY.into(),
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

#[test]
fn a_written_click_session_loads_back_as_click_rows() {
    let geometry = desk();
    let out      = geometry.outputs.iter()
        .find(|o| o.name == CLICK_DISPLAY)
        .expect("the click's panel is in the desk config");

    let px     = out.uv_to_px(0.45, 0.55);
    let (u, v) = out.px_to_uv(px);

    let t_press = 10.0;

    // 90 Hz over the whole window, the way the device streams.
    let window: Vec<TimedFrame> = (0..144)
        .map(|i| frame(t_press - 1.2 + f64::from(i) / 90.0, 0.5, 0.5))
        .collect();

    let click = ClickRecord {
        n           : 0,
        button      : "left".into(),
        output      : CLICK_DISPLAY.into(),
        px          : px,
        t_press     : t_press,
        t_release   : t_press + 0.08,
        moved_px    : 0.7,
        multi       : 1,
        element     : ClickElement {
            kind  : "button".into(),
            bbox  : Rect { x: px.x - 40.0, y: px.y - 14.0, w: 80.0, h: 28.0 },
            text  : Some("Save".into()),
            score : 0.91,
        },
        crop_luma   : 0.34,
        frame_age_s : 0.028,
    };

    let stop = StopWindow {
        u        : u,
        v        : v,
        px       : px,
        t_start  : t_press - 0.6,
        t_end    : t_press + 0.1,
        parallax : false,
    };

    let dir  = std::env::temp_dir();
    let path = dir.join("gaze-clicks-roundtrip.jsonl");
    let _    = std::fs::remove_file(&path);

    let meta = meta();
    let mut session = ClickSession::create(&dir, Some(&path), &meta)
        .expect("the session file is created");

    session.write_click(&click, Some(&stop), &window).expect("the click is written");
    session.finish(&meta.blob_sha256).expect("the end line is written");

    // Now read it back with the exporter's own loader.
    let loaded = Session::load(&path).expect("the written session loads");

    assert!(loaded.blob_is_stable());
    assert_eq!(loaded.clicks.len(), 1);
    assert_eq!(loaded.stops.len() , 1);
    assert_eq!(loaded.stops[0].display.as_deref(), Some(CLICK_DISPLAY));
    assert_eq!(loaded.frames.len(), window.len());

    let rows = rows(&loaded, &geometry);

    assert!(!rows.is_empty(), "the click produced rows");

    for row in &rows {
        assert_eq!(row.session_phase, "click");
        assert_eq!(row.background   , "screen");
        assert_eq!(row.hold_key     , "click_0");
        assert_eq!(row.element_kind , "button");

        assert!((row.element_w_px - 80.0).abs() < 1e-9);
        assert!((row.element_h_px - 28.0).abs() < 1e-9);
        assert!((row.crop_luma - 0.34).abs() < 1e-9);

        assert_eq!(row.to_csv().split(',').count(), CSV_COLUMNS.len());
    }

    // The window is 700 ms of a 90 Hz stream, so the stop is well past the minimum
    // for an aggregated row.
    assert_eq!(rows.iter().filter(|r| r.is_mean).count(), 1);

    // The target is on the panel that was clicked, not on the tracker's own display.
    let target = out.px_to_world(px);
    let device = geometry.outputs.iter()
        .find(|o| o.name == DEVICE_DISPLAY)
        .expect("the device panel");

    let wrong = device.px_to_world(px);

    assert!(target.distance(wrong) > 10.0,
            "the two panels have to disagree for this to mean anything");

    for c in 0..3 {
        assert!((rows[0].target_mm[c] - target.to_array()[c]).abs() < 1e-9,
                "target axis {c}");
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_click_session_without_gaze_still_loads() {
    // `--no-tracker` writes click records and nothing else. The reader has to survive
    // a session whose only records it does not recognise.
    let dir  = std::env::temp_dir();
    let path = dir.join("gaze-clicks-roundtrip-notracker.jsonl");
    let _    = std::fs::remove_file(&path);

    let meta = meta();
    let mut session = ClickSession::create(&dir, Some(&path), &meta)
        .expect("the session file is created");

    let click = ClickRecord {
        n           : 0,
        button      : "right".into(),
        output      : CLICK_DISPLAY.into(),
        px          : GlobalPx { x: 500.0, y: 500.0 },
        t_press     : 3.0,
        t_release   : 3.05,
        moved_px    : 0.0,
        multi       : 2,
        element     : ClickElement {
            kind  : "text".into(),
            bbox  : Rect { x: 480.0, y: 490.0, w: 90.0, h: 20.0 },
            text  : None,
            score : 0.55,
        },
        crop_luma   : 0.9,
        frame_age_s : 0.041,
    };

    session.write_click(&click, None, &[]).expect("the click is written");
    session.finish(&meta.blob_sha256).expect("the end line is written");

    let loaded = Session::load(&path).expect("the written session loads");

    assert_eq!(loaded.clicks.len(), 1);
    assert!(loaded.stops.is_empty());
    assert!(loaded.frames.is_empty());

    // No stop means no rows, and that is not an error.
    assert!(rows(&loaded, &desk()).is_empty());

    let _ = std::fs::remove_file(&path);
}
