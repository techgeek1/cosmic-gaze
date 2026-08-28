//! `SyntheticProvider`: grabs a real evdev mouse, integrates its relative motion into a
//! clean virtual gaze point, and pushes that point through the desk's noise model to
//! emit `GazeSample`s that look like they came from a real remote eye tracker.
//!
//! # Off-axis convention
//!
//! A remote PCCR tracker's accuracy is best when the user looks straight back at it and
//! degrades as gaze points away from it (see `gaze_core::noise` and `SigmaProfile`). Sigma
//! is looked up against `DesktopGeometry::off_axis_deg(&ray)`: the angle between the
//! reversed gaze ray (`-ray.dir`, since `ray.dir` points from the eye out toward the
//! panel) and the tracker -> eye axis. Zero means the user is looking straight back at
//! the tracker; it grows as gaze points away from it.
//!
//! # Bias vs. jitter
//!
//! `SigmaProfile::sigma_at` gives the *total* per-sample error, but real trackers spend
//! most of that budget on a per-fixation bias rather than independent per-sample noise:
//! sample-to-sample precision is 0.1-0.3 deg even when the reported accuracy is much
//! worse. Drawing a fresh `N(0, sigma)` every sample -- this crate's first version --
//! makes consecutive samples uncorrelated, which looks like continuous 50-120 deg/s
//! motion to an I-VT fixation classifier and starves it of fixations entirely. `NoiseState`
//! (in this file) instead draws a per-fixation bias from `N(0, NoiseModel::bias_sigma)`
//! and holds it until the clean point moves more than `NoiseModel::bias_redraw_deg` of
//! visual angle or the local sigma changes substantially, adding only small per-sample
//! `N(0, NoiseModel::jitter_deg)` jitter on top each tick.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, NoiseModel, Ray};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

use crate::device::{DeviceError, WheelAccumulator, find_device};
use crate::provider::{Button, ButtonState, GazeProvider, ProviderEvent};

/// How long the device-reader thread sleeps between polls of a non-blocking evdev fd
/// while waiting for new events. Small enough that `stop()` returns promptly, large
/// enough not to busy-spin a core for a mouse that mostly sits still.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// A `GazeProvider` backed by a grabbed evdev mouse. See the module docs for the off-axis
/// convention used to look up sigma, and PLAN.md's gaze-provider-synthetic contract for
/// the full spec this implements.
pub struct SyntheticProvider {
    sample_rx     : Receiver<GazeSample>,
    events_rx     : Receiver<ProviderEvent>,
    clean_point   : Arc<Mutex<GlobalPx>>,
    buttons       : Arc<Mutex<ButtonState>>,
    stop_flag     : Arc<AtomicBool>,
    device_thread : Option<JoinHandle<()>>,
    ticker_thread : Option<JoinHandle<()>>,
}

// --- SyntheticProvider ---

impl SyntheticProvider {
    /// Entry point for the builder: `SyntheticProvider::create().geometry(g).model(m).start()`.
    pub fn create() -> SyntheticProviderBuilder {
        SyntheticProviderBuilder::new()
    }

    /// Current clean (noise-free) gaze point, for scoring how far a noisy sample landed
    /// from ground truth. Updated continuously by the device-reader thread.
    pub fn truth(&self) -> GlobalPx {
        *self.clean_point.lock().expect("clean_point mutex poisoned")
    }

    /// Current up/down state of each button on the grabbed device.
    pub fn buttons(&self) -> ButtonState {
        *self.buttons.lock().expect("buttons mutex poisoned")
    }

    /// Drains and returns the button events queued since the last call, without blocking.
    pub fn events(&mut self) -> impl Iterator<Item = ProviderEvent> + '_ {
        self.events_rx.try_iter()
    }
}

impl GazeProvider for SyntheticProvider {
    fn next(&mut self) -> Option<GazeSample> {
        self.sample_rx.recv().ok()
    }

    fn try_next(&mut self) -> Option<GazeSample> {
        self.sample_rx.try_recv().ok()
    }

