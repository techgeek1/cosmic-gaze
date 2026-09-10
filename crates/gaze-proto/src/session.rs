//! The gaze loop: a gaze source to filters to snap to overlay to click, scroll or warp.
//!
//! This runs on the caller's thread (the daemon's session thread, or the prototype's
//! main thread) and owns the gaze source, the filter stack, the snap engine, the overlay
//! handle and the injector. The perception thread feeds it element boxes through an
//! [`ElementStore`], and a tree thread answers what scrolls under the gaze through a
//! [`SurfaceCache`]; the owner reaches in through a [`Live`], polled once per sample.
//! Nothing else crosses a thread boundary.
//!
//! Three buttons carry every control, on whichever device the active provider reads (see
//! [`GazeSource`]): left commits, right exits, middle forces a redetect. The eyes alone
//! scroll: a dwell in the lower or upper band of the surface being looked at moves it,
//! and looking elsewhere stops it (`edge_scroll`). The eyes do one thing at a time: with
//! a thumb on the pad or F14 latched they point, and the scroller sees nothing; with the
//! thumb up they scroll, and the overlay shows the band they are near as a faint zone
//! instead of any control.
//!
//! The Daydream controller, when one is paired, is a second control source on top of
//! the mouse, and the only one with a fine channel: a thumb on its pad captures the snap
//! point (or the gaze point when nothing snapped), the thumb moves it, and the pad's
//! click commits there instead of at the snap point (`daydream`, [`Refined`]).
//!
//! # What reaches the real desktop
//!
//! [`SessionConfig::click`] is the master switch. Off, there is no injector at all and
//! commits, scrolls and warps are logged. On, all three are live: the warps and the
//! edge scrolls are what carry the clicks to where they land, and nothing gaze-side
//! moves the pointer except to borrow it for one of them and give it back.
//!
//! # While it runs
//!
//! The owner can pause it (nothing drawn or injected, the tracker still streaming),
//! replace the tuning (the filter stack and snap engine are rebuilt, everything else
//! takes its new parameters in place), ask for the offset to be forgotten, and stop it;
//! it reports a [`Status`] in return. All through [`Live`].

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, TryRecvError};
use gaze_config::{Mode, Status, Tuning};
use gaze_core::{DesktopGeometry, Element, ElementKind, GazeSample, GlobalPx};
use gaze_daydream::DaydreamError;
use gaze_inject::{Button as InjectButton, Injector, Key};
use gaze_overlay::{Overlay, OverlayState, Pointer, Target, Zone};
use gaze_provider_et5::{ClickFeedback, ClickVia};
use gaze_provider_synthetic::to_jsonl_line;
use gaze_snap::{FixationState, Filtered, SnapEngine};
use tracing::{debug, error, info, warn};

use crate::config::{
    DaydreamSpec, OverlayMode, SessionConfig, daydream_config, edge_params, engine_for, filter_for,
    pointer_style,
};
use crate::daydream::{Daydream, DaydreamConfig};
use crate::edge_scroll::{APPROACH_FRACTION, Action as EdgeAction, EdgeScroller, Eyes, Scrollable};
use crate::feedback::{self, ClickFeed, Press};
use crate::keys::LatchKeys;
use crate::live::Live;
use crate::perception::{ElementStore, Perception, PerceptionConfig};
use crate::score::{Scoreboard, classify};
use crate::source::{Control, GazeSource, Refine};
use crate::surface::SurfaceCache;
use crate::verify::{Verdict, Verifier};
use crate::warp::{WarpReason, Warper};

/// How far the filtered gaze point must move before the overlay is repainted. Below this
/// the ring would jitter in place and cost a frame per sample for no visible change.
const OVERLAY_MOVE_PX: f64 = 2.0;

/// The near gate's closing distance as a multiple of its opening one (`near_deg`).
const NEAR_HYSTERESIS: f64 = 1.5;

/// Capture passes per second on the perception thread. Capture is ~35 ms per ultrawide, so
/// this leaves the cores to the detector and the gaze loop.
const PERCEPTION_HZ: f64 = 5.0;

/// Kernel clock ticks per second, for reading CPU time out of `/proc/self/stat`. `USER_HZ`
/// is 100 on every Linux build this runs on. The figure is a diagnostic, not a benchmark.
const USER_HZ: f64 = 100.0;

/// A refine that moved less than this is a thumb landing and lifting, not an adjustment,
/// and is forgotten when the thumb lifts.
const REFINE_MIN_PX: f64 = 2.0;

/// A standing refine resumes only while the pointer is still within this of its point;
/// farther and something else (the mouse, a scroll) has moved it, so the point is stale.
const RESUME_NEAR_PX: f64 = 24.0;

/// The scroll zone's closing reach as a multiple of its opening one
/// ([`APPROACH_FRACTION`] band heights), so its inner edge does not blink the zone.
const ZONE_HYSTERESIS: f64 = 1.5;

/// How long between attempts to open a paired controller that is asleep or away.
const DAYDREAM_RETRY: Duration = Duration::from_secs(10);

/// A paired controller, being opened, open, or given up on.
///
/// Opening blocks for the seconds BlueZ takes to connect, so it happens on a thread of
/// its own and the loop polls for the result. A controller that is not paired at all is
/// never retried: pairing is a ceremony, not something that happens while a session
/// runs. One that is paired but asleep is retried every [`DAYDREAM_RETRY`] until Home
/// wakes it.
struct DaydreamSlot {
    /// Which controller to open.
    address  : Option<String>,
    /// The mapping to open it with, kept current by the tuning.
    config   : DaydreamConfig,
    /// The open controller.
    daydream : Option<Daydream>,
    /// An attempt in flight.
    pending  : Option<Receiver<Result<Daydream>>>,
    /// When to try next; `None` means never.
    retry_at : Option<Instant>,
    /// Whether the first failure has been reported, so the retries stay at debug.
    warned   : bool,
}

/// The fine channel's state: where the commit point is being moved from and by how much.
#[derive(Clone, Copy, Debug)]
struct Refined {
    /// Where the gesture started: the snap point, or the gaze point when nothing snapped.
    anchor  : GlobalPx,
    /// Accumulated motion, logical pixels.
    dx_px   : f64,
    dy_px   : f64,
    /// Whole pixels already sent to the pointer as relative motion, so a fraction carried
    /// across reports is not lost and the pointer is never asked for the same pixel twice.
    sent_x  : i64,
    sent_y  : i64,
    /// Whether the thumb is still on the pad. Off the pad the point stands until it is
    /// committed or the snap moves on.
    engaged : bool,
}

