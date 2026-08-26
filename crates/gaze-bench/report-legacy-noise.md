# gaze-bench: offline snap-correct rate

15 screenshots, 3345 elements after filtering (1441 of them widgets, 0 removed by the size filter), 20 trials per element single-sample and 20 in sequence mode.

| setting | value |
|---|---|
| seed | 0 |
| noise model | legacy: the full sigma drawn independently every sample |
| ambiguity margin | 0.50 (duplicate rivals ignored) |
| snap radius | 2.00 deg |
| hysteresis margin | 0.150 |
| score weights | kind 0.600, area 0.150, distance 1.000 |
| size filter | short side >= 0 px, long side <= none |
| landing model | uniform in the box shrunk 20% per side, centre under 4 px |
| sequence fixation | 24 samples at 120 Hz, commit at zero latency |
| monte carlo wall clock | 3.4 s |

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
| 0.70  | single   | 66900  | 29.1      | 65.8   | 0.0    | 5.0       | 7.0            | 6.3          | 81.7        |
| 0.70  | sequence | 66900  | 37.1      | 60.4   | 2.5    | 0.0       | 7.8            | 5.4          | 84.3        |

### widget

| sigma | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|-------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.70  | single   | 28820  | 35.9      | 54.9   | 0.0    | 9.3       | 9.8            | 4.0          | 76.8        |
| 0.70  | sequence | 28820  | 47.0      | 48.3   | 4.7    | 0.0       | 11.9           | 5.1          | 78.3        |

### line

| sigma | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|-------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.70  | single   | 22040  | 26.7      | 72.2   | 0.0    | 1.1       | 5.3            | 5.6          | 88.0        |
| 0.70  | sequence | 22040  | 32.5      | 66.9   | 0.5    | 0.0       | 5.2            | 3.7          | 90.5        |

## Widget targets: do text distractors steal snaps?

Left half is widget targets scored against everything the detector found. Right half is the same targets with only widgets in the candidate set, which is what dropping OCR boxes from the snap index would look like. The gap is the cost of letting text runs compete.

| sigma | mode     | correct % (all) | correct % (widgets) | delta | conf-wrong % (all) | conf-wrong % (widgets) | ambiguous % (all) | ambiguous % (widgets) |
|-------|----------|-----------------|---------------------|-------|--------------------|------------------------|-------------------|-----------------------|
| 0.70  | single   | 35.9            | 42.4                | +6.6  | 4.0                | 4.5                    | 76.8              | 70.2                  |
| 0.70  | sequence | 47.0            | 53.5                | +6.5  | 5.1                | 4.5                    | 78.3              | 72.7                  |

## Ambiguity margin sweep (widget targets, sigma 0.70)

The two-tier gate: snap when the winner is clear, refine when it is not. `confident-wrong` is what the user has to undo and needs to be near zero; `ambiguous` is the refinement load. Percentages are of trials that produced an answer, so they exclude `none` and `no_gaze`.

| margin | candidates | mode     | answered | confident-correct % | confident-wrong % | ambiguous % |
|--------|------------|----------|----------|---------------------|-------------------|-------------|
| 0.25   | all        | single   | 26141    | 20.5                | 17.7              | 61.9        |
| 0.50   | all        | single   | 26141    | 10.9                | 4.4               | 84.7        |
| 1.00   | all        | single   | 26141    | 2.3                 | 0.1               | 97.6        |
| 0.25   | all        | sequence | 27454    | 19.5                | 9.1               | 71.5        |
| 0.50   | all        | sequence | 27454    | 12.5                | 5.3               | 82.2        |
| 1.00   | all        | sequence | 27454    | 5.6                 | 4.3               | 90.1        |
| 0.25   | widgets    | single   | 26083    | 28.1                | 17.4              | 54.6        |
| 0.50   | widgets    | single   | 26083    | 17.5                | 5.0               | 77.5        |
| 1.00   | widgets    | single   | 26083    | 8.6                 | 0.3               | 91.1        |
| 0.25   | widgets    | sequence | 27475    | 26.5                | 8.3               | 65.2        |
| 0.50   | widgets    | sequence | 27475    | 19.0                | 4.7               | 76.3        |
| 1.00   | widgets    | sequence | 27475    | 11.8                | 3.8               | 84.5        |

## Sanity: injected error

