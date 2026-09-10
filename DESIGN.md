# cosmic-gaze — Design & Research Dossier

**Status:** pre-implementation research complete. **Research date:** 2026-08-25 (links and market facts are time-sensitive). Findings are also mirrored in Agentic Memory (`gaze-input-landscape-2026-08`, `gaze-snapping-design-references-2026-08`, `cua-pipeline-mining-2026-08`).

---

## 1. Motivation

Developing RSI from sustained keyboard/mouse use. Voice input (custom dictation stack) covers most typing, especially with agentic workflows. The remaining bulk of hand use is pointing: clicking and scrolling. cosmic-gaze aims to cover that with gaze input on a Linux/COSMIC desktop, reducing mouse use to near zero.

This is an accessibility project, not a gadget: latency, accuracy, and fatigue characteristics are health-relevant. The metric that matters is hands-on-mouse minutes per day, not raw pointing throughput.

**Existence proof for the target setup:** Josh Comeau daily-drove Tobii 5 + Talon fully hands-free for 7 months of professional dev work with RSI (joshwcomeau.com/blog/hands-free-coding/).

## 2. Hard constraints

- **Physiological accuracy floor: ~0.5–1° of visual angle**, regardless of sensor quality. The fovea spans ~2°, so the eye does not need to point precisely at a target to see it; fixational microsaccades and drift jitter the true gaze point. At ~65 cm viewing distance, 1° ≈ 11 mm ≈ 50–70 px. Lab-grade trackers and Vision Pro (independently measured 0.93–1.11°, arXiv 2406.00255) both sit at this floor.
- **Consequence:** gaze is a coarse, fast pointing channel. The system design must absorb ~1.5° of error via target snapping, refinement, or large targets. It can never be a raw pixel pointer — no shipping product anywhere does raw-gaze pixel pointing.
- **Midas touch:** eyes are sensory organs; looking must never itself be the click. Dwell-primary selection is slow, error-prone (43% errors vs 11.7% for gaze+manual commit, Vertegaal ICMI 2008), and fatiguing. A discrete commit channel is mandatory.
- **Latency budget:** end-to-end gaze→feedback under ~50 ms for the warp to feel instant. Snapping decisions must run at gaze rate (single-digit ms against a cached element index); perception (vision parsing, tree walks) must be asynchronous.
- **Error structure:** 99.2% of gaze selection errors are slips (selected a neighbor), not misses (arXiv 2603.15991). Snapping + explicit commit attacks exactly this class.
- **Physical display geometry (user setup, 2026-08-25): three displays, not coplanar, two of them curved.** Consequences: (a) angular error ≠ constant pixel error — px/degree varies with distance and obliquity per output, so every σ, snap radius, and filter threshold must be expressed in degrees and converted through a per-output geometry model; (b) a remote PCCR tracker is bound to the one screen it is mounted under and needs a near-frontal view of the eyes, so a single ET5 covers one display, not three; (c) curvature is irrelevant to snapping itself (a ~1500R cylinder is flat at 2° scale) but must be modelled for ray→surface→pixel mapping, and it makes px/degree *more* uniform across the panel, not less. (d) One tracker only, at the seam between the left display and the ultrawide (where most viewing happens): full precision inside its gaze cone, head-pose-driven coarse tier outside it. See §11.
- **Throughput expectations (honest):** raw gaze+commit ~2.1–2.6 bits/s vs mouse 3.2–4.5. Hybrid warp+refine systems informally reach ~4.5 (PolyMouse). Snapping is the lever that closes the gap without a refine step.

## 3. Interaction design

Principles, all backed by shipped systems or studies:

1. **Gaze points, something else commits.** Commit channels: voice (primary — already have the stack), optionally a foot pedal or single key. Vision Pro (gaze+pinch), Talon (gaze+voice/pop) validate the split.
2. **Warp + refine, not continuous cursor.** Gaze warps the pointer to the fixation region (saccades ~50 ms — faster than any mouse traverse); snapping resolves the target; a refinement modality (zoom overlay, head nudge, voice grid) handles the residual cases. Talon Control Mouse gen2 heuristics worth copying: gaze warps, head refines with exponential gain, head motion locks the cursor against gaze jitter. Tobii's shipped warp defaults: warp on mouse move, 1–10 cm dead zone.
3. **Hide the raw cursor; show the snapped target.** Freeze-and-snap with a subtle hover highlight on the resolved target (the Vision Pro glow). Two independent studies support frozen/hidden cursor over a live gaze cursor (Zhang, Ergonomics 2020; IJHCI 2025). The highlight is load-bearing feedback: a wrong snap is seen and corrected by re-fixation before commit.
4. **Scrolling is tier one.** Needs almost no precision, is the highest-volume interaction: scroll-under-gaze (route wheel/scroll events to the surface under gaze), focus-follows-gaze, continuous scroll on viewport-edge dwell. Shippable before clicking works at all.
5. **Continuous online recalibration from real mouse clicks.** During the transition period every physical click is ground truth (user looks at what they click). Kills drift without calibration screens. (Same idea appears in Apple patent [patent reference removed] — saliency-attractor drift correction — and GazeSwipe's implicit auto-calibration.)
6. **Voice–gaze fusion as a first-class citizen.** Content-addressed clicking: say a visible word, gaze disambiguates which instance (wolfmanstout's talon-gaze-ocr pattern). Sidesteps the accuracy floor entirely; OCR boxes are already in the vision stack. Gaze also scopes voice (dictation targets the fixated field, not keyboard focus); voice disambiguates snapping ("the second one", "close button").
7. **Late-trigger correction is mandatory, not optional.** 86.6% of gaze+commit errors are late triggers — the eyes left the target before the commit landed (GazeHandSync, ETRA 2025). Keep a ring buffer of recent fixation targets and attribute commits retroactively (corrects >94%). Voice commit latency (hundreds of ms) makes this essential.
8. **Speculative pre-grounding.** Voice commit gives a few hundred ms of warning; pre-resolve candidates around the current fixation while the command is still being recognized (pattern from PASTE, arXiv 2603.18897). Stacks with the ring buffer.

## 4. Hardware

### Decision: Tobii Eye Tracker 5 (desk-mounted) as gaze provider zero

~$260, ~0.5–1° accuracy, and two live reverse-engineered Linux drivers as of 2026:

- **nottobii** (github.com/puffnfresh/nottobii) — **Rust**, USB protocol from packet captures, includes `squinput` (filtered gaze → uinput virtual pointer) and Wayland layer-shell calibration. Preferred base.
- **tobiifree** (github.com/Aetherall/tobiifree) — Zig core (native + WASM), Linux daemon exposing gaze over Unix/WebSocket sockets, GTK4 + wlr-layer-shell overlay. Cites EU interoperability law.

Tobii officially remains Windows-only for the ET5; sold via Amazon (~$259–299). Talon historically proved ET5-class hardware as a Linux daily-driver.

Gaze providers sit behind a trait boundary — every alternative below slots in later.

### Alternatives evaluated (Aug 2026)

| Option | Accuracy | Cost | Status |
|---|---|---|---|
| **PSVR2 + PC adapter + PSVR2Toolkit** | Tobii-grade | ~$400+$60 | ET unlocked on PC Aug 2025 by BnuuySolutions' open toolkit (own API lib, VRCFT module); **Linux via "Ignition"** layer. It's a full headset — testbed, not daily driver. |
| **DIY EyeTrackVR** (XIAO ESP32-S3 Sense + OV2640 IR, ~$100–150) | **measured 4.8° ± 1.8° raw** (independent eval, Hu 2025) — NOT input-grade without heavy snapping | ~$100–300 | Project alive (active through Aug 2026). TUM ETRA'26 WearableEyeTracker (github.com/VIVARefSys/WearableEyeTracker) is a glasses-mounted stereo reference design on ETVR hardware. Closing the accuracy gap is on us. |
| **Pupil DIY** (modified Logitech webcams + Pupil Capture) | ~1–2° class | low | DIY docs alive; mature native-Linux pupil-detection + calibration pipeline for free. Pupil Core still sold (€3,615); Neon €6,250 (glasses form factor, network realtime API, works from Linux; phone-in-loop latency). |
| **Bigscreen Beyond 2e** | Tobii-grade | ~$1,219 | Most open hardware (raw eye cams, OpenXR/OSC); but Linux eye-cam enumeration blocked and firmware updates Windows-only (as of mid-2026). |
| **Quest Pro + ALVR + oscavmgr** | good | ~$300–500 used | Proven Linux gaze pipeline (OSC); hardware EOL; opaque headset. |
| **Webcam-only (appearance-based)** | 2–5° real-world; no 2026 breakthrough (EMC-Gaze: 5.79° RMSE realistic) | free | Not viable as primary. |

### The glasses wave (future providers)

- **XREAL Aura** — Android XR, tethered puck, eye+hand tracking confirmed, ≤$1,500, launches fall 2026. First buyable glasses-form-factor ET.
- **Meta Phoenix** — leaked glasses+puck, gaze-and-pinch primary input, slipped to H1 2027. Watch Meta Connect Sept 23–24 2026.
- **Android XR exposes real gaze**: `XR_ANDROID_eye_tracking` / `XR_EXT_eye_gaze_interaction` behind `EYE_TRACKING_FINE` runtime permission (shipping on Samsung Galaxy XR, $1,799, Oct 2025). Integration = write an Android XR app that streams gaze out; monitor localization via anchors/fiducials inside their runtime. Not a hijack, a tenancy.
- **Apple Vision Pro**: gaze fully locked (hover composited out-of-process; apps get one sample per pinch). Dead end. M5 refresh changed nothing.
- **AdHawk MindLink** (MEMS ET): acquired by **Google** (~$115M, Mar 2025), absorbed into Android XR; nothing purchasable, site dead.
- Everything else shipping in glasses form (Ray-Ban Display, Even Realities G2, Brilliant Labs Halo, Snap Specs) has **no eye tracking** — battery economics push consumer glasses to EMG wristbands/rings instead.
- Radical future option: once glasses have display+gaze, stream the desktop *into* the glasses — gaze and screen quad then live in the same coordinate frame and the screen-mapping problem evaporates. The interaction layer (warp/snap/commit) is identical either way; nothing built now gets stranded.

## 5. Platform landscape (why COSMIC-native wins)

- **Talon exited Linux entirely (May 2026)** — X11 support removed, Wayland never planned (dev states Wayland lacks needed APIs; disputed by KDE devs). No maintained gaze-input product exists on Linux/Wayland. Displaced user base exists. The niche is vacant.
- **The AT-SPI geometry trap:** on Wayland, a11y *tree content* works (Chromium/Firefox/Electron/GTK/Qt all expose trees) but element geometry is **window-relative only, by design** — clients cannot know global surface positions (at-spi2-core#14). No portable external tool can ever compute where a button is on screen. This is the wall Talon hit.
- **The compositor unlock:** cosmic-comp knows every surface's position, z-order, occlusion, and damage. Window-relative a11y coords + compositor geometry = correct global targets. Architecturally impossible for portable tools, nearly free here. This is the project's structural moat.
- **AT-SPI performance reality (load-bearing):** on-demand tree walks are non-viable — full-desktop walks 15 s degrading to 80 s (OSWorld #241), large LibreOffice Calc sheet ~10 min (#185), VS Code dialogs empty (#105), Electron/Chrome content often missing without force-enabling (#263). Root cause: one D-Bus round trip per property per object. Universal rule (confirmed independently on UIA — 150× from batching — and CDP): **one bulk snapshot + event-driven increments, never per-node round trips.**
- **Newton** (Wayland-native push-model a11y, compositor-cached tree): architecturally exactly the element daemon this project needs, but prototype-stage, stalled since 2024, and exposes no screen coordinates. Track for convergence. **AccessKit** survived: merged into GTK 4.18; System76 has an iced+AccessKit PoC; COSMIC Epoch 2 roadmap (Feb 2026) includes libcosmic iced rebase + screen reader work. Timing is favorable — the plumbing is being built now.
- COSMIC shipped stable Dec 2025 (Pop!_OS 24.04 LTS), rolling releases since. No gaze/dwell items on any DE roadmap (GNOME/KDE/COSMIC).
- Input synthesis on Wayland: libei/libeis + XDG RemoteDesktop/InputCapture portals is the sanctioned generic path; compositor integration bypasses the need.

## 6. Architecture

```
                       ┌──────────────────────────────────────────────┐
 gaze provider (trait) │ element index daemon (the novel core)        │
  ET5/nottobii ──┐     │  • a11y mirror: event-driven AT-SPI cache    │
  PSVR2Toolkit ──┼──►  │    (Odilia odilia-cache pattern, atspi crate)│
  ETVR/Pupil  ───┘     │  • CDP mirrors for browsers/Electron         │
       │               │    (bulk DOMSnapshot+AXTree, stable node ids)│
       ▼               │  • cosmic-comp: global geometry, z-order,    │
  filter stack         │    occlusion, damage events                  │
  I-VT state machine   │  • vision fallback: TargetFinder-class YOLO  │
  gating one-euro      │    + OCR det boxes, damage-gated, fused      │
  (fixation-only)      │    UFO2-style (a11y authoritative,           │
       │               │    discard vision boxes at IoU>0.1,          │
       ▼               │    wrap survivors as pseudo-a11y nodes)      │
  snap engine ◄────────┴──────────────────────────────────────────────┘
  fuzzy hit testing: multi-ray candidate enumeration,
  rank by (type, depth, angular distance) + hysteresis
       │
       ▼
  commit layer: voice / pedal / key
  ring buffer of recent fixation targets → retroactive attribution
  speculative pre-grounding during voice recognition
       │
       ▼
  action synthesis (compositor-side): click, scroll-under-gaze,
  focus-follows-gaze, drag, zoom-refine overlay fallback
```

Nothing published does the two things at this design's center: **compositor-damage-driven incremental re-detection** and a **persistent live element map on Linux**. Both are open ground — that's the novel engineering; every other component has a spec, a paper, or working code.

## 7. Algorithms & key numbers

- **Snapping — Apple "fuzzy hit testing" patent [patent reference removed]** (the closest thing to a published visionOS spec): assume ~1° error; cast ~10–30 rays in a pattern around the gaze point; enumerate non-occluded candidates; rank by (1) element type / hover-capability, (2) UI nesting depth, (3) angular distance, (4) **hysteresis favoring the currently-hovered target** (anti-flicker). Runs in a privileged layer; apps receive an already-snapped point. Post-landing inference — no saccade prediction.
- **Effective target size:** visionOS minimum eye target 60 pt ≈ **2.5°**. Snap acceptance radius ~1.5°; refuse to snap on ambiguity rather than guess (fall back to zoom-refine).
- **Filtering — Tobii zero-delay architecture ([patent reference removed]):** I-VT state machine gates a smoothing filter that runs *only during fixations*, bypassed during saccades (zero saccade latency, high fixation precision). Tobii I-VT defaults: 30°/s velocity threshold, 20 ms window, gap interpolation ≤75 ms, merge fixations ≤75 ms/0.5°, discard <60 ms. One-euro (mincutoff≈0.3, beta≈0.3 reported for gaze) inside fixations.
- **Skip saccade endpoint prediction** (v1 at least): nobody confirmed shipping it for selection; the several-hundred-ms post-saccadic acuity window (Meta SIGGRAPH 2024) provides a forgiveness period for snap correction from mere saccade *detection*.
- **v2 scoring upgrades:** BayesGaze posterior accumulation over the fixation; endpoint-distribution scoring (+32% over raw hit testing, Wei CHI 2023); Point & Grasp (CHI 2026) is the only recent paper with released code (github.com/drlxj/point-and-grasp). Also: Sticky (temporal hold) + Magnetic (spatial attraction) heuristics from Google's Galaxy XR study (arXiv 2603.26608).
- **Tobii G2OM** (ML gaze-to-object mapping in their XR SDK) is the closed commercial analog — proof the problem is ML-tractable, nothing reusable.

## 8. Vision fallback stack

Verdict: viable today as components; no monolith exists. VLMs are the wrong shape (instruction-conditioned, generation-bound — enumeration cost scales with element count).

- **Detector:** **TargetFinder** (arXiv 2607.19907) — fine-tuned YOLO26n@640 desktop widget detector; **measured ~200 ms e2e / 10–24 FPS on M3 Max CPU** (4–7% CPU, ~400 MB RAM), F1 0.885 (vs OmniParser YOLO11m 0.698); open source, PyPI + dataset. Expect well under 100 ms on GPU. Alternatives: OmniParser `icon_detect_v3` (YOLOv9-E, **MIT** as of Jul 2026 — v1/v2 detectors are AGPL), Salesforce GPA-GUI-Detector (MIT, pure boxes).
- **Text targets:** PP-OCRv6 **detection stage only** (v6-small full pipeline 42.7 ms on V100@2048px; det stage is a fraction; Apache-2.0). Text boxes double as the index for content-addressed voice clicking.
- **Damage gating:** re-parse only changed regions, driven by cosmic-comp damage events (ReVision, arXiv 2605.11212, validates temporal-redundancy gating with a measured 22 ms change detector — ours is free from the compositor).
- **Slow path (disambiguation, not per-frame):** GoClick (230M Florence-2 grounder, measured ~100–150 ms on L20) for "find the thing I named"; Moondream 3 (BSL 1.1) / Qwen3-VL-4B (Apache, real-world grounding caveats — QwenLM/Qwen3-VL#1576) for open-ended queries. ScreenParse (ICML 2026, 316M dense parser) worth watching.
- **Training data if a purpose-built snapper detector is warranted** (boxes + clickability + priority, no captions):
  - **GroundCUA** (MIT): 56K shots / 3.56M human-verified elements, 87 desktop apps incl. Linux — hf.co/datasets/ServiceNow/GroundCUA
  - **ScreenParse data** (CC-BY-4.0): 1.45M web shots, 55 classes, **explicit interactability boolean** — hf.co/datasets/docling-project/screenparse
  - **OS-Atlas-data** (Apache-2.0, 13M+ elements incl. Linux), **Jedi** (Apache), **Aria-UI** (Apache, 7.8K Ubuntu desktop shots)
  - Eval: **UI-Vision** (MIT, 83 FOSS desktop apps), **GroundUI-18K** (MIT)
  - Avoid: Wave-UI (no license), UGround Web-Hybrid (CC-BY-NC-SA), OmniParser v1/v2 detector weights (AGPL), Screenpipe (relicensed commercial).
- **Fusion recipe (UFO2, arXiv 2504.14603, MIT):** a11y authoritative; IoU-test each vision box against a11y boxes, discard at IoU > 0.1; wrap survivors as pseudo-a11y nodes so downstream is origin-agnostic. Their measured yield: vision recovers ~10–12.5% of controls the tree misses.

## 9. Reusable code inventory

| Component | Source | License | Language |
|---|---|---|---|
| ET5 USB driver + uinput cursor | github.com/puffnfresh/nottobii | (check) | Rust |
| ET5 daemon + overlay | github.com/Aetherall/tobiifree | GPL-3.0 | Zig |
| AT-SPI event-driven cache | Odilia `odilia-cache` + `atspi` crates (github.com/odilia-app/odilia) | MIT/Apache | Rust |
| CDP extraction patterns | browser-use `cdp-use` rewrite (clickability via listener enumeration, paint-order occlusion, stable node hashing) | MIT | Python (patterns) |
| Vision detector | TargetFinder (PyPI) / icon_detect_v3 (HF) | OSS / MIT | Python→ONNX |
| OCR det | PaddleOCR PP-OCRv6 | Apache-2.0 | Python→ONNX |
| Gaze+OCR voice clicking | github.com/wolfmanstout/talon-gaze-ocr | (check) | Python (pattern) |
| I-VT filter reference | `tobii-ivt-filter` (PyPI), uxifiit/GazeToolkit | — | Python/C# |
| One-euro | github.com/casiez/OneEuroFilter | — | many |
| Probabilistic pointing (code!) | github.com/drlxj/point-and-grasp | (check) | — |
| PSVR2 gaze (future provider) | github.com/BnuuySolutions/PSVR2Toolkit | non-commercial | — |
| AccessKit diff protocol | accesskit.dev | MIT/Apache | Rust |

## 10. Sequencing

**Decision (2026-08-25): vision-first feasibility gate before any hardware or a11y work.**
The a11y mirror and cosmic-comp integration are deferred until gaze + vision snapping
is shown to work at ET5-class error. Rationale: vision-only is the *harder* path — if
it closes the error budget on a mixed desktop, the a11y mirror is pure upside
(precision boxes, semantics) rather than a load-bearing dependency with a Wayland-shaped
hole in it. The provider trait makes the tracker swap trivial; the open question is the
error budget, not the interface.

### Phase 0 — Feasibility gate (no hardware, no D-Bus)

Five pieces: synthetic gaze provider, screen capture, detector, snap engine, click injection.

0. **`DisplayGeometry` module (day one, shared by every provider).** Per-output: pose
   (position + orientation), physical size, curvature radius (0 = flat; cylindrical otherwise),
   pixel dims/scale. Hand-configured TOML — Wayland exposes logical layout, not physical pose.
   Provides pixel → 3D surface point, gaze ray → surface intersection → pixel, and the local
   px/degree Jacobian at any point. The snap engine ranks by angular distance (as Apple's
   fuzzy hit testing does) and works in pixels only via that local scale.
1. **Synthetic gaze provider.** Mouse position + injected error model: Gaussian noise
   (configurable σ, default 0.7° ≈ ET5 residual), slow drift term, sample latency, optional
   scripted saccade/fixation profile. Noise is injected in *degrees*: pixel → surface point →
   perturb ray from a nominal eye point (~65 cm from the seam) → re-intersect → pixel. **σ is a
   function of angle from a configured tracker axis**: flat inside the gaze envelope (~±25° H /
   ±15° V), growing beyond it, degrading to head-pose-only past the head box — so the prototype
   exercises the precision-cone / coarse-tier handoff from day one. Every sample carries its σ;
   the snap engine bails to the coarse tier when σ exceeds the snap radius. Read the mouse via evdev from a specific device node so
   the provider's input and the injected pointer are distinct sources. **Latch the gaze sample
   at commit** — the post-commit warp must not feed back into the fixation detector. Keep all
   filter thresholds in degrees and seconds, never samples (rates change 30 → 133 Hz across
   providers). This is also the permanent deterministic regression harness.
2. **Screen capture.** xdg-desktop-portal screencast via PipeWire, or `ext-image-copy-capture`
   if the installed cosmic-comp exposes it (check). Low rate; frame-diff threshold as the
   stand-in for compositor damage; re-run detector on demand at commit; cache boxes between runs.
3. **Detector.** TargetFinder (YOLO26n → ONNX via `ort`) + PP-OCRv6 detection stage. ~200 ms
   CPU is acceptable because detection is off the commit path (commit reads the cache; the
   late-trigger ring buffer handles the rest).
4. **Snap engine.** Fuzzy hit testing (type → nesting → angular distance) with hysteresis;
   I-VT gating one-euro; ring buffer with retroactive attribution.
5. **Commit + injection.** Keyboard/voice commit; pointer warp + click via uinput or libei
   (RemoteDesktop portal).

**Success criterion (write the number down, not the feeling):** at σ = 0.7° on a real mixed
desktop (browser, editor, terminal, COSMIC Settings), first-commit snap-correct rate over a
few hundred targets. **≥ ~90% → in the realm of possibility.** Then re-run at σ = 1.5° for
the degradation curve; that curve decides whether the ET5 is sufficient or whether this waits
on the glasses wave.

Optional second stand-in before hardware: a webcam appearance-based tracker (L2CS-Net + 9-point
polynomial calibration, or EyeGestures) — 2–5°, 30 Hz, ~100 ms. Validates real fixation
dynamics, head motion, and hidden-cursor behaviour, **not** the error budget (a webcam-only
result would be a false negative on the idea).

### Phase 1+ — after the gate passes

1. **Real gaze source.** ET5 + nottobii on the *primary* display only; the real risk is the
   Linux driver on this kernel/USB stack, not accuracy. Order at gate-pass and overlap with the
   next steps while it ships. Validate accuracy/latency on the actual monitor; own calibration
   overlay fitted against the true (possibly cylindrical) surface — check whether nottobii
   exposes pre-calibration eye positions/gaze vectors or only Tobii's flat-display 2D point (if
   the latter, stack a per-screen polynomial correction on top). Secondaries: webcam head-pose
   (opentrack-style) for which-display + scroll/focus tier only, until either N trackers with an
   arbiter or a head-mounted provider. See §11.
2. **Ship tier 1: scrolling.** Scroll-under-gaze + focus-follows-gaze via cosmic-comp. No
   precision needed; immediate daily-driver value.
3. **Element index daemon.** Odilia-pattern AT-SPI mirror + cosmic-comp geometry fusion; CDP
   mirror for the browser. UFO2-fused with the vision boxes from Phase 0 (a11y authoritative).
4. **Compositor integration.** Damage-driven re-detection replaces frame-diff; hover highlight
   rendered compositor-side; mouse-click online recalibration.
5. **Refinement layer.** Zoom-refine overlay for ambiguous/tiny targets; optional head-nudge.
6. **v2:** Bayesian/endpoint scoring; purpose-trained snapper detector on
   GroundCUA+ScreenParse+OS-Atlas if TargetFinder falls short; new gaze providers (PSVR2,
   glasses wave) behind the provider trait.

## 10a. Phase 0 build status (2026-08-25) — complete; verdict in §10b

Workspace scaffolded: `PLAN.md` holds the per-crate contracts and verified environment
facts (cosmic-comp exposes `ext_image_copy_capture_v1` and `zwlr_layer_shell_v1`; portal
has no RemoteDesktop so injection is uinput, for which the user already has an ACL; evdev
needs the `input` group). `config/desk.toml` carries the measured logical layout and
eyeballed physical poses (marked MEASURE). Crates: gaze-core, gaze-provider-synthetic,
gaze-capture, gaze-detect, gaze-snap, gaze-overlay, gaze-inject, then gaze-bench and
gaze-proto. Gaze source for Phase 0: a grabbed spare mouse (Lenovo optical) integrated into
a virtual point, so the real cursor (G502) and the gaze point are independent.

## 10b. Phase 0 first result (2026-08-25, bench v1)

All seven crates plus `gaze-bench` and `gaze-proto` built and live-verified in one session
(capture ~35 ms/ultrawide, detect ~400 ms, closed-loop uinput warp within 1 px on all
outputs, overlay click-through, grabbed-mouse provider at 120 Hz). Bench v1: 15 captures of
the real desktop (Discord, browser, terminals), 3118 detected elements as targets, 20
trials each, first-commit snap-correct rate:

| sigma | single | 24-sample fixation |
|---|---|---|
| 0.5° | 39% | 48% |
| 0.7° | **30%** | 38% |
| 1.0° | 21% | 27% |
| 1.5° | 13% | 14% |

Against a 90% gate. What it actually says:

- 94% of failures are slips to a genuinely different adjacent element; 74% of all targets
  are under 0.5° on their short side, and the typical confusion is a neighbouring text line
  0.3–0.7° away (chat messages, terminal rows). That is below the physiological floor for
  *any* gaze system; no snapping algorithm fixes it. Engine knobs (weights, radius) move the
  number only between 25% and 31%. Whole-pane detector boxes are not the cause (removing
  them changes 30.0 → 30.4%).
- The noise model was wrong in a way that suppressed the fixation path: full σ drawn
  independently per sample reads as 50–120°/s motion to I-VT, so only 20% of samples
  classified as fixating and `commit` often had an empty ring. Real trackers are
  bias-dominated (per-fixation) with 0.1–0.3° per-sample jitter. Fixed in `NoiseModel`
  (`jitter_deg`, `bias_sigma`); bench v2 and the provider are being updated.
- The sigma profile on this desk puts ~36% of trials outside the tracker's valid envelope
  (`no_gaze`): a desk-layout fact, consistent with §11.

Reframing that follows: the target set that matters is *widgets* (buttons, links, inputs,
tabs), not every OCR line; and the metric that matters is **confident-wrong rate**, not
raw correct rate, because an ambiguous snap can defer to a refinement tier (zoom, voice
disambiguation, content-addressed click) at low cost while a wrong click is expensive. Bench
v2 reports per target class (line / widget / other), with and without text distractors, and
an ambiguity flag from the engine's candidate margins. The gate becomes: on widgets,
confident-wrong under ~5% with ambiguous under ~40%.

### Bench v2 (same day, bias+jitter noise, target classes, ambiguity)

- Noise fix confirmed the v1 "fixation averaging helps" result was an artifact: with a
  per-fixation bias, 24 samples buy back only the 0.2° jitter. Single-sample numbers are
  the real numbers. Fixation classification went 21% → 84%.
- **Widgets at σ = 0.7°: 36% correct; 43% with OCR lines removed as candidates.** Text
  distractors are a contributing cause (+7 pts), not the cause.
- **The two-tier gate fails as specified**: best trade-off is confident-wrong 4.9% at
  ambiguous 77% (widgets, margin 0.5); ambiguous 54% costs confident-wrong 17%. Ambiguity
  is genuine neighbours (detector duplicates/nesting explain only ~2.5 pts).
- **GUI-heavy captures are *worse*** (COSMIC Settings sidebar: 27% widget-correct, 88%
  ambiguous). Denser GUI = denser targets; widget pitch on a real desktop sits at or below
  the σ = 0.7° resolution limit. The corpus was never the problem.
- Suspect in the scoring: distance is to the nearest box edge, so every box containing the
  gaze point ties at 0. A centre-normalised term is being added and swept.
- 9% of widget trials scored `no_gaze` because widgets cluster at panel edges and an
  off-panel ray was treated as lost; being changed to clamp to the nearest edge.

Conclusion so far: **gaze alone cannot select among desktop-density targets at ET5-class
error; that is physics, not engineering.** The design pivots from "snap, refine when
ambiguous" to "gaze is the coarse channel, always paired with a fine channel". The number
that picks the fine channel is top-k: if the intended target is in the engine's top 2–4
candidates ~95% of the time, a gaze-localised hint (2–4 numbered labels, one spoken or
pressed token) resolves it in one step; otherwise zoom-refine or head-pose fine cursor
(ET5 provides head pose) is the fallback. Bench v3 measures top-k.

### Bench v3 (2026-08-26): top-k, edge clamp, scoring — Phase 0 verdict

- **Top-k, widgets, σ = 0.7°**: top-1 42% (48% widgets-only candidates), top-3 75% (83%),
  top-5 87% (94%). At σ = 0.5°: top-3 86% (91%), top-5 94% (97%). Mean candidates within
  the ambiguity margin 3.2, p90 5. So the hint set is small, but the target is outside a
  3-way hint a quarter of the time at nominal ET5 error.
- Edge clamp (off-panel ray → nearest panel edge, as a real tracker reports) recovered
  +5.7 pts on widgets; `no_gaze` is now only genuine envelope loss.
- Scoring: the normalised centre term is harmful (saturates equally for a row and its
  label). `center_deg` 0.2 with `kind` 1.5 is the balanced point: +3 correct, +3.7 top-3,
  +4.8 on cross-kind-nested targets, +3 confident-wrong; now the engine default. Nesting
  across kinds is 22% of widgets and costs ~6 pts within that population — real but not
  the main event. Every remaining knob trades ambiguity against confident-wrong at a near
  constant product; gains beyond this come from the candidate set (a11y boxes), not weights.

- **Nudge distance (the fine-channel cost), widgets, σ = 0.7°**: 46% of trials need no
  correction at all; median 0.14° / 10 px; p90 1.0° / 66 px; p99 1.9° / 130 px. Degrades
  linearly with σ (p99 1.2° at 0.5°, 4.2° at 1.5°). A directional flick alone resolves
  57%; the rest need a short nudge. This is the number that sizes the controller's job: a
  fraction of a thumb swipe, never a zoom.

**Verdict.** As a standalone pointer, gaze + vision snapping at ET5-class error picks the
right widget ~45% of the time on a real desktop and cannot be tuned past ~50%: target
pitch on a 4K desktop is at or below the accuracy floor. As the *coarse stage* of a
two-stage input it is in good shape: the target is within the top 3–5 candidates 75–94%
of the time and, with a hover highlight and a physical commit, a wrong snap is a visible
wrong highlight the user nudges rather than a wrong click. Two levers remain for the
coarse stage itself: online recalibration from every refined commit (each one is a labelled
gaze→target sample, and per-fixation bias is largely a smooth calibration residual, so
effective σ should drift toward the 0.3–0.5° precision floor with use: top-3 → ~90%), and
the a11y mirror for a cleaner candidate set. Neither is needed to start using it.

### Webcam provider (2026-08-26): built, first real-eyes data

`sidecar/` (Python, uv): MediaPipe BlazeFace + Face Landmarker (478 pts, Apache-2.0),
solvePnP against MediaPipe's canonical face model (6–9 px reprojection), L2CS-Net
ResNet-50 (Gaze360 weights: research-only licence — swap before any release). GPU via torch
ROCm wheels on the 7900 XTX: 17 ms inference, camera-bound at 30 fps, 15 ms capture-to-send,
JSON lines over `/run/user/1000/gaze-ml.sock`. Rust `gaze-provider-webcam`: camera→desk
transform (fixed 180° roll then panel-convention yaw/pitch/roll), measured eye position as
ray origin, edge clamp, calibration sweep on the layer-shell overlay (global angular offset
before intersection + per-output quadratic on the residual; 0.04° RMS recovering a known
synthetic distortion), `config/calibration.toml`.

First live numbers, before any camera adjustment: subject still → 100% face detection,
but L2CS gaze yaw **sd 23°** over 300 frames (head pose sd 11°), far worse than the 2–4°
expected. **Root cause (found 2026-08-26, same day): the sidecar's preprocessing applied a
`CenterCrop(224)` copied from L2CS's archived demo, which discarded the outer half of the
face crop — the network was seeing a nose and mouth with the eyes clipped off — and fed
224 px instead of the model's 448 px input (worth another 30° of bias, silent because of the
adaptive pool).** Lesson: an appearance model on a bad crop produces stable, plausible,
meaningless output; only looking at the input image reveals it. After the fix the sidecar
agrees with the vendored reference pipeline to 0.6° yaw / 0.3° pitch, and a still-face
capture gives L2CS yaw sd 6.3° / pitch 4.9° (subject motion accounts for ~2°). A second,
geometric estimator was added (`--estimator iris`: iris centre vs eye corners from the
478-pt mesh, composed with PnP head pose, 0.14 ms): yaw sd 5.1°, pitch 2.2°, and
corr(L2CS yaw, iris yaw) = +0.81 — two independent methods agreeing is the best evidence
without ground truth. Pitch is the weak axis for both (corr +0.02); the per-subject iris
offset (~14° disparity between eyes) is a bias the calibration removes. The camera has since
been moved to a bottle at the seam, ~43 cm from the eyes, looking up.

Earlier suspicion (kept for the record) was the setup, not the model: the C920 looks across the room with
the head at the bottom edge of the frame (chin clipped) in a dim room lit by the monitors
(mean luminance 18/255); the driver was stretching exposure to 66 ms. Fixes already in
code: padded face crops at the frame edge, `exposure_auto_priority` cleared, capture on a
drain thread. Physical fixes before judging accuracy: tilt the camera down so the face is
centred, add fill light, then run the calibration sweep — its per-target spread is the
honest accuracy measurement. Expectation stays 2–3° at best: a coarse "which region"
signal for feel, scroll/focus tier, and the warp+nudge loop, not for snapping.

**Webcam calibration and feel (2026-08-26, later).** Three real sweeps. Findings, in order:
(1) glancing at the terminal to read prompts was the 20–30° "noise" — the calibration
sweep (targets on screen) is the only honest probe; (2) fitting in pixel space after
intersection was wrong (rays that missed a panel were edge-clamped before fitting) —
calibration is now an angle-space polynomial fitted before intersection, scored by
leave-one-target-out, with the runtime path proven identical to the fitter by a replay
check; (3) a cubic overfit *between* targets (local gain 0.35 → 3×; the marker reached
half-way to the edges) — calibrations are now gated on their **gain field**, not just their
residuals, a lesson worth keeping; (4) with 76 targets, per-sample sd 1.5°, the honest
number is **4.1° held-out** for a quadratic, and the raw L2CS map is *asymmetric*: linear
toward the VIOTEK, kinked toward the LG (slope 2.3 near the axis, 0.4 beyond −20°),
suspected lighting/head-pose (lamp on one side). A **thin-plate spline** (λ=0.1, one centre per target, extrapolation clamped) plus a
per-output quadratic pixel stage follows the kink where polynomials could not: **held-out
2.41°, in-sample 1.55°** (~140–180 px at each panel centre), the ±6° column errors on the
LG gone, gain field 0.31–1.51 and gated only where the user actually looks. Derivative-
regularised cubics (3.7°), piecewise quadratics (3.1°) and monotone splines (5.7°, folds)
all lost to it. Head pose as a fit input does not help;
camera pose error does not matter (±5°/±30 mm → ≤0.14°). In use: accurate enough to nearly
point at elements on the small panel directly behind the camera; unusable on the big
displays. **The webcam's usable cone is ~±10° about its axis — the precision-cone concept,
felt.** The ET5 at the same spot has a ±25° cone and 4–5× the accuracy inside it. Cheap
stopgap if wanted: a second webcam on the LG's bezel, arbitrated by head yaw. Filter
presets for noisy sources added to gaze-proto (velocity over 100 ms, 80°/s, one-euro
0.6 Hz) because ET5-tuned I-VT classified every webcam sample as a saccade and never
smoothed.

**Webcam phase verdict (2026-08-26, end of day):** at the C920's ceiling (2.4° held-out,
1.5° jitter; literature floor ~2° for appearance-based gaze). "Definitely a lot more
accurate, not usable." The stack is not the limit; the sensor is. Next: Tobii ET5 +
nottobii at the camera's spot (check availability; the 4C shares the protocol family).
Day one: driver binds; ray vs on-screen point (decides the virtual-screen trick); one 5×5
sweep through the calibration diagnostics; glasses on. In parallel, the Daydream fine
channel below, developed against the synthetic provider.

### Planned fine channel: Daydream controller (2026-08-26)

The user has a Google Daydream controller and intends to use it (with voice) as the
input companion to gaze. BLE GATT, not HID: a custom characteristic streams ~20-byte
packets at ~60–100 Hz with orientation/accel/gyro (13-bit fields), touchpad x/y (8-bit),
and five button bits; the format is reverse-engineered (mrdoob's WebBluetooth demo is the
canonical decoder). Integration: a small `gaze-daydream` daemon on BlueZ (`bluer` crate)
emitting events directly into the gaze loop; optional uinput mirror for general use.

Roles, in order of expected value:
1. **Commit**: touchpad click / app button. Physical zero-ambiguity commit (Vision Pro's
   pinch). Voice commit becomes the fallback.
2. **Refinement**: gaze warps to the snap point, then either the touchpad as a relative
   fine cursor (gain is ours; 8-bit resolution is plenty for ±1°) or the **gyro as a
   gyro-mouse** (very precise for small motions, no pad-edge problem; drift is irrelevant
   because every gaze warp resets it — Daydream's own laser pointer worked this way). Try
   both; the gyro is the bet.
3. **Candidate selection**: on an ambiguous snap, a *flick* on the pad toward the intended
   neighbour resolves the top-k set without labels or a spoken token; numbered hints remain
   for stacked-identical cases where direction is ambiguous.

This makes the post-pivot design concrete: gaze = coarse (where), controller = fine
(exactly which) + commit (now), voice = text and content-addressed commands.

## 10c. ET5 provider (2026-08-27/28): host-owned device state, then a state-conditioned model

**Status 2026-08-27.** `gaze-provider-et5` speaks the ET5's USB protocol natively (TTP framing,
HMAC-MD5 realm unlock, 0x500 gaze stream: per-eye origins raw and calibrated, per-eye rays as
plane intersections, combined uv, pupil diameters). A compound sweep trained the on-device eye
model on a ring of points, then fitted a per-display polynomial correction field and a
head-gain regression on top (best cross-validated R² for the head channel 0.64; per-eye
triangulation carried 120–350 mm of head-generalisation bias). Lived experience: constant
regression, recalibration needed every other sitting even after the device sat off for hours,
and every calibration slightly off and drifting. Restoring a blob backup made things worse.

**Research (2026-08-27, night).** The Windows Tobii Platform Runtime stores `calibration.setpm`
and `screenplane.setpm` per user profile on the host and, per nottobii's pcap-derived init
sequence, uploads the blob on **every connect, twice** (after hello and before `CONFIG_3D_SET`;
again after auth and eye-enable, before subscribe). Talon's plaintext `eye_mouse.py` does
`display_setup` then `CALIBRATE_UPLOAD(calib.bin)` on every attach. Neither trusts the device's
flash; we did, and only compared the blob's size. Elsewhere, blob restores silently failed
until the ~400–600 KB blob was sent in 8 KB transfers with a per-transfer envelope (our
transport already does this). The two references disagree on order: nottobii's captured
Windows sequence uploads before the plane is declared and again after eye-enable; Talon
declares the plane first and uploads after. Killing a process mid-upload wedges the tracker
until unplugged.
Talon calibrates in rounds (1, 4, 4) with `POINTS_APPLY` after each over a 600×340 mm area
bottom-centred on the screen, adds a point only once the device's gaze has settled on it, and a
third party measured 0.69° from one round. Full opcode map recovered from Talon:
START 0x3f2, STOP 0x3fc, POINT_ADD2D 0x406, CLEAR 0x424, POINTS_APPLY 0x42e, EYE_APPLY 0x42f,
GET_POINT_SUGGESTION 0x442, DOWNLOAD 0x44c, UPLOAD 0x456. Tobii's consumer software offers only
profiles, a second "improve" calibration per profile for other lighting, and separate profiles
for glasses — no automatic recalibration.

Patent directions worth copying (host-side, all feasible on our stream): implicit
recalibration from interactions with stimulus-type time windows, RANSAC, inlier-ratio and
minimum-count gates (Tobii [patent reference removed]), a foveal-tolerance accept plus an error buffer that
escalates to explicit recalibration (Microsoft [patent reference removed]), per-pair validation against held
ground truth and head-jump-triggered episodes (Apple [patent reference removed]); pupil-radius offset
k·R + m per eye fitted from calibrations at two illumination levels (Tobii [patent reference removed]); per-eye
weighting from rolling pupil-signal variance through a sigmoid with a moving average (Tobii
[patent reference removed]); zone-wise offsets and zone-wise smoothing windows updated from button
selections (Tobii [patent reference removed]); corneal radius re-estimated every 10–60 min because it
drifts (Tobii [patent reference removed]); a radial gain with angle from the axis (Tobii [patent reference removed]);
read-back of typed text as an unbiased offset signal (Microsoft [patent reference removed]) and
reading-line assignment for vertical drift. One ET5 paper reports 1.13–1.37° stock and 0.19°
after a neural net, almost certainly in-sample.

**Decisions.** The host owns device state: upload the blob on every connect in the Windows
order and verify by retrieving it; retrain the firmware once and key all client data to the
blob hash. Replace per-session calibration with one model across sessions conditioned on head
position, interocular vector, pupil diameter and angle from the axis — a residual in angle
space before intersection, kernel ridge / sparse GP so the correction fades and σ widens away
from data — trained from short recording sessions and, in steady state, from accepted mouse
clicks (the online-recalibration item of §3, now with the acceptance machinery above) plus a
minutes-scale online offset that resets on head jumps. Evaluation is leave-one-session-out
only. Build plan and contracts: `PLAN-ET5.md`.

**Results log.** (append per experiment: date, blob hash, split, numbers)

- 2026-08-28 00:xx, A0 `blob-info` ×3 over 5 min, separate connects: `cal_retrieve` is
  deterministic (604948 B, sha256 `d32f6c4b…30f7c4`, identical within and across runs) and
  the device holds exactly `config/calibration-et5.bin` (16:12 retrain). So the tracker had
  *not* lost its model at that moment; whether it does across a power cycle, and whether it
  mutates during use (`blob-watch`), are still open. `BlobCheck::Exact` is the verify mode.
- 2026-08-28, **power cycle: the model does not survive.** `blob-info` before: 604948 B
  `d32f6c4b…`; after unplug/replug: **1478 B `bfd74a83…`**, the factory default. This is the
  mechanism behind every "regressed after sitting off" episode, and the reason the Windows
  driver and Talon re-upload on every connect. Host-owned blob confirmed as the fix; the
  provider now uploads at connect, so a power cycle costs nothing once the file exists.
- 2026-08-28, **the blob is not round-trip stable, but its body is.** Pushing the 16:12 file
  (604948 B) read back the same length with the first 604428 bytes identical and the last 520
  different; pushing that read-back form handed the *original* bytes back. The trailer decodes
  as little-endian f32 pairs — the retrain's ring and lean-dot targets (0.5/0.5, 0.7/0.5,
  0.3/0.5, 0.35, 0.65…) each followed by a measured point and a `1` flag word per eye: Tobii's
  per-point calibration-result table, 13 unique targets × 40 bytes (target, per-eye
  measured position, per-eye validity). Not double-buffered: the table is **re-normalised
  against whatever display area is declared at read time** (a linear fit of read-back vs
  committed coordinates recovers the virtual plane's 880×365 mm against the trained plane's
  875 mm), so trailer bytes are a view of device state and comparing them across a round
  trip means nothing. Verification compares the body only (`BlobCheck::Body`); identity
  everywhere is the body hash (`25542046…` for the 16:12 model). `blob-info` decodes the
  table — it is the firmware's own per-point accuracy report, free on every retrieve.
- 2026-08-28, that table for the 16:12 model: ring points 0.3–4.7° per eye (worst at the
  upper-left ring stop, 4.5°; 0.5/0.8 at 2.7–3.6°); **the four lean-dot targets read 5°, 14°,
  18–21° and 24° per eye and are still flagged valid.** Either the model never fitted the
  lean posture (the head-generalisation failure in the firmware's own numbers) or those
  points were mislabelled going in; either way the old ceremony fed the on-device model
  four points it could not reconcile. The new `calibrate` has no lean dots.
- 2026-08-28 01:24, first run of the new `calibrate`: **2 of 18 points accepted** — the gate
  required the firmware's reported gaze within 3° of the target, and after `cal_clear` (or
  the one-point apply of round 1) the live gaze is absent or garbage, so 16 points timed out
  and were skipped; a two-point model was committed (90 KB blob, health 2–40°). Kernel log:
  the tracker **re-enumerated at 01:24:55–57** as the ceremony ended, no plug touched — a
  firmware reboot, which resets the model to the 1478-byte factory blob. Fixes in flight:
  Talon's vote-only gate, seed the session with the previous blob (nottobii's captured
  order), dwell fallback when no gaze arrives, no auto-skip, refuse to commit under nine
  points, abort on re-enumeration, and a reconnect-and-compare persistence check before any
  file is written. Also seen in the log: the EyeChip exposes a UVC 1.10 video interface
  (`uvcvideo 1-6:1.1`) — the Windows Hello IR camera; raw eye images may be reachable.
