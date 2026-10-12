use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use aho_corasick::AhoCorasick;
use tree_sitter::Node;

use crate::engine::parser;
use crate::engine::rust_macro_name::{
    has_token_tree_ancestor, is_field_name_in_token_tree, last_macro_identifier,
};
use crate::engine::rust_targets::RustTargets;
use crate::language::LangId;

use super::{MemberStatus, status_from_counts};

#[derive(Debug, Clone)]
struct Candidate {
    owner: String,
    bare: String,
    file: String,
}

#[derive(Debug, Default)]
pub(crate) struct RustMemberLiveness {
    statuses: HashMap<(String, String, String), MemberStatus>,
}

impl RustMemberLiveness {
    pub(crate) fn build<F>(
        candidates: &[(String, String, String, LangId)],
        canonical_dir: &Path,
        files: &[PathBuf],
        is_test_path: F,
    ) -> Self
    where
        F: Fn(&Path) -> bool,
    {
        let mut grouped: HashMap<String, Vec<Candidate>> = HashMap::new();
        for (name, kind, file, lang) in candidates {
            if *lang != LangId::Rust || !matches!(kind.as_str(), "method" | "function") {
                continue;
            }
            let Some((owner, bare)) = name.rsplit_once('.') else {
                continue;
            };
            if owner.contains('.') || bare.starts_with("r#") {
                continue;
            }
            grouped.entry(bare.to_owned()).or_default().push(Candidate {
                owner: owner.to_owned(),
                bare: bare.to_owned(),
                file: file.to_owned(),
            });
        }
        if grouped.is_empty() {
            return Self::default();
        }
        if workspace_has_proc_macro(canonical_dir, files) {
            return Self::default();
        }
        let mut method_cache = HashMap::new();
        grouped.retain(|_, group| {
            let mut seen = HashSet::new();
            group.iter().all(|candidate| {
                seen.insert((&candidate.file, &candidate.owner))
                    && method_cache
                        .entry(candidate.file.clone())
                        .or_insert_with(|| direct_inherent_methods(canonical_dir, &candidate.file))
                        .as_ref()
                        .and_then(|methods: &HashMap<(String, String), usize>| {
                            methods.get(&(candidate.owner.clone(), candidate.bare.clone()))
                        })
                        == Some(&1)
            })
        });
        if grouped.is_empty() {
            return Self::default();
        }

        let patterns: Vec<&str> = grouped.keys().map(String::as_str).collect();
        let Ok(matcher) = AhoCorasick::new(&patterns) else {
            return Self::default();
        };
        let names: HashSet<&str> = patterns.iter().copied().collect();
        let lib_modules = plain_lib_modules(canonical_dir);
        let mut ambiguous = HashSet::new();
        let mut counts: HashMap<(String, String, String), (usize, usize)> = HashMap::new();
        for path in files {
            let Ok(source) = std::fs::read(path) else {
                ambiguous.extend(names.iter().map(|name| (*name).to_owned()));
                continue;
            };
            let is_rust = crate::engine::refs::detect_source_lang(path) == Some(LangId::Rust);
            let may_have_dynamic_macro = is_rust
                && (has_dynamic_rust_macro_syntax(&source)
                    || [
                        "include",
                        "paste",
                        "concat_idents",
                        "concat",
                        "format_ident",
                        "seq",
                        "quote",
                        "Ident",
                        "proc_macro_derive",
                        "path",
                    ]
                    .iter()
                    .any(|name| contains_identifier(&source, name)));
            let matching: HashSet<&str> = matcher
                .find_overlapping_iter(&source)
                .map(|hit| patterns[hit.pattern().as_usize()])
                .collect();
            if matching.is_empty() && !may_have_dynamic_macro {
                continue;
            }
            if !is_rust {
                ambiguous.extend(
                    matching
                        .into_iter()
                        .filter(|name| contains_identifier(&source, name))
                        .map(str::to_owned),
                );
                continue;
            }
            let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
                if may_have_dynamic_macro {
                    ambiguous.extend(names.iter().map(|name| (*name).to_owned()));
                }
                ambiguous.extend(matching.into_iter().map(str::to_owned));
                continue;
            };
            let root = tree.root_node();
            if root.has_error() {
                if may_have_dynamic_macro {
                    ambiguous.extend(names.iter().map(|name| (*name).to_owned()));
                }
                ambiguous.extend(matching.into_iter().map(str::to_owned));
                continue;
            }
            if may_have_dynamic_macro && has_dynamic_rust_macro(&source, root) {
                ambiguous.extend(names.iter().map(|name| (*name).to_owned()));
            }
            if matching.is_empty() {
                continue;
            }
            let mut pending = vec![root];
            while let Some(node) = pending.pop() {
                if matches!(
                    node.kind(),
                    "macro_invocation" | "macro_definition" | "attribute_item"
                ) {
                    let contents = &source[node.byte_range()];
                    ambiguous.extend(
                        matching
                            .iter()
                            .copied()
                            .filter(|name| contains_identifier(contents, name))
                            .map(str::to_owned),
                    );
                }
                if matches!(node.kind(), "identifier" | "field_identifier")
                    && let Ok(raw_name) = node.utf8_text(&source)
                    && let name = raw_name.strip_prefix("r#").unwrap_or(raw_name)
                    && names.contains(name)
                {
                    match classify_occurrence(
                        node,
                        &source,
                        path,
                        canonical_dir,
                        lib_modules.as_ref(),
                        &grouped,
                        name,
                    ) {
                        Occurrence::Unrelated => {}
                        Occurrence::Unknown => {
                            ambiguous.insert(name.to_owned());
                        }
                        Occurrence::Owner(candidate) => {
                            let entry = counts
                                .entry((
                                    candidate.file.clone(),
                                    candidate.owner.clone(),
                                    name.to_owned(),
                                ))
                                .or_default();
                            if is_test_path(path) {
                                entry.1 += 1;
                            } else {
                                entry.0 += 1;
                            }
                        }
                    }
                }
                let mut cursor = node.walk();
                pending.extend(node.named_children(&mut cursor));
            }
        }

