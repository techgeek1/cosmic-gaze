//! The gaze loop: a gaze source to filters to snap to overlay to click, scroll or warp.
//!
//! This runs on the main thread and owns the gaze source, the filter stack, the snap
//! engine, the overlay handle and the injector. The perception thread feeds it element
//! boxes through an [`ElementStore`]; nothing else crosses a thread boundary.
//!
//! Three buttons carry every control, on whichever device the active provider reads (see
//! [`GazeSource`]): left commits, right exits, middle forces a redetect. With `--scroll`
//! the wheel on that same device is the fourth control, and it does not commit anything:
//! it routes the scroll to the window under the gaze point instead of the one under the
//! pointer.
//!
//! # What reaches the real desktop
//!
//! `--dry-run` is the master off switch and gates all three of clicks, scrolls and warps.
//! Without it, `--click` clicks, `--scroll` scrolls and warps, `--focus-follows-gaze`
//! warps. Scrolling and warping are live by default when their flag is passed, because
//! neither is a click: the worst case is a pointer somewhere the user did not ask for,
//! which the next mouse move undoes.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gaze_core::{DesktopGeometry, Element, GazeSample, GlobalPx};
use gaze_inject::{Button as InjectButton, Injector};
use gaze_overlay::{Overlay, OverlayState};
use gaze_provider_synthetic::to_jsonl_line;
use gaze_snap::{FilterStack, FixationState, Filtered, SnapEngine};
use tracing::{debug, error, info, warn};

use crate::cli::{Args, Provider};
use crate::perception::{ElementStore, Perception, PerceptionConfig};
use crate::score::{Scoreboard, classify};
use crate::source::{Control, GazeSource};
use crate::warp::{WARP_COOLDOWN, WarpReason, Warper};

/// How far the filtered gaze point must move before the overlay is repainted. Below this
/// the ring would jitter in place and cost a frame per sample for no visible change.
const OVERLAY_MOVE_PX: f64 = 2.0;

/// Capture passes per second on the perception thread. Capture is ~35 ms per ultrawide, so
/// this leaves the cores to the detector and the gaze loop.
const PERCEPTION_HZ: f64 = 5.0;

/// Kernel clock ticks per second, for reading CPU time out of `/proc/self/stat`. `USER_HZ`
/// is 100 on every Linux build this runs on. The figure is a diagnostic, not a benchmark.
const USER_HZ: f64 = 100.0;

