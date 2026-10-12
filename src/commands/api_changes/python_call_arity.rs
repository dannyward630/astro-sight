//! Python の変更済み呼び出しが、変更後の関数の引数契約を満たすか確認する。

use std::collections::HashSet;

use tree_sitter::Node;

use super::ref_index::{ApiClosureCaches, parsed_ref_file};
use crate::models::reference::SymbolReference;

#[derive(Clone)]
struct Parameter {
    name: String,
    required: bool,
    positional_only: bool,
    keyword_only: bool,
}

#[derive(Clone)]
pub(super) struct CallContract {
    parameters: Vec<Parameter>,
    varargs: bool,
    kwargs: bool,
}

fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    std::str::from_utf8(source.get(node.byte_range())?).ok()
}

fn parameter_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = if node.kind() == "identifier" {
        node
    } else {
        node.child_by_field_name("name")
            .or_else(|| node.named_child(0))?
    };
    (name.kind() == "identifier")
        .then(|| node_text(name, source))
        .flatten()
}

fn contains_identifier(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind() == "identifier" && node_text(current, source) == Some(name) {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

fn import_binds_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    let from_import = node.kind() == "import_from_statement";
    let module = node
        .child_by_field_name("module_name")
        .map(|child| child.id());
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| Some(child.id()) != module)
        .any(|child| match child.kind() {
            "wildcard_import" => true,
            "aliased_import" => child
                .child_by_field_name("alias")
                .is_some_and(|alias| node_text(alias, source) == Some(name)),
            "dotted_name" if from_import => contains_identifier(child, source, name),
            "dotted_name" => child
                .named_child(0)
                .is_some_and(|first| node_text(first, source) == Some(name)),
            _ => false,
        })
}

fn has_other_binding(root: Node<'_>, function: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let binding = match node.kind() {
            "function_definition" | "class_definition" if node.id() != function.id() => node
                .child_by_field_name("name")
                .is_some_and(|child| contains_identifier(child, source, name)),
            "assignment" | "augmented_assignment" | "for_statement" | "type_alias_statement" => {
                node.child_by_field_name("left")
                    .is_some_and(|child| contains_identifier(child, source, name))
            }
            "named_expression" => node
                .child_by_field_name("name")
                .is_some_and(|child| contains_identifier(child, source, name)),
            "as_pattern" => u32::try_from(node.named_child_count().saturating_sub(1))
                .ok()
                .and_then(|index| node.named_child(index))
                .is_some_and(|child| contains_identifier(child, source, name)),
            "import_statement" | "import_from_statement" => import_binds_name(node, source, name),
            "global_statement" => contains_identifier(node, source, name),
            "delete_statement" | "match_statement" => contains_identifier(node, source, name),
            _ => false,
        };
        if binding {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn is_identifier_attribute_chain(node: Node<'_>) -> bool {
    match node.kind() {
        "identifier" => true,
        "attribute" => {
            node.child_by_field_name("attribute")
                .is_some_and(|member| member.kind() == "identifier")
                && node
                    .child_by_field_name("object")
                    .is_some_and(is_identifier_attribute_chain)
        }
        _ => false,
    }
}

impl CallContract {
    fn from_function(function: Node<'_>, source: &[u8]) -> Option<Self> {
        // 任意の decorator は関数の呼出し契約を変更できる。
        if function
            .parent()
            .is_some_and(|parent| parent.kind() == "decorated_definition")
        {
            return None;
        }
        let list = function.child_by_field_name("parameters")?;
        let mut parameters: Vec<Parameter> = Vec::new();
        let mut names = HashSet::new();
        let mut positional_only = false;
        let mut keyword_only = false;
        let mut varargs = false;
        let mut kwargs = false;
        let mut seen_default_positional = false;
        let mut cursor = list.walk();
        for child in list.named_children(&mut cursor) {
            match child.kind() {
                "comment" => {}
                "positional_separator" if !positional_only && !keyword_only => {
                    positional_only = true;
                    for param in &mut parameters {
                        param.positional_only = true;
                    }
                }
                "keyword_separator" if !keyword_only => keyword_only = true,
                "list_splat_pattern" if !varargs && !kwargs => {
                    varargs = true;
                    keyword_only = true;
                }
                "dictionary_splat_pattern" if !kwargs => kwargs = true,
                "typed_parameter"
                    if child
                        .named_child(0)
                        .is_some_and(|pattern| pattern.kind() == "list_splat_pattern")
                        && !varargs
                        && !kwargs =>
                {
                    varargs = true;
                    keyword_only = true;
                }
                "typed_parameter"
                    if child
                        .named_child(0)
                        .is_some_and(|pattern| pattern.kind() == "dictionary_splat_pattern")
                        && !kwargs =>
                {
                    kwargs = true;
                }
                "identifier"
                | "typed_parameter"
                | "default_parameter"
                | "typed_default_parameter"
                    if !kwargs =>
                {
                    let name = parameter_name(child, source)?.to_owned();
                    if !names.insert(name.clone()) || parameters.len() >= 64 {
                        return None;
                    }
                    let required = !matches!(
                        child.kind(),
                        "default_parameter" | "typed_default_parameter"
                    );
                    if !keyword_only && required && seen_default_positional {
                        return None;
                    }
                    if !keyword_only && !required {
                        seen_default_positional = true;
                    }
                    parameters.push(Parameter {
                        name,
                        required,
                        positional_only: false,
                        keyword_only,
                    });
                }
                _ => return None,
            }
        }
        Some(Self {
            parameters,
            varargs,
            kwargs,
        })
    }

    fn accepts_call(&self, call: Node<'_>, source: &[u8]) -> bool {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        if arguments.kind() != "argument_list" {
            return false;
        }
        let mut supplied = HashSet::new();
        let mut keyword_names = HashSet::new();
        let mut positional = 0usize;
        let mut seen_keyword = false;
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            match argument.kind() {
                "comment" => {}
                // 展開される実引数は静的に確定できない。
                "list_splat" | "dictionary_splat" => return false,
                "keyword_argument" => {
                    seen_keyword = true;
                    let Some(name) = argument
                        .child_by_field_name("name")
                        .or_else(|| argument.named_child(0))
                        .and_then(|node| node_text(node, source))
                    else {
                        return false;
                    };
                    if !keyword_names.insert(name) {
                        return false;
                    }
                    if let Some(index) = self.parameters.iter().position(|p| p.name == name) {
                        let param = &self.parameters[index];
                        if param.positional_only {
                            if !self.kwargs {
                                return false;
                            }
                        } else if !supplied.insert(index) {
                            return false;
                        }
                    } else if !self.kwargs {
                        return false;
                    }
                }
                _ => {
                    if seen_keyword {
                        return false;
                    }
                    let target = self
                        .parameters
                        .iter()
                        .enumerate()
                        .filter(|(_, param)| !param.keyword_only)
                        .nth(positional)
                        .map(|(index, _)| index);
                    if let Some(index) = target {
                        if !supplied.insert(index) {
                            return false;
                        }
                    } else if !self.varargs {
                        return false;
                    }
                    positional += 1;
                }
            }
        }
        self.parameters
            .iter()
            .enumerate()
            .all(|(index, param)| !param.required || supplied.contains(&index))
    }
}