        let mut statuses = HashMap::new();
        for (bare, group) in grouped {
            for candidate in group {
                let status = if ambiguous.contains(&bare) {
                    MemberStatus::Ambiguous
                } else {
                    let (prod, test) = counts
                        .remove(&(
                            candidate.file.clone(),
                            candidate.owner.clone(),
                            bare.clone(),
                        ))
                        .unwrap_or((0, 0));
                    status_from_counts(prod, test)
                };
                statuses.insert((candidate.file, candidate.owner, candidate.bare), status);
            }
        }
        Self { statuses }
    }

    pub(crate) fn status_for(&self, owner: &str, bare: &str, file: &str) -> Option<MemberStatus> {
        self.statuses
            .get(&(file.to_owned(), owner.to_owned(), bare.to_owned()))
            .copied()
    }
}

fn workspace_has_proc_macro(root: &Path, files: &[PathBuf]) -> bool {
    if !root.join("Cargo.toml").is_file() {
        return true;
    }
    if root
        .ancestors()
        .skip(1)
        .any(|parent| parent.join("Cargo.toml").is_file())
    {
        return true;
    }
    let mut walker = ignore::WalkBuilder::new(root);
    let scan_root = root.to_path_buf();
    let scanned: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
    walker.standard_filters(false).filter_entry(move |entry| {
        entry.path() == scan_root
            || !entry.file_type().is_some_and(|kind| kind.is_dir())
            || (!matches!(entry.file_name().to_str(), Some(".git"))
                && !(entry.path() == scan_root.join("target"))
                && (!entry.path().join(".git").exists()
                    || under_rust_source_root(&scan_root, entry.path())))
    });
    for result in walker.build() {
        let Ok(entry) = result else {
            return true;
        };
        if entry.path() != root
            && entry.file_type().is_some_and(|kind| kind.is_dir())
            && entry.path().join(".git").exists()
            && under_rust_source_root(root, entry.path())
        {
            return true;
        }
        if entry.file_type().is_some_and(|kind| kind.is_symlink())
            && entry.path().extension().is_none_or(|ext| ext != "md")
        {
            return true;
        }
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs")
            && !scanned.contains(entry.path())
        {
            return true;
        }
        if entry.file_type().is_some_and(|kind| kind.is_file())
            && entry.path().extension().is_some_and(|extension| {
                let Some(extension) = extension.to_str() else {
                    return true;
                };
                extension.eq_ignore_ascii_case("rs") && extension != "rs" || !extension.is_ascii()
            })
        {
            return true;
        }
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
        {
            continue;
        }
        if entry.path() != root.join("Cargo.toml") {
            return true;
        }
        let Ok(manifest) = std::fs::read_to_string(entry.path()) else {
            return true;
        };
        let document: toml::Table = match toml::from_str(&manifest) {
            Ok(document) => document,
            Err(_) => return true,
        };
        let Some(targets) = RustTargets::parse(&manifest, Path::new("")) else {
            return true;
        };
        if !targets.has_package
            || !targets.roots_complete
            || targets
                .explicit_roots()
                .any(|path| !under_rust_source_root(Path::new(""), path))
        {
            return true;
        }
        if ["proc-macro", "proc_macro"].iter().any(|key| {
            document
                .get("lib")
                .and_then(|lib| lib.get(*key))
                .and_then(toml::Value::as_bool)
                == Some(true)
        }) || [
            "dependencies",
            "dev-dependencies",
            "dev_dependencies",
            "build-dependencies",
            "build_dependencies",
            "target",
            "workspace",
        ]
        .iter()
        .any(|section| document.get(*section).is_some_and(contains_manifest_path))
            || ["patch", "replace"]
                .iter()
                .any(|key| document.contains_key(*key))
            || document
                .get("package")
                .and_then(|package| package.get("build"))
                .is_some_and(|build| !build.is_bool())
            || ["members", "default-members"].iter().any(|key| {
                document
                    .get("workspace")
                    .and_then(|workspace| workspace.get(*key))
                    .and_then(toml::Value::as_array)
                    .is_some_and(|members| {
                        members.iter().any(|member| member.as_str() != Some("."))
                    })
            })
            || document
                .get("lib")
                .and_then(|lib| lib.get("crate-type"))
                .and_then(toml::Value::as_array)
                .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("proc-macro")))
        {
            return true;
        }
    }
    false
}

