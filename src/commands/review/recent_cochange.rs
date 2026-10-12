//! 先行コミットでの変更事実。変更義務・実装の完了状態は推定しない。

use std::collections::HashSet;
use std::process::Stdio;

use crate::models::cochange::{
    CoChangeResolution, CochangeVerificationDirection, RecentCochangeEvidence,
    RecentCochangeReason as Reason,
};
use crate::models::review::MissingCochange;

const WINDOW_SECONDS: u64 = 30 * 60;
const MAX_COMMITS: usize = 32;

pub(super) fn git(dir: &str, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let output = crate::commands::common::read_bytes_limited_and_drain(
        child.stdout.take()?,
        2 * 1024 * 1024,
        "recent cochange metadata",
    );
    if !child.wait().ok()?.success() {
        return None;
    }
    output.ok()
}

fn text(dir: &str, args: &[&str]) -> Option<String> {
    Some(
        String::from_utf8(git(dir, args)?)
            .ok()?
            .trim_end_matches('\n')
            .to_owned(),
    )
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

struct Commit {
    oid: String,
    parents: Vec<String>,
    author: (String, String),
    timestamp: u64,
}

fn commits(raw: &str) -> Option<Vec<Commit>> {
    let mut fields: Vec<_> = raw.split('\0').collect();
    if fields.last()?.trim().is_empty() {
        fields.pop();
    }
    if !fields.len().is_multiple_of(5) {
        return None;
    }
    let mut result = Vec::new();
    for chunk in fields.as_chunks::<5>().0 {
        let id = chunk[0].trim();
        let parents: Vec<String> = chunk[1].split_whitespace().map(str::to_owned).collect();
        if !oid(id) || parents.iter().any(|p| !oid(p)) {
            return None;
        }
        result.push(Commit {
            oid: id.to_owned(),
            parents,
            author: (chunk[2].to_owned(), chunk[3].to_owned()),
            timestamp: chunk[4].parse().ok()?,
        });
    }
    Some(result)
}

struct Scan {
    evidence: RecentCochangeEvidence,
    eligible: HashSet<String>,
    net_changes: HashSet<String>,
    complete: bool,
}

impl Scan {
    fn new(base: String) -> Self {
        Self {
            evidence: RecentCochangeEvidence {
                base,
                fork_point: None,
                commit: None,
                checked_commits: 0,
                window_seconds: WINDOW_SECONDS,
                reason: None,
                truncated: false,
            },
            eligible: HashSet::new(),
            net_changes: HashSet::new(),
            complete: false,
        }
    }

    fn failed(mut self, reason: Reason) -> Self {
        self.evidence.reason = Some(reason);
        self
    }
}

fn scan(dir: &str, base: &str, now: u64) -> Scan {
    let mut result = Scan::new(base.to_owned());
    if !oid(base) {
        return result.failed(Reason::InvalidMetadata);
    }
    if text(dir, &["rev-parse", "--is-shallow-repository"]).as_deref() != Some("false") {
        return result.failed(Reason::ShallowHistory);
    }
    let Some(upstream) = text(
        dir,
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
    )
    .filter(|s| !s.is_empty() && !s.starts_with('-')) else {
        return result.failed(Reason::NoUpstream);
    };
    let Some(upstream_oid) = text(
        dir,
        &["rev-parse", "--verify", &format!("{upstream}^{{commit}}")],
    )
    .filter(|s| oid(s)) else {
        return result.failed(Reason::NoUpstream);
    };
    let Some(anchor) =
        text(dir, &["merge-base", "--fork-point", &upstream, base]).filter(|s| oid(s))
    else {
        return result.failed(Reason::NoForkPoint);
    };
    if git(dir, &["merge-base", "--is-ancestor", &anchor, base]).is_none() {
        return result.failed(Reason::NoForkPoint);
    }
    result.evidence.fork_point = Some(anchor.clone());
    let Some(identity) = text(dir, &["var", "GIT_AUTHOR_IDENT"]) else {
        return result.failed(Reason::InvalidMetadata);
    };
    let Some((identity, _)) = identity.rsplit_once("> ") else {
        return result.failed(Reason::InvalidMetadata);
    };
    let Some((author_name, author_email)) = identity.rsplit_once(" <") else {
        return result.failed(Reason::InvalidMetadata);
    };
    let Some(raw) = text(
        dir,
        &[
            "log",
            "--first-parent",
            "--max-count=33",
            "--format=%H%x00%P%x00%an%x00%ae%x00%ct%x00",
            base,
            "--",
        ],
    ) else {
        return result.failed(Reason::GitFailure);
    };
    let Some(commits) = commits(&raw) else {
        return result.failed(Reason::InvalidMetadata);
    };
    if commits.first().is_none_or(|c| c.oid != base) {
        return result.failed(Reason::InvalidMetadata);
    }
    let mut lower = None;
    for commit in commits {
        if commit.oid == anchor {
            result.complete = true;
            break;
        }
        if commit.timestamp > now {
            return result.failed(Reason::InvalidMetadata);
        }
        if now.saturating_sub(commit.timestamp) > WINDOW_SECONDS {
            result.evidence.reason = Some(Reason::OutsideWindow);
            break;
        }
        if commit.parents.len() != 1 {
            result.evidence.reason = Some(Reason::MergeBoundary);
            break;
        }
        // mailmap による同一視ではなく、raw author の厳密一致で限定する。
        if commit.author.0 != author_name || commit.author.1 != author_email {
            result.evidence.reason = Some(Reason::OtherAuthor);
            break;
        }
        if result.eligible.len() == MAX_COMMITS {
            result.evidence.truncated = true;
            result.eligible.clear();
            return result.failed(Reason::Truncated);
        }
        lower = commit.parents.first().cloned();
        result.eligible.insert(commit.oid);
        result.evidence.checked_commits += 1;
    }
    if text(
        dir,
        &["rev-parse", "--verify", &format!("{upstream}^{{commit}}")],
    )
    .as_deref()
        != Some(upstream_oid.as_str())
    {
        result.eligible.clear();
        return result.failed(Reason::ReferenceChanged);
    }
    if let Some(lower) = lower {
        let mut args = vec!["diff", "--name-only", "-z", "--relative", "--no-renames"];
        args.extend(crate::git_support::GIT_DIFF_PARSEABLE_OUTPUT_ARGS);
        args.extend([lower.as_str(), base, "--"]);
        let Some(paths) = git(dir, &args).and_then(|p| String::from_utf8(p).ok()) else {
            result.eligible.clear();
            return result.failed(Reason::GitFailure);
        };
        result.net_changes = paths
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    result
}

pub(super) fn annotate(dir: &str, base: Option<&str>, items: &mut [MissingCochange]) {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    annotate_at(dir, base, items, now.as_secs());
}

fn annotate_at(dir: &str, base: Option<&str>, items: &mut [MissingCochange], now: u64) {
    if items.is_empty() {
        return;
    }
    let base = base
        .map(str::to_owned)
        .or_else(|| text(dir, &["rev-parse", "--verify", "HEAD^{commit}"]))
        .unwrap_or_default();
    let scan = scan(dir, &base, now);
    for item in items {
        let mut evidence = scan.evidence.clone();
        if scan.net_changes.contains(&item.file) && !scan.eligible.is_empty() {
            let commit = text(
                dir,
                &[
                    "--literal-pathspecs",
                    "log",
                    "-1",
                    "--first-parent",
                    "--format=%H",
                    &base,
                    "--",
                    &item.file,
                ],
            );
            if let Some(commit) = commit.filter(|c| scan.eligible.contains(c)) {
                evidence.commit = Some(commit);
                item.interpretation.resolution = CoChangeResolution::ChangedInRecentCommits;
                item.interpretation.verification_direction =
                    Some(CochangeVerificationDirection::CheckCurrentChangeAgainstPriorChange);
            } else {
                evidence.reason = Some(Reason::GitFailure);
            }
        } else if scan.complete {
            item.interpretation.resolution = CoChangeResolution::NotChangedInRecentSuffix;
        }
        item.interpretation.recent = Some(evidence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::cochange::CoChangeInterpretation;
    use std::path::Path;

    const START: u64 = 1_700_000_000;

    fn command(repo: &Path, args: &[&str]) {
        let output = crate::git_support::isolated_git_command()
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(repo: &Path, path: &str, body: &str, time: u64, other_author: bool) {
        std::fs::write(repo.join(path), body).unwrap();
        command(repo, &["add", "-A"]);
        let output = crate::git_support::isolated_git_command()
            .args(["commit", "--no-verify", "-qm", "change"])
            .current_dir(repo)
            .env(
                "GIT_AUTHOR_NAME",
                if other_author { "Other" } else { "Test" },
            )
            .env(
                "GIT_AUTHOR_EMAIL",
                if other_author {
                    "other@example.com"
                } else {
                    "you@example.com"
                },
            )
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "you@example.com")
            .env("GIT_AUTHOR_DATE", format!("{time} +0000"))
            .env("GIT_COMMITTER_DATE", format!("{time} +0000"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repo(upstream: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        command(p, &["init", "-qb", "main"]);
        command(p, &["config", "user.name", "Test"]);
        command(p, &["config", "user.email", "you@example.com"]);
        command(p, &["config", "commit.gpgsign", "false"]);
        std::fs::write(p.join("a.txt"), "initial").unwrap();
        commit(p, "b.txt", "initial", START, false);
        if upstream {
            command(
                p,
                &["remote", "add", "origin", "https://example.com/project.git"],
            );
            command(p, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
            command(p, &["config", "branch.main.remote", "origin"]);
            command(p, &["config", "branch.main.merge", "refs/heads/main"]);
        }
        dir
    }

    fn item(path: &str, source: &str) -> MissingCochange {
        MissingCochange {
            file: path.to_owned(),
            expected_with: source.to_owned(),
            confidence: 1.0,
            co_changes: 3,
            denominator: Some(3),
            evidence: None,
            interpretation: CoChangeInterpretation::HISTORY_ONLY,
            ranking_score: Some(0.5),
        }
    }

    #[test]
    fn prior_change_selects_verification_direction_in_both_file_orders() {
        for (prior, current) in [("a.txt", "b.txt"), ("b.txt", "a.txt")] {
            let dir = repo(true);
            let p = dir.path();
            commit(p, prior, "prior update", START + 100, false);
            std::fs::write(p.join(current), "current update").unwrap();
            let mut items = vec![item(prior, current)];
            annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
            let i = &items[0].interpretation;
            assert_eq!(
                i.resolution,
                CoChangeResolution::ChangedInRecentCommits,
                "{i:?}"
            );
            assert_eq!(
                i.verification_direction,
                Some(CochangeVerificationDirection::CheckCurrentChangeAgainstPriorChange)
            );
            assert!(i.recent.as_ref().unwrap().commit.is_some());
            assert_eq!(items[0].confidence, 1.0);
            assert_eq!(items[0].expected_with, current);
        }
    }

    #[test]
    fn unassessed_history_has_a_reason_and_is_distinct_from_no_recent_change() {
        for (upstream, other_author, now, reason) in [
            (false, false, START + 200, Reason::NoUpstream),
            (true, true, START + 200, Reason::OtherAuthor),
            (true, false, START + 2000, Reason::OutsideWindow),
        ] {
            let dir = repo(upstream);
            let p = dir.path();
            commit(p, "b.txt", "prior update", START + 100, other_author);
            let mut items = vec![item("b.txt", "a.txt")];
            annotate_at(p.to_str().unwrap(), None, &mut items, now);
            assert_eq!(
                items[0].interpretation.resolution,
                CoChangeResolution::NotAssessed
            );
            assert_eq!(
                items[0].interpretation.recent.as_ref().unwrap().reason,
                Some(reason)
            );
            assert!(items[0].interpretation.verification_direction.is_none());
        }
        let dir = repo(true);
        let p = dir.path();
        commit(p, "a.txt", "prior update", START + 100, false);
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::NotChangedInRecentSuffix
        );
        assert_eq!(
            items[0].interpretation.recent.as_ref().unwrap().reason,
            None
        );
    }

    #[test]
    fn reverted_changes_do_not_count_as_addressed_and_history_is_pinned() {
        let dir = repo(true);
        let p = dir.path();
        commit(p, "b.txt", "prior update", START + 100, false);
        let base = text(p.to_str().unwrap(), &["rev-parse", "HEAD"]).unwrap();
        commit(p, "b.txt", "initial", START + 150, false);
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), Some(&base), &mut items, START + 200);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::ChangedInRecentCommits
        );
        let mut current = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut current, START + 200);
        assert_eq!(
            current[0].interpretation.resolution,
            CoChangeResolution::NotChangedInRecentSuffix
        );
    }

    #[test]
    fn long_suffix_reports_truncation_without_a_completion_claim() {
        let dir = repo(true);
        let p = dir.path();
        for n in 1..=33 {
            commit(p, "b.txt", &format!("update {n}"), START + n, false);
        }
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut items, START + 100);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::NotAssessed
        );
        let evidence = items[0].interpretation.recent.as_ref().unwrap();
        assert!(evidence.truncated);
        assert_eq!(evidence.reason, Some(Reason::Truncated));
        assert_eq!(evidence.checked_commits, MAX_COMMITS);
    }

    #[test]
    fn a_merge_commit_does_not_establish_a_recent_suffix() {
        let dir = repo(true);
        let p = dir.path();
        command(p, &["checkout", "-qb", "side"]);
        commit(p, "b.txt", "side update", START + 100, false);
        command(p, &["checkout", "-q", "main"]);
        commit(p, "a.txt", "main update", START + 150, false);
        let output = crate::git_support::isolated_git_command()
            .args(["merge", "--no-verify", "--no-ff", "-qm", "merge", "side"])
            .current_dir(p)
            .env("GIT_AUTHOR_DATE", format!("{} +0000", START + 180))
            .env("GIT_COMMITTER_DATE", format!("{} +0000", START + 180))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::NotAssessed
        );
        assert_eq!(
            items[0].interpretation.recent.as_ref().unwrap().reason,
            Some(Reason::MergeBoundary)
        );
    }

    #[test]
    fn recent_paths_are_relative_to_the_review_subdirectory() {
        let dir = repo(true);
        let p = dir.path();
        std::fs::create_dir(p.join("pkg")).unwrap();
        std::fs::write(p.join("pkg/b.txt"), "current").unwrap();
        commit(p, "pkg/a.txt", "prior update", START + 100, false);
        let mut items = vec![item("a.txt", "b.txt")];
        annotate_at(
            p.join("pkg").to_str().unwrap(),
            None,
            &mut items,
            START + 200,
        );
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::ChangedInRecentCommits,
            "{:?}",
            items[0].interpretation
        );
    }

    #[test]
    fn supporting_commit_uses_literal_route_and_colon_paths() {
        for (literal, other) in [("app/[id]/page.tsx", "app/i/page.tsx"), (":state", "state")] {
            let dir = repo(true);
            let p = dir.path();
            for path in [literal, other] {
                if let Some(parent) = p.join(path).parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
            }
            commit(p, literal, "literal update", START + 100, false);
            let expected = text(p.to_str().unwrap(), &["rev-parse", "HEAD"]).unwrap();
            commit(p, other, "other update", START + 150, false);
            let mut items = vec![item(literal, "a.txt")];
            annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
            assert_eq!(
                items[0].interpretation.resolution,
                CoChangeResolution::ChangedInRecentCommits
            );
            assert_eq!(
                items[0]
                    .interpretation
                    .recent
                    .as_ref()
                    .unwrap()
                    .commit
                    .as_deref(),
                Some(expected.as_str())
            );
        }
    }

    #[test]
    fn deletion_is_a_net_change() {
        let dir = repo(true);
        let p = dir.path();
        std::fs::remove_file(p.join("b.txt")).unwrap();
        commit(p, "a.txt", "initial", START + 100, false);
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::ChangedInRecentCommits
        );
    }

    #[test]
    #[cfg(unix)]
    fn file_kind_change_is_a_net_change() {
        let dir = repo(true);
        let p = dir.path();
        std::fs::remove_file(p.join("b.txt")).unwrap();
        std::os::unix::fs::symlink("a.txt", p.join("b.txt")).unwrap();
        commit(p, "a.txt", "initial", START + 100, false);
        let mut items = vec![item("b.txt", "a.txt")];
        annotate_at(p.to_str().unwrap(), None, &mut items, START + 200);
        assert_eq!(
            items[0].interpretation.resolution,
            CoChangeResolution::ChangedInRecentCommits
        );
    }
}
