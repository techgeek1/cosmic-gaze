# gaze-bench: offline snap-correct rate

15 screenshots, 3345 elements after filtering (1441 of them widgets, 0 removed by the size filter), 20 trials per element single-sample and 20 in sequence mode.

| setting | value |
|---|---|
| seed | 0 |
| noise model | per-fixation bias + per-sample jitter, jitter 0.20 deg |
| ambiguity margin | 0.50 (duplicate rivals ignored) |
| off-desk samples | clamped to the panel edge |
| snap radius | 2.00 deg |
| hysteresis margin | 0.150 |
| score weights | kind 1.500, area 0.150, distance 1.000, center 0.000, center_deg 0.200 |
| size filter | short side >= 0 px, long side <= none |
| landing model | uniform in the box shrunk 20% per side, centre under 4 px |
| sequence fixation | 24 samples at 120 Hz, commit at zero latency |
| monte carlo wall clock | 5.3 s |

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

| sigma   | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|---------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.50    | single   | 66900  | 39.0      | 61.0   | 0.0    | 0.0       | 13.4           | 10.9         | 75.7        |
| 0.50    | sequence | 66900  | 39.8      | 60.2   | 0.0    | 0.0       | 13.8           | 10.6         | 75.6        |
| 0.70    | single   | 66900  | 31.3      | 68.7   | 0.0    | 0.0       | 11.2           | 13.2         | 75.6        |
| 0.70    | sequence | 66900  | 31.7      | 68.3   | 0.0    | 0.0       | 11.4           | 13.0         | 75.6        |
| 1.00    | single   | 66900  | 24.0      | 75.9   | 0.2    | 0.0       | 9.0            | 16.4         | 74.4        |
| 1.00    | sequence | 66900  | 24.0      | 75.9   | 0.1    | 0.0       | 9.0            | 16.2         | 74.7        |
| 1.50    | single   | 66900  | 16.9      | 82.0   | 1.1    | 0.0       | 6.5            | 20.6         | 71.9        |
| 1.50    | sequence | 66900  | 17.1      | 82.1   | 0.9    | 0.0       | 6.5            | 20.4         | 72.2        |
| profile | single   | 66900  | 16.7      | 42.6   | 10.8   | 30.0      | 7.1            | 11.4         | 40.8        |
| profile | sequence | 66900  | 16.9      | 42.4   | 10.7   | 30.0      | 7.2            | 11.2         | 40.9        |

### widget

| sigma   | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|---------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.50    | single   | 28820  | 55.9      | 44.1   | 0.0    | 0.0       | 21.0           | 3.7          | 75.3        |
| 0.50    | sequence | 28820  | 57.0      | 43.0   | 0.0    | 0.0       | 21.8           | 3.2          | 75.0        |
| 0.70    | single   | 28820  | 44.8      | 55.2   | 0.0    | 0.0       | 17.4           | 7.1          | 75.6        |
| 0.70    | sequence | 28820  | 45.5      | 54.5   | 0.0    | 0.0       | 17.6           | 6.7          | 75.6        |
| 1.00    | single   | 28820  | 34.6      | 65.1   | 0.3    | 0.0       | 13.8           | 11.2         | 74.8        |
| 1.00    | sequence | 28820  | 34.5      | 65.3   | 0.2    | 0.0       | 13.6           | 11.1         | 75.1        |
| 1.50    | single   | 28820  | 24.8      | 73.7   | 1.5    | 0.0       | 9.8            | 16.9         | 71.8        |
| 1.50    | sequence | 28820  | 25.0      | 73.8   | 1.2    | 0.0       | 9.8            | 16.8         | 72.2        |
| profile | single   | 28820  | 23.5      | 33.1   | 13.9   | 29.6      | 11.2           | 7.5          | 37.9        |
| profile | sequence | 28820  | 23.7      | 32.9   | 13.9   | 29.6      | 11.3           | 7.2          | 38.0        |

### line