fn under_rust_source_root(root: &Path, path: &Path) -> bool {
    ["src", "tests", "examples", "benches"]
        .iter()
        .any(|component| path.starts_with(root.join(component)))
}

fn contains_manifest_path(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => {
            table.contains_key("path") || table.values().any(contains_manifest_path)
        }
        toml::Value::Array(items) => items.iter().any(contains_manifest_path),
        _ => false,
    }
}

fn has_dynamic_rust_macro_syntax(source: &[u8]) -> bool {
    if memchr::memmem::find(source, b"[<").is_some() {
        return true;
    }
    [
        b"include".as_slice(),
        b"paste",
        b"concat_idents",
        b"format_ident",
        b"seq",
        b"quote",
    ]
    .iter()
    .any(|name| {
        memchr::memmem::find_iter(source, name).any(|start| {
            if start > 0 && is_identifier_byte(source[start - 1]) {
                return false;
            }
            let mut end = start + name.len();
            while source.get(end).is_some_and(u8::is_ascii_whitespace) {
                end += 1;
            }
            source.get(end) == Some(&b'!')
        })
    })
}

fn has_metavar_concat(source: &[u8]) -> bool {
    for start in memchr::memchr_iter(b'$', source) {
        if source.get(start + 1) != Some(&b'{') {
            continue;
        }
        let mut index = start + 2;
        while source.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if source.get(index..index + 6) != Some(b"concat".as_slice()) {
            continue;
        }
        index += 6;
        while source.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if source.get(index) == Some(&b'(') {
            return true;
        }
    }
    false
}

