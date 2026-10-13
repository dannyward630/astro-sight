//! `.gitattributes` の最小パーサ。`linguist-generated` 属性付きファイルを
//! dead-code 検出対象から除外するために利用する。

use std::path::Path;

/// `.gitattributes` から抽出した `linguist-generated` ルール集。
/// ルールは登場順に保持し、後勝ちで評価する (`-linguist-generated` で解除可)。
#[derive(Debug, Default, Clone)]
pub struct GitAttributes {
    rules: Vec<Rule>,
}

#[derive(Debug, Clone)]
struct Rule {
    pattern: Pattern,
    /// true=set, false=unset (`-linguist-generated`)
    set: bool,
}

#[derive(Debug, Clone)]
struct Pattern {
    /// `/` 区切りで分解したセグメント。
    segments: Vec<Segment>,
    /// パターンが `/` で始まる、または途中に `/` を含む場合 true。
    /// この場合はリポジトリルート基準の完全パスでのみマッチする。
    anchored: bool,
}

#[derive(Debug, Clone)]
enum Segment {
    /// `**` — 任意の数の中間ディレクトリにマッチ。
    DoubleStar,
    /// glob セグメント (`*`, `?`, リテラル文字を含む)。`/` は含まない。
    Glob(String),
}

impl GitAttributes {
    /// リポジトリルートの `.gitattributes` を読み込む。存在しなければ空。
    pub fn load(root: &Path) -> Self {
        let path = root.join(".gitattributes");
        match std::fs::read_to_string(&path) {
            Ok(content) => Self::parse(&content),
            Err(_) => Self::default(),
        }
    }

    /// 文字列から直接パース。
    pub fn parse(content: &str) -> Self {
        let mut rules = Vec::new();
        for raw in content.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern_str) = parts.next() else {
                continue;
            };
            let pattern = Pattern::parse(pattern_str);
            for attr in parts {
                if let Some(set) = match_linguist_generated(attr) {
                    rules.push(Rule {
                        pattern: pattern.clone(),
                        set,
                    });
                }
            }
        }
        Self { rules }
    }

    /// リポジトリ相対パスが `linguist-generated` として扱われるか。
    /// ルールは登場順に評価し、最後にマッチしたものが勝つ。
    pub fn is_generated(&self, rel_path: &str) -> bool {
        let normalized = rel_path.replace('\\', "/");
        let mut generated = false;
        for rule in &self.rules {
            if rule.pattern.matches(&normalized) {
                generated = rule.set;
            }
        }
        generated
    }
}

fn match_linguist_generated(attr: &str) -> Option<bool> {
    // 受理: linguist-generated, linguist-generated=true, linguist-generated=set,
    //       -linguist-generated, !linguist-generated, linguist-generated=false, =unset
    if let Some(rest) = attr.strip_prefix('-').or_else(|| attr.strip_prefix('!')) {
        if rest == "linguist-generated" {
            return Some(false);
        }
        return None;
    }
    let (name, value) = match attr.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (attr, None),
    };
    if name != "linguist-generated" {
        return None;
    }
    match value {
        None => Some(true),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            "true" | "set" | "1" => Some(true),
            "false" | "unset" | "0" => Some(false),
            _ => None,
        },
    }
}

impl Pattern {
    fn parse(raw: &str) -> Self {
        let trimmed = raw.trim_start_matches('/');
        let anchored = raw.starts_with('/') || raw.trim_end_matches('/').contains('/');
        let segments = trimmed
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s == "**" {
                    Segment::DoubleStar
                } else {
                    Segment::Glob(s.to_string())
                }
            })
            .collect();
        Self { segments, anchored }
    }

    fn matches(&self, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let matched = match_segments_from_each_start(&self.segments, &parts);
        if self.anchored {
            matched[0]
        } else {
            // 非 anchored パターンは任意のディレクトリから始まる一致を許す。
            // 例: `parser.c` は `src/parser.c` にもマッチ。
            matched.contains(&true)
        }
    }
}