/// Runs a live session until the right mouse button, `--seconds`, or the sample stream
/// ending.
pub fn run(args: &Args) -> Result<()> {
    // --- desk config ---

    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;

    let geometry = DesktopGeometry::from_toml(&text)
        .with_context(|| format!("parsing {}", args.config.display()))?;

    let model = args.noise_model(geometry.noise)?;

    let outputs : Vec<String> = geometry
        .outputs
        .iter()
        .filter(|output| output.enabled)
        .map(|output| output.name.clone())
        .collect();

    if outputs.is_empty() {
        anyhow::bail!("{} has no enabled outputs", args.config.display());
    }

    info!(
        outputs        = ?outputs,
        provider       = ?args.provider,
        sigma_deg      = model.profile.sigma_deg,
        rate_hz        = model.rate_hz,
        commit_latency = args.commit_latency,
        clicks         = args.click,
        scroll         = args.scroll,
        focus          = args.focus_follows_gaze,
        "starting"
    );

    // --- perception ---

    let store = ElementStore::new();

    let mut perception = Perception::spawn(
        PerceptionConfig {
            outputs            : outputs,
            models_dir         : args.models.clone(),
            redetect_threshold : args.redetect_threshold,
            redetect_interval  : args.redetect_interval(),
            period             : Duration::from_secs_f64(1.0 / PERCEPTION_HZ),
        },
        Arc::clone(&store),
    )?;

    // --- overlay ---

    let (overlay, overlay_join) = Overlay::spawn().context("spawning the overlay")?;

    // --- injector ---

    // Opened only when something is actually allowed to reach the real pointer. In a dry
    // run there is no injector at all, so no tier can inject even by accident.
    let mut injector = {
        if args.injects() {
            if args.click {
                warn!("clicks are live: committed targets will be clicked for real");
            }

            if args.scroll || args.focus_follows_gaze {
                warn!("warps are live: the real pointer will move to the gaze point");
            }

            Some(Injector::create().context("creating the uinput injector")?)
        }
        else {
            info!("dry run: commits, scrolls and warps will be logged, not injected");

            None
        }
    };

    // --- gaze source ---

    let mut source = GazeSource::open(args, &geometry, model)?;

    // --- filters and snap ---

    // The I-VT defaults (30 deg/s over 20 ms) assume tracker-class precision. A webcam
    // source jitters ~1.5 deg per sample at 30 Hz, which reads as 60 deg/s and classifies
    // every sample as a saccade, so the one-euro smoother never engages and the marker
    // shows the raw jitter. Webcam mode measures velocity over a longer window and raises
    // the threshold; explicit flags override either preset.
    let (velocity_deg_s, window_s, min_cutoff_hz, beta) = match args.provider {
        Provider::Webcam => (80.0, 0.10, 0.6, 0.02),
        _                => (30.0, 0.02, 0.3, 0.30),
    };
    let mut filter = FilterStack::create()
        .scale(Box::new(geometry.clone()))
        .velocity_threshold_deg_s(args.filter_velocity_deg_s.unwrap_or(velocity_deg_s))
        .window_s(args.filter_window_s.unwrap_or(window_s))
        .one_euro(args.filter_min_cutoff_hz.unwrap_or(min_cutoff_hz), args.filter_beta.unwrap_or(beta))
        .build();

    let mut engine = SnapEngine::create()
        .scale(Box::new(geometry.clone()))
        .build();

    // --- recording ---

    let mut record = args
        .record
        .as_ref()
        .map(|path| -> Result<_> {
            let file = File::create(path)
                .with_context(|| format!("creating {}", path.display()))?;

            Ok(BufWriter::new(file))
        })
        .transpose()?;

    // --- loop ---

    let mut elements = store.snapshot();
    let mut last_gen = store.generation();

    let mut board       = Scoreboard::default();
    let mut warper      = Warper::new(args.scroll_warp_deg, WARP_COOLDOWN);
    let mut last_gaze   : Option<GlobalPx>   = None;
    let mut last_target : Option<u64>        = None;
    let mut last_sample : Option<GazeSample> = None;
    // `since_s` of the fixation focus-follows-gaze has already warped for, so one dwell
    // warps once however long the user keeps looking.
    let mut focus_done  : Option<f64>        = None;
    let mut samples     = 0u64;
    let mut reason      = "the provider stopped";

    let started      = Instant::now();
    let cpu_at_start = cpu_seconds();
    let deadline     = args.seconds.map(|s| started + Duration::from_secs_f64(s));

    'session: loop {
        // Only touch the lock when the perception thread published something new.
        let generation = store.generation();

        if generation != last_gen {
            elements = store.snapshot();
            last_gen = generation;

            debug!(count = elements.len(), generation = generation, "elements updated");
        }

        // Controls before samples: a commit should not wait out a sample tick.
        let controls = source.controls();

        for control in controls {
            match control {
                Control::Commit => {
                    commit(
                        &mut engine,
                        source.truth(),
                        &elements,
                        if args.click { injector.as_mut() } else { None },
                        &mut board,
                        last_sample,
                        args.commit_latency,
                    );
                }

                Control::Exit => {
                    reason = "the right button";

                    break 'session;
                }

                Control::Redetect => {
                    info!("middle button: forcing a redetect on every output");

                    perception.force_redetect();
                }

                Control::Wheel(detents) => {
                    if args.scroll {
                        scroll_under_gaze(
                            &geometry,
                            &mut warper,
                            injector.as_mut(),
                            last_gaze,
                            last_sample.map(|s| s.sigma_deg).unwrap_or(f64::NAN),
                            detents,
                            source.grabbed(),
                        );
                    }
                }
            }
        }

        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            reason = "--seconds";

            break;
        }

        // Blocks for at most one sample interval, so the deadline above is checked at the
        // provider's rate.
        let Some(sample) = source.next_sample() else {
            break;
        };

        samples    += 1;
        last_sample = Some(sample);

        // Cheap (one mutex, deduplicated by frame sequence) and only meaningful for the
        // webcam provider, which is the only one with a socket that can go quietly bad.
        source.poll_health();

        if let Some(writer) = record.as_mut() {
            match to_jsonl_line(&sample) {
                Ok(line) => writeln!(writer, "{line}").context("writing the recording")?,
                Err(e)   => warn!(error = %e, "dropping a sample from the recording"),
            }
        }

        let filtered = filter.push(sample);
        let target   = engine.update(&filtered, &elements);

        // The overlay redraws on every state it is handed, so only hand it one when there
        // is something new to see.
        let gaze      = filtered.sample.point.filter(|_| filtered.sample.valid);
        let target_id = target.as_ref().map(|t| t.element.id);

        let moved = match (last_gaze, gaze) {
            (Some(a), Some(b)) => (a.x - b.x).hypot(a.y - b.y) > OVERLAY_MOVE_PX,
            (None, None)       => false,
            _                  => true,
        };

        if moved || target_id != last_target {
            let state = OverlayState {
                gaze       : gaze,
                highlight  : target.as_ref().map(|t| t.element.bbox),
                truth      : if args.show_truth { source.truth() } else { None },
                label      : Some(label(&target, &filtered)),
                background : None,
            };

            if overlay.set(state).is_err() {
                reason = "the overlay thread exited";

                break;
            }

            last_target = target_id;
        }

        // Tracked every sample, not just on repaint: the scroll tier reads this the moment
        // a wheel event arrives and wants the newest point, not the last one drawn.
        last_gaze = gaze;

        // Focus-follows-gaze: one warp per dwell, and only when the eyes are on an output
        // the pointer is not.
        if args.focus_follows_gaze
            && let FixationState::Fixating { since_s } = filtered.state
            && filtered.sample.t_s - since_s >= args.focus_dwell_s
            && focus_done != Some(since_s)
            && let Some(gaze) = gaze
            && focus_warp(
                &geometry,
                &mut warper,
                injector.as_mut(),
                gaze,
                filtered.sample.sigma_deg,
                filtered.sample.t_s - since_s,
            )
        {
            focus_done = Some(since_s);
        }
    }

    // --- shutdown ---

    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);

    let cpu_percent = match (cpu_at_start, cpu_seconds()) {
        (Some(before), Some(after)) => Some(100.0 * (after - before) / elapsed),
        _                           => None,
    };

    // Before `stop`: the reader thread clears `connected` on its way out, so asking
    // afterwards would report every session as having ended disconnected.
    source.log_health();

    source.stop();
    overlay.stop();

    let _ = overlay_join.join();

    perception.stop();

    if let Some(writer) = record.as_mut() {
        writer.flush().context("flushing the recording")?;
    }

    info!(
        reason          = reason,
        provider        = source.label(),
        seconds         = elapsed,
        samples         = samples,
        sample_hz       = samples as f64 / elapsed,
        elements        = elements.len(),
        commits         = board.commits,
        graded          = board.graded(),
        hits            = board.hits,
        slips           = board.slips,
        misses          = board.misses,
        hit_rate        = board.hit_rate(),
        mean_latency_ms = board.mean_latency_s() * 1000.0,
        scrolls         = warper.scrolls(),
        warps           = warper.warps(),
        cpu_percent     = ?cpu_percent,
        "session summary"
    );

    Ok(())
}