fn has_dynamic_rust_macro(source: &[u8], root: Node<'_>) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if crate::engine::rust_dynamic_source::node_includes_external_source(node, source) {
            return true;
        }
        if node.kind() == "identifier"
            && has_token_tree_ancestor(node)
            && !is_field_name_in_token_tree(node, source)
            && node.utf8_text(source).ok().is_none_or(|name| {
                matches!(
                    name.strip_prefix("r#").unwrap_or(name),
                    "paste" | "concat_idents" | "format_ident" | "seq" | "quote"
                )
            })
        {
            return true;
        }
        if node.kind() == "macro_invocation"
            && last_macro_identifier(node, source).is_none_or(|name| {
                matches!(
                    name,
                    "paste" | "concat_idents" | "format_ident" | "seq" | "quote"
                )
            })
        {
            return true;
        }
        if node.kind() == "use_declaration"
            && ["paste", "concat_idents", "format_ident", "seq", "quote"]
                .iter()
                .any(|name| contains_identifier(&source[node.byte_range()], name))
        {
            return true;
        }
        if node.kind() == "attribute_item"
            && contains_identifier(&source[node.byte_range()], "proc_macro_derive")
        {
            return true;
        }
        if node.kind() == "call_expression"
            && node
                .child_by_field_name("function")
                .and_then(|function| function.utf8_text(source).ok())
                .is_some_and(|function| {
                    let compact: String =
                        function.chars().filter(|ch| !ch.is_whitespace()).collect();
                    compact.ends_with("Ident::new") || compact.ends_with("Ident::new_raw")
                })
        {
            return true;
        }
        if node.kind() == "token_tree"
            && (has_dynamic_rust_macro_syntax(&source[node.byte_range()])
                || has_metavar_concat(&source[node.byte_range()]))
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

fn contains_identifier(source: &[u8], name: &str) -> bool {
    memchr::memmem::find_iter(source, name.as_bytes()).any(|start| {
        let end = start + name.len();
        (start == 0 || !is_identifier_byte(source[start - 1]))
            && (end == source.len() || !is_identifier_byte(source[end]))
    })
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 128
}

fn direct_inherent_methods(root: &Path, file: &str) -> Option<HashMap<(String, String), usize>> {
    let source = std::fs::read(root.join(file)).ok()?;
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    let top = tree.root_node();
    if top.has_error() {
        return None;
    }
    if has_inner_attribute(Some(top)) {
        return None;
    }
    let mut methods = HashMap::new();
    let mut attributes = Vec::new();
    let mut cursor = top.walk();
    for item in top.named_children(&mut cursor) {
        if item.kind() == "attribute_item" {
            attributes.push(item);
            continue;
        }
        if matches!(item.kind(), "line_comment" | "block_comment") {
            continue;
        }
        let safe_attributes = attributes
            .iter()
            .all(|attr| safe_rust_attribute(*attr, &source));
        attributes.clear();
        if item.kind() != "impl_item" || item.child_by_field_name("trait").is_some() {
            continue;
        }
        if !safe_attributes {
            continue;
        }
        let owner = item
            .child_by_field_name("type")
            .and_then(|kind| {
                if kind.kind() == "type_identifier" {
                    Some(kind)
                } else if kind.kind() == "generic_type" {
                    kind.child_by_field_name("type")
                        .filter(|base| base.kind() == "type_identifier")
                } else {
                    None
                }
            })
            .and_then(|kind| kind.utf8_text(&source).ok());
        let Some(owner) = owner else {
            continue;
        };
        let Some(body) = item.child_by_field_name("body") else {
            continue;
        };
        if has_inner_attribute(Some(body)) {
            continue;
        }
        let mut method_attributes = Vec::new();
        let mut cursor = body.walk();
        for method in body.named_children(&mut cursor) {
            if method.kind() == "attribute_item" {
                method_attributes.push(method);
                continue;
            }
            if matches!(method.kind(), "line_comment" | "block_comment") {
                continue;
            }
            let safe_attributes = method_attributes
                .iter()
                .all(|attr| safe_rust_attribute(*attr, &source));
            method_attributes.clear();
            if method.kind() != "function_item" {
                continue;
            }
            if !safe_attributes {
                continue;
            }
            if has_inner_attribute(method.child_by_field_name("body")) {
                continue;
            }
            if let Some(name) = method
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(&source).ok())
            {
                *methods
                    .entry((owner.to_owned(), name.to_owned()))
                    .or_insert(0) += 1;
            }
        }
    }
    Some(methods)
}