Distance from the clean landing point to the first noisy sample. Two Gaussian axes make the magnitude Rayleigh distributed, so the expectation is `sigma * sqrt(pi/2)` = 1.253 sigma. The bias/jitter split changes the correlation between samples, not the marginal per-sample distribution, so this line should match the same expectation under either noise model.

| sigma | samples | mean |noisy - clean| px | mean deg | expected deg |
|-------|---------|-------------------------|----------|--------------|
| 0.70  | 63546   | 57.6                    | 0.856    | 0.877        |

## By screenshot at sigma 0.70 (single-sample, full candidate set)

| screenshot | widgets | widget correct % | widget conf-wrong % | widget ambiguous % | lines | line correct % | all correct % |
|------------|---------|------------------|---------------------|--------------------|-------|----------------|---------------|
| DP-1-1     | 102     | 34.7             | 3.9                 | 80.5               | 154   | 27.1           | 29.3          |
| DP-1-2     | 102     | 35.1             | 4.2                 | 78.9               | 154   | 25.9           | 28.6          |
| DP-1-3     | 101     | 33.7             | 3.8                 | 81.5               | 154   | 27.1           | 28.6          |
| DP-1-4     | 102     | 33.6             | 3.9                 | 80.2               | 154   | 25.7           | 27.8          |
| DP-1-5     | 263     | 28.0             | 3.2                 | 88.3               | 95    | 25.1           | 24.4          |
| DP-2-1     | 130     | 41.0             | 3.8                 | 72.7               | 27    | 40.6           | 36.1          |
| DP-2-2     | 130     | 40.0             | 4.8                 | 70.0               | 26    | 43.8           | 36.8          |
| DP-2-3     | 130     | 39.5             | 4.8                 | 72.0               | 26    | 39.4           | 36.0          |
| DP-2-4     | 134     | 40.5             | 3.9                 | 70.7               | 27    | 39.6           | 36.5          |
| DP-2-5     | 157     | 35.4             | 4.8                 | 76.0               | 70    | 29.1           | 30.2          |
| HDMI-A-1-1 | 18      | 40.3             | 3.9                 | 61.1               | 43    | 20.0           | 19.5          |
| HDMI-A-1-2 | 18      | 41.7             | 3.9                 | 60.6               | 43    | 21.3           | 20.7          |
| HDMI-A-1-3 | 18      | 42.5             | 2.5                 | 59.2               | 43    | 19.1           | 19.5          |
| HDMI-A-1-4 | 18      | 38.9             | 3.6                 | 63.3               | 43    | 19.8           | 19.1          |
| HDMI-A-1-5 | 18      | 42.8             | 2.2                 | 64.7               | 43    | 20.5           | 20.2          |

## Breakdowns at sigma 0.70 (full candidate set)

### By output

| output   | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| DP-1     | 34840  | 27.5      | 69.7   | 0.0    | 2.8       | 5.4          | 86.4        | 34.6          |
| DP-2     | 23860  | 34.8      | 57.7   | 0.0    | 7.5       | 7.3          | 75.4        | 45.1          |
| HDMI-A-1 | 8200   | 19.8      | 72.9   | 0.0    | 7.3       | 7.0          | 80.4        | 24.3          |

### By target class

| class      | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| widget     | 28820  | 35.9      | 54.9   | 0.0    | 9.3       | 4.0          | 76.8        | 47.0          |
| line       | 22040  | 26.7      | 72.2   | 0.0    | 1.1       | 5.6          | 88.0        | 32.5          |
| text-other | 16040  | 20.5      | 76.7   | 0.0    | 2.7       | 11.4         | 81.9        | 25.5          |

### By element kind

| kind     | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| Button   | 37180  | 34.6      | 57.9   | 0.0    | 7.5       | 3.5          | 80.3        | 44.8          |
| Input    | 580    | 56.0      | 41.2   | 0.0    | 2.8       | 3.8          | 69.1        | 70.2          |
| Link     | 1900   | 50.1      | 49.1   | 0.0    | 0.8       | 4.6          | 75.0        | 61.9          |
| Text     | 27200  | 19.6      | 78.5   | 0.0    | 1.9       | 10.3         | 84.5        | 23.9          |
| Checkbox | 40     | 85.0      | 15.0   | 0.0    | 0.0       | 0.0          | 35.0        | 97.5          |

