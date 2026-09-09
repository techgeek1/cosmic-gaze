//! The gaze loop: a gaze source to filters to snap to overlay to click, scroll or warp.
//!
//! This runs on the main thread and owns the gaze source, the filter stack, the snap
//! engine, the overlay handle and the injector. The perception thread feeds it element
//! boxes through an [`ElementStore`], and with `--edge-scroll` a tree thread answers what
//! scrolls under the gaze through a [`SurfaceCache`]; nothing else crosses a thread
//! boundary.
//!
//! Three buttons carry every control, on whichever device the active provider reads (see
//! [`GazeSource`]): left commits, right exits, middle forces a redetect. With `--scroll`
//! the wheel on that same device is the fourth control, and it does not commit anything:
//! it routes the scroll to the window under the gaze point instead of the one under the
//! pointer. With `--edge-scroll` the eyes alone scroll: a dwell in the lower or upper band
//! of the surface being looked at moves it, and looking elsewhere stops it
//! (`edge_scroll`).
//!
//! With `--daydream` the controller is a second control source on top of the mouse, and
//! the only one with a fine channel: a thumb on its pad captures the snap point (or the
//! gaze point when nothing snapped), the wrist or thumb moves it, and the pad's click
//! commits there instead of at the snap point (`daydream`, [`Refined`]).
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
use gaze_core::{DesktopGeometry, Element, ElementKind, GazeSample, GlobalPx};
use gaze_inject::{Button as InjectButton, Injector, Key};
use gaze_overlay::{Overlay, OverlayState, Pointer, Target};
use gaze_provider_et5::{ClickFeedback, ClickVia};
use gaze_provider_synthetic::to_jsonl_line;
use gaze_snap::{FilterStack, FixationState, Filtered, SnapEngine};
use tracing::{debug, error, info, warn};

use crate::cli::{Args, Provider};
use crate::daydream::{Daydream, DaydreamConfig, Owner, parse_axes};
use crate::edge_scroll::{Action as EdgeAction, EdgeScroller, Eyes};
use crate::feedback::{self, ClickFeed, Press};
use crate::perception::{ElementStore, Perception, PerceptionConfig};
use crate::score::{Scoreboard, classify};
use crate::source::{Control, GazeSource, Refine};
use crate::surface::SurfaceCache;
use crate::verify::{Verdict, Verifier};
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

/// A refine that moved less than this is a thumb landing and lifting, not an adjustment,
/// and is forgotten when the thumb lifts.
const REFINE_MIN_PX: f64 = 2.0;

/// A standing refine resumes only while the pointer is still within this of its point;
/// farther and something else (the mouse, a scroll) has moved it, so the point is stale.
const RESUME_NEAR_PX: f64 = 24.0;

/// A snap target no wider and no taller than this is an icon or a small button, and eyes
/// on one are aiming, not reading: the edge scroller does not arm while they are. Rows,
/// links in text and paragraphs are wider than this and keep scrolling as before. Text
/// boxes never count whatever their size (see [`aiming_at`]).
const AIM_TARGET_PX: f64 = 48.0;

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