fn safe_rust_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let Ok(text) = node.utf8_text(source) else {
        return false;
    };
    let Some(body) = text.strip_prefix("#[") else {
        return false;
    };
    let name = body
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .next();
    matches!(name, Some("allow" | "doc" | "inline"))
}

fn has_conditional_attribute(item: Node<'_>, source: &[u8]) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                let Ok(text) = sibling.utf8_text(source) else {
                    return true;
                };
                let Some(body) = text.strip_prefix("#[") else {
                    return true;
                };
                let name = body
                    .trim_start()
                    .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                    .next();
                if matches!(name, Some("cfg" | "cfg_attr")) {
                    return true;
                }
            }
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    false
}

fn has_inner_attribute(container: Option<Node<'_>>) -> bool {
    container.is_some_and(|body| {
        let mut cursor = body.walk();
        body.named_children(&mut cursor)
            .any(|child| child.kind() == "inner_attribute_item")
    })
}

fn plain_lib_modules(root: &Path) -> Option<HashSet<String>> {
    if [
        "src/main.rs",
        "src/bin",
        "tests",
        "examples",
        "benches",
        "build.rs",
    ]
    .iter()
    .any(|path| root.join(path).exists())
    {
        return None;
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let targets = RustTargets::parse(&manifest, Path::new(""))?;
    if !targets.has_package || !targets.roots_complete || targets.has_explicit_roots() {
        return None;
    }
    let document: toml::Table = toml::from_str(&manifest).ok()?;
    if ["bin", "example", "test", "bench"]
        .iter()
        .any(|key| document.contains_key(*key))
    {
        return None;
    }
    let source = std::fs::read(root.join("src/lib.rs")).ok()?;
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    let top = tree.root_node();
    if top.has_error() {
        return None;
    }
    let mut modules = HashSet::new();
    let mut cursor = top.walk();
    for item in top.named_children(&mut cursor) {
        if matches!(
            item.kind(),
            "attribute_item" | "inner_attribute_item" | "macro_invocation" | "macro_definition"
        ) {
            return None;
        }
        if item.kind() == "mod_item" {
            if item.child_by_field_name("body").is_some() {
                return None;
            }
            let name = item.child_by_field_name("name")?.utf8_text(&source).ok()?;
            if !modules.insert(name.to_owned()) {
                return None;
            }
        }
    }
    Some(modules)
}

fn receiver_import_is_direct(receiver: Node<'_>, source: &[u8], module: &str, owner: &str) -> bool {
    let mut top = receiver;
    while let Some(parent) = top.parent() {
        top = parent;
    }
    if top.kind() != "source_file" {
        return false;
    }
    let expected = format!("crate::{module}::{owner}");
    let mut imports = 0;
    let mut pending = vec![top];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "mod_item"
                | "attribute_item"
                | "inner_attribute_item"
                | "macro_invocation"
                | "macro_definition"
        ) {
            return false;
        }
        if node.kind() == "use_declaration" {
            if node.parent().is_none_or(|parent| parent.id() != top.id())
                || node
                    .child_by_field_name("argument")
                    .and_then(|argument| argument.utf8_text(source).ok())
                    != Some(expected.as_str())
            {
                return false;
            }
            imports += 1;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    imports == 1
}

enum Occurrence<'a> {
    Unrelated,
    Owner(&'a Candidate),
    Unknown,
}

fn classify_occurrence<'a>(
    node: Node<'_>,
    source: &[u8],
    path: &Path,
    root: &Path,
    lib_modules: Option<&HashSet<String>>,
    groups: &'a HashMap<String, Vec<Candidate>>,
    bare: &str,
) -> Occurrence<'a> {
    let Some(parent) = node.parent() else {
        return Occurrence::Unknown;
    };
    if parent.kind() == "function_item"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        return Occurrence::Unrelated;
    }
    let mut ancestor = Some(parent);
    while let Some(current) = ancestor {
        if matches!(
            current.kind(),
            "macro_invocation" | "token_tree" | "attribute_item"
        ) {
            return Occurrence::Unknown;
        }
        ancestor = current.parent();
    }
    if parent.kind() == "field_expression"
        && parent
            .child_by_field_name("field")
            .is_some_and(|field| field.id() == node.id())
    {
        if !parent.parent().is_some_and(|call| {
            call.kind() == "call_expression"
                && call
                    .child_by_field_name("function")
                    .is_some_and(|function| function.id() == parent.id())
        }) {
            return Occurrence::Unknown;
        }
        let Some(receiver) = parent.child_by_field_name("value") else {
            return Occurrence::Unknown;
        };
        if let Some(candidate) =
            resolve_receiver(receiver, source, path, root, lib_modules, groups, bare)
        {
            return Occurrence::Owner(candidate);
        }
        return Occurrence::Unknown;
    }
    if parent.kind() == "scoped_identifier"
        && parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id())
    {
        let mut ancestor = parent.parent();
        while let Some(current) = ancestor {
            if current.kind() == "use_declaration" {
                return Occurrence::Unrelated;
            }
            ancestor = current.parent();
        }
        return Occurrence::Unknown;
    }
    Occurrence::Unrelated
}