### By target size (shorter side, at the box centre)

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 49900  | 25.1      | 70.4   | 0.0    | 4.5       | 6.5          | 84.2        | 32.0          |
| 0.5 - 1 deg | 14320  | 39.6      | 52.6   | 0.0    | 7.8       | 5.3          | 75.5        | 51.5          |
| 1 - 2 deg   | 1740   | 54.1      | 45.7   | 0.0    | 0.2       | 7.6          | 65.3        | 56.8          |
| > 2 deg     | 940    | 40.1      | 59.9   | 0.0    | 0.0       | 8.4          | 76.7        | 49.9          |

### Widget targets by size

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 17640  | 30.4      | 60.4   | 0.0    | 9.1       | 4.5          | 79.5        | 40.2          |
| 0.5 - 1 deg | 10620  | 42.3      | 47.6   | 0.0    | 10.0      | 3.4          | 74.6        | 55.9          |
| 1 - 2 deg   | 560    | 83.4      | 16.1   | 0.0    | 0.5       | 2.1          | 35.5        | 90.9          |
| > 2 deg     | 0      | 0.0       | 0.0    | 0.0    | 0.0       | 0.0          | 0.0         | 0.0           |

## Top 10 confusions (single-sample, full candidate set)

| n  | screenshot | target               | target box      | chosen             | chosen box      | gap deg | overlap   |
|----|------------|----------------------|-----------------|--------------------|-----------------|---------|-----------|
| 19 | DP-2-1     | #168 Text text-other | 476,391 37x14   | #114 Button widget | 390,406 167x54  | 0.68    | separate  |
| 19 | DP-2-1     | #169 Text text-other | 529,393 14x10   | #114 Button widget | 390,406 167x54  | 1.21    | separate  |
| 19 | DP-2-3     | #167 Text text-other | 476,391 37x14   | #116 Button widget | 390,406 167x54  | 0.68    | separate  |
| 19 | DP-2-4     | #173 Text text-other | 529,395 13x7    | #112 Button widget | 390,406 167x54  | 1.21    | separate  |
| 18 | DP-1-3     | #297 Text text-other | 2810,1326 41x39 | #42 Button widget  | 2812,1329 34x32 | 0.02    | duplicate |
| 18 | DP-1-5     | #192 Button widget   | 5260,116 25x23  | #64 Input widget   | 5169,109 120x36 | 0.55    | nested    |
| 18 | DP-1-5     | #325 Text line       | 3720,162 337x35 | #303 Button widget | 3833,169 124x22 | 0.09    | nested    |
| 18 | DP-1-5     | #330 Text text-other | 4966,211 26x19  | #133 Button widget | 4962,238 24x19  | 0.38    | separate  |
| 18 | DP-1-5     | #357 Text text-other | 4059,427 110x33 | #219 Button widget | 4063,433 109x23 | 0.05    | duplicate |
| 18 | DP-2-2     | #167 Text text-other | 476,391 37x14   | #114 Button widget | 390,406 167x54  | 0.68    | separate  |

## Slip anatomy (full candidate set)

Every slip, classified by how the chosen box overlaps the intended one. `duplicate` is IoU >= 0.5: one thing on screen, two detections. `nested` is 80% of the smaller box inside the larger one but not the same box: a label inside its button, a terminal line inside its pane. `separate` is a genuinely different target.

| sigma | mode     | slips | duplicate % | nested % | separate % |
|-------|----------|-------|-------------|----------|------------|
| 0.70  | single   | 44039 | 1.4         | 5.7      | 92.9       |
| 0.70  | sequence | 40406 | 1.9         | 6.9      | 91.2       |

## Sequence-mode diagnostic

`fixating %` is the share of the 24 samples the I-VT classifier called a fixation; only those reach the engine's ring buffer, and `commit` can only answer from the ring. Under the legacy noise model this collapses, because independent per-sample error at the full sigma looks like 50 to 120 deg/s of motion to a 30 deg/s threshold. `last update correct %` is what the engine's final `update` said, which is what an overlay highlight would have been showing.

| sigma | trials | fixating % | commit correct % | last update correct % |
|-------|--------|------------|------------------|-----------------------|
| 0.70  | 66900  | 20.7       | 37.1             | 30.4                  |