impl Refined {
    /// The commit point as refined so far.
    fn point(&self) -> GlobalPx {
        GlobalPx { x: self.anchor.x + self.dx_px, y: self.anchor.y + self.dy_px }
    }

    /// How far the point has been moved from the anchor.
    fn moved_px(&self) -> f64 {
        self.dx_px.hypot(self.dy_px)
    }

    /// The whole-pixel step that brings the pointer up to the accumulated motion, and
    /// books it as sent.
    fn take_step(&mut self) -> (i32, i32) {
        let want_x = self.dx_px.round() as i64;
        let want_y = self.dy_px.round() as i64;
        let step   = (want_x - self.sent_x, want_y - self.sent_y);

        self.sent_x = want_x;
        self.sent_y = want_y;

        (step.0 as i32, step.1 as i32)
    }
}

/// Runs a session until the exit button, the deadline, a stop through `live`, or the
/// sample stream ending.
pub fn run(config: &SessionConfig, live: &Live) -> Result<()> {
    // --- desk config ---

    let geometry   = &config.geometry;
    let mut tuning = live.tuning();

    // Taken now so a change made before the loop started is not replayed on its first
    // sample: everything below is built from `tuning` already.
    let _ = live.take_tuning_change();

    // The recogniser watches the outputs the desk says to (`detect`), which on a desk
    // whose tracker reaches one panel is that panel; the rest stay in the geometry for
    // the pointer, the warps and the clamp.
    let outputs : Vec<String> = geometry
        .outputs
        .iter()
        .filter(|output| output.enabled && output.detect)
        .map(|output| output.name.clone())
        .collect();

    if outputs.is_empty() {
        anyhow::bail!("the desk has no enabled outputs with detect = true");
    }

    info!(
        outputs        = ?outputs,
        source         = ?config.source,
        commit_latency = tuning.commit_latency_s,
        clicks         = config.click,
        "starting"
    );

    // --- perception ---

    let store = ElementStore::new();

    let mut perception = Perception::spawn(
        PerceptionConfig {
            outputs            : outputs,
            models_dir         : config.models_dir.clone(),
            redetect_threshold : tuning.redetect_threshold as f32,
            redetect_interval  : Duration::from_secs_f64(tuning.redetect_interval_s.max(0.0)),
            period             : Duration::from_secs_f64(1.0 / PERCEPTION_HZ),
        },
        Arc::clone(&store),
    )?;

    // --- overlay ---

    let (overlay, overlay_join) = Overlay::spawn_styled(pointer_style(&tuning))
        .context("spawning the overlay")?;

    // --- injector ---

    // Opened only when something is actually allowed to reach the real pointer. In a dry
    // run there is no injector at all, so nothing can inject even by accident.
    let mut injector = {
        if config.click {
            warn!("clicks are live: committed targets will be clicked for real");
            warn!("edge scrolling is live: surfaces under a dwelling gaze will scroll, and the pointer will be borrowed for it");

            Some(Injector::create().context("creating the uinput injector")?)
        }
        else {
            info!("dry run: commits, scrolls and warps will be logged, not injected");

            None
        }
    };

    // --- gaze source ---

    let mut source = GazeSource::open(&config.source, config.buttons.as_deref(), config.grab, geometry)?;

    // --- controller ---

    let mut daydream = DaydreamSlot::new(&config.daydream, daydream_config(&tuning));

    // --- click feedback ---

    // The real mouse is read when a source learns from its clicks. The feed stamps
    // the source's clock; nothing else reads those timestamps.
    let labels     = source.click_clock().is_some();
    let mut clicks = feedback::open_if_useful(source.click_clock());

    // --- filters and snap ---

    let mut filter = filter_for(geometry, &tuning);
    let mut engine = engine_for(geometry, &tuning);

    // --- overlay arming ---

    // The pointer look shows while a thumb rests on the pad or the latch is on; F14
    // toggles the latch. Without a readable keyboard the pad is the only switch, which
    // is logged rather than fatal: the session is no worse off than with no latch key.
    let mut latch_keys = match config.overlay {
        OverlayMode::Always => None,
        _                   => match LatchKeys::open() {
            Ok(keys) => {
                info!(nodes = keys.paths().len(), "overlay latch: F14 toggles the pointer look");

                Some(keys)
            }
            Err(e)   => {
                warn!("overlay latch key unavailable, the pad is the only switch: {e:#}");

                None
            }
        },
    };

    // --- a11y verdicts ---

    // What the application says the eyes are on, for the pointer look; its own tree
    // thread, separate from the edge scroller's.
    let mut verifier = Verifier::spawn();

    // --- edge scrolling ---

    info!(params = ?edge_params(&tuning), "edge scrolling on");

    let mut scroller = EdgeScroller::new(edge_params(&tuning));
    let mut surfaces = SurfaceCache::spawn();

    // --- recording ---

    let mut record = config
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
    let mut warper      = Warper::new();
    let mut last_gaze   : Option<GlobalPx>   = None;
    let mut last_target : Option<u64>        = None;
    let mut last_sample : Option<GazeSample> = None;
    // Whether the highlight is hidden: during a scroll, and after one until the perception
    // thread has published a detection newer than the scroll, because until then every
    // box is where the content was.
    let mut last_hidden                      = false;
    let mut last_blocked                     = false;
    // The pointer look's state apart from position, so a change in what is near
    // repaints even when the eyes have not moved.
    let mut last_pointer : Option<(bool, Option<u64>)> = None;
    let mut last_armed   = false;
    let mut last_zone    : Option<Zone> = None;
    let mut stale_gen   : Option<u64>        = None;
    // Set when a scroll starts or stops; retargeting stays off until a fixation that began
    // later than this, which is the eyes having moved on purpose.
    let mut retarget_block : Option<f64>     = None;
    let mut refined     : Option<Refined>    = None;
    // Whether a thumb is on the pad, and whether F14 has latched the look on. Either
    // means the eyes are pointing: it arms the pointer look and silences the edge
    // scroller. `--overlay-always` arms the look for the whole run without doing that.
    let mut pad_down    = false;
    let mut latched     = false;
    // Where the pointer was before an edge scroll warped it into the surface; it goes
    // back there when the scroll stops, so scrolling by eye leaves the pointer alone.
    let mut parked      : Option<GlobalPx> = None;
    // The element the overlay is marking, as of the last sample, and its centre. With a
    // thumb on the pad the pointer is borrowed and sits on it: what is highlighted is
    // what a pad press clicks, and the hover the app shows agrees.
    let mut mark        : Option<(u64, GlobalPx)> = None;
    let mut marked      : Option<GlobalPx>        = None;
    // Where the pointer was before the thumb borrowed it; it goes back there when the
    // thumb lifts, so the mouse hand finds it where it left it.
    let mut borrowed    : Option<GlobalPx>        = None;
    // Whether the perception thread is capturing and detecting. Off while the eyes are
    // only reading or scrolling, since no box is wanted then and the detector is the
    // hot part of the session.
    let mut perceiving  = true;
    // Whether push-to-talk is forwarded down right now, so an exit mid-hold releases it.
    let mut ptt_down    = false;
    // Whether the owner has paused the session, as of the last sample.
    let mut paused      = false;
    let mut samples     = 0u64;
    let mut reason      = "the provider stopped";

    let started      = Instant::now();
    let cpu_at_start = cpu_seconds();
    let deadline     = config.seconds.map(|s| started + Duration::from_secs_f64(s));

    'session: loop {
        if live.stopped() {
            reason = "a stop request";

            break;
        }

        // A tuning change rebuilds what is built from it and hands the rest their new
        // parameters. The filter and the engine start cold, which costs one fixation.
        if let Some(t) = live.take_tuning_change() {
            info!("tuning changed, applying it");

            filter = filter_for(geometry, &t);
            engine = engine_for(geometry, &t);

            scroller.set_params(edge_params(&t));
            daydream.set_config(daydream_config(&t));
            perception.set_redetect(
                t.redetect_threshold as f32,
                Duration::from_secs_f64(t.redetect_interval_s.max(0.0)),
            );

            if let Err(e) = overlay.set_style(pointer_style(&t)) {
                warn!(error = %e, "overlay did not take the new style");
            }

            tuning = t;
        }

        if live.take_reset_offset() {
            source.reset_offset();
        }

        // A paired controller that was asleep may have woken; a connect that finished
        // lands here.
        daydream.poll();

        // Only touch the lock when the perception thread published something new.
        let generation = store.generation();

        if generation != last_gen {
            elements = store.snapshot();
            last_gen = generation;

            debug!(count = elements.len(), generation = generation, "elements updated");
        }

        // Real clicks feed the source's online offset. Before the controls, so a press
        // is attributed against the rays that preceded it and not against a sample
        // drawn after it.
        if let Some(feed) = clicks.as_mut() {
            let presses = feed.presses(warper.last_point());

            if labels {
                offer_clicks(feed, &mut source, presses);
            }
        }

        // Controls before samples: a commit should not wait out a sample tick.
        let mut controls = source.controls();
        let mouse_n      = controls.len();

        if let Some(daydream) = daydream.daydream.as_mut() {
            controls.extend(daydream.controls());
        }

        if let Some(keys) = latch_keys.as_ref() {
            controls.extend(keys.events());
        }

        for (i, control) in controls.into_iter().enumerate() {
            let from_pad = i >= mouse_n;

            match control {
                Control::Commit | Control::Context => {
                    let button = match control {
                        Control::Context => InjectButton::Right,
                        _                => InjectButton::Left,
                    };

                    // A refined point is spent by the commit, whether or not it clicked.
                    // The pointer was steered by eye through relative motion, so where
                    // the compositor says it is beats where the deltas add up to.
                    //
                    // No refine: the marked element, whose highlight is the promise that
                    // a commit lands on it, wherever the pointer and the snap engine's
                    // own pick are. Nothing marked: the engine's pick.
                    let at = refined
                        .take()
                        .map(|r| {
                            injector
                                .as_mut()
                                .and_then(|i| i.last_known_position().ok().flatten())
                                .unwrap_or_else(|| r.point())
                        })
                        .or(marked);

                    let clicked = commit(
                        &mut engine,
                        source.truth(),
                        &elements,
                        if paused { None } else { injector.as_mut() },
                        &mut board,
                        last_sample,
                        tuning.commit_latency_s,
                        at,
                        button,
                    );

                    // A pad commit is a gaze label like a mouse press is: the user was
                    // looking at what they committed. A mouse commit is already seen by
                    // the click feed as the physical press it is, so it is not fed twice.
                    if from_pad
                        && let (Some(px), Some(sample)) = (clicked, last_sample)
                    {
                        let feedback = source.observe_click(px, sample.t_s, ClickVia::Pad);

                        log_click_feedback("pad", px, feedback);
                    }
                }

                Control::Exit => {
                    reason = "the exit button";

                    break 'session;
                }

                Control::Refine(Refine::Begin) => {
                    // A thumb that ran off the pad edge and landed again picks the drag
                    // up where it left it: the point stands, the pointer is already on
                    // it, and warping back to the anchor would undo the sweep. The
                    // pointer having gone elsewhere since (the mouse took it, or the
                    // eyes moved on and a scroll warped it) means the standing point is
                    // stale and this is a fresh gesture.
                    if let Some(r) = refined.as_mut()
                        && !r.engaged
                    {
                        let pointer = pointer_position(injector.as_mut(), &warper);
                        let point   = r.point();
                        let near    = pointer.is_none_or(|p| {
                            (p.x - point.x).hypot(p.y - point.y) <= RESUME_NEAR_PX
                        });

                        if near {
                            r.engaged = true;

                            debug!(dx_px = r.dx_px, dy_px = r.dy_px, "refine resumed");

                            continue;
                        }

                        debug!(?pointer, "pointer left the standing refine; starting a fresh one");

                        refined = None;
                    }

                    // The marked element if there is one (the highlight is the promise,
                    // and a pad press moves the thumb enough to begin a refine, so the
                    // press must start from what is marked), else the snap point, else
                    // the gaze itself: the fine channel is also how an unlabelled
                    // target gets clicked.
                    let anchor = marked.or(engine.current().map(|t| t.point)).or(last_gaze);

                    match anchor {
                        Some(anchor) => {
                            if borrowed.is_none() {
                                borrowed = pointer_position(injector.as_mut(), &warper);
                            }

                            refined = Some(Refined {
                                anchor  : anchor,
                                dx_px   : 0.0,
                                dy_px   : 0.0,
                                sent_x  : 0,
                                sent_y  : 0,
                                engaged : true,
                            });

                            do_warp(
                                injector.as_mut(),
                                &mut warper,
                                anchor,
                                last_sample.map(|s| s.sigma_deg).unwrap_or(f64::NAN),
                                None,
                                WarpReason::Refine,
                                Instant::now(),
                            );
                        }

                        None => debug!("refine began with no gaze to anchor on, ignoring it"),
                    }
                }

                Control::Refine(Refine::Move { dx_px, dy_px }) => {
                    if let Some(r) = refined.as_mut()
                        && r.engaged
                    {
                        // Boxed around the anchor before the desk clamp: gaze put the
                        // point close, and the fine channel only covers the residual.
                        let half = tuning.refine_range_px / 2.0;

                        r.dx_px = (r.dx_px + dx_px).clamp(-half, half);
                        r.dy_px = (r.dy_px + dy_px).clamp(-half, half);

                        let point = clamp_to_desk(geometry, r.point());

                        r.dx_px = point.x - r.anchor.x;
                        r.dy_px = point.y - r.anchor.y;

                        // One plain mouse report per controller report. `move_to` here
                        // would run the closed loop sixty times a second, and its
                        // measure-and-correct flurry shakes the cursor visibly.
                        let (dx, dy) = r.take_step();

                        if let Some(injector) = injector.as_mut()
                            && let Err(e) = injector.move_by(dx, dy)
                        {
                            warn!(error = %e, "refine move failed");
                        }
                    }
                }

                Control::Refine(Refine::End) => {
                    if let Some(r) = refined.as_mut() {
                        r.engaged = false;

                        if r.moved_px() < REFINE_MIN_PX {
                            refined = None;
                        }
                        else {
                            debug!(dx_px = r.dx_px, dy_px = r.dy_px, "refined point stands for the next commit");
                        }
                    }
                }

                Control::Arm { down } => {
                    pad_down = down;

                    // The thumb lifting gives the pointer back, after any commit in
                    // the same report has clicked where it was.
                    if !down
                        && let Some(home) = borrowed.take()
                    {
                        do_warp(
                            injector.as_mut(),
                            &mut warper,
                            home,
                            last_sample.map(|s| s.sigma_deg).unwrap_or(f64::NAN),
                            None,
                            WarpReason::Return,
                            Instant::now(),
                        );
                    }
                }

                Control::ToggleOverlay => {
                    latched = !latched;

                    info!(on = latched, "overlay latch toggled");
                }

                Control::Redetect => {
                    info!("middle button: forcing a redetect on every output");

                    perception.force_redetect();
                }

                Control::PushToTalk { down } => {
                    match injector.as_mut() {
                        Some(injector) => match injector.key(Key::F13, down) {
                            Ok(())  => ptt_down = down,
                            Err(e)  => error!(error = %e, down, "push-to-talk forward failed"),
                        },
                        None => debug!(down, "push-to-talk pressed in a dry run, not forwarded"),
                    }
                }

                Control::Wheel(detents) => {
                    wheel(injector.as_mut(), &warper, detents, source.grabbed());
                }
            }
        }

        if let Some(deadline) = deadline
            && Instant::now() >= deadline
        {
            reason = "the deadline";

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
        let gaze     = filtered.sample.point.filter(|_| filtered.sample.valid);

        // A thumb on the pad locks the gaze out: the pointer is the controller's until the
        // thumb lifts, so no retarget, no edge scroll and no focus warp may move it. The
        // filter still runs so the fixation state is current when the lock lifts.
        let locked = refined.is_some_and(|r| r.engaged);

        // Paused: the eyes neither point nor scroll. The thumb and the latch are still
        // tracked so a resume picks up where they are.
        paused = live.paused();

        // Thumb down or latched: the eyes are pointing. Thumb up: they are reading, and
        // may scroll. One or the other, never both, so a scroll never fights a refine
        // and a control in the band never holds a scroll off.
        let input = (pad_down || latched) && !paused;

        // Edge scrolling first: a scroll decides whether the snap engine may retarget at
        // all this sample, and a scroll that starts here takes the highlight down with it.
        let (scrolling, zone) = {
            let was                = scroller.scrolling();
            let quiet              = input || paused;
            let (stopped, surface) = edge_scroll(
                &mut scroller,
                &mut surfaces,
                &mut warper,
                injector.as_mut(),
                geometry,
                &filtered,
                gaze,
                quiet,
                &mut parked,
            );

            if scroller.scrolling() && !was {
                retarget_block = Some(filtered.sample.t_s);
                engine.reset();
            }

            if stopped {
                // The content moved; every box under it is stale until the output is
                // detected again. Ask for that output first, and hide the highlight
                // until the answer lands.
                match gaze.and_then(|g| output_at(geometry, g)) {
                    Some(name) => perception.force_redetect_output(name),
                    None       => perception.force_redetect(),
                }

                surfaces.invalidate();
                stale_gen      = Some(store.generation());
                // Fixations begun while the content moved do not count as aiming.
                retarget_block = Some(filtered.sample.t_s);
            }

            // The band the eyes are in or approaching, for the overlay. It opens on
            // a fixation only, like the dot, and closes at 1.5x the reach it opened
            // at; a running scroll keeps it up whatever the eyes do.
            let reach = match last_zone {
                Some(_) => APPROACH_FRACTION * ZONE_HYSTERESIS,
                None    => APPROACH_FRACTION,
            };
            let settled = matches!(filtered.state, FixationState::Fixating { .. });
            let zone    = surface
                .filter(|_| scroller.scrolling() || last_zone.is_some() || settled)
                .and_then(|s| scroller.near_band(gaze, &s, reach))
                .map(|(_, rect)| Zone { rect: rect, active: scroller.scrolling() });

            (scroller.scrolling(), zone)
        };

        // Retargeting needs the eyes to have moved: a fixation that began after the scroll
        // is aiming, a fixation that merely persisted while content moved under it is not.
        if let Some(blocked_at) = retarget_block
            && !scrolling
            && let FixationState::Fixating { since_s } = filtered.state
            && since_s > blocked_at
        {
            retarget_block = None;
        }

        let target = match (locked, retarget_block) {
            (true, _)        => engine.current().cloned(),
            (false, Some(_)) => None,
            (false, None)    => engine.update(&filtered, &elements),
        };

        // The overlay redraws on every state it is handed, so only hand it one when there
        // is something new to see.
        let target_id = target.as_ref().map(|t| t.element.id);

        let moved = match (last_gaze, gaze) {
            (Some(a), Some(b)) => (a.x - b.x).hypot(a.y - b.y) > OVERLAY_MOVE_PX,
            (None, None)       => false,
            _                  => true,
        };

        if target_id != last_target {
            log_candidates(&engine, &elements, gaze);

            // The eyes moved on; a refine the thumb has left is about the old target.
            if refined.is_some_and(|r| !r.engaged) {
                refined = None;
            }
        }

        if stale_gen.is_some_and(|g| g != store.generation()) {
            stale_gen = None;
        }

        let hidden  = scrolling || stale_gen.is_some();
        let blocked = retarget_block.is_some();

        // The pointer look's intent. The highlight goes down during a scroll and until
        // the detector has caught up with it, since every box is where the content was;
        // the dot is drawn where the fine channel has moved the commit point while a
        // thumb is on the pad, and at the gaze otherwise. Both are gated on the gaze
        // being within `near_deg` of the element: the engine will snap from further
        // out than that, but a highlight on an element the eyes are nowhere near reads
        // as the overlay guessing, not the eyes aiming.
        //
        // The tree, when it answers, outranks the recogniser: a control it names is
        // marked with its own box, and text it names is not marked whatever the
        // recogniser called it.
        let aim   = target.as_ref().map(|t| t.point).or(gaze);
        let armed = (config.overlay == OverlayMode::Always && !paused) || input;

        // The detector runs only while its boxes can be wanted: the eyes are pointing,
        // or nothing on this desk can say when they are (no controller, no latch key).
        // Resuming asks for the output under the gaze first, so the thumb landing sees
        // fresh boxes within one detection rather than a full pass.
        let want_boxes = armed || (!paused && daydream.daydream.is_none() && latch_keys.is_none());

        if want_boxes != perceiving {
            perceiving = want_boxes;

            perception.set_paused(!perceiving);

            if perceiving {
                match gaze.and_then(|g| output_at(geometry, g)) {
                    Some(name) => perception.force_redetect_output(name),
                    None       => perception.force_redetect(),
                }
            }

            debug!(on = perceiving, "perception");
        }

        {
            let settled = matches!(filtered.state, FixationState::Fixating { .. });

            verifier.update(aim.filter(|_| armed && settled && !locked));
        }

        // Unarmed, no control is marked: the eyes are reading, and a highlight the user
        // did not ask for is the thing that fights them. The look comes up with the
        // thumb on the pad or the latch, and the presenter fades it out when it goes.
        // Reading eyes near a scroll band get the dot and the zone instead, so a scroll
        // is seen coming.
        let pointer = gaze.filter(|_| armed || zone.is_some()).map(|g| {
            let refining = refined.filter(|r| r.engaged);
            // Hysteresis on the near gate: it opens at `near_deg` and closes at half
            // as much again, so a gaze sitting at the edge does not flicker the dot.
            let reach    = match last_pointer {
                Some((true, _)) => tuning.near_deg * NEAR_HYSTERESIS,
                _               => tuning.near_deg,
            };
            let close    = |c: &gaze_snap::Candidate| {
                c.distance_deg <= reach && worth_marking(&elements[c.index], &tuning)
            };
            let verdict  = aim.map_or(Verdict::Unknown, |p| verifier.verdict(p));
            let holding  = aim.is_some_and(|p| verifier.holding(p));

            let (near, target) = match verdict {
                _ if !armed                       => (false, None),
                Verdict::Control { id, rect, .. } => (true, Some(Target { id: id, rect: rect })),
                Verdict::Static                   => (false, None),
                Verdict::Unknown if holding       => (engine.ranked().any(close), None),
                Verdict::Unknown                  => (
                    engine.ranked().any(close),
                    target
                        .as_ref()
                        .filter(|t| engine.ranked().any(|c| c.id == t.element.id && close(c)))
                        .map(|t| Target { id: t.element.id, rect: t.element.bbox }),
                ),
            };

            // A saccade never brings the dot up. The eye in flight sweeps over whatever
            // lies between two fixations, and the return sweep to the start of the next
            // line of a paragraph crosses controls it is not aiming at; only a fixation
            // opens the gate. In flight the dot may stay up (it was near and still is)
            // or go down, never come up.
            let in_flight = !matches!(filtered.state, FixationState::Fixating { .. });
            let was_near  = matches!(last_pointer, Some((true, _)));
            let near      = near && (was_near || !in_flight);

            Pointer {
                gaze   : refining.map_or(g, |r| r.point()),
                near   : refining.is_some() || near || zone.is_some(),
                target : target.filter(|_| !hidden && near),
                zone   : zone,
            }
        });

        let pointer_key = pointer.map(|p| (p.near, p.target.map(|t| t.id)));
        let armed_now   = pointer.is_some();

        let mark_now = pointer.and_then(|p| p.target).map(|t| (t.id, t.rect.center()));

        marked = mark_now.map(|(_, p)| p);

        // Thumb down borrows the pointer: it goes to the mark when one appears or
        // changes, and stays put while nothing is marked or a refine has it. The
        // thumb lifting gives it back (see `Control::Arm`).
        if pad_down
            && !refined.is_some_and(|r| r.engaged)
            && let Some((id, point)) = mark_now
            && mark.is_none_or(|(last, _)| last != id)
        {
            if borrowed.is_none() {
                borrowed = pointer_position(injector.as_mut(), &warper);
            }

            do_warp(
                injector.as_mut(),
                &mut warper,
                point,
                filtered.sample.sigma_deg,
                None,
                WarpReason::Mark,
                Instant::now(),
            );
        }

        mark = mark_now;

        if moved || target_id != last_target || hidden != last_hidden || blocked != last_blocked
            || pointer_key != last_pointer || armed_now != last_armed
            || zone != last_zone
        {
            let state = {
                if config.overlay == OverlayMode::Debug {
                    // A highlight flickering across moving text, or parked on where text
                    // used to be, is noise; the label says what is happening instead.
                    OverlayState {
                        gaze       : gaze,
                        highlight  : target.as_ref().filter(|_| !hidden).map(|t| t.element.bbox),
                        truth      : if config.show_truth { source.truth() } else { None },
                        label      : Some(match (scrolling, hidden, retarget_block) {
                            _ if refined.is_some_and(|r| r.engaged) => "refine".to_string(),
                            (true, _, _)        => "edge scroll".to_string(),
                            (false, true, _)    => "redetecting".to_string(),
                            (false, false, Some(_)) => "move eyes to retarget".to_string(),
                            _                   => label(&target, &filtered),
                        }),
                        background : None,
                        pointer    : None,
                        mark       : None,
                    }
                }
                else {
                    OverlayState {
                        gaze       : None,
                        highlight  : None,
                        truth      : if config.show_truth { source.truth() } else { None },
                        label      : None,
                        background : None,
                        pointer    : pointer,
                        mark       : None,
                    }
                }
            };

            last_pointer = pointer_key;
            last_armed   = armed_now;
            last_zone    = zone;
            last_hidden  = hidden;
            last_blocked = blocked;

            if overlay.set(state).is_err() {
                reason = "the overlay thread exited";

                break;
            }

            last_target = target_id;
        }

        // Tracked every sample, not just on repaint: a refine anchors on this the moment
        // the thumb lands and wants the newest point, not the last one drawn.
        last_gaze = gaze;

        // What the owner sees. Stored only on a change, so this is a compare per sample.
        live.set_status(status_of(&source, &daydream, paused, input, scrolling));
    }

    // --- shutdown ---

    // The kernel would release it when the device goes away, but the injector may
    // outlive this loop by a while and the voice stack should not hear a hold that long.
    if ptt_down
        && let Some(injector) = injector.as_mut()
        && let Err(e) = injector.key(Key::F13, false)
    {
        error!(error = %e, "releasing push-to-talk on exit failed");
    }

    let elapsed = started.elapsed().as_secs_f64().max(f64::EPSILON);

    let cpu_percent = match (cpu_at_start, cpu_seconds()) {
        (Some(before), Some(after)) => Some(100.0 * (after - before) / elapsed),
        _                           => None,
    };

    // Before `stop`: the reader thread clears `connected` on its way out, so asking
    // afterwards would report every session as having ended disconnected.
    source.log_health();

    if let Some(feed) = clicks.as_mut() {
        feed.stop();
    }

    source.stop();
    daydream.stop();

    overlay.stop();

    let _ = overlay_join.join();

    if let Some(keys) = latch_keys.as_mut() {
        keys.stop();
    }

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
        warps           = warper.warps(),
        edge_starts     = scroller.starts,
        edge_units      = scroller.units,
        surfaces_asked  = surfaces.asked,
        surfaces_found  = surfaces.found,
        a11y_asked      = verifier.asked,
        a11y_controls   = verifier.controls,
        a11y_statics    = verifier.statics,
        a11y_empty      = verifier.empty,
        daydream_reports = daydream.daydream.as_ref().map(|d| d.reports),
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
///
/// `refined` is where the fine channel moved the point. Given, the click lands there
/// whatever the snap engine thinks, target or no target; the engine's answer is still
/// taken so the log says what gaze alone would have chosen.
///
/// Returns the point the commit resolved to, clicked or (in a dry run) merely chosen,
/// `None` when there was nothing to click.
#[allow(clippy::too_many_arguments)]
fn commit(
    engine    : &mut SnapEngine,
    truth     : Option<GlobalPx>,
    elements  : &[Element],
    injector  : Option<&mut Injector>,
    board     : &mut Scoreboard,
    sample    : Option<GazeSample>,
    latency_s : f64,
    refined   : Option<GlobalPx>,
    button    : InjectButton,
)
    -> Option<GlobalPx>
{
    let Some(sample) = sample else {
        warn!("commit before the first gaze sample, ignoring it");

        return None;
    };

    let press = Instant::now();

    let target = engine.commit(sample.t_s, latency_s);

    let mut clicked = false;

    let click_at = refined.or_else(|| target.as_ref().map(|t| t.point));

    if let Some(point) = click_at {
        match injector {
            Some(injector) => {
                match injector.click_at(point, button) {
                    Ok(())  => clicked = true,
                    Err(e)  => error!(error = %e, "click injection failed"),
                }
            }

            None => {
                debug!(x = point.x, y = point.y, "dry run: would click here");
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
        refined    = ?refined.map(|p| format!("{:.0},{:.0}", p.x, p.y)),
        sigma_deg  = sample.sigma_deg,
        latency_ms = latency.as_secs_f64() * 1000.0,
        clicked    = clicked,
        button     = ?button,
        "commit"
    );

    click_at
}

// --- Click feedback ---

/// Hands every new real press to the source and tallies what it made of it. One log
/// line per press, because a user tuning the feel wants to see each leftover as it
/// happens.
fn offer_clicks(feed: &mut ClickFeed, source: &mut GazeSource, presses: Vec<Press>) {
    for press in presses {
        feed.offered += 1;

        let feedback = source.observe_click(press.px, press.t_s, ClickVia::Mouse);

        log_click_feedback("mouse", press.px, feedback);

        match feedback {
            Some(ClickFeedback::Accepted { .. }) => feed.accepted += 1,
            Some(ClickFeedback::Adopted { .. })  => feed.adopted += 1,
            Some(ClickFeedback::Rejected { .. }) => feed.rejected += 1,
            None                                 => feed.unplaced += 1,
        }
    }
}

/// One line per label offered to the source's online offset, whatever offered it.
fn log_click_feedback(via: &str, px: GlobalPx, feedback: Option<ClickFeedback>) {
    match feedback {
        Some(ClickFeedback::Accepted { leftover_deg, offset_deg, anchors }) => {
            info!(
                via = via,
                x = px.x, y = px.y,
                leftover_yaw_deg   = format_args!("{:+.2}", leftover_deg[0]),
                leftover_pitch_deg = format_args!("{:+.2}", leftover_deg[1]),
                offset_yaw_deg     = format_args!("{:+.2}", offset_deg[0]),
                offset_pitch_deg   = format_args!("{:+.2}", offset_deg[1]),
                anchors            = anchors,
                "click accepted",
            );
        }
        Some(ClickFeedback::Rejected { leftover_deg }) => {
            info!(
                via = via,
                x = px.x, y = px.y,
                leftover_yaw_deg   = format_args!("{:+.2}", leftover_deg[0]),
                leftover_pitch_deg = format_args!("{:+.2}", leftover_deg[1]),
                "click rejected: past the gate",
            );
        }
        Some(ClickFeedback::Adopted { leftover_deg, jump_deg, offset_deg, anchors, clicks }) => {
            warn!(
                via = via,
                x = px.x, y = px.y,
                leftover_yaw_deg   = format_args!("{:+.2}", leftover_deg[0]),
                leftover_pitch_deg = format_args!("{:+.2}", leftover_deg[1]),
                jump_yaw_deg       = format_args!("{:+.2}", jump_deg[0]),
                jump_pitch_deg     = format_args!("{:+.2}", jump_deg[1]),
                offset_yaw_deg     = format_args!("{:+.2}", offset_deg[0]),
                offset_pitch_deg   = format_args!("{:+.2}", offset_deg[1]),
                anchors            = anchors,
                clicks             = clicks,
                "click adopted: the last few rejects agreed, the bias jumped",
            );
        }
        None => {
            debug!(via = via, x = px.x, y = px.y, "click unplaced: no rays or no panel");
        }
    }
}

// --- Scroll and warp ---

/// Injects one wheel event from the controller's volume keys at the pointer, wherever it
/// is. No warp: with the thumb up the pointer is the user's, and a wheel where it sits
/// is what a wheel on the mouse would do.
///
/// `grabbed` is whether the device the event came from is grabbed; it never is for the
/// controller, so this is only kept honest for a grabbed mouse's wheel, which the
/// compositor never saw and which therefore has to be re-injected here.
fn wheel(injector: Option<&mut Injector>, warper: &Warper, detents: i32, grabbed: bool) {
    let mut injector = injector;

    let Some(point) = pointer_position(injector.as_deref_mut(), warper) else {
        debug!(detents = detents, "wheel with no known pointer position, dropped");

        return;
    };

    match injector {
        Some(injector) => {
            if let Err(e) = injector.scroll(point, detents) {
                error!(error = %e, "scroll injection failed");
            }
        }

        None => {
            info!(
                x       = %format_args!("{:.0}", point.x),
                y       = %format_args!("{:.0}", point.y),
                detents = detents,
                grabbed = grabbed,
                "dry run: would scroll here"
            );
        }
    }
}

/// One sample of edge scrolling: the surface under the gaze from the cache, the
/// scroller's verdict, and the injection it asks for. Returns whether a scroll stopped on
/// this sample, which the caller turns into a re-detection, and the surface the scroller
/// was shown, which the caller draws the zone on.
///
/// A start puts the pointer at the gaze point first, every time: the wheel goes to the
/// innermost scroller under the pointer, and a pointer merely inside the surface may be
/// over a nested one (YouTube's mix list inside its page, which then ate the page's
/// scroll). Units then go out as high-resolution wheel motion without moving the pointer
/// again, so gaze jitter during the scroll does not drag it about.
/// Where it was is kept in `parked`, and the stop puts it back there if nothing else has
/// moved it since, so a scroll by eye borrows the pointer rather than taking it. (Wayland
/// delivers wheel motion to the surface under the pointer and nowhere else, so borrowing
/// it is the least the scroll can do without compositor help.)
///
/// `quiet` (the eyes are pointing: a thumb on the pad or the latch on) shows the
/// scroller no surface and no eyes, which stops a running scroll and arms nothing,
/// without the gaze ever reaching it.
#[allow(clippy::too_many_arguments)]
fn edge_scroll(
    scroller : &mut EdgeScroller,
    surfaces : &mut SurfaceCache,
    warper   : &mut Warper,
    injector : Option<&mut Injector>,
    geometry : &DesktopGeometry,
    filtered : &Filtered,
    gaze     : Option<GlobalPx>,
    quiet    : bool,
    parked   : &mut Option<GlobalPx>,
)
    -> (bool, Option<Scrollable>)
{
    let mut injector = injector;
    let surface      = match quiet {
        true  => None,
        false => surfaces.scrollable(gaze, scroller.scrolling()),
    };

    // Eyes tracked but on no panel: project the ray onto the surface's own panel, extended
    // past its edges, so the scroller can tell "went off the bottom" from "looked away".
    let eyes = match (gaze, filtered.sample.valid, &filtered.sample.ray) {
        _ if quiet              => Eyes::Lost,
        (Some(g), _, _)         => Eyes::On(g),
        (None, true, Some(ray)) => {
            surface
                .and_then(|s| output_at(geometry, s.viewport.center()))
                .and_then(|name| geometry.outputs.iter().find(|o| o.name == name))
                .and_then(|output| output.project_px(ray))
                .map_or(Eyes::Lost, Eyes::Off)
        }
        _                      => Eyes::Lost,
    };

    let stopped = match scroller.update(filtered.sample.t_s, eyes, surface.as_ref()) {
        EdgeAction::Nothing => false,

        EdgeAction::Start { dir, point } => {
            let surface  = surface.expect("a start comes from a surface");
            let viewport = surface.viewport;
            let pointer  = pointer_position(injector.as_deref_mut(), warper);

            info!(
                dir   = ?dir,
                role  = surfaces.current().map(|s| s.clip.role.as_str()).unwrap_or("?"),
                x     = %format_args!("{:.0}", viewport.x),
                y     = %format_args!("{:.0}", viewport.y),
                w     = %format_args!("{:.0}", viewport.w),
                h     = %format_args!("{:.0}", viewport.h),
                above = %format_args!("{:.0}", surface.above_px),
                below = %format_args!("{:.0}", surface.below_px),
                live  = injector.is_some(),
                "edge scroll start",
            );

            *parked = pointer;

            do_warp(
                injector.as_deref_mut(),
                warper,
                point,
                filtered.sample.sigma_deg,
                None,
                WarpReason::EdgeScroll,
                Instant::now(),
            );

            false
        }

        EdgeAction::Scroll { units } => {
            match injector {
                Some(injector) => {
                    if let Err(e) = injector.scroll_hi_res(units) {
                        error!(error = %e, "edge scroll injection failed");
                    }
                }

                None => debug!(units = units, "dry run: would edge scroll"),
            }

            false
        }

        EdgeAction::Stop => {
            info!(units = scroller.units, "edge scroll stop");

            // The pointer goes home if it is still where the start put it; moved since
            // (the mouse took it), it is the user's and stays.
            if let Some(home) = parked.take() {
                let pointer = pointer_position(injector.as_deref_mut(), warper);
                let lent    = warper.last_point();
                let untouched = match (pointer, lent) {
                    (Some(p), Some(l)) => (p.x - l.x).hypot(p.y - l.y) <= RESUME_NEAR_PX,
                    (None, _)          => true,
                    _                  => false,
                };

                if untouched {
                    do_warp(
                        injector,
                        warper,
                        home,
                        filtered.sample.sigma_deg,
                        None,
                        WarpReason::Return,
                        Instant::now(),
                    );
                }
            }

            true
        }
    };

    (stopped, surface)
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

/// `p` pulled into the nearest enabled output, so a refine cannot run the point off the
/// desk. A point already on an output is returned as is.
fn clamp_to_desk(geometry: &DesktopGeometry, p: GlobalPx) -> GlobalPx {
    if geometry.output_at(p).is_some() {
        return p;
    }

    geometry
        .outputs
        .iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let a = o.uv_to_px(0.0, 0.0);
            let b = o.uv_to_px(1.0, 1.0);

            GlobalPx {
                x : p.x.clamp(a.x.min(b.x), a.x.max(b.x)),
                y : p.y.clamp(a.y.min(b.y), a.y.max(b.y)),
            }
        })
        .min_by(|a, b| {
            let da = (a.x - p.x).hypot(a.y - p.y);
            let db = (b.x - p.x).hypot(b.y - p.y);

            da.total_cmp(&db)
        })
        .unwrap_or(p)
}

/// Name of the enabled output containing `p`, if any.
fn output_at(geometry: &DesktopGeometry, p: GlobalPx) -> Option<&str> {
    geometry.outputs.iter()
        .find(|output| output.enabled && output.contains_px(p))
        .map(|output| output.name.as_str())
}

// --- Status ---

/// What the owner is told, from what the loop knows this sample.
fn status_of(
    source    : &GazeSource,
    daydream  : &DaydreamSlot,
    paused    : bool,
    input     : bool,
    scrolling : bool,
)
    -> Status
{
    let tracker = source.connected();
    let offset  = source.offset_summary();

    let mode = match (tracker, paused, scrolling, input) {
        (false, _, _, _)   => Mode::NoTracker,
        (_, true, _, _)    => Mode::Paused,
        (_, _, true, _)    => Mode::Scrolling,
        (_, _, _, true)    => Mode::Pointing,
        _                  => Mode::Reading,
    };

    Status {
        tracker          : tracker,
        calibrated       : source.calibrated(),
        model            : source.has_model(),
        controller       : daydream.daydream.as_ref().is_some_and(Daydream::connected),
        paused           : paused,
        mode             : mode,
        offset_updates   : offset.map_or(0, |o| o.updates),
        offset_jumps     : offset.map_or(0, |o| o.jumps),
        offset_yaw_deg   : offset.map_or(0.0, |o| o.global_deg[0]),
        offset_pitch_deg : offset.map_or(0.0, |o| o.global_deg[1]),
    }
}

// --- DaydreamSlot ---

impl DaydreamSlot {
    /// A slot for the controller `spec` names, due to be opened on the first poll.
    fn new(spec: &DaydreamSpec, config: DaydreamConfig) -> DaydreamSlot {
        let (address, retry_at) = match spec {
            DaydreamSpec::Off              => (None, None),
            DaydreamSpec::Auto             => (None, Some(Instant::now())),
            DaydreamSpec::Address(address) => (Some(address.clone()), Some(Instant::now())),
        };

        DaydreamSlot {
            address  : address,
            config   : config,
            daydream : None,
            pending  : None,
            retry_at : retry_at,
            warned   : false,
        }
    }

    /// Collects a finished attempt, and starts one when it is due. Never blocks.
    fn poll(&mut self) {
        if self.daydream.is_some() {
            return;
        }

        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok(daydream)) => {
                    info!(
                        address = daydream.address(),
                        "daydream controller: pad commits, Home exits, App holds push-to-talk, volume scrolls",
                    );

                    self.daydream = Some(daydream);
                    self.pending  = None;
                    self.retry_at = None;
                }

                Ok(Err(e)) => {
                    self.pending = None;

                    // Not paired is a ceremony away, not a retry away.
                    let not_paired = e.chain().any(|cause| {
                        cause
                            .downcast_ref::<DaydreamError>()
                            .is_some_and(|d| matches!(d, DaydreamError::NotPaired { .. }))
                    });

                    if not_paired {
                        info!("no daydream controller is paired; the mouse and the latch are the controls");

                        self.retry_at = None;
                    }
                    else {
                        if !self.warned {
                            warn!(error = %format_args!("{e:#}"), retry_s = DAYDREAM_RETRY.as_secs(), "daydream controller not opened, retrying");
                        }
                        else {
                            debug!(error = %format_args!("{e:#}"), "daydream controller still not opened");
                        }

                        self.warned   = true;
                        self.retry_at = Some(Instant::now() + DAYDREAM_RETRY);
                    }
                }

                Err(TryRecvError::Empty)        => {}
                Err(TryRecvError::Disconnected) => {
                    // The opener panicked; treat it as a failure worth retrying.
                    self.pending  = None;
                    self.retry_at = Some(Instant::now() + DAYDREAM_RETRY);
                }
            }

            return;
        }

        if self.retry_at.is_some_and(|at| Instant::now() >= at) {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let address  = self.address.clone();
            let config   = self.config;

            let spawned = std::thread::Builder::new()
                .name("gaze-daydream-open".to_string())
                .spawn(move || {
                    let _ = tx.send(Daydream::open(address.as_deref(), config));
                });

            match spawned {
                Ok(_)  => self.pending = Some(rx),
                Err(e) => {
                    warn!(error = %e, "could not spawn the daydream opener");

                    self.retry_at = Some(Instant::now() + DAYDREAM_RETRY);
                }
            }
        }
    }

    /// Replaces the mapping's tunables, for the open controller and any opened later.
    fn set_config(&mut self, config: DaydreamConfig) {
        self.config = config;

        if let Some(daydream) = self.daydream.as_mut() {
            daydream.set_config(config);
        }
    }

    /// Stops the open controller. An attempt in flight finishes on its own thread and
    /// its result is dropped with the slot.
    fn stop(&mut self) {
        if let Some(daydream) = self.daydream.as_mut() {
            daydream.stop();
        }
    }
}

