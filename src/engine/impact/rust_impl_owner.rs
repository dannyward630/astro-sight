//! Rust の合成 impl owner を impact に出す必要があるか判定する。
//!
//! symbols の impl owner はメソッドの container 解決に必要なので残し、
//! impact の Pass 1 だけでメンバー変更に伴う重複報告を抑える。

use tree_sitter::Node;

use crate::engine::diff::ChangedLineFacts;
use crate::models::symbol::{Symbol, SymbolKind};

use super::descendant_for_range;

pub(super) fn is_impl_owner(root: Node<'_>, symbol: &Symbol) -> bool {
    symbol.kind == SymbolKind::Type
        && descendant_for_range(root, &symbol.range).is_some_and(|node| node.kind() == "impl_item")
}

pub(super) fn is_member_only_change(
    root: Node<'_>,
    source: &[u8],
    symbol: &Symbol,
    change_type: &str,
    facts: &ChangedLineFacts,
) -> bool {
    if root.has_error() || !is_impl_owner(root, symbol) {
        return false;
    }
    let Some(impl_item) = descendant_for_range(root, &symbol.range) else {
        return false;
    };
    let Some(body) = impl_item.child_by_field_name("body") else {
        return false;
    };
    if change_type == "added" {
        // マーカー trait や associated const/type だけの impl はメソッド追加ではない。
        // owner を消すと新規実装の痕跡がなくなるため、そのまま残す。
        let mut cursor = body.walk();
        let mut has_method = false;
        for child in body.named_children(&mut cursor) {
            match child.kind() {
                "function_item" => has_method = true,
                "line_comment" | "block_comment" | "attribute_item" => {}
                _ => return false,
            }
        }
        return has_method;
    }
    if change_type != "modified" {
        return false;
    }
    let start = symbol.range.start.line;
    let end = symbol.range.end.line;
    // 純削除 hunk の境界が impl に触れるだけのケースでは、変更の所在を証明できない。
    if !facts
        .added_lines
        .iter()
        .any(|line| (start..=end).contains(line))
        && !facts
            .deletion_gaps
            .iter()
            .any(|gap| (start..=end).contains(gap))
    {
        return false;
    }
    // 削除行には新側の AST ノードが無い。gap が生存メソッド内に見えても、
    // 実際には隣接メソッドの宣言が削除された可能性があるため owner を残す。
    if facts
        .deletion_gaps
        .iter()
        .any(|gap| (start..=end).contains(gap))
    {
        return false;
    }
    let members = method_ranges(body, impl_item.start_position().row);
    let lines: Vec<_> = source.split(|byte| *byte == b'\n').collect();
    facts
        .added_lines
        .iter()
        .filter(|line| (start..=end).contains(line))
        .all(|line| {
            members.iter().any(|member| {
                (member.start..=member.end).contains(line)
                    || ((member.prefix_start..member.start).contains(line)
                        && super::symbol_lines_all_added(
                            member.start,
                            member.end,
                            &facts.added_lines,
                        ))
            }) || lines
                .get(*line)
                .is_some_and(|text| text.iter().all(u8::is_ascii_whitespace))
        })
}

/// メソッドと直前に連続する doc/comment/attribute の行範囲。
/// 単一行 impl はヘッダとメンバーを行単位で分離できないため証明しない。
struct MethodRange {
    prefix_start: usize,
    start: usize,
    end: usize,
}

fn method_ranges(body: Node<'_>, impl_start: usize) -> Vec<MethodRange> {
    let mut cursor = body.walk();
    let mut ranges = Vec::new();
    let mut prefix: Option<(usize, usize)> = None;
    for child in body.named_children(&mut cursor) {
        let start = child.start_position().row;
        let end = child.end_position().row;
        match child.kind() {
            "line_comment" | "block_comment" | "attribute_item" => {
                prefix = match prefix {
                    Some((first, last)) if start <= last.saturating_add(1) => Some((first, end)),
                    _ => Some((start, end)),
                };
            }
            "function_item" => {
                let first = prefix
                    .filter(|(_, last)| start <= last.saturating_add(1))
                    .map_or(start, |(first, _)| first);
                if first > impl_start {
                    ranges.push(MethodRange {
                        prefix_start: first,
                        start,
                        end,
                    });
                }
                prefix = None;
            }
            _ => prefix = None,
        }
    }
    ranges
}
