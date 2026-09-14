//! Desk geometry: where each output physically is, what shape it is, and how to move
//! between compositor pixels, points on the panels, and gaze rays from the eye.
//!
//! World frame: origin at the tracker (top edge of the small panel, at the seam), +X to
//! the user's right, +Y up, +Z toward the user. Millimetres throughout.
//!
//! Each output has a local frame at the centre of its visible area: +X along the chord
//! tangent to the right, +Y up, +Z toward the user (the panel normal at its centre).
//! A curved panel is a vertical cylinder section of arc length `physical_w_mm` and radius
//! `radius_mm`, concave toward the user, so its local surface is
//! `x = R sin(phi)`, `z = R (1 - cos(phi))`, `phi = (u - 0.5) * physical_w_mm / R` for
//! `u` in [0, 1] across the width, and `y = (0.5 - v) * physical_h_mm` for `v` in [0, 1]
//! down the height. A flat panel is the `R -> infinity` limit: `x = (u - 0.5) * W`,
//! `z = 0`. Local-to-world is `world = position + R_yaw * R_pitch * R_roll * local` with
//! yaw about +Y, pitch about +X, roll about +Z, right-hand rule, applied in that order.
//!
//! Pixel mapping: an output covers logical rect `(logical_x, logical_y, logical_w,
//! logical_h)` in global pixels; `u = (px.x - logical_x) / logical_w`, likewise `v`.

use glam::{DQuat, DVec3};
use serde::{Deserialize, Serialize};

use crate::types::{GlobalPx, Ray};

/// Slack allowed on the `[0, 1]` surface bounds when accepting an intersection. Purely a
/// floating-point guard so a ray aimed exactly at an edge is not rejected by a rounding
/// error in the last bit; far too small to matter physically (1e-9 of a panel width is
/// under a nanometre).
const UV_EPS: f64 = 1e-9;

/// Pixel offset used for the central differences in `px_per_deg`. Small enough that the
/// panel is linear over the interval, large enough that the angle difference is far above
/// `f64` noise.
const JACOBIAN_STEP_PX: f64 = 2.0;

/// How far past an output's edge a neighbouring output may start and still count as
/// beyond that edge, logical pixels. Compositors place adjacent outputs edge to edge, but
/// a layout arranged by hand can leave a pixel or two of gap or overlap.
const EDGE_SLACK_PX: f64 = 4.0;

/// One physical output and its placement. Loaded from `config/desk.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputGeometry {
    /// Connector name as reported by `wl_output`.
    pub name          : String,
    /// False for an output that is configured but currently absent from the compositor.
    #[serde(default = "default_true")]
    pub enabled       : bool,
    /// Whether the screen recogniser watches this output. False for a panel the tracker
    /// never reaches: capturing and detecting it costs a core for nothing, and a terminal
    /// scrolling a log there re-triggers detection every half second.
    #[serde(default = "default_true")]
    pub detect        : bool,
    pub logical_x     : f64,
    pub logical_y     : f64,
    pub logical_w     : f64,
    pub logical_h     : f64,
    /// Visible arc width (curved) or width (flat).
    pub physical_w_mm : f64,
    pub physical_h_mm : f64,
    /// Cylinder radius; `0.0` means flat.
    #[serde(default)]
    pub radius_mm     : f64,
    /// World position of the panel's visible-area centre. Only the tracker's own
    /// display needs a real one; an output the tracker never reaches may leave it out.
    #[serde(default)]
    pub position_mm   : [f64; 3],
    #[serde(default)]
    pub yaw_deg       : f64,
    #[serde(default)]
    pub pitch_deg     : f64,
    #[serde(default)]
    pub roll_deg      : f64,
}

/// The whole desk: outputs plus the nominal eye and tracker placement. The eye is what
/// the snap engine's angular scale and the ceremony's training rectangle are measured
/// from; the tracker's own frames carry the real eye.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesktopGeometry {
    /// Nominal eye position (midpoint between the eyes) for a seated user.
    pub eye_mm     : [f64; 3],
    /// Tracker position; the world origin by convention but configurable so the config
    /// stays honest if the tracker moves.
    #[serde(default)]
    pub tracker_mm : [f64; 3],
    pub outputs    : Vec<OutputGeometry>,
}

/// Result of intersecting a ray with the desk: which output, the world point, and the
/// global pixel it maps to.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceHit {
    pub output   : String,
    pub world_mm : DVec3,
    pub px       : GlobalPx,
    /// Distance from the ray origin to the hit, millimetres.
    pub t_mm     : f64,
}

// --- DesktopGeometry ---

impl DesktopGeometry {
    /// Parses a `desk.toml` document.
    pub fn from_toml(text: &str) -> Result<Self, GeometryError> {
        toml::from_str(text).map_err(|e| GeometryError::Parse(e.to_string()))
    }