fn resolve_receiver<'a>(
    receiver: Node<'_>,
    source: &[u8],
    path: &Path,
    root: &Path,
    lib_modules: Option<&HashSet<String>>,
    groups: &'a HashMap<String, Vec<Candidate>>,
    bare: &str,
) -> Option<&'a Candidate> {
    if receiver.kind() != "identifier" {
        return None;
    }
    let receiver_name = receiver.utf8_text(source).ok()?;
    let statement = receiver.parent()?.parent()?.parent()?;
    if statement.kind() != "expression_statement" {
        return None;
    }
    let block = statement.parent()?;
    if block.kind() != "block" {
        return None;
    }
    let mut cursor = block.walk();
    let mut preceding = None;
    for item in block.named_children(&mut cursor) {
        if item.id() == statement.id() {
            break;
        }
        preceding = Some(item);
    }
    let binding = preceding.filter(|item| item.kind() == "let_declaration")?;
    if binding.child_by_field_name("type").is_some() {
        return None;
    }
    if binding
        .child_by_field_name("pattern")?
        .utf8_text(source)
        .ok()?
        != receiver_name
    {
        return None;
    }
    let value = binding.child_by_field_name("value")?;
    if value.kind() != "identifier" {
        return None;
    }
    let owner = value.utf8_text(source).ok()?;
    let matches: Vec<&Candidate> = groups
        .get(bare)?
        .iter()
        .filter(|item| item.owner == owner)
        .collect();
    if matches.len() != 1 {
        return None;
    }
    let candidate = matches[0];
    if !candidate_has_unambiguous_unit_value(root, candidate) {
        return None;
    }
    let module = candidate.file.strip_prefix("src/")?.strip_suffix(".rs")?;
    if module.contains('/') || path == root.join(&candidate.file) {
        return None;
    }
    let reference = path.strip_prefix(root).ok()?.to_str()?;
    let reference_module = reference.strip_prefix("src/")?.strip_suffix(".rs")?;
    if reference_module.contains('/')
        || !lib_modules?.contains(module)
        || !lib_modules?.contains(reference_module)
    {
        return None;
    }
    let text = std::str::from_utf8(source).ok()?;
    // import と初期化子以外に同名の束縛がある場合は確定しない。
    if text.matches(owner).count() != 2 {
        return None;
    }
    if !receiver_import_is_direct(receiver, source, module, owner) {
        return None;
    }
    Some(candidate)
}