- 2026-08-28, session zero (the 16:24 readings imported as `config/sessions/1787873083-d32f6c4b.jsonl`,
  1462 rows after saccade gating, no outlier gates). Firmware-only residual of the filtered
  ray, in-sample, no split: **3.75° rms** overall (p50 2.12, p90 5.69); stops 2.48°, glides
  3.04°, **head-sweep holds 5.16°** — the head-generalisation failure as 466 labelled rows.
  Mean bias −0.14° yaw / −0.91° pitch. Pupil range 2.4–7.3 mm. Python harness smoke test
  (one session split by stop parity — not the gate number): firmware 4.01°, kernel model
  3.56° held-out / 1.55° in-sample, old per-session quadratic 8.99° held-out; pupil slope
  −0.22°/mm left eye (R² 0.01, significant), right eye nil; predicted variance vs |error|
  Spearman 0.13 in-sample.
- 2026-08-28, bug: `load_tracker_pitch` used `str::parse::<toml::Value>` (a single value since
  toml 0.9), silently returned 0, so **every `calibrate` so far declared the trained plane in
  the desk frame, without the 13° mount pitch**. Fixed; changes session zero's residual only
  3.68 → 3.75° rms, but the next retrain declares a different plane than all previous ones.
- 2026-08-28 11:56, **first successful retrain with the fixed ceremony** (seeded from the
  16:12 blob, vote-only gate, 9 targets at 5/50/95% of a 600×340 mm area × black/white).
  14 of 18 slots accepted; committed body sha256 `70289bb2…` (654498 B), persisted across the
  reconnect check, `blob-info` retrieve twice identical. Firmware's own table (against the
  875×370 plane): 12 of 14 points at 0.4–3.2° (right eye ≤1° on most, left eye ~1° worse
  everywhere) — versus the 16:12 model's 0.3–4.7° ring and 5–24° lean dots. **Top-left
  target failed identically on both passes**: left eye invalid, right eye 9–11° off, and the
  right-eye reading is the same both times (uv ≈ 0.08, −0.1), i.e. a systematic
  extrapolation error, not a transient — consistent with the firmware discarding samples
  where one eye is invalid, so those two slots taught the model nothing. Health pass (grey
  3×3, live gaze, 20 samples each): centre column 0.36 / 0.23 / 1.41°, right column
  2.13 / 0.38 / 3.50°, left column **8.29° (7 valid samples of 20)** / 1.52 / 1.00°. In use:
  "feels a lot better". Open: why the left eye drops out at the top-left (glint/lid
  geometry? map validity over a `record` session). The four missing slots were not
  skipped: the trailer lists black TM·TL·BR·BL·TR, then white C·BM·ML·MR, then white
  TM·TL·BR·BL·TR — exactly the newest 14 of the 18 in insertion order. **The device's
  calibration store is a FIFO capped at 14 points, or at ~640 KiB** (this blob is
  654,498 B at ~46.6 KB per point, 862 bytes under; the two limits cannot yet be told
  apart, and the 16:12 run's 13-of-14 is inconclusive because its lean centre may have
  been skipped). Consequence: the double-background schedule silently threw away the
  black centre and mid-edges. Ceremony now runs 13 points (black centre / mid-edges /
  corners, white corners) and reports how many the device kept. Talon's 9 and Tobii's
  5/7/9 never reach the cap, which is why nobody mentions it.