// --- Commit ---

/// Resolves one left-button press to a target, clicks it, and grades it.
///
/// The click is issued before any of the scoring work so the recorded latency is the real
/// press-to-click delay. In dry run there is no click, so the same figure measures the
/// press-to-decision path instead and is correspondingly optimistic.
///
/// `truth` is the provider's noise-free point, which only the synthetic provider has. With
/// no truth the commit is still counted and timed, just not graded: there is nothing to
/// grade a real gaze sample against.
fn commit(
    engine    : &mut SnapEngine,
    truth     : Option<GlobalPx>,
    elements  : &[Element],
    injector  : Option<&mut Injector>,
    board     : &mut Scoreboard,
    sample    : Option<GazeSample>,
    latency_s : f64,
)
{
    let Some(sample) = sample else {
        warn!("commit before the first gaze sample, ignoring it");

        return;
    };

    let press = Instant::now();

    let target = engine.commit(sample.t_s, latency_s);

    let mut clicked = false;

    if let Some(target) = &target {
        match injector {
            Some(injector) => {
                match injector.click_at(target.point, InjectButton::Left) {
                    Ok(())  => clicked = true,
                    Err(e)  => error!(error = %e, "click injection failed"),
                }
            }

            None => {
                debug!(x = target.point.x, y = target.point.y, "dry run: would click here");
            }
        }
    }

    let latency = press.elapsed();

    // Truth is read at press time by the caller, not after the click, so a moving mouse
    // cannot make a hit look like a slip.
    let outcome = truth.map(|truth| classify(truth, target.as_ref().map(|t| &t.element), elements));

    match outcome {
        Some(outcome) => board.record(outcome, latency),
        None          => board.record_ungraded(latency),
    }

    let (id, kind, bbox, point) = match &target {
        Some(target) => (
            Some(target.element.id),
            Some(format!("{:?}", target.element.kind)),
            Some(format!(
                "{:.0},{:.0} {:.0}x{:.0}",
                target.element.bbox.x,
                target.element.bbox.y,
                target.element.bbox.w,
                target.element.bbox.h,
            )),
            Some(format!("{:.0},{:.0}", target.point.x, target.point.y)),
        ),

        None => (None, None, None, None),
    };

    info!(
        t_s        = sample.t_s,
        id         = ?id,
        kind       = ?kind,
        bbox       = ?bbox,
        click      = ?point,
        truth      = ?truth.map(|t| format!("{:.0},{:.0}", t.x, t.y)),
        outcome    = ?outcome.map(|o| o.to_string()),
        sigma_deg  = sample.sigma_deg,
        latency_ms = latency.as_secs_f64() * 1000.0,
        clicked    = clicked,
        "commit"
    );
}

