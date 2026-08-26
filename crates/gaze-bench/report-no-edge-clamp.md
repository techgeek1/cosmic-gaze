# gaze-bench: offline snap-correct rate

15 screenshots, 3345 elements after filtering (1441 of them widgets, 0 removed by the size filter), 20 trials per element single-sample and 20 in sequence mode.

| setting | value |
|---|---|
| seed | 0 |
| noise model | per-fixation bias + per-sample jitter, jitter 0.20 deg |
| ambiguity margin | 0.50 (duplicate rivals ignored) |
| off-desk samples | scored as lost gaze |
| snap radius | 2.00 deg |
| hysteresis margin | 0.150 |
| score weights | kind 1.500, area 0.150, distance 1.000, center 0.000, center_deg 0.200 |
| size filter | short side >= 0 px, long side <= none |
| landing model | uniform in the box shrunk 20% per side, centre under 4 px |
| sequence fixation | 24 samples at 120 Hz, commit at zero latency |
| monte carlo wall clock | 3.3 s |

Target classes come from the box, not the detector's label: `line` is wider than 6:1 and under 40 px tall (terminal rows, chat messages, OCR runs, whatever the model called them), `widget` is any remaining control class, `text-other` is the rest. A trial is `ambiguous` when a rival candidate's cost came within the ambiguity margin of the winner: right or wrong, it is a trial the two-tier design would hand to refinement rather than click.

## Element counts

| screenshot | elements | filtered out | widget | line | text-other | Button | Input | Link | Text | Checkbox |
|------------|----------|--------------|--------|------|------------|--------|-------|------|------|----------|
| DP-1-1     | 322      | 0            | 102    | 154  | 66         | 164    | 0     | 5    | 152  | 1        |
| DP-1-2     | 322      | 0            | 102    | 154  | 66         | 164    | 0     | 5    | 152  | 1        |
| DP-1-3     | 320      | 0            | 101    | 154  | 65         | 164    | 0     | 5    | 151  | 0        |
| DP-1-4     | 322      | 0            | 102    | 154  | 66         | 165    | 0     | 5    | 152  | 0        |
| DP-1-5     | 456      | 0            | 263    | 95   | 98         | 298    | 3     | 3    | 152  | 0        |
| DP-2-1     | 221      | 0            | 130    | 27   | 64         | 133    | 5     | 6    | 77   | 0        |
| DP-2-2     | 218      | 0            | 130    | 26   | 62         | 133    | 6     | 6    | 73   | 0        |
| DP-2-3     | 219      | 0            | 130    | 26   | 63         | 133    | 6     | 6    | 74   | 0        |
| DP-2-4     | 225      | 0            | 134    | 27   | 64         | 138    | 5     | 6    | 76   | 0        |
| DP-2-5     | 310      | 0            | 157    | 70   | 83         | 182    | 4     | 13   | 111  | 0        |
| HDMI-A-1-1 | 82       | 0            | 18     | 43   | 21         | 37     | 0     | 7    | 38   | 0        |
| HDMI-A-1-2 | 82       | 0            | 18     | 43   | 21         | 37     | 0     | 7    | 38   | 0        |
| HDMI-A-1-3 | 82       | 0            | 18     | 43   | 21         | 37     | 0     | 7    | 38   | 0        |
| HDMI-A-1-4 | 82       | 0            | 18     | 43   | 21         | 37     | 0     | 7    | 38   | 0        |
| HDMI-A-1-5 | 82       | 0            | 18     | 43   | 21         | 37     | 0     | 7    | 38   | 0        |
| **all**    | 3345     | 0            | 1441   | 1102 | 802        | 1859   | 29    | 95   | 1360 | 2        |

## Headline (full candidate set)

### all targets

| sigma | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|-------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.70  | single   | 66900  | 28.6      | 66.4   | 0.0    | 5.0       | 10.1           | 12.8         | 72.0        |
| 0.70  | sequence | 66900  | 29.5      | 67.0   | 1.5    | 2.0       | 11.0           | 13.4         | 72.0        |

### widget

| sigma | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|-------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.70  | single   | 28820  | 39.1      | 51.7   | 0.0    | 9.2       | 15.4           | 6.8          | 68.5        |
| 0.70  | sequence | 28820  | 40.6      | 52.7   | 2.9    | 3.8       | 17.2           | 8.0          | 68.1        |

### line

| sigma | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|-------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.70  | single   | 22040  | 24.8      | 74.0   | 0.0    | 1.2       | 7.4            | 11.3         | 80.1        |
| 0.70  | sequence | 22040  | 25.4      | 73.9   | 0.3    | 0.4       | 7.6            | 11.4         | 80.3        |

## Top-k accuracy (single-sample)