- 2026-08-28, decision: **passive labels from mouse clicks** (`gaze-clicks`, PLAN-ET5 B4).
  People look at what they click, so every deliberate click on a recognised text or
  control element is a labelled gaze sample — hundreds a day, no ceremony, and it does not
  need gaze pointing to be usable first, which is the chicken-and-egg the click flywheel
  (Phase E) otherwise has. Every piece already existed: pointer position from the
  compositor's cursor-capture session (`gaze-capture::CursorTracker`), buttons read-only
  off the input-remapper clone over evdev without a grab, the recognition layer
  (`gaze-capture` + `gaze-detect`) on a crop around the click, and the `record` session
  format. Rules: pre-click frame only (a post-click capture shows menus closing and pages
  navigating), drags and clicks on nothing rejected, the smallest containing element's
  kind/box/text stored so the export can weight by target size, gaze window
  −1.2 s..+0.4 s, and the firmware's own offset at each click reported live as the daily
  drift number. Where the eye actually is relative to the click point (it leads by
  100–300 ms and sometimes leaves early) is left to the export, which keeps the whole
  window.
- 2026-08-28, `gaze-clicks` recognition: **crops lose wide flat widgets.** On identical
  pixels (one DP-2 capture of Discord + a browser, detected whole as the reference, then
  re-detected as crops around the twelve largest widget centres): 512 px crop agreed
  **0/12**, 640 px crop (tile scale 1) **0/12**, whole frame 12/12. Every crop returned
  the widget's inner OCR text or nothing: Discord channel and member rows, the URL bar
  (Input 636×35), search (Input 350×47), a Link 437×27. Small square widgets survive
  cropping. Not a scale effect — the widget model needs the surrounding layout. (A first
  measurement of 15/30 vs 30/30 was an origin double-added in the harness; the corrected
  one is starker.) Recognition moved to the whole output frame (~250–400 ms, off the
  click's critical path), capture and detection on separate threads so a double-click's
  second capture is never queued behind the first click's detection; a full detect queue
  refuses the click as `overrun` rather than waiting. Live `recognition_check` against a
  fresh reference: 11/12, at the ceiling set by the screen changing between captures
  (two captures a second apart agree 29/30).
- 2026-08-28, `gaze-clicks` recognition v3: **pointer-local tiles + native OCR window, and a
  "nothing here" rule.** Two problems from the live probe: ~390 ms per whole frame, and the
  probe (polling at 1 Hz, drawing ~450 ms later) made it look worse than the collector
  (frame frozen 20 ms after the press; speed only bites via `overrun`). And text under the
  pointer was missed on the 3840 panel because full-frame OCR runs at a 1600 px longest
  side (2.4× shrink) where terminal lines ~6 px apart fuse into paragraph blobs
  (`Text 794×505`), while widget boxes at `widget_conf 0.25` sat over styled prose (every
  sub-0.5 box on two captures was an inline-code chip, timestamp, heading or message body;
  every real control scored ≥ 0.5; no box on either capture covered truly blank pixels).
  Fix: the "full frame" pass is already 1024 px tiles at 15% overlap, and any box that
  contains the pointer and is whole in some tile is whole in a tile containing the
  pointer, so `Detector::detect_near` runs only those 1–4 tiles (28 ms each) — checked
  identical to the full pass for pointer-containing widget boxes at 26/26 points across
  both captures — plus native-resolution OCR on a 640 px window (62 ms; line boxes,
  median ~20 px, none over 25 where the full pass gave 61). Collector gates: score ≥ 0.5,
  widget size ≤ 1200×240 frame px applied *before* text fusion so a spurious panel cannot
  swallow the lines under it, and a model-free flat-pixel check (luma σ < 0.02 in a
  ±24 px window) that refuses a box taller than 60 px when the pointer is on blank;
  collector and probe share `pick`. Live: ~100 ms capture-to-outline at 4 Hz, and the
  pointer on empty desktop reads `NOTHING` (0 boxes, σ 0.006) rather than a claim.
- 2026-08-28, `gaze-clicks` recognition v3.1: **a full-resolution pointer tile.** Live
  report: on YouTube's action column the probe outlined the count label or the icon glyph
  instead of the button. Cause, reproduced on the screenshot: the 1024 px plan tiles reach
  the 640 px model input at 0.625×, and the 50 px circles (scale 1) score 0.23–0.42 there
  while the labels beneath score 0.55–0.58, so the 0.5 gate keeps the label; the thumb
  glyph itself comes back from native OCR as 10×9 px "text". A second widget tile of
  640 px centred on the pointer (`tile_at`, `NearConfig::tile_px`) shows the model the
  pixels unscaled: the same circles score 0.79–0.95 (icon+label as one control), and a
  Discord server-icon column the plan tiles miss outright comes back at 0.51–0.90. It
  supplements the plan rather than replacing it (a box wider than the tile is whole only in
  a plan tile), goes through the same NMS, and costs one inference: 72 → 95 ms at a
  one-tile point, idle machine. Over 40 sampled points on the two captures the pick changed
  at five — two same-box refinements, two nested sub-controls, one gained icon — plus one
  regression that exposed a latent bug: the widget model has a `Text` class, and its
  paragraph box from the new tile counted as a widget in `fuse_text` and swallowed the OCR
  lines inside it (a 22 px line became a 562×209 paragraph). `Text`-class widgets no longer
  claim labels; the lines survive and smallest-box wins.
- 2026-08-28, **the probe was recognising itself.** Seven live failure cases came in at
  once: a box that grew every tick on an idle mouse, a 25×25 "button" (s=0.85) on empty
  editor background around the probe's own cross, and picks cycling button/text/nothing
  on a static screen. Instrumented run reproduced it exactly (`NOTHING → Text 14×15 →
  Button 18×19 → Text 25×23 → Button 31×28` at one idle point). Capture ruled out first
  (fresh `ext_image_copy_capture` session and buffer per call, full damage, no cursor).
  Cause was in `gaze-overlay`: it double-buffers, computed the repaint as *this buffer's
  old content ∪ new content*, and used that as the `wl_surface` damage. A blank frame
  drawn into the already-blank buffer repainted nothing, so it was committed with no
  damage and the compositor kept showing the other buffer's box and caption through the
  probe's 60 ms blank. Frame callbacks 2–6 ms after commit were a red herring (pacing
  hints, not presentation). Fix: damage = on-screen content ∪ new content (whole surface
  on background change), tracked per surface; repaint region unchanged. Verified: blank
  commits now carry the previous caption's box and thirteen idle ticks read identically.
  Lesson filed: on a static screen, suspect the probe's own output before the model.
  Still open from the same batch, all genuine model gaps rather than probe artefacts:
  Discord's message field is not an `Input` to the model (best box 0.12 at 195×54 on the
  DP-2 capture; the pointer there will now read the placeholder line as `Text`, which the
  collector accepts), a 20 px pause glyph on a dark player bar reads `NOTHING`, OCR finds
  text inside thumbnails and avatars, and a two-arrow button group comes back as one box.
