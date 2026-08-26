#!/usr/bin/env python3
"""Angle-space probe for the webcam gaze sidecar.

Run with the sidecar serving. Prompts you to hold your gaze on a series of physical
points; for each, samples the raw sidecar stream and prints mean/sd of gaze yaw/pitch
(camera frame, degrees), head rotation, eye position, and confidence. This measures the
model's gain and jitter independently of the desk geometry and the calibration fit.

    python3 scripts/gaze_probe.py [--socket /run/user/1000/gaze-ml.sock] [--seconds 4]
"""
import argparse, json, math, os, socket, statistics as st, sys, time

POSES = [
    ("straight into the camera lens", "camera"),
    ("the far LEFT edge of the VIOTEK, mid height", "far-left"),
    ("the far RIGHT edge of the LG, mid height", "far-right"),
    ("the TOP edge of the LG above the seam", "top"),
    ("the centre of the small panel below the seam", "bottom"),
    ("the seam between the two big displays, mid height", "seam"),
    ("straight into the camera lens again", "camera-2"),
]

def yaw_pitch(g):
    # OpenCV camera frame: +x right, +y down, +z out of the lens. Looking into the lens
    # is (0, 0, -1). Yaw positive = subject looks toward camera +x, pitch positive = up.
    return (math.degrees(math.atan2(g[0], -g[2])), math.degrees(math.asin(max(-1.0, min(1.0, -g[1])))))

def cue(kind):
    """Audible cue so the subject never has to look at the terminal. `kind` is
    "start" (window open, hold the gaze) or "stop" (window closed, read the next prompt)."""
    import shutil, subprocess
    names = {"start": "message", "stop": "complete"}
    path = f"/usr/share/sounds/freedesktop/stereo/{names[kind]}.oga"
    player = shutil.which("paplay") or shutil.which("pw-play")
    if player and os.path.exists(path):
        subprocess.Popen([player, path], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        sys.stdout.write("\a"); sys.stdout.flush()

def sample(sock, seconds):
    rows, buf, t0 = [], b"", time.time()
    sock.settimeout(1.0)
    while time.time() - t0 < seconds:
        try:
            d = sock.recv(65536)
        except socket.timeout:
            continue
        if not d:
            break
        buf += d
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return rows

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default="/run/user/1000/gaze-ml.sock")
    ap.add_argument("--seconds", type=float, default=4.0)
    ap.add_argument("--settle", type=float, default=2.5)
    a = ap.parse_args()
    s = socket.socket(socket.AF_UNIX)
    s.connect(a.socket)
    print("per pose, per estimator: median yaw/pitch, robust sd (1.4826*MAD), plain sd,")
    print("spikes = frames further than 15 deg from the median, blink-ish = valid but low conf")
    results = []
    for text, tag in POSES:
        print(f"\n>>> next: look at {text}. Move your eyes when you hear the first sound;"
              f" hold until the second sound.", flush=True)
        time.sleep(a.settle)
        cue("start")
        time.sleep(1.0)      # travel + settle after the cue
        sample(s, 0.2)       # drain
        rows = sample(s, a.seconds)
        cue("stop")
        v = [r for r in rows if r.get("valid")]
        if not v:
            print(f"{tag:12}{len(rows):4}{0:6}  (no valid frames)")
            continue
        hy = st.mean(math.degrees(r["head_rot"][1]) for r in v)
        hp = st.mean(math.degrees(r["head_rot"][0]) for r in v)
        ez = st.mean(r["eye_mm"][2] for r in v)
        conf = st.mean(r["conf"] for r in v)
        print(f"{tag:12} n={len(rows)} valid={len(v)} head yaw/pitch {hy:.1f}/{hp:.1f} eye z {ez:.0f} conf {conf:.2f}")
        for key in ("gaze", "gaze_l2cs", "gaze_iris"):
            if key not in v[0] or v[0][key] is None:
                continue
            yp = [yaw_pitch(r[key]) for r in v if r.get(key)]
            yaw = [p[0] for p in yp]; pit = [p[1] for p in yp]
            my, mp = st.median(yaw), st.median(pit)
            rsd_y = 1.4826 * st.median(abs(a - my) for a in yaw)
            rsd_p = 1.4826 * st.median(abs(a - mp) for a in pit)
            spikes = sum(1 for a, b in zip(yaw, pit) if abs(a - my) > 15 or abs(b - mp) > 15)
            print(f"    {key:10} yaw {my:7.1f} (rsd {rsd_y:4.1f}, sd {st.pstdev(yaw):4.1f})  pitch {mp:7.1f} (rsd {rsd_p:4.1f}, sd {st.pstdev(pit):4.1f})  spikes {spikes:3}/{len(yaw)}")
        results.append(tag)
    print("\nExpected geometry (camera on the LG, ~65 cm): far-left vs far-right should differ by")
    print("roughly 60-70 deg of yaw; top vs bottom by roughly 30-40 deg of pitch. Per-sample sd")
    print("under ~3 deg is model-limited; sd over ~10 deg means the crop/lighting is failing.")

if __name__ == "__main__":
    sys.exit(main())