| sigma   | mode     | trials | correct % | slip % | none % | no_gaze % | conf-correct % | conf-wrong % | ambiguous % |
|---------|----------|--------|-----------|--------|--------|-----------|----------------|--------------|-------------|
| 0.50    | single   | 22040  | 31.0      | 69.0   | 0.0    | 0.0       | 8.8            | 10.0         | 81.3        |
| 0.50    | sequence | 22040  | 31.8      | 68.2   | 0.0    | 0.0       | 8.9            | 9.7          | 81.4        |
| 0.70    | single   | 22040  | 25.4      | 74.6   | 0.0    | 0.0       | 7.8            | 11.1         | 81.1        |
| 0.70    | sequence | 22040  | 25.9      | 74.1   | 0.0    | 0.0       | 7.9            | 11.3         | 80.8        |
| 1.00    | single   | 22040  | 19.3      | 80.7   | 0.0    | 0.0       | 6.5            | 13.9         | 79.6        |
| 1.00    | sequence | 22040  | 19.3      | 80.7   | 0.0    | 0.0       | 6.6            | 13.6         | 79.8        |
| 1.50    | single   | 22040  | 13.7      | 85.8   | 0.5    | 0.0       | 4.9            | 17.2         | 77.4        |
| 1.50    | sequence | 22040  | 13.9      | 85.8   | 0.4    | 0.0       | 4.9            | 16.9         | 77.7        |
| profile | single   | 22040  | 12.6      | 40.1   | 6.6    | 40.7      | 4.5            | 9.3          | 38.9        |
| profile | sequence | 22040  | 12.9      | 39.8   | 6.6    | 40.7      | 4.6            | 9.1          | 39.0        |

## Top-k accuracy (single-sample)

Candidates ranked by cost ascending. `ranked` counts trials that produced a candidate list at all, so it excludes lost gaze and the sigma-over-radius bail; the percentages are of those. A candidate the rival policy says is the same thing on screen as the target counts as the target, so detector duplication cannot push a target down its own ranking. `near` is how many candidates sat within the 0.50 ambiguity margin of the winner, which is the size of the hint a refinement tier would show.

| candidates | class  | sigma   | ranked | top-1 % | top-2 % | top-3 % | top-5 % | mean near | p90 near |
|------------|--------|---------|--------|---------|---------|---------|---------|-----------|----------|
| all        | all    | 0.50    | 66900  | 40.1    | 61.4    | 73.6    | 85.6    | 2.6       | 4        |
| all        | all    | 0.70    | 66894  | 32.3    | 51.8    | 64.7    | 79.7    | 2.6       | 4        |
| all        | all    | 1.00    | 66779  | 24.8    | 41.3    | 53.4    | 69.8    | 2.5       | 4        |
| all        | all    | 1.50    | 66188  | 17.7    | 30.2    | 40.2    | 54.7    | 2.5       | 4        |
| all        | all    | profile | 39663  | 29.0    | 46.9    | 58.9    | 74.1    | 2.3       | 4        |
| all        | widget | 0.50    | 28820  | 55.9    | 78.1    | 88.2    | 95.6    | 2.5       | 4        |
| all        | widget | 0.70    | 28815  | 44.8    | 66.3    | 78.4    | 90.2    | 2.6       | 4        |
| all        | widget | 1.00    | 28734  | 34.7    | 54.1    | 66.1    | 80.3    | 2.5       | 4        |
| all        | widget | 1.50    | 28386  | 25.2    | 40.2    | 50.3    | 63.2    | 2.5       | 4        |
| all        | widget | profile | 16291  | 41.5    | 61.3    | 71.6    | 83.3    | 2.2       | 4        |
| all        | line   | 0.50    | 22040  | 32.1    | 53.0    | 66.6    | 80.5    | 2.7       | 4        |
| all        | line   | 0.70    | 22039  | 26.3    | 44.8    | 58.3    | 74.6    | 2.7       | 4        |
| all        | line   | 1.00    | 22033  | 20.1    | 35.1    | 47.5    | 64.4    | 2.7       | 4        |
| all        | line   | 1.50    | 21937  | 14.5    | 25.5    | 35.7    | 51.0    | 2.7       | 4        |
| all        | line   | profile | 11619  | 24.7    | 42.4    | 56.0    | 72.5    | 2.4       | 4        |
| widgets    | all    | 0.50    | 28820  | 59.4    | 82.1    | 91.1    | 97.2    | 2.3       | 4        |
| widgets    | all    | 0.70    | 28795  | 49.1    | 71.9    | 83.5    | 93.7    | 2.3       | 4        |
| widgets    | all    | 1.00    | 28486  | 39.3    | 60.8    | 73.6    | 86.4    | 2.3       | 4        |
| widgets    | all    | 1.50    | 27265  | 30.1    | 47.7    | 58.9    | 71.4    | 2.3       | 4        |
| widgets    | all    | profile | 16112  | 45.5    | 65.9    | 76.4    | 86.7    | 2.0       | 3        |
| widgets    | widget | 0.50    | 28820  | 59.4    | 82.1    | 91.1    | 97.2    | 2.3       | 4        |
| widgets    | widget | 0.70    | 28795  | 49.1    | 71.9    | 83.5    | 93.7    | 2.3       | 4        |
| widgets    | widget | 1.00    | 28486  | 39.3    | 60.8    | 73.6    | 86.4    | 2.3       | 4        |
| widgets    | widget | 1.50    | 27265  | 30.1    | 47.7    | 58.9    | 71.4    | 2.3       | 4        |
| widgets    | widget | profile | 16112  | 45.5    | 65.9    | 76.4    | 86.7    | 2.0       | 3        |

