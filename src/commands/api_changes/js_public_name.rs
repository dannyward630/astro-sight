//! JS/TS の宣言が、宣言名と違う公開名で公開されているかを調べる。
//!
//! `export default function formatPrice` / `export { formatPrice as price }` / `export = f` /
//! `module.exports = f` の利用者は、`import fmt from` / `import { price } from` のように公開名
//! (または任意のローカル名) で書く。宣言名で集めた参照は利用者を表さないので、参照が 0 件・
//! 全部更新済み・同じファイル内だけ、のいずれも「利用者がいない / 追随済み」の証明にならない。

use std::collections::HashSet;

use tree_sitter::Node;

use crate::language::LangId;

/// 宣言名と違う公開名で公開されている宣言名の集合。
#[derive(Debug, Default)]
pub(crate) struct PublishedNames {
    renamed: HashSet<String>,
    /// 構文エラーなどで公開名を確かめられず、別名で公開している可能性を否定できない。
    uncertain: bool,
}

impl PublishedNames {
    /// ソースを読めず公開名を確かめられない場合。どの名前も違う可能性ありとする。
    pub(crate) fn uncertain() -> Self {
        Self {
            renamed: HashSet::new(),
            uncertain: true,
        }
    }

    /// `name` (bare 名) の公開名が宣言名と違う可能性があるか。
    pub(crate) fn may_differ(&self, name: &str) -> bool {
        self.uncertain || self.renamed.contains(name)
    }

    fn merge(&mut self, other: PublishedNames) {
        self.renamed.extend(other.renamed);
        self.uncertain |= other.uncertain;
    }
}

/// `sources` (旧版・新版) のどれかで、宣言名と違う公開名で公開されている宣言名を集める。
/// 解析木に構文エラーがあり、別名で公開し得る構文の字面が残っている場合は `uncertain`。
pub(crate) fn published_names(sources: &[&[u8]], lang: LangId) -> PublishedNames {
    let mut names = PublishedNames::default();
    for source in sources {
        names.merge(published_names_in(source, lang));
    }
    names
}

/// 変更・削除ファイルの旧版 (`base_blobs`) と新版 (作業ツリー) で、宣言名と違う公開名で
/// 公開されている JS/TS の宣言名。JS/TS 以外は None。ファイルの組ごとに 1 回だけ解析する。
/// どちらの版も読めなければ、確かめられないので `uncertain` にする。
pub(crate) fn published_names_for_diff_file(
    dir: &str,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    old_path: &str,
    new_path: &str,
    caches: &mut super::ref_index::ApiClosureCaches,
) -> Option<std::rc::Rc<PublishedNames>> {
    let path = if new_path == "/dev/null" {
        old_path
    } else {
        new_path
    };
    let lang = LangId::from_path(camino::Utf8Path::new(path)).ok()?;
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return None;
    }
    let key = (old_path.to_string(), new_path.to_string());
    if let Some(found) = caches.js_published_names.get(&key) {
        return Some(std::rc::Rc::clone(found));
    }
    let old = (old_path != "/dev/null")
        .then(|| base_blobs.read(old_path))
        .flatten();
    let new = (new_path != "/dev/null")
        .then(|| super::source_pair::load_new_source(dir, new_path))
        .flatten();
    let mut sources: Vec<&[u8]> = Vec::new();
    if let Some(old) = old.as_deref() {
        sources.push(old);
    }
    if let Some(new) = new.as_deref() {
        sources.push(new);
    }
    let mut names = published_names(&sources, lang);
    if sources.is_empty() {
        names.uncertain = true;
    }
    let names = std::rc::Rc::new(names);
    caches
        .js_published_names
        .insert(key, std::rc::Rc::clone(&names));
    Some(names)
}

fn published_names_in(source: &[u8], lang: LangId) -> PublishedNames {
    let mut names = PublishedNames::default();
    let Ok(tree) = crate::engine::parser::parse_source(source, lang) else {
        names.uncertain = true;
        return names;
    };
    let root = tree.root_node();
    if root.has_error() {
        // 壊れた部分にある export を見落とさないよう、別名で公開し得る字面が残っていれば保守側。
        let text = String::from_utf8_lossy(source);
        names.uncertain = [
            "export default",
            "export =",
            " as ",
            "module.exports",
            "exports.",
        ]
        .iter()
        .any(|marker| text.contains(marker));
    }
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        match statement.kind() {
            "export_statement" => collect_export_statement(statement, source, &mut names.renamed),
            "expression_statement" => {
                if let Some(assignment) = statement
                    .named_child(0)
                    .filter(|node| node.kind() == "assignment_expression")
                {
                    collect_commonjs_export(assignment, source, &mut names.renamed);
                }
            }
            _ => {}
        }
    }
    names
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(source).ok()
}

fn has_token(node: Node<'_>, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == kind)
}