fn candidate_has_unambiguous_unit_value(root: &Path, candidate: &Candidate) -> bool {
    let Ok(source) = std::fs::read(root.join(&candidate.file)) else {
        return false;
    };
    let Ok(tree) = parser::parse_source(&source, LangId::Rust) else {
        return false;
    };
    let top = tree.root_node();
    if top.has_error() {
        return false;
    }
    if has_inner_attribute(Some(top)) {
        return false;
    }
    let mut unit_structs = 0;
    let mut simple_methods = 0;
    let mut cursor = top.walk();
    for item in top.named_children(&mut cursor) {
        let item_name = item
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(&source).ok());
        if item_name == Some(candidate.owner.as_str()) {
            if item.kind() == "struct_item" {
                if has_conditional_attribute(item, &source) {
                    return false;
                }
                let Some(name) = item.child_by_field_name("name") else {
                    return false;
                };
                let mut cursor = item.walk();
                let unit = item
                    .named_children(&mut cursor)
                    .all(|child| child.id() == name.id() || child.kind() == "visibility_modifier");
                if unit {
                    unit_structs += 1;
                }
            } else if matches!(item.kind(), "const_item" | "static_item" | "function_item") {
                return false;
            }
        }
        if item.kind() != "impl_item" || item.child_by_field_name("trait").is_some() {
            continue;
        }
        let Some(impl_type) = item.child_by_field_name("type") else {
            continue;
        };
        if impl_type.kind() != "type_identifier"
            || impl_type.utf8_text(&source).ok() != Some(candidate.owner.as_str())
        {
            continue;
        }
        if has_conditional_attribute(item, &source) {
            return false;
        }
        let Some(body) = item.child_by_field_name("body") else {
            continue;
        };
        if has_inner_attribute(Some(body)) {
            return false;
        }
        let mut cursor = body.walk();
        for method in body.named_children(&mut cursor) {
            if method.kind() != "function_item"
                || method
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(&source).ok())
                    != Some(candidate.bare.as_str())
            {
                continue;
            }
            if has_conditional_attribute(method, &source) {
                return false;
            }
            if has_inner_attribute(method.child_by_field_name("body")) {
                return false;
            }
            let Some(parameters) = method.child_by_field_name("parameters") else {
                continue;
            };
            let mut cursor = parameters.walk();
            let self_parameter = parameters
                .named_children(&mut cursor)
                .find(|parameter| parameter.kind() == "self_parameter");
            if self_parameter
                .and_then(|parameter| parameter.utf8_text(&source).ok())
                .is_some_and(|parameter| {
                    matches!(
                        parameter
                            .chars()
                            .filter(|ch| !ch.is_whitespace())
                            .collect::<String>()
                            .as_str(),
                        "self" | "mutself" | "&self" | "&mutself"
                    )
                })
            {
                simple_methods += 1;
            }
        }
    }
    unit_structs == 1 && simple_methods == 1
}

#[cfg(test)]
mod tests {
    use super::has_dynamic_rust_macro;
    use crate::engine::parser;
    use crate::language::LangId;

    #[test]
    fn spaced_or_commented_include_is_a_dynamic_source_barrier() {
        for source in [
            "std :: include!(\"extra.rs\");\n",
            "std /*a*/ :: /*b*/ include!{\"extra.rs\"}\n",
            "r#include![\"extra.rs\"];\n",
            "macro_rules! m { () => { include /*c*/ !(\"extra.rs\"); } }\n",
            "macro_rules! m { () => { use std::include as load; load!(\"extra.rs\"); } }\n",
            "macro_rules! m { () => { x . include!(\"extra.rs\"); } }\n",
            "# [path = \"extra.inc\"] mod extra;\n",
            "#[path /*note*/ = \"extra.inc\"] mod extra;\n",
            "#[cfg_attr(all(), path = \"extra.inc\")] mod extra;\n",
            "macro_rules! m { () => { # [cfg_attr(all(), path = \"extra.inc\")] mod extra; } }\n",
        ] {
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).expect("parse Rust");
            assert!(
                !tree.root_node().has_error(),
                "unexpected parse error: {source}"
            );
            assert!(
                has_dynamic_rust_macro(source.as_bytes(), tree.root_node()),
                "dynamic source was missed: {source}"
            );
        }
        let source = b"println!(\"include is text\");\n";
        let tree = parser::parse_source(source, LangId::Rust).expect("parse Rust");
        assert!(!has_dynamic_rust_macro(source, tree.root_node()));
        let source = b"fn f() { println!(\"{}\", args.include); }\n";
        let tree = parser::parse_source(source, LangId::Rust).expect("parse Rust");
        assert!(!has_dynamic_rust_macro(source, tree.root_node()));
    }
}
