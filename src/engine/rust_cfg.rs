//! Rust の排他的な `cfg` 分岐で表された同一定数を識別する。

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

struct ConstArm {
    predicate: Option<String>,
    ty: String,
    visibility: String,
}

/// ファイル直下にある同名・同型の定数 2 件が `cfg(P)` / `cfg(not(P))` で
/// 排他的な場合だけ、その名前を返す。曖昧な属性や 3 件以上の同名宣言は保持する。
pub(crate) fn exclusive_const_names(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    if root.kind() != "source_file" || root.has_error() {
        return HashSet::new();
    }

    let mut groups: HashMap<String, Vec<ConstArm>> = HashMap::new();
    let mut attributes: Vec<Option<String>> = Vec::new();
    let mut cursor = root.walk();
    for item in root.named_children(&mut cursor) {
        if item.kind() == "attribute_item" {
            attributes.push(cfg_predicate(item, source));
            continue;
        }
        if item.kind() == "const_item"
            && let (Some(name), Some(ty)) = (
                item.child_by_field_name("name"),
                item.child_by_field_name("type"),
            )
            && let (Ok(name), Ok(ty)) = (name.utf8_text(source), ty.utf8_text(source))
        {
            let visibility = item
                .named_children(&mut item.walk())
                .find(|child| child.kind() == "visibility_modifier")
                .and_then(|child| child.utf8_text(source).ok())
                .unwrap_or("");
            groups.entry(name.to_owned()).or_default().push(ConstArm {
                predicate: (attributes.len() == 1)
                    .then(|| attributes.pop())
                    .flatten()
                    .flatten(),
                ty: ty.to_owned(),
                visibility: visibility.to_owned(),
            });
        }
        attributes.clear();
    }

    let mut names: HashSet<String> = groups
        .into_iter()
        .filter_map(|(name, arms)| {
            let [a, b] = arms.as_slice() else {
                return None;
            };
            (a.ty == b.ty
                && a.visibility == b.visibility
                && a.predicate
                    .as_deref()
                    .zip(b.predicate.as_deref())
                    .is_some_and(|(a, b)| complementary(a, b)))
            .then_some(name)
        })
        .collect();

    // 出力のシンボル名には module / impl の所有者が付かない。別スコープに
    // 同名の定数があれば、直下の 2 分岐と出力行を対応付けられない。
    if !names.is_empty() {
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if matches!(node.kind(), "const_item" | "static_item")
                && (node.kind() == "static_item"
                    || node.parent().is_none_or(|parent| parent.id() != root.id()))
                && let Some(name) = node.child_by_field_name("name")
                && let Ok(name) = name.utf8_text(source)
            {
                names.remove(name);
                if names.is_empty() {
                    break;
                }
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
    }
    names
}

fn cfg_predicate(item: Node<'_>, source: &[u8]) -> Option<String> {
    let attribute = item.named_child(0)?;
    if attribute.kind() != "attribute" || attribute.named_child(0)?.utf8_text(source).ok()? != "cfg"
    {
        return None;
    }
    let argument = attribute
        .child_by_field_name("arguments")?
        .utf8_text(source)
        .ok()?;
    let predicate = argument.strip_prefix('(')?.strip_suffix(')')?.trim();
    (!predicate.is_empty()).then(|| predicate.to_owned())
}

fn complementary(a: &str, b: &str) -> bool {
    fn inner_not(predicate: &str) -> Option<&str> {
        predicate
            .strip_prefix("not(")?
            .strip_suffix(')')
            .map(str::trim)
    }
    inner_not(a) == Some(b) || inner_not(b) == Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(source: &str) -> HashSet<String> {
        let tree =
            crate::engine::parser::parse_source(source.as_bytes(), crate::language::LangId::Rust)
                .unwrap();
        exclusive_const_names(tree.root_node(), source.as_bytes())
    }

    #[test]
    fn only_complementary_same_type_pairs_are_equivalent() {
        let source = r#"
#[cfg(target_os = "macos")]
pub const PAIR: &str = "a";
#[cfg(not(target_os = "macos"))]
pub const PAIR: &str = "b";

#[cfg(feature = "one")]
pub const OVERLAP: u8 = 1;
#[cfg(feature = "two")]
pub const OVERLAP: u8 = 2;

#[cfg(feature = "one")]
pub const DIFFERENT_TYPE: u8 = 1;
#[cfg(not(feature = "one"))]
pub const DIFFERENT_TYPE: u16 = 2;

#[cfg(feature = "one")]
const NESTED: u8 = 1;
#[cfg(not(feature = "one"))]
const NESTED: u8 = 2;
pub mod first { pub const NESTED: u8 = 3; }
pub mod second { pub const NESTED: u8 = 4; }
"#;
        assert_eq!(names(source), HashSet::from(["PAIR".to_owned()]));
    }
}