    fn stop(&mut self) {
        // Idempotent: a second call finds both handles already taken and does nothing.
        self.stop_flag.store(true, Ordering::Relaxed);

        if let Some(handle) = self.device_thread.take() {
            let _ = handle.join();
        }

        if let Some(handle) = self.ticker_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SyntheticProvider {
    fn drop(&mut self) {
        // Guarantees the grabbed device is released even if the caller forgets to call
        // `stop()`: closing the evdev fd (inside the device thread, once it exits)
        // releases EVIOCGRAB automatically.
        self.stop();
    }
}

/// Builds a `SyntheticProvider`. Entry point is `SyntheticProvider::create()`, not `new()`
/// on the provider itself, per this workspace's builder convention.
pub struct SyntheticProviderBuilder {
    device_name       : String,
    device_path       : Option<PathBuf>,
    geometry          : Option<DesktopGeometry>,
    model             : Option<NoiseModel>,
    gain_px_per_count : f64,
    seed              : u64,
}

// --- SyntheticProviderBuilder ---

impl SyntheticProviderBuilder {
    fn new() -> Self {
        Self {
            device_name       : "Lenovo".to_string(),
            device_path       : None,
            geometry          : None,
            model             : None,
            gain_px_per_count : 1.0,
            seed              : 0,
        }
    }

    /// Name substring to match against `/dev/input/event*` devices when `device_path`
    /// isn't set. Defaults to `"Lenovo"`, the spare mouse on this desk (see PLAN.md's
    /// Environment facts).
    pub fn device_name(mut self, name: impl Into<String>) -> Self {
        self.device_name = name.into();
        self
    }

    /// Exact device node to open, bypassing name matching.
    pub fn device_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.device_path = Some(path.into());
        self
    }

    /// Desk geometry used to lift the clean point to a ray, look up off-axis angle, and
    /// intersect the perturbed ray back to a point. Required.
    pub fn geometry(mut self, geometry: DesktopGeometry) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// Sample rate, sigma profile, drift, and latency to synthesize. Required.
    pub fn model(mut self, model: NoiseModel) -> Self {
        self.model = Some(model);
        self
    }

    /// Logical pixels of clean-point motion per raw `REL_X`/`REL_Y` count. Defaults to 1.0.
    pub fn gain_px_per_count(mut self, gain: f64) -> Self {
        self.gain_px_per_count = gain;
        self
    }

    /// Seeds the noise RNG for reproducible runs. Defaults to `0`.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Opens and grabs the device, spawns the reader and ticker threads, and starts
    /// emitting samples. The clean point starts at the centre of the first enabled
    /// output in `geometry`.
    pub fn start(self) -> Result<SyntheticProvider, ProviderError> {
        let geometry = self.geometry.ok_or(ProviderError::MissingGeometry)?;
        let model    = self.model.ok_or(ProviderError::MissingModel)?;

        let start_point = start_point(&geometry)?;

        let (path, mut device) = find_device(self.device_path.as_deref(), &self.device_name)?;

        device.grab().map_err(|source| ProviderError::Grab { path: path.clone(), source })?;

        // Non-blocking so the device thread can poll `stop_flag` instead of parking
        // forever in a blocking read with no way to interrupt it.
        device.set_nonblocking(true)
            .map_err(|source| ProviderError::SetNonblocking { path: path.clone(), source })?;

        let geometry = Arc::new(geometry);
        let clean_point = Arc::new(Mutex::new(start_point));
        let buttons     = Arc::new(Mutex::new(ButtonState::default()));
        let stop_flag   = Arc::new(AtomicBool::new(false));

        let (sample_tx, sample_rx) = crossbeam_channel::unbounded();
        let (events_tx, events_rx) = crossbeam_channel::unbounded();

        let start = Instant::now();

        let device_thread = thread::spawn({
            let geometry    = Arc::clone(&geometry);
            let clean_point = Arc::clone(&clean_point);
            let buttons     = Arc::clone(&buttons);
            let stop_flag   = Arc::clone(&stop_flag);
            let gain        = self.gain_px_per_count;

            move || run_device_thread(device, &geometry, gain, &clean_point, &buttons, &events_tx, &stop_flag)
        });

        let ticker_thread = thread::spawn({
            let geometry    = Arc::clone(&geometry);
            let clean_point = Arc::clone(&clean_point);
            let stop_flag   = Arc::clone(&stop_flag);
            let seed        = self.seed;

            move || run_ticker_thread(&geometry, model, &clean_point, &sample_tx, &stop_flag, start, seed)
        });

        Ok(SyntheticProvider {
            sample_rx     : sample_rx,
            events_rx     : events_rx,
            clean_point   : clean_point,
            buttons       : buttons,
            stop_flag     : stop_flag,
            device_thread : Some(device_thread),
            ticker_thread : Some(ticker_thread),
        })
    }
}

/// Reads evdev events off `device` until `stop_flag` is set, integrating `REL_X`/`REL_Y`
/// into `clean_point` and mirroring button state into `buttons`, emitting a
/// `ProviderEvent::ButtonPressed` on each press and a `ProviderEvent::Wheel` for each
/// whole detent of wheel motion.
fn run_device_thread(
    mut device  : Device,
    geometry    : &DesktopGeometry,
    gain        : f64,
    clean_point : &Mutex<GlobalPx>,
    buttons     : &Mutex<ButtonState>,
    events_tx   : &Sender<ProviderEvent>,
    stop_flag   : &AtomicBool,
)
{
    // One accumulator for the life of the device: which wheel axis this mouse uses is a
    // property of the mouse, and sub-detent motion has to survive across read batches.
    let mut wheel = WheelAccumulator::new();

    while !stop_flag.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                // A batch can carry several axis reports before the SYN_REPORT that ends
                // it; accumulate and apply as one integration step so a diagonal move
                // clamps against the desk as a single candidate point, not two.
                let mut dx = 0.0_f64;
                let mut dy = 0.0_f64;

                for event in events {
                    match event.destructure() {
                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_X, value) => {
                            dx += value as f64;
                        }
                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_Y, value) => {
                            dy += value as f64;
                        }
                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL, value) => {
                            wheel.notch(value);
                        }
                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL_HI_RES, value) => {
                            wheel.hi_res(value);
                        }
                        EventSummary::Key(_, code, value) => {
                            handle_key(code, value, buttons, events_tx);
                        }
                        _ => {}
                    }
                }

                if dx != 0.0 || dy != 0.0 {
                    integrate(geometry, clean_point, dx * gain, dy * gain);
                }

                // Taken once per batch, after both wheel axes have been seen, so the
                // accumulator can decide which of them this device actually uses.
                let detents = wheel.take();

                if detents != 0 {
                    let _ = events_tx.send(ProviderEvent::Wheel(detents));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(DEVICE_POLL_INTERVAL);
            }
            Err(e) => {
                tracing::error!("gaze device read error, stopping reader thread: {e}");
                break;
            }
        }
    }
}

