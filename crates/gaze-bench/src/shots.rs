//! Screenshots in, element sets out.
//!
//! A shots directory holds `<output>-<n>.png` files written by `gaze-capture-cli`, one
//! per output, in that output's *physical* pixels. The desk config says where each output
//! sits in global logical pixels, and the ratio of the PNG's width to the output's logical
//! width recovers the compositor scale, which is how a 1920 px wide HDMI panel at
//! fractional scale 1.15 lands as a 1670 px wide logical rectangle.
//!
//! Detection is the slow part (~400 ms per ultrawide frame), so every element set is
//! cached as `<output>-<n>.elements.json` next to its PNG and reused unless the cache is
//! stale or `--redetect` is passed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use gaze_core::{DesktopGeometry, Element, GlobalPx};
use gaze_detect::Detector;
use gaze_snap::PxScale;
use serde::{Deserialize, Serialize};

use crate::run::CandidateSet;
use crate::stats::{TargetClass, classify_target};

/// Version stamp written into every cache file. Bump it when the detector's output for
/// the same input would change in a way the bench cares about, so stale caches are
/// re-detected instead of silently producing yesterday's number.
const CACHE_VERSION : u32 = 1;

/// One screenshot and everything the bench knows about it.
pub struct Shot {
    /// Connector name the frame came from, parsed out of the file name.
    pub output   : String,
    /// Frame index within that output's series, parsed out of the file name.
    pub index    : u32,
    pub path     : PathBuf,
    /// Frame size in physical pixels.
    pub width    : u32,
    pub height   : u32,
    /// Output's top-left corner in global logical pixels.
    pub origin   : GlobalPx,
    /// Compositor scale, physical pixels per logical pixel.
    pub scale    : f64,
    /// Detected elements in global logical pixels, after the size filter.
    pub elements : Vec<Element>,
    /// The `TargetClass::Widget` subset of `elements`, renumbered densely so the ids are
    /// still indices into this slice. Used by the widgets-only candidate set, which asks
    /// whether text distractors are stealing snaps from real controls.
    pub widgets  : Vec<Element>,
    /// How many elements the size filter removed. Reported so a filtered run is never
    /// mistaken for an unfiltered one.
    pub filtered : usize,
}

/// Every screenshot in a shots directory, in file-name order.
pub struct ShotSet {
    pub shots     : Vec<Shot>,
    /// True when at least one frame had to be detected rather than loaded from cache.
    pub detected  : bool,
}

/// Adapts the desk geometry to the snap crate's scale trait through an `Arc`, so every
/// rayon worker shares one geometry instead of cloning it per engine.
///
/// The newtype exists for the orphan rule: `PxScale` and `Arc` are both foreign.
pub struct DeskScale(Arc<DesktopGeometry>);

/// On-disk detector cache for one screenshot.
#[derive(Debug, Deserialize, Serialize)]
struct CacheFile {
    version  : u32,
    /// Frame size the elements were detected from, so a re-captured frame of a different
    /// size invalidates the cache.
    width    : u32,
    height   : u32,
    origin   : GlobalPx,
    scale    : f64,
    elements : Vec<Element>,
}

// --- Shot ---

impl Shot {
    /// The elements the snap engine may choose between under a candidate set.
    pub fn candidates(&self, set: CandidateSet) -> &[Element] {
        match set {
            CandidateSet::All         => &self.elements,
            CandidateSet::WidgetsOnly => &self.widgets,
        }
    }
}

// --- Loading ---