    /// Nominal eye position as a vector.
    pub fn eye(&self) -> DVec3 {
        DVec3::from_array(self.eye_mm)
    }

    /// Tracker position as a vector.
    pub fn tracker(&self) -> DVec3 {
        DVec3::from_array(self.tracker_mm)
    }

    /// Output containing `p`, if any enabled output's logical rect does.
    pub fn output_at(&self, p: GlobalPx) -> Option<&OutputGeometry> {
        self.outputs.iter().find(|o| o.enabled && o.contains_px(p))
    }

    /// The output that claims a point past `from`'s edge, if one does. `p` is where a
    /// ray landed on `from`'s surface extended past its edges (see
    /// [`OutputGeometry::project_px`]); a point still inside `from` is nobody else's.
    /// Another output claims it when its logical rect contains it, or when it sits
    /// beyond the edge `p` left through and spans `p` along that edge: a display
    /// arranged below another owns everything under that edge across its width, however
    /// far down, because the eyes there are on it, not past a bezel.
    ///
    /// Disabled outputs count. A display the tracker's calibration does not cover is
    /// disabled for ray intersection so it cannot steal hits, but it is still on the desk,
    /// and a gaze on it is not a gaze past the calibrated one's edge.
    pub fn output_beyond(&self, from: &str, p: GlobalPx) -> Option<&OutputGeometry> {
        let a = self.outputs.iter().find(|o| o.name == from)?;

        if a.contains_px(p) {
            return None;
        }

        let a_right  = a.logical_x + a.logical_w;
        let a_bottom = a.logical_y + a.logical_h;

        self.outputs.iter().filter(|b| b.name != from).find(|b| {
            if b.contains_px(p) {
                return true;
            }

            let b_right  = b.logical_x + b.logical_w;
            let b_bottom = b.logical_y + b.logical_h;
            let spans_x  = p.x >= b.logical_x && p.x < b_right;
            let spans_y  = p.y >= b.logical_y && p.y < b_bottom;

            // Only one edge can be the one `p` left through on each axis; the other
            // output has to lie on that side of `a` and cover `p` along the edge.
            let below = p.y >= a_bottom    && b.logical_y >= a_bottom - EDGE_SLACK_PX && spans_x;
            let above = p.y <  a.logical_y && b_bottom    <= a.logical_y + EDGE_SLACK_PX && spans_x;
            let right = p.x >= a_right     && b.logical_x >= a_right - EDGE_SLACK_PX && spans_y;
            let left  = p.x <  a.logical_x && b_right     <= a.logical_x + EDGE_SLACK_PX && spans_y;

            below || above || right || left
        })
    }

    /// World point on the panel surface under a global pixel. `None` if no enabled output
    /// covers it.
    pub fn px_to_world(&self, p: GlobalPx) -> Option<DVec3> {
        let out = self.output_at(p)?;

        Some(out.px_to_world(p))
    }

    /// Nearest forward intersection of `ray` with any enabled output surface.
    pub fn intersect(&self, ray: &Ray) -> Option<SurfaceHit> {
        let mut best: Option<SurfaceHit> = None;

        // Panels are few (three here) and may overlap in angle from the eye, so the only
        // correct answer is the smallest positive `t` over all of them.
        for out in self.outputs.iter().filter(|o| o.enabled) {
            let Some((t_mm, u, v)) = out.intersect(ray) else {
                continue;
            };

            if best.as_ref().is_some_and(|b| b.t_mm <= t_mm) {
                continue;
            }

            best = Some(SurfaceHit {
                output   : out.name.clone(),
                world_mm : ray.origin + ray.dir * t_mm,
                px       : out.uv_to_px(u, v),
                t_mm     : t_mm,
            });
        }

        best
    }

    /// Ray from the nominal eye through the panel point under `p`. The inverse of
    /// `intersect` for on-screen points, used to lift 2D-only providers into ray space.
    pub fn px_to_ray(&self, p: GlobalPx) -> Option<Ray> {
        let target = self.px_to_world(p)?;
        let origin = self.eye();

        Some(Ray { origin: origin, dir: (target - origin).normalize() })
    }

    /// Angle in degrees between a gaze ray and the tracker axis, which is the direction
    /// from the tracker to the eye. Zero means the user is looking straight into the
    /// tracker, which is where a PCCR device is most accurate.
    ///
    /// The gaze ray points from the eye out toward a panel, so it is reversed before
    /// comparing: `angle(-dir, eye - tracker)`.
    pub fn off_axis_deg(&self, ray: &Ray) -> f64 {
        let axis = self.eye() - self.tracker();

        if axis.length_squared() <= 0.0 {
            return 0.0;
        }

        (-ray.dir).angle_between(axis).to_degrees()
    }

    /// Visual angle in degrees between two global pixels as seen from `eye`.
    pub fn angle_between_deg(&self, eye: DVec3, a: GlobalPx, b: GlobalPx) -> Option<f64> {
        let wa = self.px_to_world(a)?;
        let wb = self.px_to_world(b)?;

        Some((wa - eye).angle_between(wb - eye).to_degrees())
    }

