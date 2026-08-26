//! Markdown rendering. Nothing here computes anything the run did not already count; it
//! only groups, sorts and formats.

use std::fmt::Write as _;

use gaze_core::{DesktopGeometry, ElementKind, Rect};

use crate::run::{
    BenchConfig, CandidateSet, FeedMode, RunResult, SigmaSetting, run_error, run_fixation,
    run_last_correct, run_flick, run_near, run_nudge, run_slips, run_sweep, run_tally,
    run_topk, run_topk_where,
};
use crate::shots::Shot;
use crate::stats::{
    DUPLICATE_IOU, ElementStats, FrameResult, NEAR_BUCKETS, NESTED_CONTAINMENT, SWEEP_MARGINS,
    FLICK_TOP_K, NUDGE_DEG_WIDTH, NUDGE_PX_WIDTH, SizeBucket, SlipClass, Tally, TargetClass,
    classify_slip, classify_target, near_stats, slip_slot,
};

/// Every element kind, in declaration order, so tables keep a stable column order.
const ALL_KINDS : [ElementKind; 8] = [
    ElementKind::Button,
    ElementKind::Icon,
    ElementKind::Input,
    ElementKind::Link,
    ElementKind::Text,
    ElementKind::Checkbox,
    ElementKind::Slider,
    ElementKind::Unknown,
];

/// Target classes in report order.
const ALL_CLASSES : [TargetClass; 3] = [
    TargetClass::Widget,
    TargetClass::Line,
    TargetClass::TextOther,
];

/// How many confusion pairs the report lists.
const TOP_CONFUSIONS : usize = 10;

/// Everything the report needs that is not in the runs themselves.
pub struct ReportInputs<'a> {
    pub shots           : &'a [Shot],
    pub geometry        : &'a DesktopGeometry,
    pub runs            : &'a [RunResult],
    pub config          : &'a BenchConfig,
    /// Sigma the breakdown sections are computed at.
    pub breakdown_sigma : f64,
    pub min_size_px     : f64,
    pub max_size_px     : f64,
    /// Wall clock of the Monte Carlo, seconds.
    pub elapsed_s       : f64,
}

// --- Rendering ---

/// Renders the whole report.
pub fn render_report(inputs: &ReportInputs<'_>) -> String {
    let mut out = String::with_capacity(32 * 1024);

    render_header(&mut out, inputs);
    render_elements(&mut out, inputs);
    render_headline(&mut out, inputs);
    render_topk(&mut out, inputs);
    render_nudge(&mut out, inputs);
    render_distractor_comparison(&mut out, inputs);
    render_margin_sweep(&mut out, inputs);
    render_error_sanity(&mut out, inputs);
    render_per_frame(&mut out, inputs);
    render_breakdowns(&mut out, inputs);
    render_confusions(&mut out, inputs);
    render_slip_anatomy(&mut out, inputs);
    render_sequence_diagnostic(&mut out, inputs);

    out
}

/// Title, configuration and totals.
fn render_header(out: &mut String, inputs: &ReportInputs<'_>) {
    let config   = inputs.config;
    let elements = inputs.shots.iter().map(|s| s.elements.len()).sum::<usize>();
    let widgets  = inputs.shots.iter().map(|s| s.widgets.len()).sum::<usize>();
    let filtered = inputs.shots.iter().map(|s| s.filtered).sum::<usize>();

    let _ = writeln!(out, "# gaze-bench: offline snap-correct rate\n");

    let _ = writeln!(
        out,
        "{} screenshots, {elements} elements after filtering ({widgets} of them widgets, \
         {filtered} removed by the size filter), {} trials per element single-sample and \
         {} in sequence mode.\n",
        inputs.shots.len(),
        config.trials,
        config.seq_trials,
    );

    let noise = {
        if config.legacy {
            "legacy: the full sigma drawn independently every sample".to_string()
        }
        else {
            format!(
                "per-fixation bias + per-sample jitter, jitter {:.2} deg",
                config.model.jitter_deg,
            )
        }
    };

    let _ = writeln!(out, "| setting | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| seed | {} |", config.seed);
    let _ = writeln!(out, "| noise model | {noise} |");
    let _ = writeln!(
        out,
        "| ambiguity margin | {:.2} ({}) |",
        config.margin,
        config.rivals.label(),
    );
    let _ = writeln!(
        out,
        "| off-desk samples | {} |",
        if config.clamp { "clamped to the panel edge" } else { "scored as lost gaze" },
    );
    let _ = writeln!(out, "| snap radius | {:.2} deg |", config.radius_deg);
    let _ = writeln!(out, "| hysteresis margin | {:.3} |", config.hysteresis);
    let w = config.weights.to_list();

    let _ = writeln!(
        out,
        "| score weights | kind {:.3}, area {:.3}, distance {:.3}, center {:.3}, \
         center_deg {:.3} |",
        w[0], w[1], w[2], w[3], w[4],
    );
    let _ = writeln!(out, "| size filter | short side >= {:.0} px, long side <= {} |",
        inputs.min_size_px, limit(inputs.max_size_px));
    let _ = writeln!(out, "| landing model | uniform in the box shrunk 20% per side, centre under 4 px |");
    let _ = writeln!(out, "| sequence fixation | 24 samples at 120 Hz, commit at zero latency |");
    let _ = writeln!(out, "| monte carlo wall clock | {:.1} s |", inputs.elapsed_s);
    let _ = writeln!(out);

    let _ = writeln!(
        out,
        "Target classes come from the box, not the detector's label: `line` is wider than \
         6:1 and under 40 px tall (terminal rows, chat messages, OCR runs, whatever the \
         model called them), `widget` is any remaining control class, `text-other` is the \
         rest. A trial is `ambiguous` when a rival candidate's cost came within the \
         ambiguity margin of the winner: right or wrong, it is a trial the two-tier design \
         would hand to refinement rather than click.\n"
    );
}