/// Updates `buttons` and, on a press, queues a `ProviderEvent`. Non-mouse-button keys on
/// the grabbed device (there shouldn't be any on a mouse, but a keyboard-with-mouse combo
/// device is possible) are ignored.
fn handle_key(code: KeyCode, value: i32, buttons: &Mutex<ButtonState>, events_tx: &Sender<ProviderEvent>) {
    let Some(button) = map_button(code) else {
        return;
    };

    let pressed = value != 0;

    {
        let mut state = buttons.lock().expect("buttons mutex poisoned");

        match button {
            Button::Left   => state.left   = pressed,
            Button::Right  => state.right  = pressed,
            Button::Middle => state.middle = pressed,
        }
    }

    if pressed {
        // The events channel is unbounded and drained by `events()`; this never blocks.
        let _ = events_tx.send(ProviderEvent::ButtonPressed(button));
    }
}

/// Maps an evdev key code to a `Button`, or `None` for keys this crate doesn't track.
fn map_button(code: KeyCode) -> Option<Button> {
    match code {
        KeyCode::BTN_LEFT   => Some(Button::Left),
        KeyCode::BTN_RIGHT  => Some(Button::Right),
        KeyCode::BTN_MIDDLE => Some(Button::Middle),
        _                   => None,
    }
}

/// Centre of the first enabled output in config order, where the clean point starts.
fn start_point(geometry: &DesktopGeometry) -> Result<GlobalPx, ProviderError> {
    geometry.outputs.iter()
        .find(|o| o.enabled)
        .map(|o| o.uv_to_px(0.5, 0.5))
        .ok_or(ProviderError::NoEnabledOutputs)
}

/// Applies a `(dx, dy)` motion step to the clean point, clamped to the union of enabled
/// outputs: a step that would land the point in the gap between outputs is rejected and
/// the point stays exactly where it was, rather than being clamped to the nearest edge.
/// Crossing between adjacent outputs (no gap at the seam) is unaffected.
fn integrate(geometry: &DesktopGeometry, clean_point: &Mutex<GlobalPx>, dx: f64, dy: f64) {
    let mut point = clean_point.lock().expect("clean_point mutex poisoned");
    let candidate = GlobalPx { x: point.x + dx, y: point.y + dy };

    if geometry.outputs.iter().any(|o| o.enabled && o.contains_px(candidate)) {
        *point = candidate;
    }
}

/// Fraction change in local sigma, relative to the sigma the current bias was drawn
/// against, that forces a bias re-draw even though the clean point hasn't moved far
/// enough to count as a new fixation on its own -- e.g. gaze swept into a steeper
/// off-axis region of the sigma profile without technically leaving the redraw radius.
const BIAS_RESIGMA_FRACTION: f64 = 0.20;