    /// Local scale at `p`: logical pixels per degree of visual angle along the panel's
    /// horizontal and vertical directions, as seen from `eye`. Lets pixel-space code use a
    /// snap radius specified in degrees.
    pub fn px_per_deg(&self, eye: DVec3, p: GlobalPx) -> Option<(f64, f64)> {
        // Central differences on the output owning `p`, not on the desk as a whole: the
        // offset samples may fall a couple of pixels outside the logical rect near an
        // edge, and extrapolating this panel is what we want there, not hopping to the
        // neighbouring one.
        let out  = self.output_at(p)?;
        let step = JACOBIAN_STEP_PX;

        let angle_deg = |a: GlobalPx, b: GlobalPx| {
            let va = out.px_to_world(a) - eye;
            let vb = out.px_to_world(b) - eye;

            va.angle_between(vb).to_degrees()
        };

        let h_deg = angle_deg(
            GlobalPx { x: p.x - step, y: p.y },
            GlobalPx { x: p.x + step, y: p.y },
        );
        let v_deg = angle_deg(
            GlobalPx { x: p.x, y: p.y - step },
            GlobalPx { x: p.x, y: p.y + step },
        );

        // A degenerate or coincident-with-the-eye panel gives a zero or NaN angle; there
        // is no meaningful scale to report there.
        if !h_deg.is_finite() || !v_deg.is_finite() || h_deg <= 0.0 || v_deg <= 0.0 {
            return None;
        }

        Some((2.0 * step / h_deg, 2.0 * step / v_deg))
    }

    /// Rotates `ray` by `dx_deg` about the world up axis and `dy_deg` about the ray's own
    /// horizontal axis, for injecting angular error.
    ///
    /// Both are real rotations, so the direction stays unit length and the perturbation is
    /// exact rather than a small-angle approximation. Positive `dx_deg` swings the ray to
    /// the user's left (right-hand rule about +Y); positive `dy_deg` tilts it up.
    pub fn perturb_ray(ray: &Ray, dx_deg: f64, dy_deg: f64) -> Ray {
        let up    = DVec3::Y;
        let yawed = DQuat::from_axis_angle(up, dx_deg.to_radians()) * ray.dir;

        // Take the horizontal axis after the yaw so the two rotations compose into one
        // orientation change instead of two independent nudges of the original direction.
        let horiz = yawed.cross(up);
        let dir   = {
            if horiz.length_squared() > 1e-18 {
                DQuat::from_axis_angle(horiz.normalize(), dy_deg.to_radians()) * yawed
            }
            else {
                // Looking straight up or down: there is no distinguished horizontal axis,
                // so the vertical rotation is a no-op rather than an arbitrary choice.
                yawed
            }
        };

        Ray { origin: ray.origin, dir: dir.normalize() }
    }
}

// --- OutputGeometry ---

impl OutputGeometry {
    /// True when `p` is inside this output's logical rect.
    pub fn contains_px(&self, p: GlobalPx) -> bool {
        p.x >= self.logical_x
            && p.x < self.logical_x + self.logical_w
            && p.y >= self.logical_y
            && p.y < self.logical_y + self.logical_h
    }

    /// Normalised `(u, v)` in [0, 1] for a global pixel on this output.
    pub fn px_to_uv(&self, p: GlobalPx) -> (f64, f64) {
        let u = (p.x - self.logical_x) / self.logical_w;
        let v = (p.y - self.logical_y) / self.logical_h;

        (u, v)
    }

    /// Global pixel for normalised `(u, v)`.
    pub fn uv_to_px(&self, u: f64, v: f64) -> GlobalPx {
        GlobalPx {
            x : self.logical_x + u * self.logical_w,
            y : self.logical_y + v * self.logical_h,
        }
    }

    /// World point on the panel surface for a global pixel. Caller guarantees `p` is on
    /// this output; out-of-range pixels extrapolate the surface.
    pub fn px_to_world(&self, p: GlobalPx) -> DVec3 {
        let (u, v) = self.px_to_uv(p);

        self.uv_to_world(u, v)
    }

    /// World point for normalised surface coordinates (see module docs for the surface
    /// parameterisation).
    pub fn uv_to_world(&self, u: f64, v: f64) -> DVec3 {
        DVec3::from_array(self.position_mm) + self.rotation() * self.uv_to_local(u, v)
    }

    /// Outward surface normal at `(u, v)`, in world space. Points toward the user, so it
    /// is the side a gaze ray arrives from.
    pub fn normal_at(&self, u: f64, _v: f64) -> DVec3 {
        let local = {
            if self.is_curved() {
                let phi = (u - 0.5) * self.physical_w_mm / self.radius_mm;

                // The centre of curvature sits at local (0, y, R), on the user's side of
                // the panel, so the user-facing normal points from the surface to the axis.
                DVec3::new(-phi.sin(), 0.0, phi.cos())
            }
            else {
                DVec3::Z
            }
        };

        self.rotation() * local
    }