/// `segments` が `parts[start..]` 全体に一致するかを、`start` = 0..=parts.len() のそれぞれに
/// ついて返す。
///
/// セグメントを先頭から 1 つずつ再帰で照合すると、パターンのセグメント数だけスタックを使い
/// (`**/**/**/...` が続く 1 行で溢れる)、`**` が続くと試す組み合わせが指数的に増える。
/// 末尾のセグメントから順に「各位置以降に一致するか」の表を作り、セグメント数 × パスの長さで
/// 済ませる。結果は再帰で照合した場合と同じ。
fn match_segments_from_each_start(segments: &[Segment], parts: &[&str]) -> Vec<bool> {
    let n = parts.len();
    // セグメントが残っていなければ、パスも残っていないときだけ一致する。
    let mut matched: Vec<bool> = (0..=n).map(|start| start == n).collect();
    for (i, segment) in segments.iter().enumerate().rev() {
        matched = match segment {
            // 末尾の `**` は、残りのパスが何であっても一致する。
            Segment::DoubleStar if i + 1 == segments.len() => vec![true; n + 1],
            // `**` は 0 個以上のセグメントを読み飛ばす: start 以降のどこかから残りが一致すればよい。
            Segment::DoubleStar => {
                let mut any_later = false;
                let mut row = vec![false; n + 1];
                for start in (0..=n).rev() {
                    any_later |= matched[start];
                    row[start] = any_later;
                }
                row
            }
            // 通常のセグメントは先頭の 1 つに一致し、残りがその次から一致すればよい。
            Segment::Glob(pattern) => (0..=n)
                .map(|start| start < n && matched[start + 1] && glob_match(pattern, parts[start]))
                .collect(),
        };
    }
    matched
}

/// セグメント単位の glob マッチ。`*`, `?`, `[...]` のうち `*` と `?` のみ対応。
/// `/` を含まない 1 セグメントに対して動作する。
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    glob_match_inner(&pat, 0, &txt, 0)
}