/// Noise state carried across ticks: the seeded RNG, the accumulated random-walk drift
/// offset, and the current per-fixation bias. Bundled into one struct so `build_sample`
/// stays under the arg-count lint threshold and so the RNG/drift/bias lifecycle is
/// obviously one unit.
///
/// Real trackers' headline accuracy figure is dominated by a per-fixation bias (the
/// corneal-reflection model settles on a slightly-off calibration for this gaze angle and
/// holds it for the fixation); sample-to-sample precision is far tighter. Drawing fresh
/// noise at the full sigma every sample -- the first version of this provider -- makes
/// consecutive samples uncorrelated, which reads as continuous saccade-speed motion to an
/// I-VT classifier and no fixation ever registers. Splitting sigma into a per-fixation
/// bias plus small per-sample jitter (`NoiseModel::jitter_deg`) fixes that while keeping
/// the same total sigma.
struct NoiseState {
    rng            : StdRng,
    drift_x_deg    : f64,
    drift_y_deg    : f64,
    /// Current per-fixation bias, degrees. Persists across samples until re-drawn.
    bias_x_deg     : f64,
    bias_y_deg     : f64,
    /// Clean point the current bias was drawn at, for measuring fixation movement.
    /// `None` before the first draw, which forces an initial draw unconditionally.
    bias_anchor    : Option<GlobalPx>,
    /// Local sigma the current bias was drawn against, for the re-sigma check.
    bias_sigma_deg : Option<f64>,
}

impl NoiseState {
    /// Starts with zero accumulated drift and no bias drawn yet, per-axis noise seeded
    /// from `seed`.
    fn new(seed: u64) -> Self {
        Self {
            rng            : StdRng::seed_from_u64(seed),
            drift_x_deg    : 0.0,
            drift_y_deg    : 0.0,
            bias_x_deg     : 0.0,
            bias_y_deg     : 0.0,
            bias_anchor    : None,
            bias_sigma_deg : None,
        }
    }

    /// Total per-axis angular offset to apply this tick: the persistent per-fixation
    /// bias, plus fresh per-sample jitter, plus accumulated random-walk drift.
    ///
    /// Re-draws the bias when `point` has moved more than `model.bias_redraw_deg` of
    /// visual angle from where the current bias was drawn (a new fixation), or when
    /// `sigma_deg` has moved by more than `BIAS_RESIGMA_FRACTION` from the sigma the
    /// current bias was drawn against (a different part of the sigma profile).
    fn offset_deg(
        &mut self,
        geometry  : &DesktopGeometry,
        model     : &NoiseModel,
        point     : GlobalPx,
        sigma_deg : f64,
        dt_s      : f64,
    )
        -> (f64, f64)
    {
        let moved_enough = self.bias_anchor
            .and_then(|anchor| geometry.angle_between_deg(geometry.eye(), anchor, point))
            .is_none_or(|moved_deg| moved_deg > model.bias_redraw_deg);

        let sigma_changed_enough = self.bias_sigma_deg
            .is_none_or(|prev| (sigma_deg - prev).abs() > prev * BIAS_RESIGMA_FRACTION);

        if moved_enough || sigma_changed_enough {
            let bias = Normal::new(0.0, model.bias_sigma(sigma_deg))
                .expect("bias sigma is finite and non-negative");

            self.bias_x_deg = bias.sample(&mut self.rng);
            self.bias_y_deg = bias.sample(&mut self.rng);
            self.bias_anchor = Some(point);
            self.bias_sigma_deg = Some(sigma_deg);
        }

        let jitter = Normal::new(0.0, model.jitter_deg).expect("jitter_deg is finite and non-negative");
        let jitter_x_deg = jitter.sample(&mut self.rng);
        let jitter_y_deg = jitter.sample(&mut self.rng);

        let drift_step = Normal::new(0.0, model.drift_deg * dt_s.sqrt())
            .expect("drift_deg is finite and non-negative");

        self.drift_x_deg += drift_step.sample(&mut self.rng);
        self.drift_y_deg += drift_step.sample(&mut self.rng);

        (self.bias_x_deg + jitter_x_deg + self.drift_x_deg, self.bias_y_deg + jitter_y_deg + self.drift_y_deg)
    }
}

/// Ticks at `model.rate_hz`, reading the current clean point and pushing a synthesized
/// `GazeSample` through `tx` on each tick, until `stop_flag` is set.
fn run_ticker_thread(
    geometry    : &DesktopGeometry,
    model       : NoiseModel,
    clean_point : &Mutex<GlobalPx>,
    tx          : &Sender<GazeSample>,
    stop_flag   : &AtomicBool,
    start       : Instant,
    seed        : u64,
)
{
    let period = Duration::from_secs_f64(1.0 / model.rate_hz);
    let mut noise = NoiseState::new(seed);

    let mut last_tick = start;
    let mut next_tick = start + period;

    while !stop_flag.load(Ordering::Relaxed) {
        let now = Instant::now();

        if now < next_tick {
            thread::sleep((next_tick - now).min(period));
            continue;
        }

        let dt_s = (next_tick - last_tick).as_secs_f64().max(f64::EPSILON);
        let sample = build_sample(geometry, &model, clean_point, &mut noise, start, dt_s);

        last_tick = next_tick;
        next_tick += period;

        // Unbounded channel: a consumer that isn't polling yet still sees every sample
        // once it starts, at the cost of unbounded queueing if it never polls at all.
        let _ = tx.send(sample);
    }
}