// --- Scroll and warp ---

/// Routes one wheel event to the window under the gaze point.
///
/// Warps the pointer to the gaze point first when gaze is far enough away and the warp
/// policy allows it, then scrolls. What "then scrolls" means depends on `grabbed`:
///
/// * On the grabbed device the compositor never saw the wheel, so the scroll has to be
///   injected here or it is simply lost.
/// * On an un-grabbed device (`--no-grab`) the compositor is already delivering the user's
///   own wheel, so this only warps and lets that delivery land on the newly-pointed window.
///   Injecting as well would scroll twice. The first detent of a burst still lands on the
///   old window there, because the warp cannot happen before an event that has already been
///   dispatched. Grabbing is the default precisely because it has no such gap.
fn scroll_under_gaze(
    geometry  : &DesktopGeometry,
    warper    : &mut Warper,
    injector  : Option<&mut Injector>,
    gaze      : Option<GlobalPx>,
    sigma_deg : f64,
    detents   : i32,
    grabbed   : bool,
)
{
    warper.record_scroll();

    let now = Instant::now();
    let mut injector = injector;

    let pointer = pointer_position(injector.as_deref_mut(), warper);

    // Both angles are `None` when the point is off every panel or unknown, which the warp
    // policy reads as "far away".
    let gap_deg = gaze.zip(pointer)
        .and_then(|(gaze, pointer)| geometry.angle_between_deg(geometry.eye(), pointer, gaze));

    let moved_deg = gaze.zip(warper.last_point())
        .and_then(|(gaze, last)| geometry.angle_between_deg(geometry.eye(), last, gaze));

    let warped = {
        if let Some(gaze) = gaze
            && warper.should_warp(now, gap_deg, moved_deg)
        {
            do_warp(injector.as_deref_mut(), warper, gaze, sigma_deg, gap_deg, WarpReason::Scroll, now)
        }
        else {
            false
        }
    };

    // After a warp the pointer is at the gaze point. Without one it is wherever it already
    // was, and only gaze is left if nothing can say where that is.
    let Some(point) = (if warped { gaze } else { pointer.or(gaze) }) else {
        debug!(detents = detents, "wheel with no gaze point and no known pointer, dropped");

        return;
    };

    match (grabbed, injector) {
        (true, Some(injector)) => {
            match injector.scroll(point, detents) {
                Ok(())  => {}
                Err(e)  => error!(error = %e, "scroll injection failed"),
            }
        }

        (true, None) => {
            info!(
                x         = %format_args!("{:.0}", point.x),
                y         = %format_args!("{:.0}", point.y),
                detents   = detents,
                sigma_deg = sigma_deg,
                warped    = warped,
                "dry run: would scroll here"
            );
        }

        // The compositor already has the user's own wheel event; the warp above is this
        // tier's whole contribution.
        (false, _) => {
            debug!(
                x       = %format_args!("{:.0}", point.x),
                y       = %format_args!("{:.0}", point.y),
                detents = detents,
                warped  = warped,
                "wheel passed through to the compositor"
            );
        }
    }
}