/// Loads every `<output>-<n>.png` in `dir`, detecting or reading cached elements.
///
/// `min_size_px` and `max_size_px` filter the element set by the shorter and longer side
/// of the box respectively, in logical pixels. Filtered elements are removed entirely:
/// they are neither trial targets nor distractors, which is the point of the filter (a
/// whole terminal pane boxed as a Button is not a target a user would aim at, and it also
/// should not be allowed to win against the line inside it).
pub fn load_shots(
    dir         : &Path,
    geometry    : &DesktopGeometry,
    models      : &Path,
    redetect    : bool,
    min_size_px : f64,
    max_size_px : f64,
)
    -> Result<ShotSet>
{
    let mut paths = Vec::new();

    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();

        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) {
            paths.push(path);
        }
    }

    paths.sort();

    if paths.is_empty() {
        bail!("no .png screenshots in {}", dir.display());
    }

    // The detector is only built when something actually needs detecting: loading two
    // ONNX models costs a second or so and a fully cached run should not pay it.
    let mut detector = None;
    let mut shots    = Vec::with_capacity(paths.len());
    let mut detected = false;

    for path in paths {
        let (output, index) = parse_name(&path)?;

        let out = geometry.outputs.iter()
            .find(|o| o.name == output)
            .ok_or_else(|| anyhow!("{} names output {output}, which is not in the desk config", path.display()))?;

        let image = image::open(&path)
            .with_context(|| format!("reading {}", path.display()))?
            .to_rgba8();

        let (w, h) = image.dimensions();
        let origin = GlobalPx { x: out.logical_x, y: out.logical_y };
        let scale  = w as f64 / out.logical_w;

        let cache_path = cache_path(&path);
        let cached     = {
            if redetect {
                None
            }
            else {
                read_cache(&cache_path, &path, w, h, origin, scale)
            }
        };

        let elements = match cached {
            Some(elements) => elements,

            None => {
                if detector.is_none() {
                    detector = Some(
                        Detector::load(models)
                            .with_context(|| format!("loading models from {}", models.display()))?
                    );
                }

                let detector = detector.as_ref().expect("just built");

                let elements = detector.detect(image.as_raw(), w, h, origin, scale)
                    .with_context(|| format!("detecting in {}", path.display()))?;

                write_cache(&cache_path, w, h, origin, scale, &elements)?;
                detected = true;

                elements
            }
        };

        let before   = elements.len();
        let elements = filter_by_size(elements, min_size_px, max_size_px);

        shots.push(Shot {
            output   : output,
            index    : index,
            path     : path,
            width    : w,
            height   : h,
            origin   : origin,
            scale    : scale,
            filtered : before - elements.len(),
            widgets  : widgets_only(&elements),
            elements : elements,
        });
    }

    Ok(ShotSet { shots: shots, detected: detected })
}

/// Splits `DP-1-3.png` into `("DP-1", 3)`. Output names contain dashes (`HDMI-A-1`), so
/// only the last dash-separated field is the index.
fn parse_name(path: &Path) -> Result<(String, u32)> {
    let stem = path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{} has no usable file name", path.display()))?;

    let (output, index) = stem.rsplit_once('-')
        .ok_or_else(|| anyhow!("{stem} is not <output>-<n>"))?;

    let index = index.parse::<u32>()
        .with_context(|| format!("{stem} is not <output>-<n>"))?;

    Ok((output.to_string(), index))
}

/// Sidecar path for a screenshot: `DP-1-0.png` -> `DP-1-0.elements.json`.
fn cache_path(image: &Path) -> PathBuf {
    image.with_extension("elements.json")
}

/// Reads a cache file, returning `None` when it is missing, unreadable, older than the
/// screenshot, or written for a different frame. A bad cache is never an error:
/// re-detecting is always correct.
///
/// The mtime check matters because the capture CLI writes `<output>-<n>.png` by index, so
/// a fresh capture silently replaces the frame a sidecar describes while leaving its
/// dimensions, origin and scale identical.
fn read_cache(
    path   : &Path,
    image  : &Path,
    w      : u32,
    h      : u32,
    origin : GlobalPx,
    scale  : f64,
)
    -> Option<Vec<Element>>
{
    if !newer_than(path, image) {
        return None;
    }

    let text  = std::fs::read_to_string(path).ok()?;
    let cache = serde_json::from_str::<CacheFile>(&text).ok()?;

    let matches = cache.version == CACHE_VERSION
        && cache.width == w
        && cache.height == h
        && cache.origin == origin
        && (cache.scale - scale).abs() < 1.0e-9;

    matches.then_some(cache.elements)
}

/// Whether `cache` was modified no earlier than `image`. False when either mtime is
/// unavailable, which errs toward re-detecting.
fn newer_than(cache: &Path, image: &Path) -> bool {
    let Ok(cache_at) = std::fs::metadata(cache).and_then(|m| m.modified()) else {
        return false;
    };

    let Ok(image_at) = std::fs::metadata(image).and_then(|m| m.modified()) else {
        return false;
    };

    cache_at >= image_at
}