- 2026-08-28, **the pointer's shape is a free signal.** Twelve more live cases after the
  overlay fix, with the failures the model has no answer to: Discord's search and message
  fields (`NOTHING`; no `Input` box above 0.1 on either), a YouTube card's blank padding,
  the description panel, a channel watermark, a terminal's empty prompt line, and the
  player bar boxed as one 216×40 button with the time display fused into it. Two of the
  twelve were the pointer 3–4 px *outside* a tight glyph or placeholder box (the
  emoji-picker button read as its 31×25 glyph; the search placeholder ending short of the
  caret): containment now allows 6 logical px of slop. For the rest, the one thing the
  pixels do not carry is what the application itself thinks is there, and it says so in
  the cursor: hand over a link or a card, I-beam over an input or a terminal. The cursor
  session already reports the image's hotspot and now its `buffer_size` too, and the pair
  names the shape without copying a pixel (Adwaita 24 px: arrow `@3,1`, hand `@7,5`,
  I-beam `@11,12`; `gaze-clicks/src/cursor.rs` has the bands and the two themes' tables;
  the centre is shared with `wait`, `crosshair` and the resize arrows, so an I-beam is
  only called by exact hotspot; the grabbing hands are a pixel from the pointing one and
  are listed exactly too, since a grab means a drag). Ruling, same day: **the I-beam is
  trusted, the hand is not yet.** A click on nothing recognisable under an I-beam is
  accepted as a `caret` (nominal 24 px box at the pointer, `element_kind = "caret"`,
  score 0), because an input, a terminal or a document is a place the eye was and inputs
  are among the most clicked things on a desktop; a large flat box under an I-beam stays
  `blank` (the empty body of an editor). The pointing hand vouches for links and cards,
  but a card's padding is a click the eye may have made from the title 100 px away, so
  it is recorded on every click and its refusals are counted on the status line, and
  that count decides later whether it joins the I-beam. The probe outlines the caret box
  and captions `CARET`, so it shows exactly what a click would write.
- 2026-08-28, **the collector asks the accessibility tree first.** The remaining failures
  (a card is one link, a grey rectangle is an input, a thumbnail is a picture) are
  contextual, and pixels are pixels; the design always meant the vision path for where
  AT-SPI is absent, not instead of it. Measured: the AT-SPI bus is up under the session,
  cosmic-comp implements `org.freedesktop.a11y.Manager`, Firefox answers
  `GetAccessibleAtPoint` in 2–9 ms with correct roles and extents; Chromium is on the bus
  with unnamed frames and answers null (accessibility off); Discord and the iced apps are
  absent. The geometry trap (§2) closes as predicted: `zcosmic_toplevel_info_v1` v2+
  gives each window's rectangle per output, `ext_foreign_toplevel_list_v1` its title, and
  window coordinates plus that origin are desk coordinates (`gaze_capture::ToplevelTracker`;
  cosmic-comp answers `get_cosmic_toplevel` after the sync reply, so `connect` waits on
  the socket). Toolkits disagree about "window" vs "screen" coordinates, and Firefox
  offsets *both* by its 20 px CSD shadow (its frame node reports `(20, 20)`); the first
  cut added the toplevel origin directly and every YouTube button drew 20 px below its
  pixels. Fix: frame-relative coordinates, `p - toplevel.origin + frame.origin` and back,
  which cancels whatever space the toolkit uses; verified by drawing a 40 px grid of tree
  rectangles onto a capture. `gaze-a11y` accepts only an answer whose node contains the
  query point. No tree
  walk anywhere — one point query plus a bounded `Parent` climb to the nearest actionable
  role (`gaze-a11y/src/bus.rs`), ~8 round trips; the §2 verdict on on-demand walks
  stands. Collector rule: the tree's target is authoritative when it contains the pointer
  (UFO2, a11y-first), subject to the same flat check as a recognised box; vision is the
  fallback; every click records `source`. The status line's `tree` count doubles as a
  measure of how much of the desktop is accessible. Verified live on the GitHub diff in
  Firefox (`TREE TABLE-CELL 1691x24`, 2–3 ms); the YouTube cases from the batch are
  Firefox too and should now read `link`/`image`/`combo box`. Not verified: Discord and
  Chromium, which need their accessibility enabled to be on the bus at all.
- 2026-08-28, two follow-ups from the first tree run. **Stacking**: agentic-studio and a
  Firefox window were both maximised on DP-1 with identical rectangles, neither focused,
  and the tracker's tie fell to Firefox, so the probe drew GitHub's tabs and URL bar over
  agentic-studio. `zcosmic_toplevel_info_v1` has no z-order; the tracker now ranks
  windows by last activation (focus is a proxy for raise) and `at` prefers the highest
  rank. A fresh tracker has no history, so the collector's day-long one answers better
  than a just-started probe; the complete answer is a capture of each candidate
  toplevel (`ext_foreign_toplevel_image_capture_source_manager_v1`, advertised) compared
  with the screen. **Discord regression**: message rows that are hovered or
  mention-highlighted read as `Button` 0.81–0.94, and `fuse_text` swallowed every OCR
  line inside them, so text clicks became 280x69 "buttons" and avatars `NOTHING`.
  Fusion now only lets a widget claim a line when it is the *only* line inside it (a
  label); rows and cards keep their lines.
- 2026-08-29, Discord on the tree. The ":1.502 Chromium" with thirteen null frames was
  steamwebhelper, not Discord; Discord was never on the bus. Its Flatpak gets the a11y
  bus but not the host dconf, so Chromium's gate (`GNOME_ACCESSIBILITY` env, else the
  `toolkit-accessibility` gsetting) reads false inside the sandbox. `flatpak override
  --user --env=GNOME_ACCESSIBILITY=1` plus `--force-renderer-accessibility` in
  `discord-flags.conf` puts it on the bus as "Discord" with a named frame, answering
  `link` rows with names and extents in 2–10 ms; list bodies climb to a scroll pane and
  fall back to vision, correctly. The session-wide route, `ScreenReaderEnabled` on the
  a11y bus, is a trap on COSMIC: the launcher mirrors it into gsettings, cosmic-session
  starts Orca off that key (auto-restart, ignores `OnlyShowIn`), Orca takes the
  keyboard, and Chromium does not consult the property. Ruling: enable accessibility
  per application; the image collector has hit its ceiling and the tree is the plan.
  Discord then had its own 10 px offset: Chromium's frame answers `(0, 0) 1291x1448`
  for the 1271x1428 toplevel, the shadow in the size rather than the origin (Firefox:
  `(20, 20) 1271x1428`). Measured symmetric on the desk, so the content origin is now
  `frame.origin + (frame.size - toplevel.size) / 2`, which covers both toolkits;
  verified with a 60 px grid drawn on a capture, headings box their text exactly.
- 2026-08-29, VS Code on the tree, and the vacant-overlay punch-through. Native package,
  so the host gsetting already had it on the bus with a named frame; the renderer tree
  needed `--force-renderer-accessibility` in `~/.config/code-flags.conf` (the
  `/usr/bin/code` wrapper reads it). Even then every content point answered a nameless,
  childless panel: VS Code's workbench keeps an invisible full-window overlay as a later
  sibling of the `document web` branch, and Chromium hit-tests topmost first, so the
  empty overlay wins its own descent. `A11y::at` now calls such a hit *vacant* (nameless,
  childless, generic container role) and punches through — climb from it, hit-test each
  ancestor's other point-containing children topmost first, first non-vacant answer wins.
  After that: menus, title-bar buttons, welcome headings and list items with names and
  extents at 3–5 ms. VS Code's frame extents equal its toplevel (no shadow insets, custom
  title bar). Pixel-grid verification blocked that day by stacking — DP-1 was wholly
  covered by another window while the ranking still resolved probe points to VS Code —
  the known focus-rank wrinkle, again.
- 2026-08-29, hardening for the first all-day drive. A D-Bus call to an application
  that stops answering blocks with no timeout — measured 138 s against a `SIGSTOP`ped
  steamwebhelper, returning only on `SIGCONT` — and one such app stalls tree queries
  for *every* window, since the app lookup walks the registry list. The collector's
  tree thread now carries a watchdog: questions outstanding and silence past 5 s means
  wedged; the thread is abandoned (it exits itself if its blocked call ever returns,
  its reply channel being closed) and replaced, with per-replacement doubling patience
  capped at 320 s so a permanently wedged app costs a bounded number of parked threads.
  Collector audit for the day run: every cache bounded (gaze ring 3 s span, rolling 2
  per output, press captures 3, parked tree replies 16, medians 20), session flushed
  per click, rotation on blob change; burst throughput is detector-bound at ~5–15
  clicks/s since `wait_for` only sleeps for the first click of a backlog.
- 2026-08-30, the dead-clone failure and reading every mouse. The first drive attempt
  wrote two empty sessions: the collector read the input-remapper clone by name, but the
  G502 had re-enumerated (a second hardware node, event24, had appeared), input-remapper
  kept grabbing the *old* node, and cosmic-comp switched to the new one directly — the
  desktop worked, the clone never spoke again, and a press dropped before reaching the
  collector bumps no tally, so the run reported zero across the board. Measured live:
  during 30 s of normal use, all motion and clicks were on event24; the clone and the
  old node carried nothing. Fix: the reader now opens *every* mouse-shaped node (left
  button, not a keyboard, plus the configured name) and rescans every 2 s for new ones.
  EVIOCGRAB semantics make this double-count-proof — a remapper's source node is grabbed
  and therefore silent to other readers, so exactly one open node speaks per physical
  press. Verified with a raw event monitor beside a live run: clicks register through
  event24 and both sides agree when idle. Lesson for anything reading evdev by identity:
  a node's continued existence says nothing about its liveness after re-enumeration.
- 2026-08-30, first real driven session (PR review, 72 min active, session
  `1788078351-70289bb2`). 213 accepted clicks, 10,193 frames, ~33 Hz, every stop window
  0.7 s with 23–24 frames, click/stop `n` gapless 0–212, `moved_px` max 5.1 (under the
  6 px drag threshold), `frame_age_s` p95 32 ms. Sources: tree 163 / caret 26 / vision
  24; all clicks on DP-1. Frame validity: 9,777/10,193 both-eyes (416 both-zero-origin
  = the meta_end invalid count exactly; a further 817 frames are one-eye — combined
  gaze still present and finite in all 10,193, so export gates must key off eye-origin
  validity, not gaze finiteness). Twelve vision clicks land ≤6 px outside their tight
  OCR/detector box (expected; box-center target absorbs it). Caret elements carry
  score 0.0 by construction. Biggest boxes are GitHub diff-hunk `table-cell`s up to
  1051×956 — exactly what the heteroscedastic box-size label noise is for; an
  export-time box-area cap is worth considering. Free drift number, median gaze vs
  click point over each stop window: **3.0° median, p90 10.9°, max 24.2°** — the tail
  is look-away/muscle-memory clicks (fixation gates at export handle it), the median is
  the raw uncorrected-calibration baseline the residual model has to beat. Bias drifted
  (−84,+49) px → (−45,+109) px first-to-last third: real intra-session drift, visible
  for free, as hoped.

- 2026-09-03, decision: **a real application as the third label source (`gaze-trainer`,
  PLAN-ET5 B5).** The five-minute dot ceremony does not produce accurate data (eight
  attempts; ruled out as the bootstrap), and passive clicks are accurate but slow and
  biased toward where the user's own applications put their controls. A trainer that
  shows a picture of an application with a highlighted target is still target
  acquisition wearing a costume; the eye behaves differently when the click has a
  consequence it is already anticipating. So the trainer is a working libcosmic
  application generated fresh per task (menus, sidebar, toolbar, tabs, lists, forms,
  articles, dialogs, dark/light), asking for two-to-four-step flows in a sentence
  ("open the Edit menu and choose Preferences"), which puts a search saccade before the
  first click and an anticipatory one before the second. It knows its own widget boxes
  and tells the collector over a socket; the collector takes its word (`source =
  "trainer"`), no recognition. Coverage steering picks each task's opening control from
  the emptiest bin of an 8×4 histogram seeded from every click already on disk; posture
  prompts every twelve tasks; theme flips for the pupil. The export gains `source`,
  `posture`, `trainer_task`, `trainer_hit`. Phase C's first question is whether
  deliberate trainer clicks predict passive ones, which the passive sessions now exist
  to answer. Not yet run against the device: the first trainer session is the next
  entry here. After that first run the task flows were dropped for free-form
  navigation over six archetypes (settings, files, mail, editor, browser, store), with
  coverage now biasing where the layout puts its sidebar and toolbar instead of
  choosing a target; the reason is that the user only needs to be navigating an
  interface, not performing a task, and being told what to click is its own kind of
  costume.
- 2026-09-04, **trainer clicks never reached the export.** The trainer labels a click
  without a capture, so the collector writes `frame_age_s: null`; the exporter's click
  record held a plain `f64`, the record failed to parse, and the loader's "unknown
  record kinds are skipped" arm swallowed it: 637 trainer clicks came out as stop rows
  with no element, source or posture. Found because the first export had 629 blank
  sources. `frame_age_s` is now `Option<f64>` everywhere (a trainer click has no frame
  age; that is a state, not a NaN).
- 2026-09-04, **Phase C on real data** (blob `70289bb2`, five click sessions 08-30 to
  09-04: two passive days, 376 tree/vision clicks; three trainer days, 637 trainer plus
  226 vision clicks; `loso_clicks.py`, one median row per click, leave-one-session-out).
  Firmware alone: per-click median **1.83°**, p90 5.21°; by source trainer 1.69°, tree
  2.10°, vision 2.22°, **caret 10.4°** (the I-beam ruling of 08-28 is wrong as a gaze
  label: a click into a terminal or an input says nothing about where the eye was, and
  carets are refused by the fit). Within a click the residual settles to its value by
  350 ms before the window's end and stays, so the window is not the noise. Models:
  global offset 1.67°; the per-session offset *oracle* 1.49°; the old quadratic screen
  field, pooled, 1.66°; the kernel model on **per-eye directions and angle from the
  axis only** (length scale 2, ridge 3, 200 centres, plus the linear pupil and radial
  terms) **1.44°** (p90 4.47°), against 1.66° with all 27 features and 1.69° with the
  head columns added, i.e. head position and the lagged head hurt held out at this
  data size. A plain 2D field on the combined ray's angles gives 1.63°; the per-eye
  form also handles monocular clicks (2.04° vs 3.05° firmware). Per session the model
  reads 2.85 / 2.64 / 1.26 / 0.98 / 1.34° against 2.05 / 2.90 / 1.72 / 1.43 / 2.18°:
  every trainer session about 30% better, the first passive session **worse**, because
  the August sessions carry a pitch bias of the opposite sign (−0.7° vs +1.1 to +1.4°)
  and a global correction moves them the wrong way. Kernel plus the per-session offset
  oracle: 1.22° (−33%), so the next term is the day's offset, which is D5. Within the
  same session (09-04) trainer buttons sit 0.8° of pitch above vision buttons, so part
  of that offset is the source, not the day; whether the eye rests above a trainer
  button's centre or the trainer's theme flips move the pupil is open. Gate: 21% on
  the median, one session worse; not met as written, built anyway to be felt.