Candidates ranked by cost ascending. `ranked` counts trials that produced a candidate list at all, so it excludes lost gaze and the sigma-over-radius bail; the percentages are of those. A candidate the rival policy says is the same thing on screen as the target counts as the target, so detector duplication cannot push a target down its own ranking. `near` is how many candidates sat within the 0.50 ambiguity margin of the winner, which is the size of the hint a refinement tier would show.

| candidates | class  | sigma | ranked | top-1 % | top-2 % | top-3 % | top-5 % | mean near | p90 near |
|------------|--------|-------|--------|---------|---------|---------|---------|-----------|----------|
| all        | all    | 0.70  | 63545  | 31.1    | 50.7    | 63.7    | 78.8    | 2.6       | 4        |
| all        | widget | 0.70  | 26158  | 43.1    | 65.1    | 77.4    | 89.7    | 2.6       | 4        |
| all        | line   | 0.70  | 21780  | 26.0    | 44.5    | 58.0    | 73.8    | 2.7       | 4        |
| widgets    | all    | 0.70  | 26098  | 47.6    | 70.9    | 82.8    | 93.2    | 2.3       | 4        |
| widgets    | widget | 0.70  | 26098  | 47.6    | 70.9    | 82.8    | 93.2    | 2.3       | 4        |

## Nudge distance and flick coverage (single-sample)