fn contract_for_target(
    dir: &str,
    target_path: &str,
    name: &str,
    caches: &mut ApiClosureCaches,
) -> Option<CallContract> {
    let target = parsed_ref_file(dir, target_path, caches)?;
    if target.lang_id != crate::language::LangId::Python || target.tree.root_node().has_error() {
        return None;
    }
    let function = super::python_signature::find_python_function_by_name(
        target.tree.root_node(),
        &target.source,
        name,
    )?;
    if has_other_binding(target.tree.root_node(), function, &target.source, name) {
        return None;
    }
    CallContract::from_function(function, &target.source)
}

/// direct call のみ対象とする。値参照・decorator・解析エラー・展開引数は証明しない。
pub(super) fn changed_top_level_call_matches_signature(
    dir: &str,
    target_path: &str,
    name: &str,
    reference: &SymbolReference,
    caches: &mut ApiClosureCaches,
) -> bool {
    let key = (target_path.to_owned(), name.to_owned());
    let contract = if let Some(cached) = caches.python_call_contracts.get(&key) {
        cached.clone()
    } else {
        let loaded = contract_for_target(dir, target_path, name, caches);
        caches.python_call_contracts.insert(key, loaded.clone());
        loaded
    };
    let Some(contract) = contract else {
        return false;
    };
    let Some(caller) = parsed_ref_file(dir, &reference.path, caches) else {
        return false;
    };
    if caller.lang_id != crate::language::LangId::Python || caller.tree.root_node().has_error() {
        return false;
    }
    let point = tree_sitter::Point {
        row: reference.line,
        column: reference.column,
    };
    let Some(callee) = caller
        .tree
        .root_node()
        .descendant_for_point_range(point, point)
    else {
        return false;
    };
    if callee.kind() != "identifier" || node_text(callee, &caller.source) != Some(name) {
        return false;
    }
    let function = match callee.parent() {
        Some(call) if call.kind() == "call" => callee,
        Some(attribute)
            if attribute.kind() == "attribute"
                && attribute
                    .child_by_field_name("attribute")
                    .is_some_and(|field| field.id() == callee.id())
                && attribute
                    .child_by_field_name("object")
                    .is_some_and(is_identifier_attribute_chain) =>
        {
            attribute
        }
        _ => return false,
    };
    let Some(call) = function.parent().filter(|parent| parent.kind() == "call") else {
        return false;
    };
    if call.child_by_field_name("function").map(|node| node.id()) != Some(function.id()) {
        return false;
    }
    contract.accepts_call(call, &caller.source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_top_level_binding_scan_distinguishes_import_sources_and_aliases() {
        for (label, suffix, expected) in [
            ("no later binding", "", false),
            ("value use is not a binding", "handler = render\n", false),
            ("assignment", "render = legacy\n", true),
            (
                "named expression",
                "if (render := legacy):\n    pass\n",
                true,
            ),
            (
                "exception alias",
                "try:\n    pass\nexcept Error as render:\n    pass\n",
                true,
            ),
            ("type alias", "type render = str\n", true),
            (
                "import module named like function",
                "from .render import helpers\n",
                false,
            ),
            ("import qualified module", "import pkg.render\n", false),
            (
                "renamed import does not bind original",
                "from x import render as other\n",
                false,
            ),
            ("import alias", "from x import fast as render\n", true),
            (
                "qualified import alias",
                "import pkg.fast as render\n",
                true,
            ),
            ("wildcard import", "from x import *\n", true),
        ] {
            let source = format!("def render(name, color):\n    return name + color\n{suffix}");
            let mut parser = tree_sitter::Parser::new();
            parser
                .set_language(&tree_sitter_python::LANGUAGE.into())
                .expect("Python grammar");
            let tree = parser.parse(&source, None).expect("Python parse");
            assert!(!tree.root_node().has_error(), "{label}: {source}");
            let function = super::super::python_signature::find_python_function_by_name(
                tree.root_node(),
                source.as_bytes(),
                "render",
            )
            .expect("function");
            assert_eq!(
                has_other_binding(tree.root_node(), function, source.as_bytes(), "render"),
                expected,
                "{label}: {source}"
            );
        }
    }
}