/// Warps the pointer to a dwelled-on gaze point when that point is on a different output
/// than the pointer.
///
/// Cross-output only, deliberately: within one output the pointer is already close enough
/// that moving it buys nothing and costs the user their place, while crossing outputs is
/// what drags keyboard focus along on a focus-follows-mouse desktop. Returns whether it
/// warped, which is what stops one dwell from warping twice.
fn focus_warp(
    geometry  : &DesktopGeometry,
    warper    : &mut Warper,
    injector  : Option<&mut Injector>,
    gaze      : GlobalPx,
    sigma_deg : f64,
    dwell_s   : f64,
)
    -> bool
{
    let mut injector = injector;

    let pointer = pointer_position(injector.as_deref_mut(), warper);

    let Some(gaze_output) = output_at(geometry, gaze) else {
        return false;
    };

    // An unknown pointer output (off every panel, or nothing has reported one yet) counts
    // as different: bringing the pointer onto the output being looked at is right either
    // way.
    if pointer.and_then(|p| output_at(geometry, p)) == Some(gaze_output) {
        return false;
    }

    let gap_deg = pointer
        .and_then(|pointer| geometry.angle_between_deg(geometry.eye(), pointer, gaze));

    debug!(output = gaze_output, dwell_s = dwell_s, "focus dwell on another output");

    do_warp(
        injector,
        warper,
        gaze,
        sigma_deg,
        gap_deg,
        WarpReason::Focus,
        Instant::now(),
    )
}

/// Moves the pointer to `point`, or logs the move in a dry run, and records it either way.
///
/// Returns whether the warp counted. A failed injection does not: the pointer did not
/// move, so remembering it as the last warp destination would make the next decision wrong.
fn do_warp(
    injector  : Option<&mut Injector>,
    warper    : &mut Warper,
    point     : GlobalPx,
    sigma_deg : f64,
    gap_deg   : Option<f64>,
    reason    : WarpReason,
    now       : Instant,
)
    -> bool
{
    if let Some(injector) = injector
        && let Err(e) = injector.move_to(point)
    {
        error!(error = %e, reason = ?reason, "pointer warp failed");

        return false;
    }

    warper.record_warp(point, now);

    info!(
        x         = %format_args!("{:.0}", point.x),
        y         = %format_args!("{:.0}", point.y),
        sigma_deg = sigma_deg,
        gap_deg   = ?gap_deg,
        reason    = ?reason,
        "warp"
    );

    true
}