/// Builds one `GazeSample` from the current clean point: lifts it to a ray, looks up
/// sigma from the off-axis angle (see module docs), perturbs the ray by noise plus
/// accumulated drift, and re-intersects. Invalid when sigma is undefined at this angle or
/// the perturbed ray misses every panel.
fn build_sample(
    geometry    : &DesktopGeometry,
    model       : &NoiseModel,
    clean_point : &Mutex<GlobalPx>,
    noise       : &mut NoiseState,
    start       : Instant,
    dt_s        : f64,
)
    -> GazeSample
{
    let point = *clean_point.lock().expect("clean_point mutex poisoned");
    let t_s   = start.elapsed().as_secs_f64() + model.latency_s;

    // `integrate` guarantees the clean point always lies over some enabled output, so
    // this should never miss; treated defensively as lost tracking rather than a panic.
    let Some(ray) = geometry.px_to_ray(point) else {
        return invalid_sample(t_s, None);
    };

    let Some(sigma_deg) = model.profile.sigma_at(geometry.off_axis_deg(&ray)) else {
        return invalid_sample(t_s, Some(ray));
    };

    let (dx_deg, dy_deg) = noise.offset_deg(geometry, model, point, sigma_deg, dt_s);
    let noisy_ray = DesktopGeometry::perturb_ray(&ray, dx_deg, dy_deg);

    match geometry.intersect(&noisy_ray) {
        Some(hit) => GazeSample { t_s: t_s, ray: Some(noisy_ray), point: Some(hit.px), sigma_deg: sigma_deg, valid: true },
        None      => GazeSample { t_s: t_s, ray: Some(noisy_ray), point: None, sigma_deg: sigma_deg, valid: false },
    }
}