/// Runs a live session until the right mouse button, `--seconds`, or the sample stream
/// ending.
pub fn run(args: &Args) -> Result<()> {
    // --- desk config ---

    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;

    let geometry = DesktopGeometry::from_toml(&text)
        .with_context(|| format!("parsing {}", args.config.display()))?;

    let model = args.noise_model(geometry.noise)?;

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
        anyhow::bail!("{} has no enabled outputs with detect = true", args.config.display());
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

            if args.scroll || args.focus_follows_gaze || args.edge_scroll {
                warn!("warps are live: the real pointer will move to the gaze point");
            }

            if args.edge_scroll {
                warn!("edge scrolling is live: surfaces under a dwelling gaze will scroll for real");
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

    // --- controller ---

    let mut daydream = match args.daydream {
        true => {
            let config = DaydreamConfig {
                mode             : args.refine,
                gyro_gain_px_rad : args.refine_gyro_gain,
                touch_gain_px    : args.refine_touch_gain,
                axes             : parse_axes(&args.refine_axes).context("--refine-axes")?,
            };

            let daydream = Daydream::open(args.daydream_address.as_deref(), config)?;

            info!(
                address = daydream.address(),
                refine  = ?args.refine,
                "daydream controller: pad commits, Home exits, App holds push-to-talk, volume scrolls",
            );

            Some(daydream)
        }

        false => None,
    };

    // --- click feedback ---

    // The real mouse is read when a source learns from its clicks, and when a controller
    // shares the pointer with it and has to know when the hand is on the mouse. The feed
    // stamps its own clock in the second case; nothing else reads those timestamps.
    let labels     = source.click_clock().is_some();
    let mut clicks = feedback::open_if_useful(
        source.click_clock().or_else(|| daydream.as_ref().map(|_| Instant::now())),
    );

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
        .radius_deg(args.snap_deg)
        .build();

    // --- a11y verdicts ---

    // What the application says the eyes are on, for the pointer look; its own tree
    // thread, since the edge scroller's exists only with `--edge-scroll`.
    let mut verifier = (!args.no_a11y).then(Verifier::spawn);

    // --- edge scrolling ---

    // The scroller and its surface cache exist only when asked for: the cache owns an
    // accessibility tree thread, which nothing else in this loop needs.
    let mut edge = args.edge_scroll.then(|| {
        info!(params = ?args.edge_params(), "edge scrolling on");

        (EdgeScroller::new(args.edge_params()), SurfaceCache::spawn())
    });

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
    // Whether the highlight is hidden: during a scroll, and after one until the perception
    // thread has published a detection newer than the scroll, because until then every
    // box is where the content was.
    let mut last_hidden                      = false;
    let mut last_blocked                     = false;
    let mut last_owner  : Option<Owner>      = None;
    // The pointer look's state apart from position, so a change in what is near
    // repaints even when the eyes have not moved.
    let mut last_pointer : Option<(bool, Option<u64>)> = None;
    let mut stale_gen   : Option<u64>        = None;
    // Set when a scroll starts or stops; retargeting stays off until a fixation that began
    // later than this, which is the eyes having moved on purpose.
    let mut retarget_block : Option<f64>     = None;
    let mut refined     : Option<Refined>    = None;
    // Whether push-to-talk is forwarded down right now, so an exit mid-hold releases it.
    let mut ptt_down    = false;
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

        if let Some(daydream) = daydream.as_mut() {
            controls.extend(daydream.controls());
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
                    let at = refined.take().map(|r| {
                        injector
                            .as_mut()
                            .and_then(|i| i.last_known_position().ok().flatten())
                            .unwrap_or_else(|| r.point())
                    });

                    let clicked = commit(
                        &mut engine,
                        source.truth(),
                        &elements,
                        if args.click { injector.as_mut() } else { None },
                        &mut board,
                        last_sample,
                        args.commit_latency,
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

                    // The snap point if there is one, else the gaze itself: the fine
                    // channel is also how an unlabelled target gets clicked.
                    let anchor = engine.current().map(|t| t.point).or(last_gaze);

                    match anchor {
                        Some(anchor) => {
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
                        let half = args.refine_range / 2.0;

                        r.dx_px = (r.dx_px + dx_px).clamp(-half, half);
                        r.dy_px = (r.dy_px + dy_px).clamp(-half, half);

                        let point = clamp_to_desk(&geometry, r.point());

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
        let gaze     = filtered.sample.point.filter(|_| filtered.sample.valid);

        // A thumb on the pad locks the gaze out: the pointer is the controller's until the
        // thumb lifts, so no retarget, no edge scroll and no focus warp may move it. The
        // filter still runs so the fixation state is current when the lock lifts.
        let locked = refined.is_some_and(|r| r.engaged);

        // The pointer is the controller's while it is held and the mouse has been idle since
        // it was picked up. The mouse takes over the moment it moves, and a controller lying
        // on the desk gives it up; either way gaze keeps showing where it is, but moves
        // nothing until the controller is used again.
        let owner = daydream
            .as_ref()
            .map(|d| d.owner(clicks.as_ref().and_then(ClickFeed::last_mouse_input)));
        let held  = owner.is_none_or(|o| o == Owner::Controller);

        // Eyes on a small control (last sample's, one tick stale) are aiming at it, and a
        // toolbar or file header at the top of a viewport sits squarely in the band. A
        // scroll already running is not interrupted by passing over one.
        let aiming = engine.current().is_some_and(|t| aiming_at(&t.element));

        // Edge scrolling first: a scroll decides whether the snap engine may retarget at
        // all this sample, and a scroll that starts here takes the highlight down with it.
        let scrolling = match edge.as_mut() {
            Some((scroller, surfaces)) => {
                let was     = scroller.scrolling();
                let quiet   = locked || !held || (aiming && !was);
                let stopped = edge_scroll(
                    scroller,
                    surfaces,
                    &mut warper,
                    injector.as_mut(),
                    &geometry,
                    &filtered,
                    gaze,
                    quiet,
                );

                if scroller.scrolling() && !was {
                    retarget_block = Some(filtered.sample.t_s);
                    engine.reset();
                }

                if stopped {
                    // The content moved; every box under it is stale until the output is
                    // detected again. Ask for that output first, and hide the highlight
                    // until the answer lands.
                    match gaze.and_then(|g| output_at(&geometry, g)) {
                        Some(name) => perception.force_redetect_output(name),
                        None       => perception.force_redetect(),
                    }

                    surfaces.invalidate();
                    stale_gen      = Some(store.generation());
                    // Fixations begun while the content moved do not count as aiming.
                    retarget_block = Some(filtered.sample.t_s);
                }

                scroller.scrolling()
            }

            None => false,
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
        // being within `--near-deg` of the element: the engine will snap from further
        // out than that, but a highlight on an element the eyes are nowhere near reads
        // as the overlay guessing, not the eyes aiming.
        //
        // The tree, when it answers, outranks the recogniser: a control it names is
        // marked with its own box, and text it names is not marked whatever the
        // recogniser called it.
        let aim = target.as_ref().map(|t| t.point).or(gaze);

        if let Some(v) = verifier.as_mut() {
            let settled = matches!(filtered.state, FixationState::Fixating { .. });

            v.update(aim.filter(|_| settled && !locked));
        }

        let pointer = gaze.map(|g| {
            let refining = refined.filter(|r| r.engaged);
            let close    = |c: &gaze_snap::Candidate| {
                c.distance_deg <= args.near_deg && worth_marking(&elements[c.index], args)
            };
            let verdict  = match (&verifier, aim) {
                (Some(v), Some(p)) => v.verdict(p),
                _                  => Verdict::Unknown,
            };
            let holding  = match (&verifier, aim) {
                (Some(v), Some(p)) => v.holding(p),
                _                  => false,
            };

            let (near, target) = match verdict {
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

            Pointer {
                gaze   : refining.map_or(g, |r| r.point()),
                near   : refining.is_some() || near,
                target : target.filter(|_| !hidden),
            }
        });

        let pointer_key = pointer.map(|p| (p.near, p.target.map(|t| t.id)));

        if moved || target_id != last_target || hidden != last_hidden || blocked != last_blocked
            || owner != last_owner || pointer_key != last_pointer
        {
            let state = {
                if args.overlay_debug {
                    // A highlight flickering across moving text, or parked on where text
                    // used to be, is noise; the label says what is happening instead.
                    OverlayState {
                        gaze       : gaze,
                        highlight  : target.as_ref().filter(|_| !hidden).map(|t| t.element.bbox),
                        truth      : if args.show_truth { source.truth() } else { None },
                        label      : Some(match (scrolling, hidden, retarget_block) {
                            _ if refined.is_some_and(|r| r.engaged) => "refine".to_string(),
                            _ if owner == Some(Owner::Mouse)  => "mouse".to_string(),
                            _ if owner == Some(Owner::Nobody) => "controller down".to_string(),
                            (true, _, _)        => "edge scroll".to_string(),
                            (false, true, _)    => "redetecting".to_string(),
                            (false, false, Some(_)) => "move eyes to retarget".to_string(),
                            _                   => label(&target, &filtered),
                        }),
                        background : None,
                        pointer    : None,
                    }
                }
                else {
                    OverlayState {
                        gaze       : None,
                        highlight  : None,
                        truth      : if args.show_truth { source.truth() } else { None },
                        label      : None,
                        background : None,
                        pointer    : pointer,
                    }
                }
            };

            last_pointer = pointer_key;
            last_hidden  = hidden;
            last_blocked = blocked;
            last_owner   = owner;

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
            && !locked
            && held
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

    if let Some(daydream) = daydream.as_mut() {
        daydream.stop();
    }

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
        edge_starts     = edge.as_ref().map(|(e, _)| e.starts),
        edge_units      = edge.as_ref().map(|(e, _)| e.units),
        surfaces_asked  = edge.as_ref().map(|(_, s)| s.asked),
        surfaces_found  = edge.as_ref().map(|(_, s)| s.found),
        a11y_asked      = verifier.as_ref().map(|v| v.asked),
        a11y_controls   = verifier.as_ref().map(|v| v.controls),
        a11y_statics    = verifier.as_ref().map(|v| v.statics),
        daydream_reports = daydream.as_ref().map(|d| d.reports),
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

/// Whether eyes on this element are aiming at it rather than reading past it: a control no
/// bigger than [`AIM_TARGET_PX`] either way. Text is never aimed at, whatever its size: a
/// scrolled page is words all the way to its edge, and every one of them snapped in the
/// band and held the scroller off, which made scrolling text jerky and stop-start. A link
/// or a button in the text is still a control.
fn aiming_at(element: &Element) -> bool {
    element.kind != ElementKind::Text
        && element.bbox.w <= AIM_TARGET_PX
        && element.bbox.h <= AIM_TARGET_PX
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

/// One sample of edge scrolling: the surface under the gaze from the cache, the
/// scroller's verdict, and the injection it asks for. Returns whether a scroll stopped on
/// this sample, which the caller turns into a re-detection.
///
/// A start puts the pointer inside the surface first, at the gaze point, unless it is
/// already there: the wheel goes to the surface under the pointer, and the user's pointer
/// is wherever they left it. Units then go out as high-resolution wheel motion without
/// moving the pointer again, so gaze jitter during the scroll does not drag it about.
///
/// `quiet` (a refine in progress, or eyes aiming at a small target) shows the scroller no
/// surface and no eyes, which stops a running scroll and arms nothing, without the gaze
/// ever reaching it.
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
)
    -> bool
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

    match scroller.update(filtered.sample.t_s, eyes, surface.as_ref()) {
        EdgeAction::Nothing => false,

        EdgeAction::Start { dir, point } => {
            let surface  = surface.expect("a start comes from a surface");
            let viewport = surface.viewport;
            let pointer  = pointer_position(injector.as_deref_mut(), warper);
            let inside   = pointer.is_some_and(|p| viewport.contains(p));

            info!(
                dir   = ?dir,
                role  = surfaces.current().map(|s| s.clip.role.as_str()).unwrap_or("?"),
                x     = %format_args!("{:.0}", viewport.x),
                y     = %format_args!("{:.0}", viewport.y),
                w     = %format_args!("{:.0}", viewport.w),
                h     = %format_args!("{:.0}", viewport.h),
                above = %format_args!("{:.0}", surface.above_px),
                below = %format_args!("{:.0}", surface.below_px),
                warp  = !inside,
                live  = injector.is_some(),
                "edge scroll start",
            );

            if !inside {
                do_warp(
                    injector.as_deref_mut(),
                    warper,
                    point,
                    filtered.sample.sigma_deg,
                    None,
                    WarpReason::EdgeScroll,
                    Instant::now(),
                );
            }

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

            true
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
fn worth_marking(element: &Element, args: &Args) -> bool {
    match element.kind {
        ElementKind::Text    => args.highlight_text,
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