/// Where the pointer is, as well as this session can tell.
///
/// The closed-loop relative injector can measure it through the compositor's cursor
/// tracker; the absolute backend and a dry run cannot, and fall back to wherever this
/// session last warped it. `None` before the first warp of a run that cannot measure, which
/// every caller reads as "far away".
fn pointer_position(injector: Option<&mut Injector>, warper: &Warper) -> Option<GlobalPx> {
    let measured = injector.and_then(|injector| {
        match injector.last_known_position() {
            Ok(position) => position,

            Err(e) => {
                warn!(error = %e, "cannot read the pointer position, falling back to the last warp");

                None
            }
        }
    });

    measured.or_else(|| warper.last_point())
}

/// Name of the enabled output containing `p`, if any.
fn output_at(geometry: &DesktopGeometry, p: GlobalPx) -> Option<&str> {
    geometry.outputs.iter()
        .find(|output| output.enabled && output.contains_px(p))
        .map(|output| output.name.as_str())
}

// --- Helpers ---

/// Builds the overlay caption. The overlay's built in font is 5x7 printable ASCII, so this
/// stays short and plain.
fn label(target: &Option<gaze_snap::SnapTarget>, filtered: &Filtered) -> String {
    let state = match filtered.state {
        // `since_s` is the fixation's first timestamp, so the duration is the difference.
        FixationState::Fixating { since_s } => {
            format!("fix {:.2}s", filtered.sample.t_s - since_s)
        }

        FixationState::Saccade => "sacc".to_string(),
        FixationState::Lost    => "lost".to_string(),
    };

    match target {
        Some(target) => format!("{:?} c={:.2} {state}", target.element.kind, target.score),
        None         => format!("no target {state}"),
    }
}

/// Total CPU time this process has used, seconds, or `None` if `/proc` did not cooperate.
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;

    // The comm field is parenthesised and may itself contain spaces, so the numeric fields
    // are counted from after the last closing paren: state, ppid, pgrp, session, tty_nr,
    // tpgid, flags, minflt, cminflt, majflt, cmajflt, then utime and stime.
    let rest = stat.rsplit_once(')')?.1;

    let mut fields = rest.split_whitespace().skip(11);

    let utime : f64 = fields.next()?.parse().ok()?;
    let stime : f64 = fields.next()?.parse().ok()?;

    Some((utime + stime) / USER_HZ)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::OutputGeometry;

    /// An `OutputGeometry` fixture with placeholder physical/pose fields: only the logical
    /// rect and `enabled` matter to `output_at`.
    fn output(name: &str, enabled: bool, x: f64, w: f64) -> OutputGeometry {
        OutputGeometry {
            name          : name.to_string(),
            enabled       : enabled,
            logical_x     : x,
            logical_y     : 0.0,
            logical_w     : w,
            logical_h     : 500.0,
            physical_w_mm : 100.0,
            physical_h_mm : 80.0,
            radius_mm     : 0.0,
            position_mm   : [0.0, 0.0, 0.0],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        }
    }

    /// One enabled output and one disabled one beside it.
    fn desk() -> DesktopGeometry {
        DesktopGeometry {
            eye_mm     : [0.0, 0.0, 650.0],
            tracker_mm : [0.0, 0.0, 0.0],
            outputs    : vec![output("DP-1", true, 0.0, 1000.0), output("DP-2", false, 1000.0, 1000.0)],
            noise      : None,
        }
    }

    /// Focus-follows-gaze compares output names, so a disabled output must not be one:
    /// warping onto a panel the session is not watching would strand the pointer there.
    #[test]
    fn output_at_finds_enabled_outputs_only() {
        let geometry = desk();

        assert_eq!(output_at(&geometry, GlobalPx { x: 500.0, y: 250.0 }), Some("DP-1"));
        assert_eq!(output_at(&geometry, GlobalPx { x: 1500.0, y: 250.0 }), None);
        assert_eq!(output_at(&geometry, GlobalPx { x: -1.0, y: 250.0 }), None);
    }
}