    /// Where `ray` meets this panel's surface *extended past its edges*, in this output's
    /// pixel space, so a point past the bottom edge has `y` beyond `logical_h`. For a gaze
    /// that is tracked but off every panel: which edge it left through, and how far. `None`
    /// when the ray never reaches the surface (parallel to a flat panel, or missing a curved
    /// one's cylinder altogether). The nearest forward root is taken, and for a curved
    /// panel the one closer to the visible arc when both are ahead.
    pub fn project_px(&self, ray: &Ray) -> Option<GlobalPx> {
        let rot    = self.rotation();
        let inv    = rot.conjugate();
        let origin = inv * (ray.origin - DVec3::from_array(self.position_mm));
        let dir    = inv * ray.dir;

        let (t0, t1) = {
            if self.is_curved() {
                self.local_cylinder_roots(origin, dir)?
            }
            else {
                (self.local_plane_root(origin, dir)?, f64::INFINITY)
            }
        };

        let candidates = [t0, t1]
            .into_iter()
            .filter(|t| t.is_finite() && *t > 0.0)
            .map(|t| self.local_to_uv(origin + dir * t));

        // Distance of (u, v) from the unit square, zero inside it.
        let outside = |(u, v): &(f64, f64)| {
            (u.clamp(0.0, 1.0) - u).hypot(v.clamp(0.0, 1.0) - v)
        };

        let (u, v) = candidates.min_by(|a, b| outside(a).total_cmp(&outside(b)))?;

        Some(self.uv_to_px(u, v))
    }

    /// Nearest forward intersection of `ray` with this panel, as `(t_mm, u, v)`, or `None`
    /// when the ray misses the visible area.
    pub fn intersect(&self, ray: &Ray) -> Option<(f64, f64, f64)> {
        let rot    = self.rotation();
        let inv    = rot.conjugate();
        let origin = inv * (ray.origin - DVec3::from_array(self.position_mm));
        let dir    = inv * ray.dir;

        let (t0, t1) = {
            if self.is_curved() {
                self.local_cylinder_roots(origin, dir)?
            }
            else {
                let t = self.local_plane_root(origin, dir)?;

                (t, f64::INFINITY)
            }
        };

        // Try the roots in order and take the first that lands on the visible section.
        // For a concave panel the far root is the back of the cylinder, whose `phi` is
        // well outside the arc, so this also drops it without a separate side test.
        for t in [t0, t1] {
            if !t.is_finite() || t <= 0.0 {
                continue;
            }

            let (u, v) = self.local_to_uv(origin + dir * t);

            if (-UV_EPS..=1.0 + UV_EPS).contains(&u) && (-UV_EPS..=1.0 + UV_EPS).contains(&v) {
                return Some((t, u, v));
            }
        }

        None
    }
}

impl OutputGeometry {
    /// Local-to-world rotation, `R_yaw * R_pitch * R_roll` as documented at the top of the
    /// module. Recomputed per call; the trig is cheap next to everything else in the loop.
    fn rotation(&self) -> DQuat {
        rotation_ypr(self.yaw_deg, self.pitch_deg, self.roll_deg)
    }

    /// True when the panel is modelled as a cylinder section rather than a plane.
    fn is_curved(&self) -> bool {
        self.radius_mm > 0.0
    }

    /// Local-frame surface point for normalised coordinates.
    fn uv_to_local(&self, u: f64, v: f64) -> DVec3 {
        let y = (0.5 - v) * self.physical_h_mm;

        if self.is_curved() {
            let phi = (u - 0.5) * self.physical_w_mm / self.radius_mm;

            DVec3::new(
                self.radius_mm * phi.sin(),
                y,
                self.radius_mm * (1.0 - phi.cos()),
            )
        }
        else {
            DVec3::new((u - 0.5) * self.physical_w_mm, y, 0.0)
        }
    }

    /// Normalised coordinates for a local-frame point assumed to lie on the surface.
    /// Inverts `uv_to_local`; points off the surface are projected onto it radially
    /// (curved) or along +Z (flat).
    fn local_to_uv(&self, p: DVec3) -> (f64, f64) {
        let v = 0.5 - p.y / self.physical_h_mm;
        let u = {
            if self.is_curved() {
                // `x = R sin(phi)` and `R - z = R cos(phi)`, so atan2 recovers phi over
                // the whole circle, giving |phi| > 90 degrees on the far side.
                let phi = p.x.atan2(self.radius_mm - p.z);

                0.5 + phi * self.radius_mm / self.physical_w_mm
            }
            else {
                0.5 + p.x / self.physical_w_mm
            }
        };

        (u, v)
    }