/// Element counts per screenshot, by kind and by class.
fn render_elements(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Element counts\n");

    let kinds = kinds_present(inputs.shots);

    let mut header = vec!["screenshot".to_string(), "elements".to_string(), "filtered out".to_string()];

    header.extend(ALL_CLASSES.iter().map(|c| class_name(*c).to_string()));
    header.extend(kinds.iter().map(|k| kind_name(*k).to_string()));

    let mut rows = Vec::new();

    for shot in inputs.shots {
        let mut row = vec![
            shot_name(shot),
            shot.elements.len().to_string(),
            shot.filtered.to_string(),
        ];

        for class in ALL_CLASSES {
            row.push(class_count(shot, class).to_string());
        }

        for kind in &kinds {
            row.push(shot.elements.iter().filter(|e| e.kind == *kind).count().to_string());
        }

        rows.push(row);
    }

    // Totals last, so the per-frame spread is what the eye lands on first.
    let mut total = vec![
        "**all**".to_string(),
        inputs.shots.iter().map(|s| s.elements.len()).sum::<usize>().to_string(),
        inputs.shots.iter().map(|s| s.filtered).sum::<usize>().to_string(),
    ];

    for class in ALL_CLASSES {
        total.push(inputs.shots.iter().map(|s| class_count(s, class)).sum::<usize>().to_string());
    }

    for kind in &kinds {
        let n: usize = inputs.shots.iter()
            .map(|s| s.elements.iter().filter(|e| e.kind == *kind).count())
            .sum();

        total.push(n.to_string());
    }

    rows.push(total);

    let header: Vec<&str> = header.iter().map(String::as_str).collect();

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// The headline, once per target class, over the full candidate set.
fn render_headline(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Headline (full candidate set)\n");

    for class in [None, Some(TargetClass::Widget), Some(TargetClass::Line)] {
        let label = match class {
            None    => "all targets",
            Some(c) => class_name(c),
        };

        let _ = writeln!(out, "### {label}\n");

        let header = [
            "sigma", "mode", "trials", "correct %", "slip %", "none %", "no_gaze %",
            "conf-correct %", "conf-wrong %", "ambiguous %",
        ];

        let mut rows = Vec::new();

        for run in inputs.runs.iter().filter(|r| r.key.candidates == CandidateSet::All) {
            let t = run_tally(run, class);

            rows.push(vec![
                sigma_label(run.key.sigma),
                mode_label(run.key.mode).to_string(),
                t.total().to_string(),
                format!("{:.1}", t.correct_pct()),
                format!("{:.1}", t.pct(t.slip)),
                format!("{:.1}", t.pct(t.none)),
                format!("{:.1}", t.pct(t.no_gaze)),
                format!("{:.1}", t.pct(t.confident_correct())),
                format!("{:.1}", t.pct(t.confident_wrong())),
                format!("{:.1}", t.pct(t.ambiguous())),
            ]);
        }

        out.push_str(&table(&header, &rows));
        let _ = writeln!(out);
    }
}

/// How far down the ranked candidate list the intended target sits.
///
/// This is the number that decides what the refinement tier has to be. A target that is
/// almost always in the best two or three candidates can be settled with a small hint
/// (numbered labels, one token spoken or pressed); a target that is often outside the top
/// five needs a zoom.
fn render_topk(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Top-k accuracy (single-sample)\n");

    let _ = writeln!(
        out,
        "Candidates ranked by cost ascending. `ranked` counts trials that produced a \
         candidate list at all, so it excludes lost gaze and the sigma-over-radius bail; \
         the percentages are of those. A candidate the rival policy says is the same thing \
         on screen as the target counts as the target, so detector duplication cannot push \
         a target down its own ranking. `near` is how many candidates sat within the \
         {:.2} ambiguity margin of the winner, which is the size of the hint a refinement \
         tier would show.\n",
        inputs.config.margin,
    );

    let header = [
        "candidates", "class", "sigma", "ranked", "top-1 %", "top-2 %", "top-3 %", "top-5 %",
        "mean near", "p90 near",
    ];

    let mut rows = Vec::new();

    for set in [CandidateSet::All, CandidateSet::WidgetsOnly] {
        for class in [None, Some(TargetClass::Widget), Some(TargetClass::Line)] {
            // A widgets-only candidate set has no line targets to report on.
            if set == CandidateSet::WidgetsOnly && class == Some(TargetClass::Line) {
                continue;
            }

            for run in inputs.runs.iter()
                .filter(|r| r.key.mode == FeedMode::Single && r.key.candidates == set)
            {
                let (topk, ranked) = run_topk(run, class);
                let (hist, sum, n) = run_near(run, class);
                let (mean, p90)    = near_stats(&hist, sum, n, 0.9);
                let denom          = ranked.max(1) as f64;

                let mut row = vec![
                    set_label(set).to_string(),
                    class.map(class_name).unwrap_or("all").to_string(),
                    sigma_label(run.key.sigma),
                    ranked.to_string(),
                ];

                for count in topk {
                    row.push(format!("{:.1}", 100.0 * count as f64 / denom));
                }

                row.push(format!("{mean:.1}"));
                row.push(format!("{}{p90}", if p90 + 1 == NEAR_BUCKETS { ">=" } else { "" }));

                rows.push(row);
            }
        }
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// How far the fine channel has to move the pointer after the warp, and whether one
/// directional flick could stand in for it.
///
/// This is the number that sizes the refinement effort now that the fine channel is a
/// handheld touchpad plus gyro: gaze warps, the thumb corrects, a physical click commits.
/// A median nudge under a degree is a flick of the thumb; several degrees is a drag.
fn render_nudge(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Nudge distance and flick coverage (single-sample)\n");

    let _ = writeln!(
        out,
        "Distance from the point the system would warp to (the snapped target's clamped \
         point, or the raw gaze when nothing snapped) to the nearest point of the intended \
         target's box, so a warp that already landed on the target measures zero. \
         Percentiles come from a {NUDGE_DEG_WIDTH:.2} deg / {NUDGE_PX_WIDTH:.0} px \
         histogram and read as upper edges; `>=` marks the overflow bin.\n"
    );

    let _ = writeln!(
        out,
        "`flick %` is the share of ranked trials where the intended target is inside the \
         best {FLICK_TOP_K} candidates *and* its direction from the warp point falls in a \
         different 45 degree sector from every other one, so a single directional flick \
         would be unambiguous.\n"
    );

    let header = [
        "class", "sigma", "trials", "inside %", "median deg", "p90 deg", "p99 deg",
        "median px", "p90 px", "p99 px", "flick %",
    ];

    let mut rows = Vec::new();

    for class in [None, Some(TargetClass::Widget)] {
        for run in inputs.runs.iter()
            .filter(|r| r.key.mode == FeedMode::Single && r.key.candidates == CandidateSet::All)
        {
            let (deg, px, inside, n) = run_nudge(run, class);
            let (ok, flick_n)        = run_flick(run, class);

            if n == 0 {
                continue;
            }

            let mut row = vec![
                class.map(class_name).unwrap_or("all").to_string(),
                sigma_label(run.key.sigma),
                n.to_string(),
                format!("{:.1}", 100.0 * inside as f64 / n as f64),
            ];

            for p in [0.5, 0.9, 0.99] {
                row.push(percentile_label(deg.percentile(p), 2));
            }

            for p in [0.5, 0.9, 0.99] {
                row.push(percentile_label(px.percentile(p), 0));
            }

            row.push(format!("{:.1}", 100.0 * ok as f64 / flick_n.max(1) as f64));

            rows.push(row);
        }
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// A percentile from a histogram, marked when it saturated the overflow bin.
fn percentile_label(value: (f64, bool), places: usize) -> String {
    let mark = if value.1 { ">=" } else { "" };

    format!("{mark}{:.places$}", value.0)
}

/// Widget targets with and without line/text distractors in the candidate set.
fn render_distractor_comparison(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Widget targets: do text distractors steal snaps?\n");

    let _ = writeln!(
        out,
        "Left half is widget targets scored against everything the detector found. Right \
         half is the same targets with only widgets in the candidate set, which is what \
         dropping OCR boxes from the snap index would look like. The gap is the cost of \
         letting text runs compete.\n"
    );

    let header = [
        "sigma", "mode", "correct % (all)", "correct % (widgets)", "delta",
        "conf-wrong % (all)", "conf-wrong % (widgets)", "ambiguous % (all)",
        "ambiguous % (widgets)",
    ];

    let mut rows = Vec::new();

    for run in inputs.runs.iter().filter(|r| r.key.candidates == CandidateSet::All) {
        let Some(only) = find_run(inputs.runs, run.key.mode, CandidateSet::WidgetsOnly, run.key.sigma)
        else {
            continue;
        };

        let a = run_tally(run, Some(TargetClass::Widget));
        let b = run_tally(only, Some(TargetClass::Widget));

        rows.push(vec![
            sigma_label(run.key.sigma),
            mode_label(run.key.mode).to_string(),
            format!("{:.1}", a.correct_pct()),
            format!("{:.1}", b.correct_pct()),
            format!("{:+.1}", b.correct_pct() - a.correct_pct()),
            format!("{:.1}", a.pct(a.confident_wrong())),
            format!("{:.1}", b.pct(b.confident_wrong())),
            format!("{:.1}", a.pct(a.ambiguous())),
            format!("{:.1}", b.pct(b.ambiguous())),
        ]);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// The ambiguity margin sweep for widgets at the breakdown sigma.
fn render_margin_sweep(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Ambiguity margin sweep (widget targets, sigma {:.2})\n",
        inputs.breakdown_sigma);

    let _ = writeln!(
        out,
        "The two-tier gate: snap when the winner is clear, refine when it is not. \
         `confident-wrong` is what the user has to undo and needs to be near zero; \
         `ambiguous` is the refinement load. Percentages are of trials that produced an \
         answer, so they exclude `none` and `no_gaze`.\n"
    );

    let header = [
        "margin", "candidates", "mode", "answered", "confident-correct %",
        "confident-wrong %", "ambiguous %",
    ];

    let mut rows = Vec::new();

    for set in [CandidateSet::All, CandidateSet::WidgetsOnly] {
        for mode in [FeedMode::Single, FeedMode::Sequence] {
            let Some(run) = find_run(
                inputs.runs, mode, set, SigmaSetting::Fixed(inputs.breakdown_sigma)
            )
            else {
                continue;
            };

            let sweep = run_sweep(run);

            for (i, margin) in SWEEP_MARGINS.iter().enumerate() {
                let row   = sweep[i];
                let total = row.iter().sum::<u64>().max(1) as f64;

                rows.push(vec![
                    format!("{margin:.2}"),
                    set_label(set).to_string(),
                    mode_label(mode).to_string(),
                    row.iter().sum::<u64>().to_string(),
                    format!("{:.1}", 100.0 * row[0] as f64 / total),
                    format!("{:.1}", 100.0 * row[1] as f64 / total),
                    format!("{:.1}", 100.0 * row[2] as f64 / total),
                ]);
            }
        }
    }

    if rows.is_empty() {
        let _ = writeln!(out, "No run at the breakdown sigma.\n");

        return;
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// Mean injected error, as a check that sigma means what it should.
fn render_error_sanity(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Sanity: injected error\n");

    let _ = writeln!(
        out,
        "Distance from the clean landing point to the first noisy sample. Two Gaussian \
         axes make the magnitude Rayleigh distributed, so the expectation is \
         `sigma * sqrt(pi/2)` = 1.253 sigma. The bias/jitter split changes the correlation \
         between samples, not the marginal per-sample distribution, so this line should \
         match the same expectation under either noise model.\n"
    );

    let header = ["sigma", "samples", "mean |noisy - clean| px", "mean deg", "expected deg"];
    let mut rows = Vec::new();

    for run in inputs.runs.iter()
        .filter(|r| r.key.mode == FeedMode::Single && r.key.candidates == CandidateSet::All)
    {
        let (px, deg, n) = run_error(run);

        let expected = match run.key.sigma {
            SigmaSetting::Fixed(s) => format!("{:.3}", s * std::f64::consts::FRAC_PI_2.sqrt()),
            // The profile's sigma varies per target, so there is no single expectation.
            SigmaSetting::Profile  => "varies".to_string(),
        };

        rows.push(vec![
            sigma_label(run.key.sigma),
            n.to_string(),
            format!("{px:.1}"),
            format!("{deg:.3}"),
            expected,
        ]);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// Per-screenshot widget numbers, which is how a GUI-heavy capture is told apart from a
/// terminal or a chat window.
fn render_per_frame(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## By screenshot at sigma {:.2} (single-sample, full candidate set)\n",
        inputs.breakdown_sigma);

    let Some(run) = find_run(
        inputs.runs, FeedMode::Single, CandidateSet::All,
        SigmaSetting::Fixed(inputs.breakdown_sigma),
    )
    else {
        let _ = writeln!(out, "No run at the breakdown sigma.\n");

        return;
    };

    let header = [
        "screenshot", "widgets", "widget correct %", "widget conf-wrong %",
        "widget ambiguous %", "lines", "line correct %", "all correct %",
    ];

    let mut rows = Vec::new();

    for frame in &run.frames {
        let shot   = &inputs.shots[frame.shot];
        let widget = frame_tally(frame, Some(TargetClass::Widget));
        let line   = frame_tally(frame, Some(TargetClass::Line));
        let all    = frame_tally(frame, None);

        rows.push(vec![
            shot_name(shot),
            class_count(shot, TargetClass::Widget).to_string(),
            format!("{:.1}", widget.correct_pct()),
            format!("{:.1}", widget.pct(widget.confident_wrong())),
            format!("{:.1}", widget.pct(widget.ambiguous())),
            class_count(shot, TargetClass::Line).to_string(),
            format!("{:.1}", line.correct_pct()),
            format!("{:.1}", all.correct_pct()),
        ]);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// By output, by kind, by size bucket and by class at the breakdown sigma.
fn render_breakdowns(out: &mut String, inputs: &ReportInputs<'_>) {
    let single = find_run(
        inputs.runs, FeedMode::Single, CandidateSet::All,
        SigmaSetting::Fixed(inputs.breakdown_sigma),
    );

    let sequence = find_run(
        inputs.runs, FeedMode::Sequence, CandidateSet::All,
        SigmaSetting::Fixed(inputs.breakdown_sigma),
    );

    let (Some(single), Some(sequence)) = (single, sequence) else {
        let _ = writeln!(
            out,
            "## Breakdowns\n\nNo run at sigma {:.2}, so no breakdowns.\n",
            inputs.breakdown_sigma
        );

        return;
    };

    let _ = writeln!(out, "## Breakdowns at sigma {:.2} (full candidate set)\n",
        inputs.breakdown_sigma);

    // By output: a frame belongs to exactly one output, so this groups whole frames.
    let mut by_output: Vec<(String, Tally, Tally)> = Vec::new();

    for (index, shot) in inputs.shots.iter().enumerate() {
        if !by_output.iter().any(|(name, _, _)| *name == shot.output) {
            by_output.push((shot.output.clone(), Tally::default(), Tally::default()));
        }

        let slot = by_output.iter_mut()
            .find(|(name, _, _)| *name == shot.output)
            .expect("just ensured the group exists");

        if let Some(frame) = single.frames.iter().find(|f| f.shot == index) {
            slot.1.merge(&frame_tally(frame, None));
        }

        if let Some(frame) = sequence.frames.iter().find(|f| f.shot == index) {
            slot.2.merge(&frame_tally(frame, None));
        }
    }

    let _ = writeln!(out, "### By output\n");
    out.push_str(&breakdown_table("output", by_output.into_iter()));
    let _ = writeln!(out);

    // By target class.
    let by_class = ALL_CLASSES.into_iter().map(|class| {
        (
            class_name(class).to_string(),
            group_tally(single, |e| e.class == class),
            group_tally(sequence, |e| e.class == class),
        )
    });

    let _ = writeln!(out, "### By target class\n");
    out.push_str(&breakdown_table("class", by_class));
    let _ = writeln!(out);

    // By kind: grouped over the target element's own detector class.
    let mut by_kind: Vec<(String, Tally, Tally)> = Vec::new();

    for kind in ALL_KINDS {
        let a = group_tally(single, |e| e.kind == kind);
        let b = group_tally(sequence, |e| e.kind == kind);

        if a.total() + b.total() > 0 {
            by_kind.push((kind_name(kind).to_string(), a, b));
        }
    }

    let _ = writeln!(out, "### By element kind\n");
    out.push_str(&breakdown_table("kind", by_kind.into_iter()));
    let _ = writeln!(out);

    // By size bucket, on the target's shorter side in degrees at its centre.
    let by_bucket = SizeBucket::all().into_iter().map(|bucket| {
        (
            bucket.label().to_string(),
            group_tally(single, |e| e.bucket == bucket),
            group_tally(sequence, |e| e.bucket == bucket),
        )
    });

    let _ = writeln!(out, "### By target size (shorter side, at the box centre)\n");
    out.push_str(&breakdown_table("size", by_bucket));
    let _ = writeln!(out);

    // Widgets split by whether something larger of a different kind wraps them. This is
    // the case the centre terms exist to fix.
    let by_nesting = [true, false].into_iter().map(|nested| {
        (
            if nested { "nested in another kind" } else { "not nested" }.to_string(),
            group_tally(single, |e| e.class == TargetClass::Widget && e.nested == nested),
            group_tally(sequence, |e| e.class == TargetClass::Widget && e.nested == nested),
        )
    });

    let _ = writeln!(out, "### Widget targets by nesting\n");

    let _ = writeln!(
        out,
        "`nested in another kind` means some larger candidate of a different detector kind \
         wraps the target, so `distance` is zero for both whenever the gaze is inside the \
         inner one and only the kind, area and centre terms can separate them.\n"
    );

    out.push_str(&breakdown_table("nesting", by_nesting));
    let _ = writeln!(out);

    // The same split, with the ranking numbers the centre terms are meant to move.
    let header = ["nesting", "elements", "ranked", "top-1 %", "top-2 %", "top-3 %", "top-5 %"];
    let mut rows = Vec::new();

    for nested in [true, false] {
        let keep = |e: &ElementStats| e.class == TargetClass::Widget && e.nested == nested;

        let elements = single.frames.iter()
            .flat_map(|f| f.elements.iter())
            .filter(|e| keep(e))
            .count();

        let (topk, ranked) = run_topk_where(single, keep);
        let denom          = ranked.max(1) as f64;

        let mut row = vec![
            if nested { "nested in another kind" } else { "not nested" }.to_string(),
            elements.to_string(),
            ranked.to_string(),
        ];

        for count in topk {
            row.push(format!("{:.1}", 100.0 * count as f64 / denom));
        }

        rows.push(row);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);

    // Widgets only, by size: the same cut restricted to the targets Phase 0 is about.
    let widgets_by_bucket = SizeBucket::all().into_iter().map(|bucket| {
        (
            bucket.label().to_string(),
            group_tally(single, |e| e.bucket == bucket && e.class == TargetClass::Widget),
            group_tally(sequence, |e| e.bucket == bucket && e.class == TargetClass::Widget),
        )
    });

    let _ = writeln!(out, "### Widget targets by size\n");
    out.push_str(&breakdown_table("size", widgets_by_bucket));
    let _ = writeln!(out);
}

/// The most frequent target-to-wrong-answer pairs.
fn render_confusions(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Top {TOP_CONFUSIONS} confusions (single-sample, full candidate set)\n");

    let Some(single) = find_run(
        inputs.runs, FeedMode::Single, CandidateSet::All,
        SigmaSetting::Fixed(inputs.breakdown_sigma),
    )
    else {
        let _ = writeln!(out, "No run at the breakdown sigma.\n");

        return;
    };

    // Keyed by frame as well as by ids, since element ids are per-frame indices.
    let mut pairs: Vec<(usize, u64, u64, u32)> = Vec::new();

    for frame in &single.frames {
        for (&(want, chosen), &count) in &frame.confusion {
            pairs.push((frame.shot, want, chosen, count));
        }
    }

    pairs.sort_by(|a, b| b.3.cmp(&a.3).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    pairs.truncate(TOP_CONFUSIONS);

    let header = [
        "n", "screenshot", "target", "target box", "chosen", "chosen box", "gap deg", "overlap",
    ];

    let mut rows = Vec::new();

    for (shot_index, want, chosen, count) in pairs {
        let shot = &inputs.shots[shot_index];

        let (Some(target), Some(other)) =
            (shot.elements.get(want as usize), shot.elements.get(chosen as usize))
        else {
            continue;
        };

        rows.push(vec![
            count.to_string(),
            shot_name(shot),
            format!("#{want} {} {}", kind_name(target.kind), class_name(classify_target(&target.bbox, target.kind))),
            rect_label(&target.bbox),
            format!("#{chosen} {} {}", kind_name(other.kind), class_name(classify_target(&other.bbox, other.kind))),
            rect_label(&other.bbox),
            format!("{:.2}", centre_gap_deg(inputs.geometry, &target.bbox, &other.bbox)),
            slip_class_name(classify_slip(&target.bbox, &other.bbox)).to_string(),
        ]);
    }

    if rows.is_empty() {
        let _ = writeln!(out, "No slips at this sigma.\n");

        return;
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// What the slips actually were: the same box twice, a nesting, or a different target.
fn render_slip_anatomy(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Slip anatomy (full candidate set)\n");

    let _ = writeln!(
        out,
        "Every slip, classified by how the chosen box overlaps the intended one. \
         `duplicate` is IoU >= {DUPLICATE_IOU:.1}: one thing on screen, two detections. \
         `nested` is {:.0}% of the smaller box inside the larger one but not the same box: \
         a label inside its button, a terminal line inside its pane. `separate` is a \
         genuinely different target.\n",
        NESTED_CONTAINMENT * 100.0,
    );

    let header = ["sigma", "mode", "slips", "duplicate %", "nested %", "separate %"];
    let mut rows = Vec::new();

    for run in inputs.runs.iter().filter(|r| r.key.candidates == CandidateSet::All) {
        let slips = run_slips(run);
        let total = slips.iter().sum::<u64>().max(1) as f64;

        rows.push(vec![
            sigma_label(run.key.sigma),
            mode_label(run.key.mode).to_string(),
            slips.iter().sum::<u64>().to_string(),
            format!("{:.1}", 100.0 * slips[slip_slot(SlipClass::Duplicate)] as f64 / total),
            format!("{:.1}", 100.0 * slips[slip_slot(SlipClass::Nested)] as f64 / total),
            format!("{:.1}", 100.0 * slips[slip_slot(SlipClass::Separate)] as f64 / total),
        ]);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

/// What the filter made of the simulated fixations, and how much `commit` cost.
fn render_sequence_diagnostic(out: &mut String, inputs: &ReportInputs<'_>) {
    let _ = writeln!(out, "## Sequence-mode diagnostic\n");

    let _ = writeln!(
        out,
        "`fixating %` is the share of the 24 samples the I-VT classifier called a fixation; \
         only those reach the engine's ring buffer, and `commit` can only answer from the \
         ring. Under the legacy noise model this collapses, because independent per-sample \
         error at the full sigma looks like 50 to 120 deg/s of motion to a 30 deg/s \
         threshold. `last update correct %` is what the engine's final `update` said, which \
         is what an overlay highlight would have been showing.\n"
    );

    let header = ["sigma", "trials", "fixating %", "commit correct %", "last update correct %"];
    let mut rows = Vec::new();

    for run in inputs.runs.iter()
        .filter(|r| r.key.mode == FeedMode::Sequence && r.key.candidates == CandidateSet::All)
    {
        let tally        = run_tally(run, None);
        let (fix, fed)   = run_fixation(run);
        let last_correct = run_last_correct(run);
        let total        = tally.total().max(1);

        rows.push(vec![
            sigma_label(run.key.sigma),
            tally.total().to_string(),
            format!("{:.1}", 100.0 * fix as f64 / fed.max(1) as f64),
            format!("{:.1}", tally.correct_pct()),
            format!("{:.1}", 100.0 * last_correct as f64 / total as f64),
        ]);
    }

    out.push_str(&table(&header, &rows));
    let _ = writeln!(out);
}

// --- Helpers ---

/// The run matching a mode, candidate set and sigma setting.
fn find_run(
    runs       : &[RunResult],
    mode       : FeedMode,
    candidates : CandidateSet,
    sigma      : SigmaSetting,
)
    -> Option<&RunResult>
{
    runs.iter().find(|r| {
        r.key.mode == mode && r.key.candidates == candidates && same_sigma(r.key.sigma, sigma)
    })
}

/// Sigma settings compare by value, with a tolerance on the float.
fn same_sigma(a: SigmaSetting, b: SigmaSetting) -> bool {
    match (a, b) {
        (SigmaSetting::Fixed(x), SigmaSetting::Fixed(y)) => (x - y).abs() < 1.0e-9,
        (SigmaSetting::Profile, SigmaSetting::Profile)   => true,
        _                                                => false,
    }
}

/// Totals over one frame, optionally restricted to a target class.
fn frame_tally(frame: &FrameResult, class: Option<TargetClass>) -> Tally {
    let mut total = Tally::default();

    for element in &frame.elements {
        if class.is_none_or(|c| element.class == c) {
            total.merge(&element.tally);
        }
    }

    total
}

/// Totals over every element of a run that `keep` accepts.
fn group_tally(run: &RunResult, keep: impl Fn(&ElementStats) -> bool) -> Tally {
    let mut total = Tally::default();

    for frame in &run.frames {
        for element in &frame.elements {
            if keep(element) {
                total.merge(&element.tally);
            }
        }
    }

    total
}

/// A breakdown table: one row per group, single-sample in full plus sequence's headline.
fn breakdown_table(label: &str, rows: impl Iterator<Item = (String, Tally, Tally)>) -> String {
    let header = [
        label, "trials", "correct %", "slip %", "none %", "no_gaze %", "conf-wrong %",
        "ambiguous %", "seq correct %",
    ];

    let rows: Vec<Vec<String>> = rows
        .map(|(name, single, sequence)| {
            vec![
                name,
                single.total().to_string(),
                format!("{:.1}", single.correct_pct()),
                format!("{:.1}", single.pct(single.slip)),
                format!("{:.1}", single.pct(single.none)),
                format!("{:.1}", single.pct(single.no_gaze)),
                format!("{:.1}", single.pct(single.confident_wrong())),
                format!("{:.1}", single.pct(single.ambiguous())),
                format!("{:.1}", sequence.correct_pct()),
            ]
        })
        .collect();

    table(&header, &rows)
}

/// Kinds that actually occur in the shot set, in declaration order.
fn kinds_present(shots: &[Shot]) -> Vec<ElementKind> {
    ALL_KINDS.into_iter()
        .filter(|kind| shots.iter().any(|s| s.elements.iter().any(|e| e.kind == *kind)))
        .collect()
}

/// Elements of one class in a screenshot.
fn class_count(shot: &Shot, class: TargetClass) -> usize {
    shot.elements.iter()
        .filter(|e| classify_target(&e.bbox, e.kind) == class)
        .count()
}

/// `<output>-<n>`, the name the screenshot is known by.
fn shot_name(shot: &Shot) -> String {
    format!("{}-{}", shot.output, shot.index)
}

/// Visual angle between two box centres, degrees. Zero when either is off-panel.
fn centre_gap_deg(geometry: &DesktopGeometry, a: &Rect, b: &Rect) -> f64 {
    geometry.angle_between_deg(geometry.eye(), a.center(), b.center()).unwrap_or(0.0)
}

/// `x,y wxh` in logical pixels, for a confusion row.
fn rect_label(r: &Rect) -> String {
    format!("{:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h)
}

/// Column header for a sigma setting.
fn sigma_label(sigma: SigmaSetting) -> String {
    match sigma {
        SigmaSetting::Fixed(s) => format!("{s:.2}"),
        SigmaSetting::Profile  => "profile".to_string(),
    }
}

/// Column header for a feeding mode.
fn mode_label(mode: FeedMode) -> &'static str {
    match mode {
        FeedMode::Single   => "single",
        FeedMode::Sequence => "sequence",
    }
}

/// Column header for a candidate set.
fn set_label(set: CandidateSet) -> &'static str {
    match set {
        CandidateSet::All         => "all",
        CandidateSet::WidgetsOnly => "widgets",
    }
}

/// Report name of a target class.
fn class_name(class: TargetClass) -> &'static str {
    match class {
        TargetClass::Widget    => "widget",
        TargetClass::Line      => "line",
        TargetClass::TextOther => "text-other",
    }
}

/// Report name of a slip class.
fn slip_class_name(class: SlipClass) -> &'static str {
    match class {
        SlipClass::Duplicate => "duplicate",
        SlipClass::Nested    => "nested",
        SlipClass::Separate  => "separate",
    }
}

/// Report name of an element kind.
fn kind_name(kind: ElementKind) -> &'static str {
    match kind {
        ElementKind::Button   => "Button",
        ElementKind::Icon     => "Icon",
        ElementKind::Input    => "Input",
        ElementKind::Link     => "Link",
        ElementKind::Text     => "Text",
        ElementKind::Checkbox => "Checkbox",
        ElementKind::Slider   => "Slider",
        ElementKind::Unknown  => "Unknown",
    }
}

/// Renders an unbounded size limit as "none" rather than "inf".
fn limit(v: f64) -> String {
    if v.is_finite() {
        format!("{v:.0} px")
    }
    else {
        "none".to_string()
    }
}

/// Builds a markdown table with columns padded to a common width, so the raw file is
/// readable without a renderer.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols     = header.len();
    let mut wide = header.iter().map(|h| h.chars().count()).collect::<Vec<_>>();

    for row in rows {
        for (i, cell) in row.iter().take(cols).enumerate() {
            wide[i] = wide[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();

    let _ = write!(out, "|");

    for (i, h) in header.iter().enumerate() {
        let _ = write!(out, " {:width$} |", h, width = wide[i]);
    }

    let _ = writeln!(out);
    let _ = write!(out, "|");

    for w in &wide {
        let _ = write!(out, "{}|", "-".repeat(w + 2));
    }

    let _ = writeln!(out);

    for row in rows {
        let _ = write!(out, "|");

        for (i, width) in wide.iter().enumerate().take(cols) {
            let cell = row.get(i).map(String::as_str).unwrap_or("");
            let _    = write!(out, " {:width$} |", cell, width = width);
        }

        let _ = writeln!(out);
    }

    out
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_pad_every_column_to_its_widest_cell() {
        let rows = vec![
            vec!["a".to_string(), "1".to_string()],
            vec!["longer".to_string(), "22".to_string()],
        ];

        let text  = table(&["k", "v"], &rows);
        let lines = text.lines().collect::<Vec<_>>();

        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|l| l.len() == lines[0].len()), "ragged table:\n{text}");
        assert!(lines[0].starts_with("| k      | v  |"));
    }

    #[test]
    fn tables_tolerate_a_short_row() {
        let rows = vec![vec!["only".to_string()]];
        let text = table(&["a", "b"], &rows);

        assert!(text.contains("| only |   |"), "{text}");
    }

    #[test]
    fn size_limits_read_as_none_when_unbounded() {
        assert_eq!(limit(f64::INFINITY), "none");
        assert_eq!(limit(800.0), "800 px");
    }

    #[test]
    fn sigma_settings_compare_by_value() {
        assert!(same_sigma(SigmaSetting::Fixed(0.7), SigmaSetting::Fixed(0.7)));
        assert!(!same_sigma(SigmaSetting::Fixed(0.7), SigmaSetting::Fixed(1.0)));
        assert!(same_sigma(SigmaSetting::Profile, SigmaSetting::Profile));
        assert!(!same_sigma(SigmaSetting::Profile, SigmaSetting::Fixed(0.7)));
    }
}