/// Builds an invalid sample: no sigma estimate or no intersection, so `point` is always
/// `None`. `sigma_deg` is set to `f64::MAX` as a sentinel (not `f64::INFINITY`: samples
/// round-trip through JSON via `ReplayProvider`, and JSON has no infinity literal);
/// consumers must not read it, or `ray`/`point`, when `valid` is false.
fn invalid_sample(t_s: f64, ray: Option<Ray>) -> GazeSample {
    GazeSample { t_s: t_s, ray: ray, point: None, sigma_deg: f64::MAX, valid: false }
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(transparent)]
    Device(#[from] DeviceError),

    #[error("desk geometry must be set via .geometry(...) before start()")]
    MissingGeometry,

    #[error("noise model must be set via .model(...) before start()")]
    MissingModel,

    #[error("desk geometry has no enabled outputs to start the gaze point on")]
    NoEnabledOutputs,

    #[error("failed to grab {}: {source}", path.display())]
    Grab { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to set {} non-blocking: {source}", path.display())]
    SetNonblocking { path: PathBuf, #[source] source: std::io::Error },
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use gaze_core::OutputGeometry;

    /// An `OutputGeometry` fixture with harmless placeholder physical/pose fields; only
    /// the logical rect and `enabled` matter to `integrate` and `start_point`.
    fn output(name: &str, enabled: bool, x: f64, y: f64, w: f64, h: f64) -> OutputGeometry {
        OutputGeometry {
            name          : name.to_string(),
            enabled       : enabled,
            logical_x     : x,
            logical_y     : y,
            logical_w     : w,
            logical_h     : h,
            physical_w_mm : 100.0,
            physical_h_mm : 80.0,
            radius_mm     : 0.0,
            position_mm   : [0.0, 0.0, 0.0],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        }
    }

    /// Two outputs side by side with a 200 px gap between them (A: x in [0, 1000), B: x
    /// in [1200, 2200)), plus a disabled output overlapping the gap to prove disabled
    /// outputs never catch a candidate point.
    fn two_outputs_with_gap() -> DesktopGeometry {
        DesktopGeometry {
            eye_mm     : [0.0, 0.0, 500.0],
            tracker_mm : [0.0, 0.0, 0.0],
            outputs    : vec![
                output("A", true, 0.0, 0.0, 1000.0, 800.0),
                output("B", true, 1200.0, 0.0, 1000.0, 800.0),
                output("disabled-in-gap", false, 1000.0, 0.0, 200.0, 800.0),
            ],
            noise: None,
        }
    }

    #[test]
    fn start_point_is_centre_of_first_enabled_output() {
        let geometry = two_outputs_with_gap();

        assert_eq!(start_point(&geometry).unwrap(), GlobalPx { x: 500.0, y: 400.0 });
    }

    #[test]
    fn start_point_skips_a_disabled_first_output() {
        let mut geometry = two_outputs_with_gap();
        geometry.outputs[0].enabled = false;

        assert_eq!(start_point(&geometry).unwrap(), GlobalPx { x: 1700.0, y: 400.0 });
    }

    #[test]
    fn start_point_errors_when_no_output_is_enabled() {
        let mut geometry = two_outputs_with_gap();

        for output in &mut geometry.outputs {
            output.enabled = false;
        }

        assert!(matches!(start_point(&geometry), Err(ProviderError::NoEnabledOutputs)));
    }

    #[test]
    fn integrate_moves_within_an_output() {
        let geometry = two_outputs_with_gap();
        let point = Mutex::new(GlobalPx { x: 500.0, y: 400.0 });

        integrate(&geometry, &point, 10.0, -5.0);

        assert_eq!(*point.lock().unwrap(), GlobalPx { x: 510.0, y: 395.0 });
    }

    #[test]
    fn integrate_rejects_a_move_into_the_gap_between_outputs() {
        let geometry = two_outputs_with_gap();
        let point = Mutex::new(GlobalPx { x: 990.0, y: 400.0 });

        // 990 + 50 = 1040, inside the gap [1000, 1200) and over the disabled output --
        // the point must stay exactly where it was.
        integrate(&geometry, &point, 50.0, 0.0);

        assert_eq!(*point.lock().unwrap(), GlobalPx { x: 990.0, y: 400.0 });
    }

    #[test]
    fn integrate_allows_a_jump_directly_into_the_next_output() {
        let geometry = two_outputs_with_gap();
        let point = Mutex::new(GlobalPx { x: 990.0, y: 400.0 });

        // A single large step landing inside B is accepted even though it leaps over the
        // gap in one integration step; only the candidate's own containment matters.
        integrate(&geometry, &point, 300.0, 0.0);

        assert_eq!(*point.lock().unwrap(), GlobalPx { x: 1290.0, y: 400.0 });
    }

    #[test]
    fn integrate_rejects_a_move_beyond_the_outer_bound_of_the_union() {
        let geometry = two_outputs_with_gap();
        let point = Mutex::new(GlobalPx { x: 10.0, y: 10.0 });

        integrate(&geometry, &point, -50.0, -50.0);

        assert_eq!(*point.lock().unwrap(), GlobalPx { x: 10.0, y: 10.0 });
    }

    #[test]
    fn map_button_covers_the_three_mouse_buttons_and_nothing_else() {
        assert_eq!(map_button(KeyCode::BTN_LEFT), Some(Button::Left));
        assert_eq!(map_button(KeyCode::BTN_RIGHT), Some(Button::Right));
        assert_eq!(map_button(KeyCode::BTN_MIDDLE), Some(Button::Middle));
        assert_eq!(map_button(KeyCode::BTN_SIDE), None);
    }

    /// Loads the frozen desk fixture (the live config drifts with the physical desk),
    /// exercising `build_sample` against the now-implemented `gaze-core` geometry
    /// (px_to_ray, off_axis_deg, sigma_at, perturb_ray, intersect).
    fn desk() -> (DesktopGeometry, NoiseModel) {
        let geometry = DesktopGeometry::from_toml(include_str!("../../../config/desk-fixture.toml")).unwrap();
        let model = geometry.noise.expect("the desk fixture has a [noise] section");

        (geometry, model)
    }

    #[test]
    fn build_sample_is_valid_at_the_centre_of_the_first_output() {
        let (geometry, model) = desk();
        let clean_point = Mutex::new(start_point(&geometry).unwrap());
        let mut noise = NoiseState::new(0);

        let sample = build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz);

        // The first enabled output's centre is well inside the profile's invalid cutoff
        // (40 deg by default), so this must land valid with a sane, bounded sigma.
        assert!(sample.valid);
        assert!(sample.sigma_deg >= model.profile.sigma_deg);
        assert!(sample.sigma_deg <= model.profile.sigma_deg * model.profile.ramp_factor);
        assert!(sample.point.is_some());
        assert!(sample.ray.is_some());
    }

    #[test]
    fn build_sample_is_deterministic_for_a_fixed_seed() {
        let (geometry, model) = desk();
        let point = start_point(&geometry).unwrap();

        let run = || {
            let clean_point = Mutex::new(point);
            let mut noise = NoiseState::new(42);

            build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz)
        };

        let a = run();
        let b = run();

        assert_eq!(a.point, b.point);
        assert_eq!(a.sigma_deg, b.sigma_deg);
        assert_eq!(a.valid, b.valid);
    }

    #[test]
    fn build_sample_is_invalid_when_sigma_is_undefined_at_any_angle() {
        let (geometry, mut model) = desk();

        // `invalid_at_deg == 0.0` means `sigma_at` returns `None` for every off-axis
        // angle (it rejects at `off >= invalid_at_deg`, and `off` is always >= 0), so this
        // exercises the "sigma is None -> invalid" path independent of the desk's actual
        // geometry or the RNG draw.
        model.profile.invalid_at_deg = 0.0;

        let clean_point = Mutex::new(start_point(&geometry).unwrap());
        let mut noise = NoiseState::new(0);

        let sample = build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz);

        assert!(!sample.valid);
        assert!(sample.point.is_none());
        assert_eq!(sample.sigma_deg, f64::MAX);
        // Lost-tracking-by-angle still reports the clean ray for debugging.
        assert!(sample.ray.is_some());
    }

    #[test]
    fn build_sample_is_invalid_when_the_perturbed_ray_misses_every_panel() {
        let (geometry, mut model) = desk();

        // A huge sigma all but guarantees the drawn per-sample noise rotates the ray past
        // every panel's angular extent, exercising the "intersect misses -> invalid" path.
        model.profile.sigma_deg   = 1.0e6;
        model.profile.flat_to_deg = 1.0e9;

        let clean_point = Mutex::new(start_point(&geometry).unwrap());
        let mut noise = NoiseState::new(0);

        let sample = build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz);

        assert!(!sample.valid);
        assert!(sample.point.is_none());
        // Unlike the sigma-undefined case, a real sigma was used, so it's reported.
        assert_eq!(sample.sigma_deg, model.profile.sigma_deg);
        assert!(sample.ray.is_some());
    }

    #[test]
    fn build_sample_drift_accumulates_across_ticks() {
        let (geometry, mut model) = desk();
        model.drift_deg = 5.0;

        let clean_point = Mutex::new(start_point(&geometry).unwrap());
        let mut noise = NoiseState::new(7);

        build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz);
        let drift_after_one = (noise.drift_x_deg, noise.drift_y_deg);

        build_sample(&geometry, &model, &clean_point, &mut noise, Instant::now(), 1.0 / model.rate_hz);
        let drift_after_two = (noise.drift_x_deg, noise.drift_y_deg);

        // Zero drift would only happen by an astronomically unlikely cancellation; in
        // practice this proves the offset is persistent state, not recomputed from zero
        // each tick.
        assert_ne!(drift_after_one, (0.0, 0.0));
        assert_ne!(drift_after_two, drift_after_one);
    }

    #[test]
    fn offset_deg_jitters_around_a_persistent_bias_at_a_static_point() {
        let (geometry, mut model) = desk();

        // Isolate bias/jitter from drift for this statistical check.
        model.drift_deg = 0.0;

        let point = start_point(&geometry).unwrap();
        let ray = geometry.px_to_ray(point).unwrap();
        let sigma_deg = model.profile.sigma_at(geometry.off_axis_deg(&ray)).unwrap();
        let dt_s = 1.0 / model.rate_hz;

        let mut noise = NoiseState::new(0);
        let first = noise.offset_deg(&geometry, &model, point, sigma_deg, dt_s);
        let bias = (noise.bias_x_deg, noise.bias_y_deg);

        // The bias should be a real, non-degenerate offset for this desk (sigma is well
        // above jitter_deg here), so "mean sits at the bias" is a meaningful claim below.
        assert!(bias.0.abs() > 0.05 || bias.1.abs() > 0.05, "bias={bias:?} looks degenerate");

        let n = 2000;
        let mut samples = vec![first];

        for _ in 1..n {
            samples.push(noise.offset_deg(&geometry, &model, point, sigma_deg, dt_s));
        }

        // The point never moved and sigma never changed, so the bias must never redraw.
        assert_eq!((noise.bias_x_deg, noise.bias_y_deg), bias);

        // RMS of consecutive differences isolates the jitter: the bias is identical in
        // both terms of each difference and cancels, leaving `jitter_i - jitter_{i-1}`,
        // which has stddev `jitter_deg * sqrt(2)` per axis.
        let mut sq_diff_sum = 0.0;

        for pair in samples.windows(2) {
            let (dx0, dy0) = pair[0];
            let (dx1, dy1) = pair[1];

            sq_diff_sum += (dx1 - dx0).powi(2) + (dy1 - dy0).powi(2);
        }

        let rms_diff = (sq_diff_sum / (2.0 * (n - 1) as f64)).sqrt();
        let expected = model.jitter_deg * 2.0_f64.sqrt();

        assert!(
            (rms_diff - expected).abs() < expected * 0.15,
            "consecutive-sample rms={rms_diff:.4} expected~{expected:.4}"
        );

        // The mean over many samples sits at the bias, not at zero: jitter and (disabled)
        // drift average out, leaving the persistent per-fixation offset.
        let mean_x: f64 = samples.iter().map(|(x, _)| x).sum::<f64>() / n as f64;
        let mean_y: f64 = samples.iter().map(|(_, y)| y).sum::<f64>() / n as f64;

        assert!((mean_x - bias.0).abs() < 0.05, "mean_x={mean_x:.4} bias={:.4}", bias.0);
        assert!((mean_y - bias.1).abs() < 0.05, "mean_y={mean_y:.4} bias={:.4}", bias.1);
        assert!(mean_x.abs() > bias.0.abs() * 0.5, "mean_x={mean_x:.4} should track the bias, not zero");
    }

    #[test]
    fn offset_deg_keeps_the_bias_for_a_small_move_within_one_fixation() {
        let (geometry, model) = desk();
        let point_a = start_point(&geometry).unwrap();
        let sigma_deg = model.profile
            .sigma_at(geometry.off_axis_deg(&geometry.px_to_ray(point_a).unwrap()))
            .unwrap();
        let dt_s = 1.0 / model.rate_hz;

        // A tiny nudge, well under `bias_redraw_deg` (1.0 by default) of visual angle.
        let (px_per_deg_x, _) = geometry.px_per_deg(geometry.eye(), point_a).unwrap();
        let point_b = GlobalPx { x: point_a.x + px_per_deg_x * 0.1, y: point_a.y };
        let moved_deg = geometry.angle_between_deg(geometry.eye(), point_a, point_b).unwrap();
        assert!(moved_deg < model.bias_redraw_deg, "test nudge of {moved_deg} deg is not small");

        let mut noise = NoiseState::new(0);
        noise.offset_deg(&geometry, &model, point_a, sigma_deg, dt_s);
        let bias_a = (noise.bias_x_deg, noise.bias_y_deg);

        noise.offset_deg(&geometry, &model, point_b, sigma_deg, dt_s);
        let bias_b = (noise.bias_x_deg, noise.bias_y_deg);

        assert_eq!(bias_a, bias_b);
    }

    #[test]
    fn offset_deg_redraws_the_bias_after_moving_more_than_bias_redraw_deg() {
        let (geometry, model) = desk();
        let point_a = start_point(&geometry).unwrap();
        let sigma_deg = model.profile
            .sigma_at(geometry.off_axis_deg(&geometry.px_to_ray(point_a).unwrap()))
            .unwrap();
        let dt_s = 1.0 / model.rate_hz;

        // Move 2 deg of visual angle, comfortably past the default 1 deg redraw radius.
        let (px_per_deg_x, _) = geometry.px_per_deg(geometry.eye(), point_a).unwrap();
        let point_b = GlobalPx { x: point_a.x + px_per_deg_x * 2.0, y: point_a.y };
        let moved_deg = geometry.angle_between_deg(geometry.eye(), point_a, point_b).unwrap();
        assert!(moved_deg > model.bias_redraw_deg, "test move of {moved_deg} deg is not large enough");

        let mut noise = NoiseState::new(0);
        noise.offset_deg(&geometry, &model, point_a, sigma_deg, dt_s);
        let bias_a = (noise.bias_x_deg, noise.bias_y_deg);

        noise.offset_deg(&geometry, &model, point_b, sigma_deg, dt_s);
        let bias_b = (noise.bias_x_deg, noise.bias_y_deg);

        assert_ne!(bias_a, bias_b);
        assert_eq!(noise.bias_anchor, Some(point_b));
    }

    #[test]
    fn offset_deg_redraws_the_bias_when_sigma_changes_by_more_than_20_percent() {
        let (geometry, model) = desk();
        let point = start_point(&geometry).unwrap();
        let dt_s = 1.0 / model.rate_hz;

        let mut noise = NoiseState::new(0);
        noise.offset_deg(&geometry, &model, point, 0.7, dt_s);
        let bias_a = (noise.bias_x_deg, noise.bias_y_deg);

        // Same point (no angular move at all), but sigma jumped by more than 20%.
        noise.offset_deg(&geometry, &model, point, 1.0, dt_s);
        let bias_b = (noise.bias_x_deg, noise.bias_y_deg);

        assert_ne!(bias_a, bias_b);
    }

    #[test]
    fn offset_deg_keeps_the_bias_when_sigma_changes_by_less_than_20_percent() {
        let (geometry, model) = desk();
        let point = start_point(&geometry).unwrap();
        let dt_s = 1.0 / model.rate_hz;

        let mut noise = NoiseState::new(0);
        noise.offset_deg(&geometry, &model, point, 0.7, dt_s);
        let bias_a = (noise.bias_x_deg, noise.bias_y_deg);

        // ~7% change: under the 20% re-sigma threshold.
        noise.offset_deg(&geometry, &model, point, 0.75, dt_s);
        let bias_b = (noise.bias_x_deg, noise.bias_y_deg);

        assert_eq!(bias_a, bias_b);
    }
}