- 2026-09-04, **D1–D3 built.** `model.rs` (the Nystrom kernel model, whitening and
  ridge folded into per-centre weights and one variance form; the label's inverse as a
  Newton step on tangent offsets, since the label's frame is built at the truth rather
  than at the firmware ray and a naive fixed point drifts by 0.3°), `train.rs` (Jacobi
  eigensolve, Cholesky, k-means++, leave-one-session-out; no `nalgebra`), `gaze-et5-cli
  fit`, and the provider path: firmware 2D → ray on the declared plane → features →
  correction faded by variance (full to the training rows' 95th percentile variance,
  gone at three times that) → rotated by the mount pitch the labels used → intersected
  with the *configured* desk, not the solved poses, so the runtime frame is the label's
  by construction. Sigma widens by the model's variance. Rust reproduces the harness
  (1.82 → 1.43°). The exporter's per-stop row is now a median (the mean sat 0.15°
  higher from saccade frames the gate misses). Not run live yet: `gaze-proto
  --provider et5` with `config/model-et5.json` present is the next thing to sit down
  for. Expected feel on a September-like day: ~1.0–1.3° median instead of ~1.5–2.2°;
  on an August-like day, no better, until D5.
- 2026-09-04, **first live run.** `gaze-proto --provider et5` with the model: the user's
  verdict was "way better" than the firmware alone. No measurement, just the feel.
- 2026-09-04, **literature pass, then three offline experiments on the same 1184 clicks
  (`model/label_timing.py`, `model/loso_clicks.py`).** *Label timing* (PACE, Huang et al.
  CHI'16, found gaze-click agreement peaks a second before the press): not here. The
  per-frame firmware residual is flat from 0.3 s before the press to the press (1.75–1.8°
  median per 100 ms bin), rises steeply earlier (5° at −1.2 s: the eye is still arriving)
  and after +0.1 s (the eye has left). The collector's window is on the minimum; the last
  stable fixation before the press is 0.14° better (1.72 vs 1.86), not worth the
  machinery. *Pupils* (Drewes 2014: 2.5° drift over 2–6 mm, per eye; 2025 reading study:
  0.4–0.9°/mm): nothing learnable. Pupils in the kernel 1.56, a pupil² explicit term 1.50,
  no pupil terms at all 1.49, against 1.44 for the plain per-eye-direction kernel. The
  one large-pupil session (5.3 mm vs 3.1–4.2) is also the one with the opposite pitch
  bias, which is consistent with the effect but is a single session; unlearnable by
  leave-one-out. *Per-eye residuals*: the mean of the two eyes' own residuals reproduces
  the firmware label to 0.45°, but two per-eye kernels combined land at 1.46 — the
  firmware's combination is not where the error is. *Causal online offset* (an EWMA of
  each click's leftover, gain 0.1, clipped at 2°, gated at 3°, clicks in time order,
  every click including the first): **1.20° median, 2.06 p75, 4.24 p90**, against the
  1.22 oracle and 1.44 for the model alone; 1.19 with a tighter clip, 1.36 ungated. The
  steady state after twenty clicks is the same as the whole. Firmware plus offset with
  no model: 1.38. The August session that got worse under the model (2.85) comes back to
  1.58 with the offset; the other August session (2.6) does not move under anything.
  The tree-vs-trainer gap noted earlier (2.7° vs 1.2°) is this same confound: tree
  clicks exist only in the two August sessions.
- 2026-09-04, **D5 built** (`offset.rs`, provider `observe_click`, `gaze-proto`
  `feedback.rs`): the EWMA above, in the label's yaw/pitch frame, added to the model's
  prediction before the ray is corrected (the model's part still fades with variance;
  the offset applies everywhere). Real clicks come from `gaze-clicks`'s evdev reader in
  `gaze-proto`, pointer from the compositor's cursor session, attributed against the
  corrected rays in the 0.4 s before the press (median leftover, at least three rays).
  Not offered: presses on `gaze-inject`'s own uinput mouse (excluded at the reader, for
  `gaze-clicks` too) and presses within 12 px of the last warp destination, both being
  the gaze clicking itself. Persists to `config/offset-et5.json`, keyed to the device
  blob like the model; `--freeze-offset` attributes and logs without moving it. Not
  built from the plan's D5 line: the head-jump reset. The trainer sessions carry
  deliberate posture changes and the simulation tracked through them at this gain, so
  a reset would only discard a converged estimate. Not run live yet.
- 2026-09-04, **snap engine: a re-detect could leave a stale highlight.** Detection ids
  restart at zero every pass, and `SnapEngine` identified its current target by id
  alone: a new box wearing the old number was "the same element", so `adopt` kept the
  old rectangle and only moved the click point, and the hysteresis let the stale box
  defend with the new box's score. Identity is now id *and* box. Found while chasing a
  live report of the highlight parking while the gaze marker moved; whether it is that
  bug or a wedged perception pass is not yet known (the `detected` info lines say).
