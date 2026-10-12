# Evidence in review cochange results

Cochange reports describe files that changed together in Git history. The raw
`confidence` (`c` in hook output) remains the observed frequency `n / d`.
`ranking_score` (`s`) is a separate ranking value. Neither value proves that a
file needs another edit. Cochange alone does not block the review hook.

Review emits the evidence source for both modes: `blame` for changed-line blame,
and `history` for the file-history fallback.

## Current references

Review ranks candidates with observed import evidence first, then path mentions,
then pairs supported only by history. The existing statistical ranking applies
within each group. The raw frequency remains unchanged.

`actionability: verification_target` accompanies `current_dependency`, whose
`kind` distinguishes `static_import_candidate` from `path_mention`. Its
`direction` is `changed_to_related`, `related_to_changed`, or `bidirectional`.
An import candidate uses a unique relative JS/TS source path or a supported
Python module path. It requires verification with the project's actual resolver.
Path mentions can occur in comments and documents. Neither fact establishes a
required edit or a semantic dependency. Without these observations, the result
remains `history_only`; that does not prove that the files are independent.

`small_sample: true` marks a denominator below five. Commit-size weighting,
smoothing, and the configured support thresholds still determine the statistical
ranking. Sample size and dependency evidence have separate fields.

Review applies explicit `--exclude-dir` and `--exclude-glob` settings to both
sources and candidates, and skips gitlinks. For a deleted source, a readable
candidate in the same supported language family is omitted from historical
suggestions only after successful parsing and import resolution find no observed
reference. Unresolved aliases, relative Python imports, other languages, dynamic
loaders, parse errors, and unavailable probes remain unassessed. Import and text
probes read the captured tree for staged input, including documents that are not
materialized as AST sources. Probe limits are 256 files, 1 MiB per file, and
32 MiB total; `cochange_diagnostics.review_filters` reports exclusions and limits.

## Changes in preceding commits

`interpretation` (`i` in hook output) can include a `recent` record. The scan uses
the pinned review base, its upstream fork point, and a contiguous first-parent
suffix. It accepts only non-merge commits with the same raw author identity as
the configured Git author, within the last 30 minutes by committer timestamp.
The scan checks at most 32 commits. A candidate must have a net file change;
changes that were subsequently reverted do not count.

| `resolution` | Meaning |
| --- | --- |
| `changed_in_recent_commits` | The related file has a net change in the verified suffix. |
| `not_changed_in_recent_suffix` | The scan reached the fork point and found no net change for this file. |
| `not_assessed` | The scan could not establish either fact for this file. |

For a verified preceding change, `verification_direction` is
`check_current_change_against_prior_change`: check whether the current edit
reflects that preceding edit. This direction is independent of the historical
frequency and import direction. It does not establish causality or completion.

The `recent` record includes the base and fork-point revisions, the supporting
commit when available, the number of checked commits, and the time window.
Unavailable upstreams, unknown fork points, shallow history, author boundaries,
merges, time limits, malformed metadata, and Git failures have explicit reason
codes. A scan that exceeds 32 commits sets `recent_scan_truncated: true` and keeps the result
unassessed. An unavailable assessment is not evidence that the file is unchanged.
For a verified change, a reason can describe the boundary where the scan stopped;
the supporting commit still establishes the change within the checked suffix.

Ordinary review output and hook output preserve the same interpretation. These
fields do not change API, impact, or dead-code blocking decisions.

Rust library callers should clone `CoChangeInterpretation` when they need an
owned copy; the DTO now owns optional revision strings and no longer implements
`Copy`.
