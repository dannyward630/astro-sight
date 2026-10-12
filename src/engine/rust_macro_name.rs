//! Rust マクロ呼び出しの末尾名を AST から取り出す。

use tree_sitter::Node;

pub(crate) fn last_macro_identifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = node.child_by_field_name("macro")?;
    let mut last = None;
    let mut pending = vec![name];
    while let Some(part) = pending.pop() {
        if part.kind() == "identifier" {
            let text = part.utf8_text(source).ok()?;
            if last.is_none_or(|(start, _)| part.start_byte() > start) {
                last = Some((part.start_byte(), text));
            }
        }
        let mut cursor = part.walk();
        pending.extend(part.named_children(&mut cursor));
    }
    last.map(|(_, text)| text.strip_prefix("r#").unwrap_or(text))
}

pub(crate) fn has_token_tree_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "token_tree" {
            return true;
        }
        node = parent;
    }
    false
}

/// token tree 内の `value.include` はフィールド参照。`value.include!` は
/// マクロ展開で `value.` を捨て得るので、動的ソースの障壁として残す。
pub(crate) fn is_field_name_in_token_tree(node: Node<'_>, source: &[u8]) -> bool {
    let mut previous = node.prev_sibling();
    while let Some(part) = previous {
        if matches!(part.kind(), "block_comment" | "line_comment") {
            previous = part.prev_sibling();
            continue;
        }
        if part.utf8_text(source).ok() != Some(".") {
            return false;
        }
        let mut next = node.next_sibling();
        while let Some(part) = next {
            if matches!(part.kind(), "block_comment" | "line_comment") {
                next = part.next_sibling();
                continue;
            }
            return part.utf8_text(source).ok().is_some_and(|text| text != "!");
        }
        return true;
    }
    false
}