fn collect_export_statement(statement: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let is_default = has_token(statement, "default");
    // `export default function f() {}` / `export default class C {}`
    if is_default
        && let Some(declaration) = statement.child_by_field_name("declaration")
        && let Some(name) = declaration
            .child_by_field_name("name")
            .and_then(|name| text(name, source))
    {
        out.insert(name.to_string());
    }
    // `export default f;`
    if is_default
        && let Some(value) = statement.child_by_field_name("value")
        && value.kind() == "identifier"
        && let Some(name) = text(value, source)
    {
        out.insert(name.to_string());
    }
    // TS の `export = f;` は値が名前付きの子 (field なし) になる。
    if has_token(statement, "=") {
        let mut cursor = statement.walk();
        for child in statement.named_children(&mut cursor) {
            if child.kind() == "identifier"
                && let Some(name) = text(child, source)
            {
                out.insert(name.to_string());
            }
        }
    }
    // `export { f as g }` / `export { f as default }` (from 句の有無を問わない)
    let mut cursor = statement.walk();
    for clause in statement
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "export_clause")
    {
        let mut specifiers = clause.walk();
        for specifier in clause
            .named_children(&mut specifiers)
            .filter(|child| child.kind() == "export_specifier")
        {
            let (Some(name), Some(alias)) = (
                specifier.child_by_field_name("name"),
                specifier.child_by_field_name("alias"),
            ) else {
                continue;
            };
            if let (Some(name), Some(alias)) = (text(name, source), text(alias, source))
                && name != alias
            {
                out.insert(name.to_string());
            }
        }
    }
}

/// CommonJS の `module.exports = f` / `exports.g = f` / `module.exports = { g: f }`。
fn collect_commonjs_export(assignment: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let (Some(left), Some(right)) = (
        assignment.child_by_field_name("left"),
        assignment.child_by_field_name("right"),
    ) else {
        return;
    };
    if left.kind() != "member_expression" {
        return;
    }
    let object = left
        .child_by_field_name("object")
        .and_then(|n| text(n, source));
    let property = left
        .child_by_field_name("property")
        .and_then(|n| text(n, source));
    match (object, property) {
        (Some("module"), Some("exports")) => match right.kind() {
            "identifier" => {
                if let Some(name) = text(right, source) {
                    out.insert(name.to_string());
                }
            }
            "object" => {
                let mut cursor = right.walk();
                for pair in right
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "pair")
                {
                    if let (Some(key), Some(value)) = (
                        pair.child_by_field_name("key")
                            .and_then(|n| text(n, source)),
                        pair.child_by_field_name("value")
                            .filter(|value| value.kind() == "identifier")
                            .and_then(|n| text(n, source)),
                    ) && key != value
                    {
                        out.insert(value.to_string());
                    }
                }
            }
            _ => {}
        },
        (Some("exports"), Some(exported)) => {
            if right.kind() == "identifier"
                && let Some(name) = text(right, source)
                && name != exported
            {
                out.insert(name.to_string());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn differs(source: &str, lang: LangId, name: &str) -> bool {
        published_names(&[source.as_bytes()], lang).may_differ(name)
    }

    #[test]
    fn detects_names_published_under_other_names() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for source in [
                "export default function formatPrice(v) { return v; }\n",
                "export default class formatPrice {}\n",
                "function formatPrice(v) { return v; }\nexport default formatPrice;\n",
                "function formatPrice(v) { return v; }\nexport { formatPrice as price };\n",
                "function formatPrice(v) { return v; }\nexport { formatPrice as default };\n",
                "function formatPrice(v) { return v; }\nmodule.exports = formatPrice;\n",
                "function formatPrice(v) { return v; }\nexports.price = formatPrice;\n",
                "function formatPrice(v) { return v; }\nmodule.exports = { price: formatPrice };\n",
            ] {
                assert!(differs(source, lang, "formatPrice"), "{lang:?}: {source}");
            }
            // 対照: 宣言名のまま公開しているもの
            for source in [
                "export function formatPrice(v) { return v; }\n",
                "function formatPrice(v) { return v; }\nexport { formatPrice };\n",
                "function formatPrice(v) { return v; }\nexport { formatPrice as formatPrice };\n",
                "function formatPrice(v) { return v; }\nmodule.exports = { formatPrice };\n",
                "export default function other() {}\nexport function formatPrice() {}\n",
            ] {
                assert!(!differs(source, lang, "formatPrice"), "{lang:?}: {source}");
            }
        }
        assert!(differs(
            "function formatPrice(v: number) { return v; }\nexport = formatPrice;\n",
            LangId::Typescript,
            "formatPrice"
        ));
    }

    #[test]
    fn syntax_error_with_export_markers_is_uncertain() {
        assert!(differs(
            "export function formatPrice() {}\nexport default (((\n",
            LangId::Typescript,
            "formatPrice"
        ));
        // 対照: 別名で公開し得る字面が無ければ構文エラーだけでは保守側にしない
        assert!(!differs(
            "export function formatPrice() {}\nconst x = (((\n",
            LangId::Typescript,
            "formatPrice"
        ));
    }
}
