//! Proof for a Rust file-root item whose only edit widens private visibility.
//!
//! The byte comparison is deliberately strict: formatting, attributes, or
//! another edit in the file make the old binding uncertain.

use std::collections::HashSet;

use tree_sitter::Node;

use crate::engine::{diff, parser};
use crate::language::LangId;
use crate::models::impact::AffectedSymbol;

pub(super) fn private_widening_names(
    file_diff: &str,
    file_path: &str,
    source: &[u8],
    root: Node<'_>,
    affected: &[AffectedSymbol],
) -> HashSet<String> {
    if root.has_error() {
        return HashSet::new();
    }
    let Some(old) = diff::reconstruct_old_source(file_diff, file_path, source) else {
        return HashSet::new();
    };
    let Ok(old_tree) = parser::parse_source(&old.source, LangId::Rust) else {
        return HashSet::new();
    };
    if old_tree.root_node().has_error() {
        return HashSet::new();
    }

    let mut result = HashSet::new();
    for changed in affected
        .iter()
        .filter(|item| item.change_type == "modified")
    {
        let mut cursor = root.walk();
        let mut matches = root.named_children(&mut cursor).filter(|node| {
            matches!(
                node.kind(),
                "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
                    | "function_item"
                    | "const_item"
                    | "static_item"
            ) && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                == Some(changed.name.as_str())
        });
        let Some(item) = matches.next() else { continue };
        if matches.next().is_some() {
            continue;
        }
        let mut children = item.walk();
        let Some(visibility) = item
            .named_children(&mut children)
            .find(|child| child.kind() == "visibility_modifier")
        else {
            continue;
        };
        if !matches!(visibility.utf8_text(source), Ok("pub(crate)" | "pub")) {
            continue;
        }
        let mut end = visibility.end_byte();
        while end < source.len() && source[end].is_ascii_whitespace() {
            end += 1;
        }
        let mut without_visibility =
            Vec::with_capacity(source.len() - (end - visibility.start_byte()));
        without_visibility.extend_from_slice(&source[..visibility.start_byte()]);
        without_visibility.extend_from_slice(&source[end..]);
        if without_visibility == old.source {
            result.insert(changed.name.clone());
        }
    }
    result
}

/// A newly visible item can enter scope through a glob import. In that case
/// even an unchanged reference to another item might become ambiguous.
pub(super) fn has_glob_import(source: &[u8]) -> bool {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration" && source[node.byte_range()].contains(&b'*') {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}