    /// Both roots of the ray against the full cylinder `x^2 + (z - R)^2 = R^2`, in
    /// increasing order, in local coordinates. `None` when the ray misses the cylinder or
    /// runs parallel to its axis.
    fn local_cylinder_roots(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        let r  = self.radius_mm;
        let oz = origin.z - r;

        let a = dir.x * dir.x + dir.z * dir.z;
        let b = 2.0 * (origin.x * dir.x + oz * dir.z);
        let c = origin.x * origin.x + oz * oz - r * r;

        // A ray parallel to the cylinder axis either never crosses the surface or lies on
        // it; either way there is no isolated hit to report.
        if a <= 0.0 {
            return None;
        }

        let disc = b * b - 4.0 * a * c;

        if disc < 0.0 {
            return None;
        }

        let sq = disc.sqrt();

        Some(((-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a)))
    }

    /// Root of the ray against the local plane `z = 0`. `None` when the ray is parallel to
    /// the panel.
    fn local_plane_root(&self, origin: DVec3, dir: DVec3) -> Option<f64> {
        if dir.z == 0.0 {
            return None;
        }

        Some(-origin.z / dir.z)
    }
}

/// The workspace's one yaw/pitch/roll convention, as a rotation: `R_yaw * R_pitch *
/// R_roll` with yaw about +Y, pitch about +X, roll about +Z, right-hand rule, applied in
/// that order. This is what `OutputGeometry` uses for its panels and what any other pose
/// in `desk.toml` (the camera block, for one) must use so the angles in the config all
/// mean the same thing.
pub fn rotation_ypr(yaw_deg: f64, pitch_deg: f64, roll_deg: f64) -> DQuat {
    DQuat::from_rotation_y(yaw_deg.to_radians())
        * DQuat::from_rotation_x(pitch_deg.to_radians())
        * DQuat::from_rotation_z(roll_deg.to_radians())
}

