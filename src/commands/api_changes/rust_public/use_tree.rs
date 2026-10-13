//! Rust `pub use` の AST 展開と path 解決。

use super::*;

/// AST を走査し `use_declaration` ノードから `pub use` re-export edge を集める。
///
/// - `current_module`: lib.rs 起点の current source 所属モジュール (super:: 解決 + source_module に使う)
/// - 戻り値 `None` = 「判定不能」 (解決不能な super:: や不正な use tree)。呼出元は index 全体を
///   `None` にして api.rm を残す (false negative より false positive を優先する fail-closed 方針)
///
/// **注**: Step A の `collect_pub_use_targets` から「inline_private_depth による pub use 除外」を外した。
/// 非 pub inline mod 配下の `pub use` でも、root から `pub use private_mod::x` されれば外部公開
/// 経路になり得るため。最終判定は `RustPubUseIndex::exposes_symbol` の固定点伝播で行う。
pub(crate) fn collect_pub_use_edges(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    current_module: &[String],
    edges: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    use std::rc::Rc;

    // 左の子から順に処理するため、子を逆順に積む。所属モジュールは同じ親の子どうしで共有する。
    fn push_children<'tree>(
        stack: &mut Vec<(tree_sitter::Node<'tree>, Rc<Vec<String>>)>,
        node: tree_sitter::Node<'tree>,
        module: &Rc<Vec<String>>,
    ) {
        let mut cursor = node.walk();
        let start = stack.len();
        stack.extend(
            node.children(&mut cursor)
                .map(|child| (child, Rc::clone(module))),
        );
        stack[start..].reverse();
    }

    // ファイル全体を辿るので、再帰せず明示スタックで前順に辿る。要素は (ノード, 所属モジュール)。
    let mut stack = vec![(node, Rc::new(current_module.to_vec()))];
    while let Some((node, current_module)) = stack.pop() {
        match node.kind() {
            "use_declaration" => {
                if !rust_node_has_unrestricted_pub_visibility(node, source) {
                    continue;
                }
                let argument = node.child_by_field_name("argument")?;
                expand_rust_use_tree_edges_ast(
                    argument,
                    source,
                    &[],
                    &current_module,
                    &current_module,
                    None,
                    edges,
                )?;
            }
            "mod_item" => {
                // #[path = "..."] でファイル名と module 名がずれる場合、source_module の解決を保守的に
                // 諦めて index 全体を None にする (codex Warning #3 対応、fail-closed)。
                if rust_mod_item_has_path_attribute(node, source) {
                    return None;
                }
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(str::to_string);
                let mut next_module = current_module.as_ref().clone();
                if let Some(seg) = name {
                    next_module.push(seg);
                }
                push_children(&mut stack, node, &Rc::new(next_module));
            }
            _ => push_children(&mut stack, node, &current_module),
        }
    }
    Some(())
}

/// `mod_item` の直前の同一スコープ sibling に `#[path = "..."]` attribute があるかを返す。
/// tree-sitter-rust では attribute_item と mod_item は親 (source_file / declaration_list) の
/// 子として **隣接 sibling** に並ぶため、prev_sibling を逆方向に辿って attribute_item を集める。
/// `#[path]` が見つかったら true。
pub(crate) fn rust_mod_item_has_path_attribute(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if sib.kind() != "attribute_item" {
            break; // 連続する attribute_item は積み上がるが、他の宣言が出たら終了
        }
        if attribute_item_is_path(sib, source) {
            return true;
        }
        prev = sib.prev_named_sibling();
    }
    false
}

/// `attribute_item` の中身が `#[path = ...]` か判定する。
pub(crate) fn attribute_item_is_path(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attr_item.walk();
    for child in attr_item.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            // attribute の最初の identifier 子 (= attribute path) の text を見る。
            let mut inner = child.walk();
            for c in child.named_children(&mut inner) {
                if c.kind() == "identifier" || c.kind() == "scoped_identifier" {
                    return c.utf8_text(source).map(str::trim) == Ok("path");
                }
            }
        }
    }
    false
}
