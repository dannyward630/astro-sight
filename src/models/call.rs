use super::location::Range;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 呼び出し位置（行と列は 0 始まり）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSite {
    pub line: usize,
    pub column: usize,
}

/// 呼び出し元または呼び出し先の記述。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEndpoint {
    pub name: String,
    pub range: Range,
}

/// 特定の呼び出し位置における単一の呼び出しエッジ（caller → callee）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallEdge {
    pub caller: CallEndpoint,
    pub callee: CallEndpoint,
    pub call_site: CallSite,
}

/// 単一ファイルのコールグラフレスポンス（full モード）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraph {
    pub language: String,
    pub calls: Vec<CallEdge>,
}

// ── compact（トークン最適化）形式 ──

/// compact 形式の呼び出し先。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallee {
    pub name: String,
    pub ln: usize,
    pub col: usize,
}

/// 同じ呼び出し元からの呼び出しグループ。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGroup {
    pub caller: String,
    pub range: [usize; 4],
    pub callees: Vec<CompactCallee>,
}

/// 呼び出し元ごとにグループ化したトークン最適化コールグラフ。
#[derive(Debug, Clone, Serialize)]
pub struct CompactCallGraph {
    pub lang: String,
    pub calls: Vec<CompactCallGroup>,
}

impl CallGraph {
    pub fn to_compact(&self) -> CompactCallGraph {
        // 初出順を維持しながら呼び出し元 (名前と範囲の組) ごとにまとめる。名前だけで
        // まとめると、別クラスの同名メソッド (`__init__` / `new`) が 1 つのグループになり、
        // 範囲は最初の関数のものになって、別の関数の呼び出し先が混ざる。
        let mut groups: Vec<CompactCallGroup> = Vec::new();
        let mut index: HashMap<(&str, [usize; 4]), usize> = HashMap::new();

        for edge in &self.calls {
            let range = [
                edge.caller.range.start.line,
                edge.caller.range.start.column,
                edge.caller.range.end.line,
                edge.caller.range.end.column,
            ];
            let ix = *index
                .entry((edge.caller.name.as_str(), range))
                .or_insert_with(|| {
                    groups.push(CompactCallGroup {
                        caller: edge.caller.name.clone(),
                        range,
                        callees: Vec::new(),
                    });
                    groups.len() - 1
                });
            groups[ix].callees.push(CompactCallee {
                name: edge.callee.name.clone(),
                ln: edge.call_site.line,
                col: edge.call_site.column,
            });
        }

        CompactCallGraph {
            lang: self.language.clone(),
            calls: groups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::location::Point;

    fn edge(caller: &str, start: usize, end: usize, callee: &str, line: usize) -> CallEdge {
        let range = |s: usize, e: usize| Range {
            start: Point { line: s, column: 0 },
            end: Point { line: e, column: 0 },
        };
        CallEdge {
            caller: CallEndpoint {
                name: caller.to_string(),
                range: range(start, end),
            },
            callee: CallEndpoint {
                name: callee.to_string(),
                range: range(line, line),
            },
            call_site: CallSite { line, column: 4 },
        }
    }

    /// 同名でも範囲が違う呼び出し元 (別クラスの `__init__` 等) は別のグループにする。
    /// 同じ呼び出し元からの呼び出しは 1 つにまとめ、初出順を保つ (対照)。
    #[test]
    fn to_compact_groups_by_caller_name_and_range() {
        let graph = CallGraph {
            language: "python".to_string(),
            calls: vec![
                edge("__init__", 1, 2, "connect", 2),
                edge("__init__", 5, 6, "make_store", 6),
                edge("__init__", 1, 2, "log", 2),
                edge("run", 8, 9, "go", 9),
            ],
        };
        let compact = graph.to_compact();
        let summary: Vec<(String, usize, Vec<String>)> = compact
            .calls
            .iter()
            .map(|group| {
                (
                    group.caller.clone(),
                    group.range[0],
                    group.callees.iter().map(|c| c.name.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    "__init__".to_string(),
                    1,
                    vec!["connect".to_string(), "log".to_string()]
                ),
                ("__init__".to_string(), 5, vec!["make_store".to_string()]),
                ("run".to_string(), 8, vec!["go".to_string()]),
            ]
        );
    }
}