- 2026-09-04, **snap engine: the kind prior no longer overrides containment.** Live
  report: with D5 running the marker followed the eye but the highlight could not be
  nudged from a sidebar row onto the row beside it ("Archive pull request" to "Lock
  conversation", "1 participant" to the avatar). Not the hysteresis: running the
  detector on the crops and the engine on its boxes showed same-kind neighbours switch
  cleanly, and a Button beside a Text row never does. `KIND_PENALTY[Text] * kind` was
  1.5° of score, so a control held from 0.3° away against a text row the gaze was
  *inside*; at a 36 px row pitch (0.4–0.5° on the LG) no gaze position in the text row
  could win. Bench v3 chose kind 1.5 on widget-correct, an eval in which text is never
  the answer. Fix: the kind penalty is charged only to boxes the gaze point is outside.
  Inside a box the position has been observed and the prior has nothing to add; nested
  boxes fall to the area term, and a click on the smaller lands in the larger anyway.
  Bench, σ 0.7° single-sample, before → after: all targets 25.0 → 25.5% correct, widgets
  32.2 → 30.2%, text lines 25.0 → 25.8% with confident-wrong 10.1 → 16.7% (a text run
  under a noisy first landing is now taken). That is the first-landing regime; the live
  complaint is steady fixation, where the position is far better than 0.7° and the prior
  deserves even less. Lowering `kind` alone (0.6 / 0.3 / 0.15 → all-targets 26.8 / 27.6 /
  27.8%) was measured too, but any residual penalty still stacks with the 0.15 hysteresis
  margin at that row pitch, so the structural rule won. `gaze-bench/report.md` is *not*
  regenerated: the README's canonical command no longer reproduces it even on the old
  code (23.5% `none` where the committed report has 0%), so the bench inputs or defaults
  have drifted since it was written and that needs its own look before a new canonical
  report means anything.
- 2026-09-04, **snap candidates are now logged.** Second live edge case: "techgeek1" in a
  PR header lost to the overlapping title text above it. The crop's detector boxes do
  not overlap and the engine picks the line under every geometry I could reconstruct, so
  `gaze-proto` now debug-logs the top three candidates (kind, box, score, distance,
  area, centre term, gaze) on every target change. The next report carries numbers.
- 2026-09-04, **snap engine: the containment rule becomes a ramp, and area doubles.** The
  numbers came back. The detector drew one 585 x 112 px `Text` box over the two-line PR
  title; the OCR line "techgeek1 wants to merge" starts 27 px above that box's bottom. At
  gaze y 296.37 against a line top of 296.70 (a third of a pixel out) the line paid the
  full 1.5° text penalty and the heading, containing the point, paid nothing: the step
  in the containment rule. Once held, the heading defended inside the margin (0.63 vs
  0.70). Fix one: the kind penalty ramps in as `min(1, distance / sigma)`, the odds the
  point is really outside; a hair out costs a hair. Fix two: even inside, the line only
  beat the heading by 0.07 because `center_deg` charged it 1.7° for being read at its
  first word, so `area` goes 0.15 → 0.3 and the line wins by 0.24. Measured and
  rejected: the centre offset along the short axis only (fixes the case, −5 points on
  widgets in both bench modes), and the same blended in by the ramp (−5 as well). Bench,
  σ 0.7 single-sample, correct all / widgets / lines: original 25.0 / 32.2 / 25.0, step
  rule 25.5 / 30.2 / 25.8, ramp 26.6 / 29.5 / 27.0, ramp + area 0.3 (shipped) 27.0 /
  30.5 / 26.9; sequence mode agrees. Widgets are still 1.7 under the original, which
  bought text rows that can be looked at. Same caveat as before: the bench's noise is
  bias-dominated first landing, the live complaint is steady fixation on a good day.
- 2026-09-04, **edge scrolling built (tier one, principle 4), on the real scroll surface.**
  The trigger is a dwell in the lower band (0.2 of the surface's height, 0.25 s) or upper
  band (0.12, 0.5 s) of the surface under the gaze, the speed grows with depth into the
  band up to 8 lines/s, ramped over 0.3 s, and the loop self-regulates: the text rises, the
  line being read leaves the band, the scroll stops (Kumar & Winograd, UIST 2007).
  Looking anywhere else stops it too; a blink coasts 0.25 s. Motion goes out as
  `REL_WHEEL_HI_RES` at the sample rate with a `REL_WHEEL` click every 120 units, the
  pointer warped into the surface once at the start if it was outside. **The surface is
  never the window**: Discord's list ends above its composer, a sidebar scrolls apart from
  the page. The accessibility tree knows the real one, and the stop condition turned out to
  be geometry, not role: AT-SPI's `scroll pane` is used by GTK alone; Firefox scrolls a
  `document web`, Discord a `section`/`panel`. But every toolkit reports a clipping node's
  extents as its viewport and the clipped child's as its full layout, so the first ancestor
  whose child pokes out vertically is the surface (`gaze_a11y::clip_surface`). Measured
  with the new `gaze-a11y-cli chain`: Discord list 882 x 4305 in a 898 x 1296 panel above
  the composer, Discord sidebar 2272 px in 302 x 1292, Firefox page 12901 px in the
  1269 x 1343 document, all correct; content that fits yields no surface, which is right.
  The question runs on `gaze-clicks`'s watched tree thread (10–30 ms, wedge-proof) via a
  containment-keyed cache; terminals and iced apps are off the bus and do not scroll, and
  the log says so. First live try did nothing in a browser: the cache polled the tree
  with `take(id, Duration::ZERO)`, which computes the time left before it ever reads the
  channel and so never collected a reply; the surface was never known. `TreeService::poll`
  is the non-blocking read it needed. Whether Firefox honours `REL_WHEEL_HI_RES`-only
  motion is still unverified (`gaze-inject-cli --smooth X Y -960` at a page is the test;
  the pointer has to be free for it). Run: `cargo run --release -p gaze-proto --
  --provider et5 --edge-scroll` (add `--dry-run` to watch the log first).
- 2026-09-04, **edge scroll, second live run: "feels pretty nice", three fixes.** (1)
  Bands 0.2 / 0.12 → 0.1 / 0.1 of the viewport. (2) At the end of a page the lower band
  swallowed the elements in it: the surface now carries how far the content overflows each
  way (`Scrollable::above_px/below_px`, from the clip's child extents), a band whose
  direction has under 2 px of room is not a band, and a scroll that runs out of room
  stops; the cache re-asks the tree every 400 ms *during* a scroll so the room tracks the
  moving content. (3) Boxes were offset and took about a second to catch up after a scroll:
  the perception pass detected outputs in config order and published the merged list only
  at the end of the pass, so a scrolled output waited behind the others (~450 ms each).
  Now a stopped scroll forces a re-detect of the output under the gaze, forced outputs go
  first, every output publishes as soon as its detection lands, and the highlight is
  hidden (label `redetecting`) from the stop until the store's generation advances, so
  the old boxes are never drawn over moved content. The remaining latency is one
  detection (~450 ms) plus up to one capture period (200 ms).
- 2026-09-04, **edge scroll: elements scrolling into view got selected and broke the
  scroll.** The 400 ms in-scroll refresh re-asks the tree at the gaze point, and under a
  moving page that hit can land on a fixed header, a nested scrollable or a recycled list
  row: a different clip, or none, replaced the surface, the scroller saw no band and
  stopped, the forced re-detect published and the highlight returned on whatever was under
  the gaze. The in-scroll refresh now accepts only a reply naming the same clip node (bus
  and path) and drops the rest, so a scroll can only end by the eyes leaving the band, the
  room running out, or the tracker losing the eyes.
- 2026-09-04, **no retargeting through a scroll without an eye movement.** Ruling: content
  moving under a still gaze is not the user choosing what it brings. From a scroll's start
  until a fixation *begins* later than its stop (the I-VT filter's `since_s`), the snap
  engine is not updated and the target is `None`; a fixation that persisted through the
  scroll, or began during it, does not count. The engine is reset at the start so the
  pre-scroll target cannot defend itself afterwards. The overlay label reads `move eyes to
  retarget` while the block holds after a stop.
- 2026-09-04, **edge scroll survives the eyes leaving the screen.** Reading fast down a page
  the eyes run past the bottom bezel while still tracked: the ET5 sample keeps its ray and
  `valid`, loses its point, and the scroller read that as a dropout and stopped after
  0.25 s. Now `OutputGeometry::project_px` puts the ray onto the surface's own panel
  extended past its edges, and the scroller takes `Eyes::Off(px)`: past the edge it is
  scrolling towards it counts as full depth and the scroll continues; past any other edge
  it is a look-away and stops; and it never *starts* a scroll, because the eyes below the
  monitor are as likely on the keyboard as on the page.
- 2026-09-04, **edge scroll: faster when asked.** "If I know I just want to go to the top
  that's very slow." Two multipliers on the band speed, both flags: a *hold* that starts
  after 1 s parked in the outer 40% of the band and doubles the speed every second after
  (reading pace leaves the band every few hundred ms and never sees it; leaving the outer
  band resets it), and a *turbo* of 3x while the tracked eyes are past the screen's edge,
  so looking above the monitor for a couple of seconds is "go to the top". Everything caps
  at 40 lines/s so a runaway stays readable. Ramp 0.3 → 0.2 s on request. Home/End keys
  deliberately not used: they go to the keyboard focus, not the surface under the gaze.
- 2026-09-04, **edge scroll: PR review by eye.** First real task done with gaze alone,
  reviewing a PR: "felt a whole lot better than just scrolling but nav was a tad slow".
  Ramp 0.2 → 0.15 s, bands 0.1 → 0.115 of the viewport, both directions. Small steps on
  purpose: the band edge is also where clickable things at the bottom of a page live.
  Second pass the same day: bands 0.12 ("still a tad small"), hold 1.0 → 0.3 s (0.6 "still
  a little laggy"), ramp kept. Verdict: "we validated the premise"; the visuals are the
  distraction now.
- 2026-09-04, **edge scroll: the line-label edge case.** In a PR diff the targeting "wants
  to prefer the line labels rather than the scroll". Not reproduced with a chain dump, but
  the geometric clip rule has an obvious hole that fits: a line-number cell whose text
  node overhangs it by a pixel or two *is* a clip by the raw test, and taking the first
  clip up the chain hides the page scroll behind a 20 px viewport the scroller then
  refuses. `clip_surface` now walks past clips under 120 px (`MIN_CLIP_PX`) instead of
  stopping at them. If the label case persists, `gaze-a11y-cli chain X,Y` on a line
  number is the next evidence to collect.
- 2026-09-04, **wink-to-click: measured and dropped.** Idea: left wink = left click, right
  wink = right click, single wink (a double "feels awkward in repetition"). Before writing a
  detector, the negative class was measured from the eight recorded trainer sessions
  (`config/sessions`, 1528 s of frames): the tracker drops exactly one eye for 150 to 600 ms
  191 times, once per 8 s; requiring both eyes tracked cleanly before and after leaves 46
  (per 33 s); requiring the open eye within 1 deg and the head within 2 mm over the episode
  leaves 14 (per 110 s). The left slot drops half again as often as the right, consistent
  with the tracker sitting to one side. Trainer sessions are a harsh sample (posture
  changes, frames only around clicks) and there is no positive class, but a phantom click
  every couple of minutes is not a starting point worth a shared-type change. Ruling: "no
  point in using wink gestures"; commit stays with the Daydream controller (§10 plan).
- 2026-09-04, **Daydream controller: first light.** Paired and read over BlueZ's D-Bus GATT
  API with `zbus` blocking (no `bluer`, no tokio; the `bluer` plan above is superseded):
  one match rule over the device subtree carries both the report `Value` and the device's
  `Connected`, and `StopNotify`'s own `Notifying` change is the reader's wake-up. Stream
  measured: 62 Hz, 20 bytes, zero drops over a minute, gyro noise at rest under 0.03 rad/s,
  gravity +1 g on the controller's y (the pad normal), so yaw is about y and pitch about
  x. mrdoob's layout holds bit for bit; one captured packet is the fixture. `gaze-proto
  --daydream` maps pad click → commit, Home → exit, App → redetect, volume → wheel, and
  makes a thumb on the pad the clutch for the fine channel: anchor at the snap point or
  the raw gaze point, pointer warped there, gyro (default, 1500 px/rad, dead zone
  0.03 rad/s) or touch deltas move it, commit clicks the refined point. The raw-gaze
  anchor is the unlabelled-target escape hatch the recogniser never had. First hand-on
  session (same day): pad x runs 0→1 left to right and y 0→1 top to bottom, as assumed;
  pitch tracked, yaw seemed not to, but `watch`'s per-axis net turn (added for this)
  measured yaw right as −y and pitch down as −x, exactly the default `-y,-x`, so the
  mapping stands and the horizontal misbehaviour was the two faults below; the gaze kept
  retargeting and edge-scrolling under the thumb, so a thumb on the
  pad now locks the gaze out entirely (no retarget, no scroll, no focus warp); and driving
  each refine step through the closed-loop `move_to` at 62 Hz made the cursor visibly
  grow, the closed loop's measure-and-correct flurry reading as a shake. Refine motion is
  now one plain relative report per controller report (`Injector::move_by`) and the
  commit clicks where the compositor's cursor tracker says the pointer is, not where the
  deltas add up to. Second session, with those fixes: gyro yaw still moved the pointer
  next to nothing and the channel was jittery, so the touchpad is now the default refine
  channel (`--refine touch`); the gyro stays as an option. The pad's dead horizontal was
  `raw`'s to explain: x steps smoothly 0.01→0.97, but every sideways sweep runs the thumb
  off the pad edge, and each re-land was a fresh `Refine::Begin` that re-anchored at the
  snap point and warped the sweep away. A re-land now resumes the standing refine; only
  the eyes moving on starts a new one. Ruled the same day: the refine is boxed to 100 px
  around the anchor (`--refine-range`), since gaze already lands within a degree or two
  and a full-screen range was fiddly; the touch gain dropped to 250 px per pad width so
  one pad count is about one pixel and half a pad crosses the box. Also from that
  session: aiming at GitHub's copy-path icon in a file header, which sits at the top of
  the diff viewport, armed the edge scroller before the thumb could land. Eyes on a snap
  target no larger than 48 px (`AIM_TARGET_PX`) now keep the scroller from arming; rows,
  text links and paragraphs are wider and scroll as before, and a scroll already running
  is not interrupted by passing over an icon. Follow-up the same day: OCR words are under
  48 px too, and a scrolled page is words to its edge, so every one of them snapped in the
  band and held the scroller off, which made scrolling text jerky and stop-start. Text
  boxes no longer count as aiming whatever their size; buttons, icons, links and inputs
  still do.
- 2026-09-05, **Offset keyed to posture; pad commits as labels.** The day's-bias filter held
  in the calibration posture and broke on a comfortable slouch, and a single bias relearnt
  over twenty clicks cannot follow a body that moves between the two several times an
  hour. The ET5 reports eye positions, so the offset is now a function of them: anchors in
  tracker space, one per posture (40 mm reach, the head wobbles 10 to 20 mm within a
  posture and moves 80 or more between sitting up and slouching), each holding its own
  yaw/pitch, blended by a Gaussian in distance and relaxing to their mean far from all.
  A click farther than the reach from every anchor founds a new one at the blended
  prediction and its clicks enter at `1/(n+1)` over a two-click prior, down to the usual
  tenth, so a new posture has half its correction in four clicks and most of it in ten,
  and returning to an old one costs nothing. Daydream pad commits now feed the filter beside mouse presses: a refined
  commit is the best label the system gets (the user moved the pointer to exactly what
  they were looking at), and an unrefined one is the snap centre the user accepted.
  Mouse-button commits still are not fed, since the click feed already sees the physical
  press. First slouched session: a second anchor spawned without the user moving and
  the offset swung a degree in one click. Two causes, both fixed: the posture key was
  the mean of the *valid* eye origins, which a monocular frame moves by half an
  interpupillary distance (~32 mm), so the key is now the binocular midpoint rebuilt
  from the last known eye spacing when an eye drops, and the reach is 60 mm; and a new
  anchor took its first click in full, so one ±1° label owned the posture, hence the
  two-click prior. Same session: the perception thread redetected HDMI-A-1 every half
  second for the whole run (30% of its pixels changing, 400 ms each, 770% CPU), almost
  certainly the session's own terminal scrolling its log on that panel. Ruled: the
  tracker reaches DP-1 only, so the desk config gained a per-output `detect` switch
  (default on) and DP-2 and HDMI-A-1 are off; they stay in the geometry for the pointer.
- 2026-09-05, **Controller and mouse take turns; tap right-clicks.** Moving the mouse left
  gaze unable to take the pointer back. Best explanation from the code: a refine the thumb
  left standing (no commit, and no retarget to clear it, which on an unlabelled target is
  never) resumed on the next touch without a warp, from wherever the mouse had put the
  pointer. A standing refine now resumes only while the pointer is within 24 px of its
  point; otherwise it is stale and the touch is a fresh gesture with its warp. The
  arbitration the user asked for is the controller's own sensors: a held controller never
  reads a gyro under 0.035 rad/s for long (on the desk it peaks under 0.03), so gyro motion,
  touch or a button within 6 s means held (first cut was 0.06 within 2 s; an arm resting at
  the side sat under that and read as put down). A controller put down hands the pointer to
  the mouse: no edge scroll, no focus warp, overlay says "controller down". The mouse also
  takes the pointer the moment it moves, the way a mouse takes over from a gamepad in a game
  (the `gaze-clicks` reader stamps every motion, wheel or button event, and now opens
  whenever a controller is present, not only for a learning source; overlay says "mouse").
  It keeps the pointer until the controller is used on purpose: a touch, a button, or a
  swing past 0.25 rad/s, which lifting it off the desk does and a hand resting around it
  while the other hand mouses does not. Picking it up
  restores gaze within a report. A tap on the pad (down and up within 250 ms, under 0.04
  pad widths of travel, no click) commits with the right button at the same point a click
  would, so context menus need no mouse. Untested on the desk: all three.

- 2026-09-09, **Remount to the 27" (DP-2); the edges were the code, not the tracker.**
  The bar moved from under the ultrawide to under the 27": its 600 mm sit inside the
  gaze envelope at the desk's distance where the 880 mm never did. Three things found
  while checking the range with `view --direct DP-2` (a plane-only calibration, no fit):
  (1) the firmware does **not** clamp its 2D output under a plane declared by corners
  (readings to 1.68 while looking below the panel), so the provider's "pinned at the
  bound" logic never fired and a reading a hair past an edge fell through to the ray
  path instead; now a reading within 3% of the panel maps to the edge at ordinary sigma
  and the 2.5° edge sigma is gone. (2) The ray path's edge clamp refused anything more
  than 25° from the panel *centre*, and this panel's corners are 28° from its centre
  from the eye, so a look just past a corner vanished; the gate is now 6° beyond the
  panel's own extent, measured at its corners. (3) A calibration with no output entry
  for the direct display disabled it, so single-eye frames (the common case at an edge)
  had no surface to land on. The mount itself reads 8° low in elevation and 33 Hz even
  when fully tracked; both to be revisited after the tilt is set.
- 2026-09-09, **The 33 Hz was never a mount problem: it is the ET5's gaze rate.** Every
  session on disk, old mount and new, streams at exactly 33.1 Hz (median 30.2 ms), and
  `frame_counter` advances by four per notification with `timestamp_us` at 7552 µs per
  tick, so the camera runs at 132.4 Hz and the firmware publishes one frame in four.
  The device says so itself: op 0x672 (nottobii's `FREQUENCIES_GET`) answers `(132, 33)`
  and 0x686 lists that as the only pair; the guessed set opcodes (0x668, 0x67c) change
  nothing. Tobii's specification for this unit reads the same way, 133 Hz image sampling
  and 33 Hz gaze (search-engine summary of the Help Center specifications page, which
  blocks fetching; treat as probable, not quoted). The 90 Hz that the code assumed was
  the 4C's rate. Consequences fixed: the retrain gate was "60 of the last 120 frames"
  capped at 2 s, which at 33 Hz meant naming the target for 1.8 s of the last 2 s, three
  times Talon's hold; it is now 0.67 s of 1.33 s (22 of 43 frames) derived from
  `GAZE_HZ`. The other rate-cited constants (fusion EMA, click-window minimum, lag gap,
  channel depth) were harmless at the true rate and only their comments changed. `info`
  now prints the device's rates and no longer promises that the rate rises with aim.
  Stream 0x500 is "gaze"; the device also lists 0x501 image, 0x504 presence, 0x508
  image_collection, 0x50e primary_camera_image, and 6000-6004 algodbg / is5_sync_stream
  / log / custom (op 0x4b0). Nothing here subscribes to any of them.
- 2026-09-09, **The flywheel writes, and the gate can be outvoted.** E1: every click
  the online offset is offered is now a JSONL record under `config/flywheel/<UTC
  day>.jsonl` with the window's median features, the firmware ray's residual against
  the clicked point, the model and offset in force, the leftover and the verdict;
  `ClickRecord::row` rebuilds the exporter's row so E3 can pool days with sessions.
  E2: rejects are no longer dropped. Four within ten minutes at one posture that agree
  to within 1° on targets 30 mm apart are a consensus, and the posture's anchor jumps
  to their median in one unclipped step (`click adopted` in the log, `jumps` in the
  offset file). Reason: a bias past the 3° gate rejected every click forever and the
  filter could never recover; adopting was ruled over merely warning. Nothing driven
  yet; the first flywheel day will say what the reject rate actually is.
- 2026-09-09, **The overlay stops shouting (PLAN-UX.md U1).** The debug overlay (cyan
  ring on the fovea, amber box on whatever the engine favoured, a caption) made text
  hard to read for the movement around it. The session now sends an intent, where the
  gaze is, whether it is moving, whether anything clickable is near and which element
  is favoured, and the overlay presents it on its own clock: an 8 px accent dot with a
  contrast halo that exists only near an interactive element and thins to a ghost once
  a fixation has held 150 ms on a target, a 150 ms tapering trail behind it in flight
  for the feel of motion, and a rounded accent highlight 3 px outside the element that
  crossfades between elements (60 ms in, 140 ms out). Text elements are not marked
  unless asked (`--highlight-text`), so prose stays unmarked while read; the price is
  that a commit on an OCR word is blind. Accent and radius come from the COSMIC theme
  via cosmic-config and follow it live. Frames: the presenter keeps a surface dirty
  while a fade or trail is in progress and stops asking for frame callbacks when it
  settles, so a settled gaze costs no repaints. Debug look still behind
  `--overlay-debug`; the ceremonies still draw with it. Verified with
  `gaze-overlay-cli --pointer` live and `--render`. **Driven the same day**, and three
  of the choices went: the trail read as a rendering bug at 33 Hz, the ghosting looked
  like the dot doing something while the eyes held still, and the 3 px inflation made
  boxes read bigger than their widgets (Discord and Reddit worst). Highlights were also
  appearing on elements the eyes were nowhere near, because the display followed the
  engine's 2° snap radius; the dot and highlight are now gated on `--near-deg` (0.8°)
  while the engine keeps `--snap-deg` (2°), so a commit past the gate lands unmarked.
  The dot now holds one weight while shown; the highlight sits 1 px outside the box.
  Two edge cases from the same session were the recogniser's kinds, not the overlay: on
  Discord it called the short last line of a message a `Button` at 0.9 (marked) and the
  channel list `Text` (unmarked). Pixels cannot settle either, so the pointer look now
  asks the accessibility tree once per place the eyes settle (`gaze-proto/src/verify.rs`,
  on the tree thread, polled) and takes its answer over the recogniser's: Discord
  answers `list item` 880x71 for the message row and `link` 286x32 for the channel, and
  a control taller than 56 px and wider than 240 is treated as text, so the row is not
  marked and the channel is, with the tree's box. Firefox answered nothing on a Reddit
  tab at the same time; unexplained, and that page runs on the recogniser meanwhile.
  The weight was the filter: the tracker's one-euro cutoff of 0.3 Hz followed a drifting
  eye with a half-second lag inside a fixation while saccades landed raw, and the
  contrast read as drag. The dot now rides a critically damped spring in the overlay,
  stepped at frame rate (settle 0.2 s, `--pointer-settle-s`), and the stack's cutoff is
  1 Hz; the spring is where the smoothing lives, the stack only steadies the snap point.
  The dot also blinked at the edge of the near gate; it now lingers 0.3 s after nothing
  is near and the gate closes at 1.5x the distance it opens at.
  Then the overlay went intent-only: shown while a thumb rests on the Daydream pad or
  F14 has latched it on, nothing otherwise. Reason: half of the remaining noise (titles,
  punctuation runs the recogniser calls controls, a dot on prose before the tree
  answers) is only noise while reading, and while aiming the same marks are the point.
  Tobii's desktop software and Talon both settled on show-on-intent. The thumb landing
  now only arms; the refine begins once the thumb is 12% of the pad from its landing
  point after a 120 ms settle (12 px of summed travel started a refine on every touch:
  the finger flattening onto the pad moves its centroid that much on its own), so a
  resting thumb shows where the eyes are and gaze keeps driving until the thumb nudges.
  With input on intent, edge scrolling went the same way: thumb down or latched, the
  eyes point and the scroller sees nothing; thumb up, the eyes read and may scroll, no
  control is marked, and the band they are in or approaching is drawn as a faint zone
  with the dot, deeper while the scroll runs. The old aiming guard (eyes on a small
  control hold the scroller off) went with it: it was gating on the recogniser's boxes,
  which the near gate and the tree verdicts had since stopped trusting. So did the
  controller's held/idle arbitration against the mouse, and the detector now sleeps
  while the thumb is up (the fans came up while scrolling by eye, and no box is wanted
  then); an edge scroll puts the pointer back where it borrowed it from, and a commit
  lands on the marked element's centre, since the highlight is the promise. YouTube's
  page in Firefox offered a scroll up at the top of the page and none down: a
  `section` with `-1x-1` extents sits between the page and the document, and taken as
  the document's child it "overflowed" 106 px above it. Nodes without extents are now
  left out before the overflow test (`gaze-a11y`). Its mix list was missed for the
  opposite reason: the rows are direct children of the 356 px clip, so the one row on
  the ancestor chain fits it and there is no single overflowing child to find. Each
  ancestor now carries a sampled span of its first and last two placed children, and
  the overflow test reads the union of the chain child and that span (740 to 3024 for
  the list); the probe went from 8 to 10 ms. The span then invented one: the 4770 px
  page section has its masthead 56 px above it, and as a "clip" its top band covered
  the video and it had nothing below to scroll. A parent taller than the window is
  content, not a viewport, and a total overflow under 48 px is overlap; both are
  skipped. Two pointer rules from the same session: an edge scroll now warps the
  pointer to the gaze point every time, since a pointer merely inside the page may be
  over the mix list and the wheel goes to the innermost scroller under it; and a refine
  anchors on the marked element when there is one, because a pad press moves the thumb
  enough to begin a refine, and the click it ends in must start from what is marked.
  Live, the mix list's band then scrolled the page: the surface cache served an answer
  for as long as its viewport contained the gaze, and the page contains the list, so
  eyes arriving in the list kept "page" for up to two seconds and the dwell started a
  page scroll. The cache is now keyed by where it asked: an answer is served within 48
  px of that point, a gaze further away is asked about again within 100 ms and gets
  nothing until the reply lands.

- 2026-09-10, **The prototype becomes a daemon and an applet (PLAN-UX.md U2, U3).**
  Decided first what not to carry: no tier toggles (clicking, edge scrolling and the
  tree verdicts are always on; the paired Daydream controller is used when it is awake
  and retried while it sleeps), no wheel routing to the window under gaze and no
  focus-follows-gaze (both moved the pointer on their own initiative, which the
  borrow-and-return model of 2026-09-09 is the answer to), no gyro refine. Forty flags
  became: a dev harness (`gaze-proto` keeps provider selection, dry run, record, the
  debug look, plus `--tune KEY=VALUE`), three XDG directories (`gaze_config::Paths`,
  `--home DIR` for a checkout), and twenty-four tuning knobs in cosmic-config
  (`gaze_config::Tuning`, one key each under `dev.techgeek1.CosmicGaze/v1`). The
  session loop is now a library the daemon runs on a thread and steers through
  `gaze_proto::Live`: a tuning change is applied at the next sample without a restart
  (filter stack and snap engine rebuilt, overlay restyled, scroller, controller mapper
  and detector handed their new parameters), pause and reset-offset are flags, status
  goes back the other way. `gazed` serves `dev.techgeek1.CosmicGaze` on the session bus
  (ten properties, `Pause`, `Resume`, `ResetOffset`), reruns the session whenever it
  ends, and stops cleanly on SIGTERM. The applet (`cosmic-ext-applet-gaze`) polls those
  properties once a second, has the pause toggle and the reset button, and draws every
  knob as a slider under an "Advanced" fold, writing straight to cosmic-config. Smoke
  test on the desk: `gazed --home . --dry-run` came up with tracker and calibration
  true, took Pause and Resume from `busctl`, reloaded twice for two edits of
  `edge_dwell_s` while running, and ended on SIGTERM with the session summary. The
  residual model was refused at start as fitted under a different device blob (the
  2026-09-09 recalibration orphaned it), so `Model` reads false until `gaze-et5-cli fit`
  is run again; not a daemon problem. Not yet driven: the applet under a real panel,
  and the daemon with clicks live. Same day, the calibration ceremony stopped drawing
  the debug look: its target is now the pointer look's ring and dot in the theme's
  accent, filling as the hold progresses, with the captions moved to the log
  (PLAN-UX.md U1). And the webcam provider went: `gaze-provider-webcam`, its sidecar
  socket, the `[camera]` block of the desk file and the `webcam` filter preset, none of
  which had been run since the ET5 arrived. `gaze-bench` and the Python model harness
  went the same day (the Rust `fit` reproduces the harness's numbers; the bench's report
  was Phase 0's). The trainer stays, to bootstrap the flywheel again if it is ever
  needed, and the click collector stays with it: the trainer's presses reach the data
  through the collector's socket.
- 2026-09-10, **The offset is the whole correction: the residual model, its pipeline
  and every non-ET5 path removed.** Ruled the same evening, reversing the trainer
  decision above. The case: the model's held-out gain was 21% (1.83° to 1.44° median,
  one session worse, "gate not met as written, built anyway to be felt"), which sits
  under the 2° snap radius; the larger term in that analysis was the per-day bias, which
  the online offset with its consensus jump now learns from ordinary clicks; and the
  desk had been running without the model since the 2026-09-09 recalibration orphaned
  it, which is the "working well as is" that prompted the question. Removed: `model.rs`,
  `train.rs`, `dataset.rs`, `record.rs`, `flywheel.rs`, the old `sweep.rs` with its
  pose solver and triangulation, the CLI's fit/record/flywheel/sessions/dataset/refit/
  collect commands, `gaze-trainer`, the click collector (`gaze-clicks` keeps the mouse
  reader and the tree thread the session uses), `gaze-provider-synthetic` and the replay,
  `gaze-proto`'s Lenovo button reader and its provider/record/truth flags, the
  hit/slip/miss scoring, the overlay's truth cross, `gaze-core`'s noise model and the
  desk file's `[noise]` section, the `Model` bus property. Three commits, about 23,000
  lines. Two things changed rather than went: the `GazeProvider` trait moved into
  `gaze-core`, and the offset no longer lives inside the model state. It runs whenever
  there is a direct-mode calibration, keyed to the calibration's blob hash, and moves
  the calibrated point (after the field and the head gain, which the model used to
  bypass) in angle space: the ray from the eyes through the point, rotated by the
  offset for the current posture, re-intersected with the desk; a click's leftover is
  measured against that ray. The desk file keeps the nominal eye and tracker (the snap
  engine's angular scale and the ceremony's training rectangle are measured from them)
  and panel curvature; the poses of the two outputs the tracker never reaches went, and
  `position_mm` became optional. Checked after the change: `gazed --home . --dry-run`
  comes up on the trimmed desk file with the offset loaded and the real mouse read for
  labels, and `calibrate --dry-run` prints the same plan; not yet driven with clicks
  live. U4 is now
  self-contained setup (PLAN-UX.md): calibrate in the daemon, a generated desk file,
  the ONNX models as a download.

## 11. Open questions

- nottobii code quality/completeness as a base vs. writing a fresh ET5 driver against its protocol notes.
- **Multi-display — decided 2026-08-25: one tracker at the seam, precision cone + graceful degradation.** Layout (photo 2026-08-25): LG 38GN950 (38" 21:10, 3840×1600, 880×370 mm, 2300R, DP-1 at logical 2559,0) right; VIOTEK GNV27DB (27" 16:9, 2560×1440, 600×340 mm, 1500R, DP-2 at 0,160) left angled in to meet it; 11" 16:10 flat panel (1920×1200 @ scale 2 = 960×600 logical, ~237×148 mm, HDMI-A-1 at 1506,1600, flaky on the output list but always rendering) on the desk below the seam with its top edge at the big displays' bottom-bezel line; webcam on top of the LG near its left quarter; seated ~65–70 cm from the seam; most time is spent looking near the seam. Multiple trackers rejected (placement/disambiguation too hard). Plan: a single ET5 mounted on the small display's top bezel at the seam, tilted at the face. Physical ceiling of PCCR: the glint must stay on the cornea, which is spherical only to ~±40–45° of the optical axis (11.5 mm chord on 7.8 mm radius), so gaze-vs-tracker angle beyond ~35° is unrecoverable — and head turning does not change that angle, it only keeps the near eye visible (ET5 falls back to monocular). Two envelopes: the *head box* (~±30° yaw, 45–95 cm — 3D eye positions + head pose available throughout) and the narrower *gaze envelope* (~±25° H / ±15° V about the tracker axis, inferred from the 27"@65 cm spec ceiling — accuracy degrades beyond as glints leave the cornea). Inside the cone (~60 cm wide at 65 cm: right part of left display, left part of ultrawide, the small display if not too far below eye level) → full snapping. Outside → head pose + coarse gaze drive the scroll/focus tier only (which display, which region); snapping disabled or large-targets-only. **Load-bearing unknown, first thing to verify with hardware:** the ET5 natively outputs a 2D point on one host-declared flat screen; covering three surfaces needs a gaze *ray*. Either (a) nottobii exposes the 3D gaze-direction stream Tobii licenses away, or (b) declare a large virtual screen plane to the device, run our own calibration overlay showing targets on the real displays at their projected virtual-plane positions, recover ray = reported eye position → virtual-plane point, intersect with `DisplayGeometry`. Risks for (b): firmware limits on declared screen size; calibration model rejecting off-axis points. Both are one-evening tests. Mount note: the tracker on the small panel looks up more steeply than a standard bottom-bezel mount (~25–30° est. vs ~20°) — measure eye height, wedge if needed. The existing webcam is the head-pose source for the coarse tier and the optional Phase 0 webcam stand-in. Head-mounted/glasses remains the long-term answer (one sensor, native ray); EyeTrackVR's 4.8° is why not today.
- How much of the element daemon belongs inside cosmic-comp vs. a separate process speaking a private protocol (crash isolation vs. latency)?
- Upstreaming: Talon's exit left a displaced accessibility user base; is cosmic-gaze a personal tool or a COSMIC accessibility feature? (Affects API/privacy design — visionOS deliberately never exposes gaze to apps; a compositor-level design can preserve the same property.)
- Licensing interactions if shipping: GPL tobiifree vs. nottobii; AGPL detector variants avoided already.

## 12. Reference index

**Interaction/intent:** Apple fuzzy hit testing [patent reference removed] · visionOS eye-target sizing (WWDC23 s10073, WWDC25 s303) · GazeHandSync ETRA'25 (late triggers) · BayesGaze (GI'21) · Wei CHI'23 endpoint distributions · Sticky/Magnetic arXiv 2603.26608 · CasualGaze arXiv 2408.12710 · GazeSwipe arXiv 2503.21094 · Point & Grasp arXiv 2604.22491 · MAGIC CHI'99 · BimodalGaze ETRA'20 · MagCursor 2025 · Vertegaal ICMI'08 · Hansen/MacKenzie ETRA'18 · slip-vs-miss arXiv 2603.15991 · Midas/dwell: Hirzle ETRA'20.
**Filtering:** Tobii zero-delay [patent reference removed] · Tobii I-VT whitepaper defaults · one-euro (Casiez CHI'12) · TimeGazer arXiv 2510.01561 · saccade forgiveness: Meta SIGGRAPH'24 arXiv 2401.16536 · endpoint prediction: Arabadzhiyska SIGGRAPH'17, TOG'23 arXiv 2205.01624.
**Vision/CUA:** TargetFinder arXiv 2607.19907 · GoClick arXiv 2604.23941 · OmniParser V2 + icon_detect_v3 (microsoft/OmniParser, HF PR#37) · GPA-GUI-Detector (HF Salesforce) · ReVision arXiv 2605.11212 · UFO2 arXiv 2504.14603 · ScreenParse arXiv 2602.14276 · PP-OCRv6 arXiv 2606.13108 · Agent-S2 arXiv 2504.00906 · PASTE arXiv 2603.18897 · Apple Screen Recognition arXiv 2101.04893.
**Platform:** at-spi2-core#14 (geometry) · OSWorld issues #241/#185/#105/#263 (AT-SPI perf) · Newton (GNOME a11y blog 2024-06, LWN 971541) · GTK 4.18 AccessKit · COSMIC Epoch 2/3 roadmap (System76 blog 2026-02-04) · Talon Linux exit (OSnews 145162) · libei (who-t 2026-07).
**Hardware:** nottobii · tobiifree · PSVR2Toolkit (BnuuySolutions) · EyeTrackVR docs + Hu 2025 accuracy eval (huyang.life) · TUM WearableEyeTracker arXiv 2604.24331 · Pupil DIY docs · Beyond 2e (store.bigscreenvr.com blog; vronlinux wiki) · Galaxy XR / XR_ANDROID_eye_tracking (developer.android.com) · XREAL Aura (roadtovr) · AdHawk→Google (Bloomberg 2025-03-11) · AVP measured accuracy arXiv 2406.00255.
**ET5 calibration research (2026-08-27):** nottobii `device.rs` init sequence · tobiifree issue #3 (opcode map, blob framing, wedge warning) and ChrisVeigl `update-deamon-calibflow` · Talon `talon_plugins/eye_mouse.py` · Tobii Help Center (ET5 calibration, test and recalibrate) · Tobii Pro SDK calibration concepts · patents [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] · [patent reference removed] (Google) · [patent reference removed] (Apple) · Springer 978-3-030-98404-5_36 (ET5 accuracy).
**Daily-driver accounts:** Josh Comeau hands-free coding · wolfmanstout talon-gaze-ocr + handsfreecoding.org · Talon wiki (Control Mouse gen2, Tobii setup).
