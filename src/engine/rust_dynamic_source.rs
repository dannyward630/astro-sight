//! Rust の取り込み構文を、空白とコメントに依存せず検出する。

use tree_sitter::Node;

use super::rust_macro_name::{
    has_token_tree_ancestor, is_field_name_in_token_tree, last_macro_identifier,
};

fn is_comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

fn previous_token(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        node = node.prev_sibling()?;
        if !is_comment(node) {
            return Some(node);
        }
    }
}

fn named(node: Node<'_>, source: &[u8], name: &str) -> bool {
    node.kind() == "identifier"
        && node
            .utf8_text(source)
            .ok()
            .is_some_and(|text| text.strip_prefix("r#").unwrap_or(text) == name)
}

fn contains_name(root: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if named(node, source, name) {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn attribute_selects_path(attribute: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attribute.walk();
    let Some(name) = attribute
        .named_children(&mut cursor)
        .find(|child| !is_comment(*child))
    else {
        return false;
    };
    named(name, source, "path")
        || named(name, source, "cfg_attr") && contains_name(attribute, source, "path")
}

fn is_attribute_token_tree(node: Node<'_>, source: &[u8]) -> bool {
    if node.child(0).and_then(|child| child.utf8_text(source).ok()) != Some("[") {
        return false;
    }
    let Some(previous) = previous_token(node) else {
        return false;
    };
    match previous.utf8_text(source).ok() {
        Some("#") => true,
        Some("!") => {
            previous_token(previous).and_then(|token| token.utf8_text(source).ok()) == Some("#")
        }
        _ => false,
    }
}

/// 既存の AST 走査から呼び出せる、ノード単位の動的ソース障壁。
pub(crate) fn node_includes_external_source(node: Node<'_>, source: &[u8]) -> bool {
    match node.kind() {
        "attribute" => attribute_selects_path(node, source),
        "token_tree" if is_attribute_token_tree(node, source) => {
            attribute_selects_path(node, source)
        }
        "macro_invocation" => {
            last_macro_identifier(node, source).is_none_or(|name| name == "include")
        }
        "identifier"
            if has_token_tree_ancestor(node) && !is_field_name_in_token_tree(node, source) =>
        {
            named(node, source, "include")
        }
        "use_declaration" => contains_name(node, source, "include"),
        _ => false,
    }
}

pub(crate) fn tree_includes_external_source(root: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node_includes_external_source(node, source) {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::tree_includes_external_source;
    use crate::{engine::parser, language::LangId};

    #[test]
    fn external_source_syntax_ignores_spacing_and_comments() {
        for (source, expected) in [
            ("#[path = \"x.inc\"] mod x;", true),
            ("# [path = \"x.inc\"] mod x;", true),
            ("# /*a*/ [path /*b*/ = \"x.inc\"] mod x;", true),
            ("#[cfg_attr(all(), path = \"x.inc\")] mod x;", true),
            (
                "macro_rules! m { () => { # [cfg_attr(all(), path = \"x.inc\")] mod x; } }",
                true,
            ),
            ("include /*note*/ !(\"x.inc\");", true),
            ("use std::{include as load}; load!(\"x.inc\");", true),
            ("#[inline] fn f() {}", false),
            ("#[doc = \"path\"] fn f() {}", false),
            ("fn f() { let path = 1; let s = \"#[path = x]\"; }", false),
            ("macro_rules! m { ($p:expr) => { let path = $p; } }", false),
            ("fn f() { println!(\"{}\", args.include); }", false),
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).expect("parse Rust");
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                tree_includes_external_source(tree.root_node(), source.as_bytes()),
                expected,
                "{source}"
            );
        }
    }
}