Distance from the point the system would warp to (the snapped target's clamped point, or the raw gaze when nothing snapped) to the nearest point of the intended target's box, so a warp that already landed on the target measures zero. Percentiles come from a 0.02 deg / 1 px histogram and read as upper edges; `>=` marks the overflow bin.

`flick %` is the share of ranked trials where the intended target is inside the best 3 candidates *and* its direction from the warp point falls in a different 45 degree sector from every other one, so a single directional flick would be unambiguous.

| class  | sigma | trials | inside % | median deg | p90 deg | p99 deg | median px | p90 px | p99 px | flick % |
|--------|-------|--------|----------|------------|---------|---------|-----------|--------|--------|---------|
| all    | 0.70  | 63547  | 35.8     | 0.28       | 1.10    | 1.98    | 18        | 74     | 132    | 44.4    |
| widget | 0.70  | 26159  | 44.0     | 0.20       | 1.02    | 1.82    | 12        | 68     | 126    | 57.1    |

## Widget targets: do text distractors steal snaps?

Left half is widget targets scored against everything the detector found. Right half is the same targets with only widgets in the candidate set, which is what dropping OCR boxes from the snap index would look like. The gap is the cost of letting text runs compete.

| sigma | mode     | correct % (all) | correct % (widgets) | delta | conf-wrong % (all) | conf-wrong % (widgets) | ambiguous % (all) | ambiguous % (widgets) |
|-------|----------|-----------------|---------------------|-------|--------------------|------------------------|-------------------|-----------------------|
| 0.70  | single   | 39.1            | 43.1                | +4.1  | 6.8                | 7.0                    | 68.5              | 64.3                  |
| 0.70  | sequence | 40.6            | 44.7                | +4.1  | 8.0                | 8.3                    | 68.1              | 64.0                  |

## Ambiguity margin sweep (widget targets, sigma 0.70)

The two-tier gate: snap when the winner is clear, refine when it is not. `confident-wrong` is what the user has to undo and needs to be near zero; `ambiguous` is the refinement load. Percentages are of trials that produced an answer, so they exclude `none` and `no_gaze`.

| margin | candidates | mode     | answered | confident-correct % | confident-wrong % | ambiguous % |
|--------|------------|----------|----------|---------------------|-------------------|-------------|
| 0.25   | all        | single   | 26158    | 27.5                | 22.9              | 49.6        |
| 0.50   | all        | single   | 26158    | 17.0                | 7.5               | 75.5        |
| 1.00   | all        | single   | 26158    | 6.8                 | 0.6               | 92.5        |
| 0.25   | all        | sequence | 26888    | 28.5                | 23.0              | 48.5        |
| 0.50   | all        | sequence | 26888    | 18.4                | 8.6               | 73.0        |
| 1.00   | all        | sequence | 26888    | 8.3                 | 2.3               | 89.4        |
| 0.25   | widgets    | single   | 26098    | 32.0                | 21.6              | 46.3        |
| 0.50   | widgets    | single   | 26098    | 21.2                | 7.8               | 71.0        |
| 1.00   | widgets    | single   | 26098    | 10.5                | 0.8               | 88.7        |
| 0.25   | widgets    | sequence | 26834    | 33.0                | 21.9              | 45.1        |
| 0.50   | widgets    | sequence | 26834    | 22.3                | 8.9               | 68.8        |
| 1.00   | widgets    | sequence | 26834    | 11.6                | 2.4               | 85.9        |

## Sanity: injected error

Distance from the clean landing point to the first noisy sample. Two Gaussian axes make the magnitude Rayleigh distributed, so the expectation is `sigma * sqrt(pi/2)` = 1.253 sigma. The bias/jitter split changes the correlation between samples, not the marginal per-sample distribution, so this line should match the same expectation under either noise model.

| sigma | samples | mean |noisy - clean| px | mean deg | expected deg |
|-------|---------|-------------------------|----------|--------------|
| 0.70  | 63547   | 57.5                    | 0.856    | 0.877        |

## By screenshot at sigma 0.70 (single-sample, full candidate set)

| screenshot | widgets | widget correct % | widget conf-wrong % | widget ambiguous % | lines | line correct % | all correct % |
|------------|---------|------------------|---------------------|--------------------|-------|----------------|---------------|
| DP-1-1     | 102     | 39.5             | 6.6                 | 72.4               | 154   | 24.2           | 28.8          |
| DP-1-2     | 102     | 39.1             | 6.8                 | 71.4               | 154   | 23.2           | 28.0          |
| DP-1-3     | 101     | 38.8             | 6.7                 | 74.0               | 154   | 24.5           | 28.6          |
| DP-1-4     | 102     | 37.4             | 7.0                 | 73.3               | 154   | 23.2           | 27.5          |
| DP-1-5     | 263     | 30.1             | 5.9                 | 81.7               | 95    | 22.8           | 23.8          |
| DP-2-1     | 130     | 43.0             | 7.0                 | 61.7               | 27    | 36.7           | 34.6          |
| DP-2-2     | 130     | 44.1             | 7.6                 | 60.5               | 26    | 41.3           | 36.3          |
| DP-2-3     | 130     | 42.2             | 8.5                 | 61.1               | 26    | 36.5           | 34.8          |
| DP-2-4     | 134     | 43.5             | 6.9                 | 60.8               | 27    | 35.7           | 34.8          |
| DP-2-5     | 157     | 38.7             | 7.1                 | 68.2               | 70    | 28.4           | 29.0          |
| HDMI-A-1-1 | 18      | 41.1             | 5.3                 | 52.8               | 43    | 19.7           | 19.3          |
| HDMI-A-1-2 | 18      | 47.2             | 6.9                 | 52.5               | 43    | 22.1           | 22.0          |
| HDMI-A-1-3 | 18      | 45.3             | 3.6                 | 55.3               | 43    | 20.3           | 20.6          |
| HDMI-A-1-4 | 18      | 41.9             | 4.7                 | 58.9               | 43    | 22.9           | 21.2          |
| HDMI-A-1-5 | 18      | 43.6             | 5.8                 | 56.4               | 43    | 21.0           | 20.6          |

## Breakdowns at sigma 0.70 (full candidate set)

### By output

| output   | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| DP-1     | 34840  | 27.1      | 70.2   | 0.0    | 2.7       | 11.5         | 77.4        | 27.7          |
| DP-2     | 23860  | 33.5      | 58.9   | 0.0    | 7.6       | 15.3         | 63.3        | 34.9          |
| HDMI-A-1 | 8200   | 20.7      | 72.0   | 0.0    | 7.2       | 11.4         | 74.5        | 21.5          |

### By target class

| class      | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| widget     | 28820  | 39.1      | 51.7   | 0.0    | 9.2       | 6.8          | 68.5        | 40.6          |
| line       | 22040  | 24.8      | 74.0   | 0.0    | 1.2       | 11.3         | 80.1        | 25.4          |
| text-other | 16040  | 14.9      | 82.4   | 0.0    | 2.7       | 25.6         | 67.2        | 15.1          |

### By element kind

| kind     | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| Button   | 37180  | 37.1      | 55.4   | 0.0    | 7.5       | 5.9          | 73.3        | 38.5          |
| Input    | 580    | 55.7      | 41.7   | 0.0    | 2.6       | 8.3          | 58.6        | 56.9          |
| Link     | 1900   | 53.2      | 45.9   | 0.0    | 0.8       | 5.8          | 63.7        | 53.3          |
| Text     | 27200  | 14.6      | 83.5   | 0.0    | 1.9       | 22.9         | 71.3        | 14.7          |
| Checkbox | 40     | 95.0      | 5.0    | 0.0    | 0.0       | 0.0          | 12.5        | 92.5          |

### By target size (shorter side, at the box centre)

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 49900  | 24.9      | 70.6   | 0.0    | 4.5       | 13.1         | 74.9        | 25.6          |
| 0.5 - 1 deg | 14320  | 40.1      | 52.2   | 0.0    | 7.7       | 10.2         | 65.2        | 41.8          |
| 1 - 2 deg   | 1740   | 44.0      | 55.7   | 0.0    | 0.3       | 19.2         | 51.3        | 43.9          |
| > 2 deg     | 940    | 23.0      | 77.0   | 0.0    | 0.0       | 24.3         | 64.3        | 21.6          |

### Widget targets by nesting

`nested in another kind` means some larger candidate of a different detector kind wraps the target, so `distance` is zero for both whenever the gaze is inside the inner one and only the kind, area and centre terms can separate them.

| nesting                | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|------------------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| nested in another kind | 6300   | 41.2      | 57.5   | 0.0    | 1.3       | 6.8          | 75.3        | 41.6          |
| not nested             | 22520  | 38.5      | 50.0   | 0.0    | 11.5      | 6.8          | 66.6        | 40.3          |

| nesting                | elements | ranked | top-1 % | top-2 % | top-3 % | top-5 % |
|------------------------|----------|--------|---------|---------|---------|---------|
| nested in another kind | 315      | 6220   | 41.7    | 63.4    | 75.3    | 88.0    |
| not nested             | 1126     | 19938  | 43.5    | 65.6    | 78.1    | 90.2    |

### Widget targets by size

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 17640  | 34.5      | 56.4   | 0.0    | 9.1       | 7.3          | 71.7        | 35.7          |
| 0.5 - 1 deg | 10620  | 44.3      | 45.8   | 0.0    | 10.0      | 6.2          | 65.5        | 46.3          |
| 1 - 2 deg   | 560    | 84.1      | 15.2   | 0.0    | 0.7       | 4.3          | 26.1        | 84.1          |
| > 2 deg     | 0      | 0.0       | 0.0    | 0.0    | 0.0       | 0.0          | 0.0         | 0.0           |

## Top 10 confusions (single-sample, full candidate set)

| n  | screenshot | target               | target box      | chosen             | chosen box      | gap deg | overlap   |
|----|------------|----------------------|-----------------|--------------------|-----------------|---------|-----------|
| 20 | DP-1-1     | #188 Text text-other | 4612,80 80x22   | #5 Button line     | 4605,45 191x31  | 0.76    | separate  |
| 20 | DP-1-1     | #261 Text line       | 2809,694 425x29 | #139 Button line   | 2823,734 318x36 | 0.95    | separate  |
| 20 | DP-1-1     | #299 Text text-other | 2810,1326 41x39 | #41 Button widget  | 2812,1329 34x32 | 0.02    | duplicate |
| 20 | DP-1-1     | #303 Text text-other | 2828,1378 65x27 | #41 Button widget  | 2812,1329 34x32 | 0.88    | separate  |
| 20 | DP-1-2     | #188 Text text-other | 4612,80 80x22   | #5 Button line     | 4605,45 191x31  | 0.76    | separate  |
| 20 | DP-1-2     | #299 Text text-other | 2810,1326 41x39 | #41 Button widget  | 2812,1329 34x32 | 0.02    | duplicate |
| 20 | DP-1-4     | #299 Text text-other | 2810,1326 41x39 | #40 Button widget  | 2812,1329 34x32 | 0.02    | duplicate |
| 20 | DP-1-5     | #380 Text text-other | 3693,757 56x34  | #229 Button widget | 3704,796 104x33 | 0.81    | separate  |
| 20 | DP-2-1     | #169 Text text-other | 529,393 14x10   | #114 Button widget | 390,406 167x54  | 1.21    | separate  |
| 20 | DP-2-2     | #167 Text text-other | 476,391 37x14   | #114 Button widget | 390,406 167x54  | 0.68    | separate  |

## Slip anatomy (full candidate set)

Every slip, classified by how the chosen box overlaps the intended one. `duplicate` is IoU >= 0.5: one thing on screen, two detections. `nested` is 80% of the smaller box inside the larger one but not the same box: a label inside its button, a terminal line inside its pane. `separate` is a genuinely different target.

| sigma | mode     | slips | duplicate % | nested % | separate % |
|-------|----------|-------|-------------|----------|------------|
| 0.70  | single   | 44419 | 1.4         | 5.7      | 92.9       |
| 0.70  | sequence | 44792 | 1.4         | 5.7      | 92.9       |

## Sequence-mode diagnostic

`fixating %` is the share of the 24 samples the I-VT classifier called a fixation; only those reach the engine's ring buffer, and `commit` can only answer from the ring. Under the legacy noise model this collapses, because independent per-sample error at the full sigma looks like 50 to 120 deg/s of motion to a 30 deg/s threshold. `last update correct %` is what the engine's final `update` said, which is what an overlay highlight would have been showing.

| sigma | trials | fixating % | commit correct % | last update correct % |
|-------|--------|------------|------------------|-----------------------|
| 0.70  | 66900  | 84.0       | 29.5             | 28.8                  |