fn glob_match_inner(pat: &[char], pi: usize, txt: &[char], ti: usize) -> bool {
    let mut pi = pi;
    let mut ti = ti;
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;
    while ti < txt.len() {
        if pi < pat.len() {
            match pat[pi] {
                '*' => {
                    star_pi = Some(pi);
                    star_ti = ti;
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                c if c == txt[ti] => {
                    pi += 1;
                    ti += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_path_matches() {
        let ga = GitAttributes::parse("src/parser.c linguist-generated\n");
        assert!(ga.is_generated("src/parser.c"));
        assert!(!ga.is_generated("src/parser.h"));
        assert!(!ga.is_generated("other/parser.c"));
    }

    #[test]
    fn unset_overrides_earlier_set() {
        let ga = GitAttributes::parse("*.c linguist-generated\nsrc/hand.c -linguist-generated\n");
        assert!(ga.is_generated("src/parser.c"));
        assert!(!ga.is_generated("src/hand.c"));
    }

    #[test]
    fn wildcard_extension() {
        let ga = GitAttributes::parse("*.generated.go linguist-generated\n");
        assert!(ga.is_generated("pkg/foo.generated.go"));
        assert!(ga.is_generated("foo.generated.go"));
        assert!(!ga.is_generated("pkg/foo.go"));
    }

    #[test]
    fn double_star_pattern() {
        let ga = GitAttributes::parse("**/gen/** linguist-generated\n");
        assert!(ga.is_generated("a/gen/foo.rs"));
        assert!(ga.is_generated("gen/foo.rs"));
        assert!(ga.is_generated("a/b/gen/c/d.rs"));
        assert!(!ga.is_generated("src/main.rs"));
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        let ga = GitAttributes::parse("# comment\n\n  \nsrc/parser.c linguist-generated\n");
        assert!(ga.is_generated("src/parser.c"));
    }

    #[test]
    fn attribute_value_variants() {
        for attr in [
            "linguist-generated",
            "linguist-generated=true",
            "linguist-generated=set",
        ] {
            let ga = GitAttributes::parse(&format!("parser.c {attr}\n"));
            assert!(ga.is_generated("src/parser.c"), "attr={attr}");
        }
        for attr in ["linguist-generated=false", "linguist-generated=unset"] {
            let ga = GitAttributes::parse(&format!("*.c linguist-generated\nparser.c {attr}\n"));
            assert!(!ga.is_generated("src/parser.c"), "attr={attr}");
        }
    }

    #[test]
    fn anchored_root_pattern() {
        let ga = GitAttributes::parse("/src/parser.c linguist-generated\n");
        assert!(ga.is_generated("src/parser.c"));
        assert!(!ga.is_generated("deep/src/parser.c"));
    }

    #[test]
    fn non_anchored_basename_matches_any_dir() {
        let ga = GitAttributes::parse("parser.c linguist-generated\n");
        assert!(ga.is_generated("src/parser.c"));
        assert!(ga.is_generated("parser.c"));
        assert!(ga.is_generated("a/b/parser.c"));
    }

    #[test]
    fn empty_file_marks_nothing() {
        let ga = GitAttributes::parse("");
        assert!(!ga.is_generated("src/parser.c"));
    }

    #[test]
    fn non_linguist_attrs_ignored() {
        let ga = GitAttributes::parse("*.c text=auto\n*.c eol=lf\n");
        assert!(!ga.is_generated("src/parser.c"));
    }

    #[test]
    fn backslash_paths_normalized() {
        let ga = GitAttributes::parse("src/parser.c linguist-generated\n");
        assert!(ga.is_generated("src\\parser.c"));
    }

    /// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
    fn match_segments_recursive(segments: &[Segment], parts: &[&str]) -> bool {
        match segments.split_first() {
            None => parts.is_empty(),
            Some((Segment::DoubleStar, rest)) => {
                if rest.is_empty() {
                    return true;
                }
                (0..=parts.len()).any(|skip| match_segments_recursive(rest, &parts[skip..]))
            }
            Some((Segment::Glob(pat), rest)) => {
                if parts.is_empty() {
                    return false;
                }
                if glob_match(pat, parts[0]) {
                    match_segments_recursive(rest, &parts[1..])
                } else {
                    false
                }
            }
        }
    }

    fn matches_recursive(pattern: &Pattern, path: &str) -> bool {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if pattern.anchored {
            match_segments_recursive(&pattern.segments, &parts)
        } else {
            (0..=parts.len())
                .any(|start| match_segments_recursive(&pattern.segments, &parts[start..]))
        }
    }

    /// 表による照合は、再帰で照合した場合と全組み合わせで一致する
    /// (`**` の連続・先頭・末尾、anchored とそうでないもの、空のパス)。
    #[test]
    fn matches_agree_with_recursive_version() {
        let segments = ["**", "*", "a", "b", "a*", "?", "c.ts"];
        let mut patterns = vec![String::new()];
        for _ in 0..3 {
            let mut next = Vec::new();
            for prefix in &patterns {
                for seg in segments {
                    next.push(if prefix.is_empty() {
                        seg.to_string()
                    } else {
                        format!("{prefix}/{seg}")
                    });
                }
            }
            patterns.extend(next);
        }
        patterns.retain(|p| !p.is_empty());
        let raw_patterns: Vec<String> = patterns
            .iter()
            .flat_map(|p| [p.clone(), format!("/{p}")])
            .collect();
        let paths = [
            "",
            "a",
            "b",
            "c.ts",
            "a/b",
            "a/c.ts",
            "b/a/c.ts",
            "a/a/a",
            "x/a/b/c.ts",
            "ab/b/c.ts",
            "a/b/c/d/e",
        ];
        let (mut matched, mut unmatched) = (0, 0);
        for raw in &raw_patterns {
            let pattern = Pattern::parse(raw);
            for path in paths {
                let expected = matches_recursive(&pattern, path);
                assert_eq!(pattern.matches(path), expected, "{raw} vs {path}");
                if expected {
                    matched += 1;
                } else {
                    unmatched += 1;
                }
            }
        }
        assert!(
            matched > 1000 && unmatched > 1000,
            "matched={matched} unmatched={unmatched}"
        );
    }

    /// `**` が 10 万個続くパターンでも照合がスタックを使い切らず、すぐに終わる。
    /// 子プロセスの 256 KiB のスタックで照合する (再帰版はここで溢れて子プロセスごと落ちる)。
    #[test]
    fn long_double_star_pattern_does_not_overflow() {
        crate::engine::tree_walk::run_deep_test_isolated(
            module_path!(),
            "long_double_star_pattern_does_not_overflow",
            || {
                let line = format!("{}gen.ts linguist-generated\n", "**/".repeat(100_000));
                let ga = GitAttributes::parse(&line);
                assert!(ga.is_generated("src/deep/gen.ts"));
                assert!(!ga.is_generated("src/deep/other.ts"));
            },
        );
    }
}