## Nudge distance and flick coverage (single-sample)

Distance from the point the system would warp to (the snapped target's clamped point, or the raw gaze when nothing snapped) to the nearest point of the intended target's box, so a warp that already landed on the target measures zero. Percentiles come from a 0.02 deg / 1 px histogram and read as upper edges; `>=` marks the overflow bin.

`flick %` is the share of ranked trials where the intended target is inside the best 3 candidates *and* its direction from the warp point falls in a different 45 degree sector from every other one, so a single directional flick would be unambiguous.

| class  | sigma   | trials | inside % | median deg | p90 deg | p99 deg | median px | p90 px | p99 px | flick % |
|--------|---------|--------|----------|------------|---------|---------|-----------|--------|--------|---------|
| all    | 0.50    | 66900  | 45.6     | 0.08       | 0.74    | 1.42    | 5         | 50     | 95     | 51.9    |
| all    | 0.70    | 66900  | 36.8     | 0.26       | 1.08    | 2.00    | 17        | 72     | 134    | 44.8    |
| all    | 1.00    | 66900  | 28.3     | 0.48       | 1.60    | 2.76    | 32        | 108    | 189    | 36.0    |
| all    | 1.50    | 66900  | 20.0     | 0.88       | 2.60    | 4.14    | 58        | 173    | 282    | 26.7    |
| all    | profile | 46858  | 28.2     | 0.50       | 2.22    | 4.86    | 32        | 143    | 323    | 41.2    |
| widget | 0.50    | 28820  | 57.0     | 0.02       | 0.64    | 1.20    | 1         | 43     | 82     | 65.4    |
| widget | 0.70    | 28820  | 45.7     | 0.14       | 1.00    | 1.88    | 10        | 66     | 130    | 57.4    |
| widget | 1.00    | 28820  | 35.3     | 0.42       | 1.58    | 2.74    | 27        | 106    | 186    | 47.1    |
| widget | 1.50    | 28820  | 25.3     | 0.84       | 2.64    | 4.24    | 54        | 175    | 288    | 35.5    |
| widget | profile | 20294  | 34.2     | 0.50       | 2.66    | 5.22    | 32        | 171    | 348    | 51.7    |

## Widget targets: do text distractors steal snaps?

Left half is widget targets scored against everything the detector found. Right half is the same targets with only widgets in the candidate set, which is what dropping OCR boxes from the snap index would look like. The gap is the cost of letting text runs compete.

| sigma   | mode     | correct % (all) | correct % (widgets) | delta | conf-wrong % (all) | conf-wrong % (widgets) | ambiguous % (all) | ambiguous % (widgets) |
|---------|----------|-----------------|---------------------|-------|--------------------|------------------------|-------------------|-----------------------|
| 0.50    | single   | 55.9            | 59.4                | +3.5  | 3.7                | 3.9                    | 75.3              | 71.2                  |
| 0.50    | sequence | 57.0            | 60.9                | +3.9  | 3.2                | 3.3                    | 75.0              | 71.1                  |
| 0.70    | single   | 44.8            | 49.0                | +4.3  | 7.1                | 7.3                    | 75.6              | 71.3                  |
| 0.70    | sequence | 45.5            | 49.4                | +3.9  | 6.7                | 7.1                    | 75.6              | 71.3                  |
| 1.00    | single   | 34.6            | 38.9                | +4.2  | 11.2               | 11.8                   | 74.8              | 70.0                  |
| 1.00    | sequence | 34.5            | 39.0                | +4.4  | 11.1               | 11.7                   | 75.1              | 70.2                  |
| 1.50    | single   | 24.8            | 28.5                | +3.7  | 16.9               | 17.5                   | 71.8              | 64.4                  |
| 1.50    | sequence | 25.0            | 28.6                | +3.6  | 16.8               | 17.6                   | 72.2              | 65.0                  |
| profile | single   | 23.5            | 25.5                | +2.0  | 7.5                | 7.4                    | 37.9              | 35.6                  |
| profile | sequence | 23.7            | 25.5                | +1.9  | 7.2                | 7.1                    | 38.0              | 35.9                  |

## Ambiguity margin sweep (widget targets, sigma 0.70)

The two-tier gate: snap when the winner is clear, refine when it is not. `confident-wrong` is what the user has to undo and needs to be near zero; `ambiguous` is the refinement load. Percentages are of trials that produced an answer, so they exclude `none` and `no_gaze`.

| margin | candidates | mode     | answered | confident-correct % | confident-wrong % | ambiguous % |
|--------|------------|----------|----------|---------------------|-------------------|-------------|
| 0.25   | all        | single   | 28815    | 28.4                | 21.6              | 50.0        |
| 0.50   | all        | single   | 28815    | 17.4                | 7.1               | 75.6        |
| 1.00   | all        | single   | 28815    | 6.5                 | 0.6               | 92.9        |
| 0.25   | all        | sequence | 28818    | 29.0                | 20.7              | 50.3        |
| 0.50   | all        | sequence | 28818    | 17.6                | 6.7               | 75.6        |
| 1.00   | all        | sequence | 28818    | 6.7                 | 0.5               | 92.8        |
| 0.25   | widgets    | single   | 28795    | 32.6                | 20.7              | 46.7        |
| 0.50   | widgets    | single   | 28795    | 21.3                | 7.3               | 71.3        |
| 1.00   | widgets    | single   | 28795    | 9.9                 | 0.7               | 89.4        |
| 0.25   | widgets    | sequence | 28807    | 33.1                | 20.3              | 46.7        |
| 0.50   | widgets    | sequence | 28807    | 21.6                | 7.1               | 71.4        |
| 1.00   | widgets    | sequence | 28807    | 10.0                | 0.6               | 89.4        |

## Sanity: injected error

Distance from the clean landing point to the first noisy sample. Two Gaussian axes make the magnitude Rayleigh distributed, so the expectation is `sigma * sqrt(pi/2)` = 1.253 sigma. The bias/jitter split changes the correlation between samples, not the marginal per-sample distribution, so this line should match the same expectation under either noise model.

| sigma   | samples | mean |noisy - clean| px | mean deg | expected deg |
|---------|---------|-------------------------|----------|--------------|
| 0.50    | 65694   | 41.1                    | 0.629    | 0.627        |
| 0.70    | 65432   | 57.5                    | 0.874    | 0.877        |
| 1.00    | 65128   | 81.0                    | 1.225    | 1.253        |
| 1.50    | 65048   | 118.4                   | 1.783    | 1.880        |
| profile | 45974   | 87.5                    | 1.384    | varies       |

## By screenshot at sigma 0.70 (single-sample, full candidate set)

| screenshot | widgets | widget correct % | widget conf-wrong % | widget ambiguous % | lines | line correct % | all correct % |
|------------|---------|------------------|---------------------|--------------------|-------|----------------|---------------|
| DP-1-1     | 102     | 42.4             | 7.5                 | 75.6               | 154   | 23.8           | 29.9          |
| DP-1-2     | 102     | 43.1             | 7.1                 | 77.8               | 154   | 24.2           | 29.9          |
| DP-1-3     | 101     | 43.1             | 7.1                 | 76.8               | 154   | 24.3           | 30.2          |
| DP-1-4     | 102     | 43.1             | 7.4                 | 77.0               | 154   | 24.3           | 29.7          |
| DP-1-5     | 263     | 31.1             | 6.1                 | 83.9               | 95    | 23.8           | 24.8          |
| DP-2-1     | 130     | 50.5             | 8.7                 | 69.8               | 27    | 37.2           | 38.8          |
| DP-2-2     | 130     | 51.1             | 6.8                 | 71.6               | 26    | 38.8           | 40.2          |
| DP-2-3     | 130     | 50.4             | 7.3                 | 71.9               | 26    | 40.8           | 40.1          |
| DP-2-4     | 134     | 52.4             | 7.0                 | 70.8               | 27    | 37.4           | 40.5          |
| DP-2-5     | 157     | 45.3             | 7.4                 | 76.8               | 70    | 30.2           | 32.9          |
| HDMI-A-1-1 | 18      | 53.6             | 6.7                 | 69.4               | 43    | 21.0           | 22.8          |
| HDMI-A-1-2 | 18      | 51.7             | 6.4                 | 73.9               | 43    | 20.6           | 22.1          |
| HDMI-A-1-3 | 18      | 56.7             | 6.1                 | 65.6               | 43    | 22.4           | 24.2          |
| HDMI-A-1-4 | 18      | 56.1             | 5.6                 | 71.4               | 43    | 22.3           | 24.0          |
| HDMI-A-1-5 | 18      | 58.9             | 6.4                 | 68.9               | 43    | 21.4           | 24.1          |

## Breakdowns at sigma 0.70 (full candidate set)

### By output

| output   | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| DP-1     | 34840  | 28.6      | 71.4   | 0.0    | 0.0       | 11.6         | 79.1        | 29.1          |
| DP-2     | 23860  | 38.1      | 61.9   | 0.0    | 0.0       | 15.8         | 69.2        | 38.3          |
| HDMI-A-1 | 8200   | 23.5      | 76.5   | 0.0    | 0.0       | 12.2         | 79.5        | 23.8          |

### By target class

| class      | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| widget     | 28820  | 44.8      | 55.2   | 0.0    | 0.0       | 7.1          | 75.6        | 45.5          |
| line       | 22040  | 25.4      | 74.6   | 0.0    | 0.0       | 11.1         | 81.1        | 25.9          |
| text-other | 16040  | 15.4      | 84.6   | 0.0    | 0.0       | 27.0         | 68.4        | 15.1          |

### By element kind

| kind     | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|----------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| Button   | 37180  | 41.7      | 58.2   | 0.0    | 0.0       | 6.0          | 79.1        | 42.4          |
| Input    | 580    | 60.2      | 39.8   | 0.0    | 0.0       | 7.9          | 55.7        | 63.6          |
| Link     | 1900   | 54.6      | 45.4   | 0.0    | 0.0       | 6.2          | 63.1        | 55.8          |
| Text     | 27200  | 14.8      | 85.2   | 0.0    | 0.0       | 23.6         | 72.3        | 14.7          |
| Checkbox | 40     | 100.0     | 0.0    | 0.0    | 0.0       | 0.0          | 5.0         | 100.0         |

### By target size (shorter side, at the box centre)

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 49900  | 27.1      | 72.9   | 0.0    | 0.0       | 13.7         | 78.0        | 27.5          |
| 0.5 - 1 deg | 14320  | 45.1      | 54.9   | 0.0    | 0.0       | 10.0         | 71.0        | 45.7          |
| 1 - 2 deg   | 1740   | 43.2      | 56.8   | 0.0    | 0.0       | 20.1         | 51.7        | 42.0          |
| > 2 deg     | 940    | 25.5      | 74.5   | 0.0    | 0.0       | 23.7         | 64.1        | 25.2          |

### Widget targets by nesting

`nested in another kind` means some larger candidate of a different detector kind wraps the target, so `distance` is zero for both whenever the gaze is inside the inner one and only the kind, area and centre terms can separate them.

| nesting                | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|------------------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| nested in another kind | 6300   | 42.0      | 58.0   | 0.0    | 0.0       | 7.0          | 77.0        | 43.2          |
| not nested             | 22520  | 45.6      | 54.4   | 0.0    | 0.0       | 7.1          | 75.2        | 46.1          |

| nesting                | elements | ranked | top-1 % | top-2 % | top-3 % | top-5 % |
|------------------------|----------|--------|---------|---------|---------|---------|
| nested in another kind | 315      | 6300   | 42.0    | 62.8    | 75.3    | 88.1    |
| not nested             | 1126     | 22515  | 45.6    | 67.3    | 79.2    | 90.8    |

### Widget targets by size

| size        | trials | correct % | slip % | none % | no_gaze % | conf-wrong % | ambiguous % | seq correct % |
|-------------|--------|-----------|--------|--------|-----------|--------------|-------------|---------------|
| < 0.5 deg   | 17640  | 40.1      | 59.9   | 0.0    | 0.0       | 7.8          | 78.7        | 40.7          |
| 0.5 - 1 deg | 10620  | 50.6      | 49.4   | 0.0    | 0.0       | 6.1          | 72.8        | 51.5          |
| 1 - 2 deg   | 560    | 83.8      | 16.2   | 0.0    | 0.0       | 3.9          | 28.9        | 82.1          |
| > 2 deg     | 0      | 0.0       | 0.0    | 0.0    | 0.0       | 0.0          | 0.0         | 0.0           |

## Top 10 confusions (single-sample, full candidate set)

| n  | screenshot | target               | target box       | chosen             | chosen box       | gap deg | overlap   |
|----|------------|----------------------|------------------|--------------------|------------------|---------|-----------|
| 20 | DP-1-1     | #188 Text text-other | 4612,80 80x22    | #5 Button line     | 4605,45 191x31   | 0.76    | separate  |
| 20 | DP-1-1     | #299 Text text-other | 2810,1326 41x39  | #41 Button widget  | 2812,1329 34x32  | 0.02    | duplicate |
| 20 | DP-1-3     | #297 Text text-other | 2810,1326 41x39  | #42 Button widget  | 2812,1329 34x32  | 0.02    | duplicate |
| 20 | DP-1-4     | #250 Text line       | 5421,517 264x29  | #89 Button line    | 5417,456 261x23  | 0.87    | separate  |
| 20 | DP-1-4     | #299 Text text-other | 2810,1326 41x39  | #40 Button widget  | 2812,1329 34x32  | 0.02    | duplicate |
| 20 | DP-1-5     | #330 Text text-other | 4966,211 26x19   | #133 Button widget | 4962,238 24x19   | 0.38    | separate  |
| 20 | DP-1-5     | #443 Text line       | 5421,1419 264x29 | #278 Button line   | 5418,1379 251x21 | 0.59    | separate  |
| 20 | DP-2-3     | #169 Text text-other | 529,395 13x7     | #116 Button widget | 390,406 167x54   | 1.21    | separate  |
| 20 | DP-2-4     | #171 Text text-other | 476,391 37x14    | #112 Button widget | 390,406 167x54   | 0.68    | separate  |
| 20 | DP-2-4     | #173 Text text-other | 529,395 13x7     | #112 Button widget | 390,406 167x54   | 1.21    | separate  |

## Slip anatomy (full candidate set)

Every slip, classified by how the chosen box overlaps the intended one. `duplicate` is IoU >= 0.5: one thing on screen, two detections. `nested` is 80% of the smaller box inside the larger one but not the same box: a label inside its button, a terminal line inside its pane. `separate` is a genuinely different target.

| sigma   | mode     | slips | duplicate % | nested % | separate % |
|---------|----------|-------|-------------|----------|------------|
| 0.50    | single   | 40797 | 1.9         | 6.9      | 91.3       |
| 0.50    | sequence | 40257 | 2.0         | 7.0      | 91.0       |
| 0.70    | single   | 45928 | 1.4         | 5.8      | 92.9       |
| 0.70    | sequence | 45672 | 1.4         | 5.9      | 92.7       |
| 1.00    | single   | 50747 | 1.0         | 4.6      | 94.4       |
| 1.00    | sequence | 50793 | 1.0         | 4.6      | 94.4       |
| 1.50    | single   | 54855 | 0.7         | 3.5      | 95.8       |
| 1.50    | sequence | 54899 | 0.7         | 3.5      | 95.8       |
| profile | single   | 28487 | 1.1         | 4.3      | 94.6       |
| profile | sequence | 28389 | 1.1         | 4.4      | 94.5       |

## Sequence-mode diagnostic

`fixating %` is the share of the 24 samples the I-VT classifier called a fixation; only those reach the engine's ring buffer, and `commit` can only answer from the ring. Under the legacy noise model this collapses, because independent per-sample error at the full sigma looks like 50 to 120 deg/s of motion to a 30 deg/s threshold. `last update correct %` is what the engine's final `update` said, which is what an overlay highlight would have been showing.

| sigma   | trials | fixating % | commit correct % | last update correct % |
|---------|--------|------------|------------------|-----------------------|
| 0.50    | 66900  | 89.7       | 39.8             | 39.8                  |
| 0.70    | 66900  | 89.7       | 31.7             | 31.7                  |
| 1.00    | 66900  | 89.7       | 24.0             | 23.9                  |
| 1.50    | 66900  | 89.7       | 17.1             | 17.0                  |
| profile | 66900  | 89.8       | 16.9             | 16.9                  |