/// Writes the cache sidecar. Pretty-printed: it is a debugging artefact as much as a
/// cache, and the frames are few.
fn write_cache(
    path     : &Path,
    w        : u32,
    h        : u32,
    origin   : GlobalPx,
    scale    : f64,
    elements : &[Element],
)
    -> Result<()>
{
    let cache = CacheFile {
        version  : CACHE_VERSION,
        width    : w,
        height   : h,
        origin   : origin,
        scale    : scale,
        elements : elements.to_vec(),
    };

    let text = serde_json::to_string_pretty(&cache)?;

    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;

    Ok(())
}

/// The widget-class subset, renumbered densely. See `Shot::widgets`.
fn widgets_only(elements: &[Element]) -> Vec<Element> {
    elements.iter()
        .filter(|e| classify_target(&e.bbox, e.kind) == TargetClass::Widget)
        .enumerate()
        .map(|(i, e)| {
            let mut e = e.clone();
            e.id = i as u64;

            e
        })
        .collect()
}

/// Drops boxes outside the size window and renumbers the survivors, because element ids
/// are indices into the slice handed to the snap engine and must stay dense.
fn filter_by_size(elements: Vec<Element>, min_size_px: f64, max_size_px: f64) -> Vec<Element> {
    elements.into_iter()
        .filter(|e| {
            let short = e.bbox.w.min(e.bbox.h);
            let long  = e.bbox.w.max(e.bbox.h);

            short >= min_size_px && long <= max_size_px
        })
        .enumerate()
        .map(|(i, mut e)| {
            e.id = i as u64;

            e
        })
        .collect()
}

// --- DeskScale ---

impl DeskScale {
    /// Wraps a shared desk geometry as a scale source.
    pub fn new(geometry: Arc<DesktopGeometry>) -> Self {
        Self(geometry)
    }
}

impl PxScale for DeskScale {
    fn px_per_deg(&self, p: GlobalPx) -> (f64, f64) {
        // Spelled out through the trait: `DesktopGeometry` also has an inherent
        // `px_per_deg` that takes an explicit eye position, and inherent methods win
        // method resolution.
        PxScale::px_per_deg(&*self.0, p)
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::{ElementKind, ElementSource, Rect};

    /// A detector-sourced element with the given box; only the box and id matter here.
    fn element(id: u64, w: f64, h: f64) -> Element {
        Element {
            id     : id,
            bbox   : Rect { x: 0.0, y: 0.0, w: w, h: h },
            kind   : ElementKind::Button,
            source : ElementSource::Detector,
            score  : 1.0,
            text   : None,
        }
    }

    #[test]
    fn the_widget_subset_drops_lines_and_renumbers() {
        let mut elements = vec![
            element(0, 80.0, 30.0),    // A control.
            element(1, 350.0, 22.0),   // A terminal row: wide and short, so a line.
            element(2, 60.0, 24.0),    // A control.
        ];

        elements[1].kind = ElementKind::Button;

        let widgets = widgets_only(&elements);

        assert_eq!(widgets.len(), 2);
        assert_eq!(widgets.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(widgets[1].bbox.w, 60.0);
    }

    #[test]
    fn names_split_on_the_last_dash_only() {
        assert_eq!(parse_name(Path::new("a/HDMI-A-1-4.png")).unwrap(), ("HDMI-A-1".into(), 4));
        assert_eq!(parse_name(Path::new("a/DP-1-0.png")).unwrap(), ("DP-1".into(), 0));
        assert!(parse_name(Path::new("a/nodashes.png")).is_err());
        assert!(parse_name(Path::new("a/DP-1-x.png")).is_err());
    }

    #[test]
    fn the_size_filter_uses_the_short_and_long_sides() {
        let elements = vec![
            element(0, 10.0, 10.0),    // Kept.
            element(1, 2.0, 400.0),    // Short side below the floor.
            element(2, 900.0, 30.0),   // Long side above the ceiling.
            element(3, 799.0, 799.0),  // Right at the ceiling, kept.
        ];

        let kept = filter_by_size(elements, 4.0, 800.0);

        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].bbox.w, 10.0);
        assert_eq!(kept[1].bbox.w, 799.0);
    }

    #[test]
    fn the_size_filter_renumbers_survivors_densely() {
        let elements = vec![element(0, 1.0, 1.0), element(1, 10.0, 10.0), element(2, 20.0, 20.0)];
        let kept     = filter_by_size(elements, 4.0, f64::INFINITY);

        // Ids are indices into the slice the engine sees, so a gap would make a returned
        // element id fail to line up with its position.
        assert_eq!(kept.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1]);
    }
}
