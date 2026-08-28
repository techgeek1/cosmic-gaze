"""Desk/tracker geometry, reproduced from the Rust sources so residuals computed here
match what the runtime would compute.

Sources (read, not imported — this is a from-scratch Python port):
  - crates/gaze-core/src/geometry.rs        (OutputGeometry / DesktopGeometry, uv<->world)
  - crates/gaze-provider-et5/src/gaze.rs    (Et5Frame, combined_ray, eye_ray)
  - crates/gaze-provider-et5/src/calibration.rs (normalise/denormalise, OutputPose)
  - crates/gaze-provider-et5/src/sweep.rs   (desk_to_sensor, plane_corners, lag/saccade
    constants)

Two frames matter here:

  - "desk frame": world frame of config/desk.toml. Origin at the tracker, +X right,
    +Y up, +Z toward the user. `OutputGeometry.position_mm` and `DesktopGeometry.eye_mm`
    live here.
  - "sensor frame": what the ET5 firmware actually reports (`Et5Frame.eye_origin_*_mm`,
    `gaze_3d_*_mm`). The mount wedge pitches the sensor up at the face by
    `tracker_pitch_deg`, so sensor frame is desk frame rotated by that pitch about the
    shared +X axis. `desk_to_sensor` / `sensor_to_desk` convert between them (mirrors
    `sweep.rs::desk_to_sensor` exactly, X untouched, Y/Z rotated).

Any quantity compared against firmware fields (origins, gaze_3d, targets) must be in
sensor frame; this module keeps that conversion explicit at every call site rather than
folding it into a single global frame, the same way the Rust code does.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np
import tomllib


# --- Rotation helpers -------------------------------------------------------------
#
# Matches gaze_core::geometry::rotation_ypr: R = Ry(yaw) @ Rx(pitch) @ Rz(roll), yaw
# about +Y, pitch about +X, roll about +Z, right-hand rule, applied local-to-world in
# that order (`world = position + R @ local`).


def _rx(deg: float) -> np.ndarray:
    t = np.radians(deg)
    c, s = np.cos(t), np.sin(t)
    return np.array([[1, 0, 0], [0, c, -s], [0, s, c]])


def _ry(deg: float) -> np.ndarray:
    t = np.radians(deg)
    c, s = np.cos(t), np.sin(t)
    return np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]])


def _rz(deg: float) -> np.ndarray:
    t = np.radians(deg)
    c, s = np.cos(t), np.sin(t)
    return np.array([[c, -s, 0], [s, c, 0], [0, 0, 1]])


def rotation_ypr(yaw_deg: float, pitch_deg: float, roll_deg: float) -> np.ndarray:
    """The one yaw/pitch/roll convention shared by every pose in the workspace."""
    return _ry(yaw_deg) @ _rx(pitch_deg) @ _rz(roll_deg)


def desk_to_sensor(p: np.ndarray, tracker_pitch_deg: float) -> np.ndarray:
    """Rotates a desk-frame point (or array of points, last axis = xyz) into sensor
    frame: a rotation about +X by the mount pitch. Mirrors `sweep.rs::desk_to_sensor`
    verbatim (X untouched; Y/Z get `y*cos - z*sin`, `y*sin + z*cos`)."""
    t = np.radians(tracker_pitch_deg)
    c, s = np.cos(t), np.sin(t)
    p = np.asarray(p, dtype=float)
    x = p[..., 0]
    y = p[..., 1] * c - p[..., 2] * s
    z = p[..., 1] * s + p[..., 2] * c
    return np.stack([x, y, z], axis=-1)


def sensor_to_desk(p: np.ndarray, tracker_pitch_deg: float) -> np.ndarray:
    """Inverse of `desk_to_sensor`."""
    return desk_to_sensor(p, -tracker_pitch_deg)


# --- Output / desktop geometry -----------------------------------------------------


@dataclass
class OutputGeometry:
    """One physical display, desk frame. Mirrors `gaze_core::geometry::OutputGeometry`."""

    name: str
    logical_x: float
    logical_y: float
    logical_w: float
    logical_h: float
    physical_w_mm: float
    physical_h_mm: float
    radius_mm: float
    position_mm: np.ndarray
    yaw_deg: float
    pitch_deg: float
    roll_deg: float

    def is_curved(self) -> bool:
        return self.radius_mm > 0.0

    def rotation(self) -> np.ndarray:
        return rotation_ypr(self.yaw_deg, self.pitch_deg, self.roll_deg)

    def px_to_uv(self, px: np.ndarray) -> np.ndarray:
        px = np.asarray(px, dtype=float)
        u = (px[..., 0] - self.logical_x) / self.logical_w
        v = (px[..., 1] - self.logical_y) / self.logical_h
        return np.stack([u, v], axis=-1)

    def uv_to_px(self, uv: np.ndarray) -> np.ndarray:
        uv = np.asarray(uv, dtype=float)
        x = self.logical_x + uv[..., 0] * self.logical_w
        y = self.logical_y + uv[..., 1] * self.logical_h
        return np.stack([x, y], axis=-1)

    def uv_to_local(self, uv: np.ndarray) -> np.ndarray:
        """Local-frame surface point (see gaze_core geometry.rs module docs): a vertical
        cylinder section concave toward the user, flat panel as the R -> infinity limit."""
        uv = np.asarray(uv, dtype=float)
        u, v = uv[..., 0], uv[..., 1]
        y = (0.5 - v) * self.physical_h_mm

        if self.is_curved():
            phi = (u - 0.5) * self.physical_w_mm / self.radius_mm
            x = self.radius_mm * np.sin(phi)
            z = self.radius_mm * (1.0 - np.cos(phi))
        else:
            x = (u - 0.5) * self.physical_w_mm
            z = np.zeros_like(u)

        return np.stack([x, y, z], axis=-1)

    def uv_to_world(self, uv: np.ndarray) -> np.ndarray:
        local = self.uv_to_local(uv)
        return self.position_mm + local @ self.rotation().T

    def px_to_world(self, px: np.ndarray) -> np.ndarray:
        return self.uv_to_world(self.px_to_uv(px))


@dataclass
class DesktopGeometry:
    """Mirrors `gaze_core::geometry::DesktopGeometry`, plus the ET5-specific
    `tracker_pitch_deg` that lives alongside it in `desk.toml`."""

    eye_mm: np.ndarray
    tracker_mm: np.ndarray
    tracker_pitch_deg: float
    outputs: dict[str, OutputGeometry]

    def axis_sensor_mm(self) -> np.ndarray:
        """The tracker axis (tracker -> nominal eye) expressed in sensor frame — the
        reference direction `off_axis_deg` measures against in `gaze_core`, converted to
        the frame the firmware actually reports positions in."""
        axis_desk = self.eye_mm - self.tracker_mm
        return desk_to_sensor(axis_desk, self.tracker_pitch_deg)

    def px_to_world_sensor(self, output_name: str, px: np.ndarray) -> np.ndarray:
        """A screen pixel's physical position, in sensor frame — the "target-to-tracker-
        space" mapping the task points at (`sweep.rs::plane_corners` does the same
        desk-to-sensor step for the three declared corners; this is the general point
        form used for every traj/stop pixel)."""
        world_desk = self.outputs[output_name].px_to_world(px)
        return desk_to_sensor(world_desk, self.tracker_pitch_deg)

    @staticmethod
    def px_per_deg(out: OutputGeometry, eye_mm: np.ndarray, px: np.ndarray) -> float:
        """Local desk-frame scale (logical px per degree of visual angle) at `px`, as
        seen from `eye_mm`. Mirrors `DesktopGeometry::px_per_deg`'s central-difference
        construction, restricted to one already-known output (the Rust version also has
        to find the output; here the caller always knows it)."""
        step = 2.0  # JACOBIAN_STEP_PX

        def angle_deg(a: np.ndarray, b: np.ndarray) -> float:
            va = out.px_to_world(a) - eye_mm
            vb = out.px_to_world(b) - eye_mm
            va = va / np.linalg.norm(va)
            vb = vb / np.linalg.norm(vb)
            cos = np.clip(np.dot(va, vb), -1.0, 1.0)
            return np.degrees(np.arccos(cos))

        h_deg = angle_deg(px + np.array([-step, 0.0]), px + np.array([step, 0.0]))
        v_deg = angle_deg(px + np.array([0.0, -step]), px + np.array([0.0, step]))

        if not (h_deg > 0) or not (v_deg > 0):
            return float("nan")

        return (2.0 * step / h_deg + 2.0 * step / v_deg) / 2.0


def load_desk_toml(path: str) -> DesktopGeometry:
    with open(path, "rb") as f:
        doc = tomllib.load(f)

    outputs = {}
    for o in doc["outputs"]:
        outputs[o["name"]] = OutputGeometry(
            name=o["name"],
            logical_x=o["logical_x"],
            logical_y=o["logical_y"],
            logical_w=o["logical_w"],
            logical_h=o["logical_h"],
            physical_w_mm=o["physical_w_mm"],
            physical_h_mm=o["physical_h_mm"],
            radius_mm=o.get("radius_mm", 0.0),
            position_mm=np.array(o["position_mm"], dtype=float),
            yaw_deg=o.get("yaw_deg", 0.0),
            pitch_deg=o.get("pitch_deg", 0.0),
            roll_deg=o.get("roll_deg", 0.0),
        )

    return DesktopGeometry(
        eye_mm=np.array(doc["eye_mm"], dtype=float),
        tracker_mm=np.array(doc.get("tracker_mm", [0.0, 0.0, 0.0]), dtype=float),
        tracker_pitch_deg=doc.get("tracker_pitch_deg", 0.0),
        outputs=outputs,
    )


# --- Angles --------------------------------------------------------------------


def angle_between_deg(a: np.ndarray, b: np.ndarray) -> float:
    a = a / np.linalg.norm(a)
    b = b / np.linalg.norm(b)
    cos = np.clip(np.dot(a, b), -1.0, 1.0)
    return np.degrees(np.arccos(cos))


def angle_from_axis_deg(dir_sensor: np.ndarray, axis_sensor: np.ndarray) -> float:
    """Angle between a gaze ray and the tracker axis, degrees. Mirrors
    `DesktopGeometry::off_axis_deg`: the ray points away from the eye, so it is
    reversed before comparing against the eye->tracker... axis (`axis` here is
    tracker->eye, so `angle(-dir, axis)` is zero when looking straight at the
    tracker, matching the Rust convention exactly)."""
    return angle_between_deg(-dir_sensor, axis_sensor)


def local_yaw_pitch_deg(dir_vec: np.ndarray, ref_dir: np.ndarray,
                         up_hint: np.ndarray = np.array([0.0, 1.0, 0.0])) -> tuple[float, float]:
    """Decomposes `dir_vec` into yaw/pitch relative to `ref_dir`: azimuth/elevation in
    the local tangent frame at `ref_dir` (right = up_hint x ref_dir, local-up = ref_dir
    x right). Zero for both axes exactly when `dir_vec` is parallel to `ref_dir`, so it
    doubles as an angle-space residual (dir_vec = measured, ref_dir = truth) and as an
    absolute direction readout (dir_vec = a gaze ray, ref_dir = the tracker axis) with
    one function. Not defined in the Rust sources (no per-axis decomposition exists
    there yet — see the report); this is this harness's choice, documented so it can be
    matched later.
    """
    ref = ref_dir / np.linalg.norm(ref_dir)
    right = np.cross(up_hint, ref)
    right_norm = np.linalg.norm(right)

    if right_norm < 1e-9:
        # ref_dir parallel to up_hint: fall back to a fixed horizontal axis so the
        # decomposition stays defined (never hit in practice — the tracker axis and
        # targets are all roughly forward, never straight up).
        right = np.array([1.0, 0.0, 0.0])
        right_norm = 1.0

    right = right / right_norm
    local_up = np.cross(ref, right)

    d = dir_vec / np.linalg.norm(dir_vec)
    forward = np.dot(d, ref)
    yaw = np.degrees(np.arctan2(np.dot(d, right), forward))
    pitch = np.degrees(np.arctan2(np.dot(d, local_up), forward))
    return float(yaw), float(pitch)


# --- Firmware ray reconstruction (mirrors gaze.rs) --------------------------------

VALIDITY_OK = 0


def eye_ray(valid: bool, origin_mm, target_mm):
    """Mirrors `gaze.rs::eye_ray`: origin/direction for one eye when tracked and
    geometrically sane (target not collapsed onto the origin)."""
    if not valid or origin_mm is None or target_mm is None:
        return None

    origin = np.array(origin_mm, dtype=float)
    target = np.array(target_mm, dtype=float)
    delta = target - origin

    if float(np.dot(delta, delta)) < 1.0:
        return None

    return origin, delta / np.linalg.norm(delta)


def combined_ray(frame: dict):
    """Mirrors `gaze.rs::combined_ray`: midpoint origin / mean direction when both eyes
    are usable, the single tracked eye otherwise, `None` if neither is. `frame` is the
    decoded `Et5Frame` dict as it appears in the readings JSONL."""
    left = eye_ray(frame.get("validity_l") == VALIDITY_OK,
                    frame.get("eye_origin_l_mm"), frame.get("gaze_3d_l_mm"))
    right = eye_ray(frame.get("validity_r") == VALIDITY_OK,
                     frame.get("eye_origin_r_mm"), frame.get("gaze_3d_r_mm"))

    if left is not None and right is not None:
        lo, ld = left
        ro, rd = right
        origin = (lo + ro) * 0.5
        direction = ld + rd
        direction = direction / np.linalg.norm(direction)
        return origin, direction, True
    if left is not None:
        return left[0], left[1], False
    if right is not None:
        return right[0], right[1], False
    return None
