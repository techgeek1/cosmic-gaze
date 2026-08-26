"""GPU face and gaze inference sidecar for cosmic-gaze.

The Rust side orchestrates; this process does nothing but turn camera frames into
`{eye_mm, gaze, head_rot}` records on a Unix socket. See `gaze_ml.schema` for the
wire protocol and `README.md` for model provenance and licences.
"""

__version__ = "0.1.0"