// --- Helpers ---

/// Builds the overlay caption. The overlay's built in font is 5x7 printable ASCII, so this
/// stays short and plain.
/// Debug-logs the engine's ranking at a target change: the top three candidates with
/// the terms that scored them, so a live "it picked the wrong box" report carries the
/// boxes and the numbers instead of a screenshot. The head of the ranking is the
/// cheapest candidate, not necessarily the target: hysteresis can hold another.
fn log_candidates(engine: &SnapEngine, elements: &[Element], gaze: Option<GlobalPx>) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }

    let held = engine.current().map(|t| t.element.id);

    for (rank, c) in engine.ranked().take(3).enumerate() {
        let e = &elements[c.index];

        debug!(
            rank       = rank,
            id         = c.id,
            held       = held == Some(c.id),
            kind       = ?e.kind,
            source     = ?e.source,
            box_x      = e.bbox.x,
            box_y      = e.bbox.y,
            box_w      = e.bbox.w,
            box_h      = e.bbox.h,
            score      = c.score,
            dist_deg   = c.distance_deg,
            area_deg2  = c.area_deg2,
            centre_deg = c.center_deg,
            gaze_x     = gaze.map(|g| g.x),
            gaze_y     = gaze.map(|g| g.y),
            "snap candidate",
        );
    }
}

/// Whether the pointer look should show an element. Controls are; text is not unless
/// asked for, because most text on a screen is being read, not aimed at, and marking it
/// is exactly the distraction the pointer look exists to avoid. The snap engine still
/// targets whatever it targets; this only decides what is drawn.
fn worth_marking(element: &Element, tuning: &Tuning) -> bool {
    match element.kind {
        ElementKind::Text    => tuning.highlight_text,
        ElementKind::Unknown => false,
        _                    => true,
    }
}

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
            detect        : true,
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

    /// Output names decide which output a redetect is asked for, so a disabled output
    /// must not be one: nothing is watching it.
    #[test]
    fn output_at_finds_enabled_outputs_only() {
        let geometry = desk();

        assert_eq!(output_at(&geometry, GlobalPx { x: 500.0, y: 250.0 }), Some("DP-1"));
        assert_eq!(output_at(&geometry, GlobalPx { x: 1500.0, y: 250.0 }), None);
        assert_eq!(output_at(&geometry, GlobalPx { x: -1.0, y: 250.0 }), None);
    }
}