fn default_true() -> bool {
    true
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum GeometryError {
    #[error("desk config parse error: {0}")]
    Parse(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The real desk, so the round-trips are asserted against the numbers the rest of the
    /// prototype actually runs on rather than a tidy synthetic layout.
    /// A frozen snapshot of the 2026-08-25 desk config. The live `config/desk.toml`
    /// tracks the physical desk and changes with every remount or re-measure, so the
    /// geometry assertions pin the coherent snapshot in `config/desk-fixture.toml`
    /// instead; the live file gets a parse-only smoke test.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    /// The live config, which must always parse even as its values drift.
    const DESK_TOML: &str = include_str!("../../../config/desk.toml");

    /// Round-trip tolerance from PLAN.md. The map is analytic in both directions, so this
    /// is really a check that no branch of the cylinder inverse picks the wrong root.
    const ROUND_TRIP_PX: f64 = 0.01;

    /// Loads the frozen fixture config, panicking if the parser has drifted.
    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(FIXTURE_TOML).expect("fixture config must parse")
    }

    /// A flat test panel facing +Z at `z_mm`, one metre square, mapped to a 1000x1000
    /// logical rect at `logical_x`.
    fn test_panel(name: &str, z_mm: f64, logical_x: f64) -> OutputGeometry {
        OutputGeometry {
            name          : name.to_string(),
            enabled       : true,
            detect        : true,
            logical_x     : logical_x,
            logical_y     : 0.0,
            logical_w     : 1000.0,
            logical_h     : 1000.0,
            physical_w_mm : 1000.0,
            physical_h_mm : 1000.0,
            radius_mm     : 0.0,
            position_mm   : [0.0, 0.0, z_mm],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        }
    }

    #[test]
    fn live_desk_config_parses() {
        let g = DesktopGeometry::from_toml(DESK_TOML).expect("config/desk.toml must parse");

        assert!(!g.outputs.is_empty());
    }

    #[test]
    fn fixture_config_parses_with_expected_outputs() {
        let g = desk();

        assert_eq!(g.outputs.len(), 3);
        assert_eq!(g.eye(), DVec3::new(0.0, 180.0, 650.0));
        assert_eq!(g.tracker(), DVec3::ZERO);

        let names: Vec<&str> = g.outputs.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["DP-1", "DP-2", "HDMI-A-1"]);

        // Two curved panels and one flat one; the flat branch has to be exercised by the
        // real config, not only by synthetic panels.
        assert!(g.outputs[0].radius_mm > 0.0);
        assert!(g.outputs[1].radius_mm > 0.0);
        assert_eq!(g.outputs[2].radius_mm, 0.0);
    }

    #[test]
    fn px_round_trips_through_world_and_ray_on_every_output() {
        let g = desk();

        // A grid that includes points 2% in from each corner, the edges and the centre.
        let fracs = [0.002, 0.02, 0.25, 0.5, 0.75, 0.98, 0.998];

        for out in &g.outputs {
            for u in fracs {
                for v in fracs {
                    let p   = out.uv_to_px(u, v);
                    let ray = g.px_to_ray(p).expect("grid point is on an output");
                    let hit = g.intersect(&ray).expect("a ray aimed at a panel must hit one");

                    assert_eq!(
                        hit.output, out.name,
                        "{} uv({u},{v}) landed on {}",
                        out.name, hit.output,
                    );
                    assert!(
                        (hit.px.x - p.x).abs() < ROUND_TRIP_PX
                            && (hit.px.y - p.y).abs() < ROUND_TRIP_PX,
                        "{} uv({u},{v}): {:?} != {:?}",
                        out.name, hit.px, p,
                    );

                    // The hit is in front of the eye and on the surface we asked for.
                    assert!(hit.t_mm > 0.0);
                    assert!((hit.world_mm - out.uv_to_world(u, v)).length() < 0.01);
                }
            }
        }
    }

    #[test]
    fn px_round_trips_across_the_seam() {
        let g = desk();

        // DP-2 spans x in [0, 2560) and DP-1 starts at 2559, so the compositor layout
        // overlaps by one pixel; `output_at` resolves it in config order (DP-1 first).
        // Round-tripping either side of the boundary must stay stable regardless.
        let seam = [
            GlobalPx { x: 2555.0, y: 800.0 },
            GlobalPx { x: 2558.0, y: 800.0 },
            GlobalPx { x: 2559.0, y: 800.0 },
            GlobalPx { x: 2560.0, y: 800.0 },
            GlobalPx { x: 2559.0, y: 200.0 },
            GlobalPx { x: 2559.0, y: 1590.0 },
        ];

        for p in seam {
            let want = g.output_at(p).expect("seam pixels are on an output").name.clone();
            let ray  = g.px_to_ray(p).unwrap();
            let hit  = g.intersect(&ray).expect("seam ray must hit a panel");

            assert_eq!(hit.output, want, "seam pixel {p:?} switched output");
            assert!(
                (hit.px.x - p.x).abs() < ROUND_TRIP_PX
                    && (hit.px.y - p.y).abs() < ROUND_TRIP_PX,
                "seam pixel {p:?} round-tripped to {:?}",
                hit.px,
            );
        }
    }

    #[test]
    fn flat_panel_is_the_large_radius_limit_of_the_cylinder() {
        let flat = test_panel("flat", -500.0, 0.0);
        let mut curved = flat.clone();
        curved.name      = "curved".to_string();
        curved.radius_mm = 1.0e7;

        for u in [0.0, 0.1, 0.5, 0.9, 1.0] {
            for v in [0.0, 0.5, 1.0] {
                let a = flat.uv_to_world(u, v);
                let b = curved.uv_to_world(u, v);

                // Sagitta of a 1 m chord on a 10 km radius is ~12.5 micrometres.
                assert!((a - b).length() < 0.03, "uv({u},{v}): {a:?} vs {b:?}");
                assert!((flat.normal_at(u, v) - curved.normal_at(u, v)).length() < 1.0e-3);
            }
        }

        // The intersection routines take completely different branches, so check they
        // agree too, not just the forward parameterisation.
        let ray = Ray {
            origin : DVec3::new(120.0, -80.0, 400.0),
            dir    : DVec3::new(-0.2, 0.15, -1.0).normalize(),
        };
        let (ta, ua, va) = flat.intersect(&ray).expect("flat panel is hit");
        let (tb, ub, vb) = curved.intersect(&ray).expect("near-flat cylinder is hit");

        assert!((ta - tb).abs() < 0.05, "t: {ta} vs {tb}");
        assert!((ua - ub).abs() < 1.0e-4 && (va - vb).abs() < 1.0e-4);
    }

    #[test]
    fn px_per_deg_at_the_lg_seam_is_in_the_expected_band() {
        let g   = desk();
        let eye = g.eye();

        // Left edge of the LG, vertically centred: the seam, where the tracker sits and
        // where most viewing happens.
        let (h, v) = g.px_per_deg(eye, GlobalPx { x: 2559.0, y: 800.0 }).unwrap();

        assert!((55.0..65.0).contains(&h), "horizontal px/deg at seam = {h}");
        assert!((55.0..65.0).contains(&v), "vertical px/deg at seam = {v}");

        // Curvature plus obliquity stretch the far end of the panel: the same degree of
        // visual angle buys noticeably more pixels there, which is the whole reason snap
        // radii are specified in degrees.
        let far = GlobalPx { x: 6398.0, y: 800.0 };
        let (fh, fv) = g.px_per_deg(eye, far).unwrap();

        assert!(fh > h * 1.3, "far right horizontal px/deg = {fh}, seam = {h}");
        assert!(fv > v * 1.2, "far right vertical px/deg = {fv}, seam = {v}");
        assert!((85.0..105.0).contains(&fh), "far right horizontal px/deg = {fh}");
        assert!((70.0..95.0).contains(&fv), "far right vertical px/deg = {fv}");
    }

    #[test]
    fn px_per_deg_agrees_with_a_wide_angle_measurement() {
        let g   = desk();
        let eye = g.eye();

        // 100 px on either side of a point in the middle of the LG, well away from the
        // edges so the local scale is close to constant over the span.
        let mid   = GlobalPx { x: 4479.0, y: 800.0 };
        let left  = GlobalPx { x: mid.x - 100.0, y: mid.y };
        let right = GlobalPx { x: mid.x + 100.0, y: mid.y };

        let (h, _)  = g.px_per_deg(eye, mid).unwrap();
        let wide    = g.angle_between_deg(eye, left, right).unwrap();
        let implied = 200.0 / wide;

        assert!((h - implied).abs() / h < 0.02, "local {h} vs wide-span {implied}");
    }

    #[test]
    fn px_per_deg_is_none_off_screen() {
        let g = desk();

        assert!(g.px_per_deg(g.eye(), GlobalPx { x: -50.0, y: -50.0 }).is_none());
    }

    #[test]
    fn intersect_picks_the_nearest_of_two_panels_on_the_same_ray() {
        // Two parallel flat panels, both facing the user, the far one 200 mm behind the
        // near one. Their logical rects are disjoint so the answer is unambiguous.
        let g = DesktopGeometry {
            eye_mm     : [0.0, 0.0, 500.0],
            tracker_mm : [0.0, 0.0, 0.0],
            outputs    : vec![
                test_panel("far",  -200.0, 2000.0),
                test_panel("near",    0.0,    0.0),
            ],
        };

        let ray = Ray { origin: DVec3::new(0.0, 0.0, 500.0), dir: -DVec3::Z };
        let hit = g.intersect(&ray).expect("the ray crosses both panels");

        assert_eq!(hit.output, "near");
        assert!((hit.t_mm - 500.0).abs() < 1.0e-9);

        // Order in the config must not matter: reverse it and the answer is the same.
        let mut reversed = g.clone();
        reversed.outputs.reverse();

        assert_eq!(reversed.intersect(&ray).unwrap().output, "near");

        // Disabling the near panel falls through to the far one rather than missing.
        let mut disabled = g.clone();
        disabled.outputs[1].enabled = false;

        let hit = disabled.intersect(&ray).expect("far panel still there");
        assert_eq!(hit.output, "far");
        assert!((hit.t_mm - 700.0).abs() < 1.0e-9);
    }

    #[test]
    fn project_px_extends_the_panel_past_its_bottom_edge() {
        let g   = desk();
        let lg  = g.outputs.iter().find(|o| o.name == "DP-1").expect("the LG");
        let eye = g.eye();

        // A pixel 300 px below the LG's bottom edge, lifted to a ray the way a provider
        // would, misses the visible panel but projects back to where it was aimed.
        let below = GlobalPx { x: lg.logical_x + 1000.0, y: lg.logical_y + lg.logical_h + 300.0 };
        let world = lg.px_to_world(below);
        let ray   = Ray { origin: eye, dir: (world - eye).normalize() };

        assert!(g.intersect(&ray).is_none(), "300 px below the panel must miss it");

        let back = lg.project_px(&ray).expect("the extended surface is still ahead");

        assert!((back.x - below.x).abs() < 1.0, "x {} vs {}", back.x, below.x);
        assert!((back.y - below.y).abs() < 1.0, "y {} vs {}", back.y, below.y);
    }

    #[test]
    fn intersect_returns_none_when_the_ray_misses_everything() {
        let g   = desk();
        let eye = g.eye();

        // Straight up at the ceiling, straight back over the user's shoulder, and sideways
        // past the left panel: none of these cross a panel in front of the eye.
        let misses = [DVec3::Y, DVec3::Z, DVec3::new(-1.0, 0.6, 0.2).normalize()];

        for dir in misses {
            let ray = Ray { origin: eye, dir: dir };

            assert!(g.intersect(&ray).is_none(), "dir {dir:?} should miss");
        }

        // A ray pointing away from a panel it would otherwise hit must not report the
        // backward intersection.
        let toward = g.px_to_ray(GlobalPx { x: 4479.0, y: 800.0 }).unwrap();
        let away   = Ray { origin: toward.origin, dir: -toward.dir };

        assert!(g.intersect(&away).is_none(), "backward ray must not hit");
    }

    #[test]
    fn perturb_ray_rotates_by_the_requested_angle() {
        let dir = DVec3::new(0.0, 0.0, -1.0);
        let ray = Ray { origin: DVec3::new(0.0, 180.0, 650.0), dir: dir };

        // Pure yaw about world +Y: the direction stays in the XZ plane and turns by
        // exactly the requested angle because it started perpendicular to the axis.
        let yawed = DesktopGeometry::perturb_ray(&ray, 3.0, 0.0);

        assert!((yawed.dir.length() - 1.0).abs() < 1.0e-12);
        assert!((yawed.dir.angle_between(dir).to_degrees() - 3.0).abs() < 1.0e-9);
        assert!(yawed.dir.y.abs() < 1.0e-12);
        assert!(yawed.dir.x < 0.0, "positive dx_deg swings to the user's left");
        assert_eq!(yawed.origin, ray.origin);

        // Pure pitch about the ray's own horizontal axis, which is perpendicular to the
        // direction by construction, so the angle is exact for any magnitude.
        let pitched = DesktopGeometry::perturb_ray(&ray, 0.0, 2.5);

        assert!((pitched.dir.angle_between(dir).to_degrees() - 2.5).abs() < 1.0e-9);
        assert!(pitched.dir.y > 0.0, "positive dy_deg tilts up");
        assert!(pitched.dir.x.abs() < 1.0e-12);

        // Zero perturbation is the identity, and the combined rotation stays unit length
        // and bounded by the sum of the two angles.
        let same = DesktopGeometry::perturb_ray(&ray, 0.0, 0.0);
        assert!((same.dir - dir).length() < 1.0e-12);

        let both = DesktopGeometry::perturb_ray(&ray, 1.0, -1.5);
        assert!((both.dir.length() - 1.0).abs() < 1.0e-12);
        assert!(both.dir.angle_between(dir).to_degrees() <= 2.5 + 1.0e-9);
    }

    #[test]
    fn perturb_ray_survives_a_direction_along_the_up_axis() {
        let ray = Ray { origin: DVec3::ZERO, dir: DVec3::Y };
        let out = DesktopGeometry::perturb_ray(&ray, 5.0, 5.0);

        // No horizontal axis exists, so only the yaw applies and it is a no-op on +Y.
        assert!(out.dir.is_finite());
        assert!((out.dir.length() - 1.0).abs() < 1.0e-12);
        assert!((out.dir - DVec3::Y).length() < 1.0e-9);
    }

    #[test]
    fn an_output_below_claims_the_gaze_past_the_bottom_edge_across_its_width() {
        let g = desk();

        // The fixture's portable panel sits below the seam, under DP-2's right part
        // (x 1506..3176 at y 1600). A projected point under DP-2 in that span is on it,
        // inside its rect and far below it alike; under DP-2's left part nothing is.
        let under_right = GlobalPx { x: 2000.0, y: 1700.0 };
        let far_below   = GlobalPx { x: 2000.0, y: 4000.0 };
        let under_left  = GlobalPx { x: 500.0, y: 1700.0 };
        let inside      = GlobalPx { x: 2000.0, y: 1500.0 };

        assert_eq!(g.output_beyond("DP-2", under_right).map(|o| o.name.as_str()), Some("HDMI-A-1"));
        assert_eq!(g.output_beyond("DP-2", far_below).map(|o| o.name.as_str()), Some("HDMI-A-1"));
        assert_eq!(g.output_beyond("DP-2", under_left), None);
        assert_eq!(g.output_beyond("DP-2", inside), None);

        // Past DP-2's right edge is the LG, whatever its `enabled` flag says.
        let mut g = g;
        let right = GlobalPx { x: 2600.0, y: 800.0 };

        assert_eq!(g.output_beyond("DP-2", right).map(|o| o.name.as_str()), Some("DP-1"));

        for o in &mut g.outputs {
            o.enabled = o.name == "DP-2";
        }

        assert_eq!(g.output_beyond("DP-2", right).map(|o| o.name.as_str()), Some("DP-1"));
        assert_eq!(g.output_beyond("DP-2", under_right).map(|o| o.name.as_str()), Some("HDMI-A-1"));
        assert_eq!(g.output_beyond("nope", right), None);
    }

    #[test]
    fn off_axis_deg_is_zero_looking_into_the_tracker() {
        let g   = desk();
        let eye = g.eye();

        let at_tracker = Ray { origin: eye, dir: (g.tracker() - eye).normalize() };
        assert!(g.off_axis_deg(&at_tracker).abs() < 1.0e-9);

        // Looking along the tracker axis away from the tracker is the fully reversed case.
        let away = Ray { origin: eye, dir: (eye - g.tracker()).normalize() };
        assert!((g.off_axis_deg(&away) - 180.0).abs() < 1.0e-6);

        // The bottom-left corner of the LG sits almost on the tracker axis, while the far
        // right end of the same panel is outside the profile's valid cone entirely.
        let near_axis = g.px_to_ray(GlobalPx { x: 2635.0, y: 1568.0 }).unwrap();
        let far_right = g.px_to_ray(GlobalPx { x: 6322.0, y: 32.0 }).unwrap();

        assert!(g.off_axis_deg(&near_axis) < 10.0);
        assert!(g.off_axis_deg(&far_right) > 45.0);
    }
}
