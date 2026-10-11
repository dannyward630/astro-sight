//! Rust 関数シグネチャの非契約差分を正規化・証明する。
//!
//! 通常引数の binding-side `mut` 除去と、root-level の公開 type alias 導入だけで
//! 実体型が変わらないことの old/new AST 証明を担う。

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use tree_sitter::Node;

use crate::engine::parser;
use crate::language::LangId;

/// old/new それぞれの同一ファイル内 `pub type` を使い、関数シグネチャの型表現が
/// 透過的な alias 導入だけで変わったかを証明するコンテキスト。
///
/// 対象は非 generic・一意・unrestricted `pub` の alias に限定する。import alias、private / restricted
/// alias、generic、重複、循環、parse 不能はすべて「同値と証明できない」側へ倒す。
pub(crate) struct RustTypeAliasEquivalence {
    old: TypeAliasIndex,
    new: TypeAliasIndex,
}

/// working tree 側を先に parse した alias 同値証明候補。
///
/// 新側に利用可能な root `pub type` が無ければ構築せず、API 検出で base blob の
/// `git show` を起動しない。impact では Pass 1 の既存 tree から構築して再 parse を避ける。
pub(crate) struct PreparedRustTypeAliases {
    new: TypeAliasIndex,
}

impl RustTypeAliasEquivalence {
    #[cfg(test)]
    pub(crate) fn new(old_source: &[u8], new_source: &[u8]) -> Option<Self> {
        Some(Self {
            old: TypeAliasIndex::from_source(old_source)?,
            new: TypeAliasIndex::from_source(new_source)?,
        })
    }

    /// working tree ソースを先に parse し、新側に証明対象の alias がある場合だけ候補を返す。
    pub(crate) fn prepare_new_source(new_source: &[u8]) -> Option<PreparedRustTypeAliases> {
        let tree = parser::parse_source(new_source, LangId::Rust).ok()?;
        Self::prepare_new_tree(tree.root_node(), new_source)
    }

    /// 呼び出し側が既に parse した working tree の root を再利用する。
    pub(crate) fn prepare_new_tree(
        new_root: Node<'_>,
        new_source: &[u8],
    ) -> Option<PreparedRustTypeAliases> {
        let new = TypeAliasIndex::from_root(new_root, new_source)?;
        (!new.aliases.is_empty()).then_some(PreparedRustTypeAliases { new })
    }

    /// old/new の完全な root 関数ヘッダを比較する。diff の変更行や抽出済みの
    /// 1 行シグネチャは名前の候補にだけ使い、同値性の証拠にはしない。
    pub(crate) fn prove_function(&self, name: &str) -> bool {
        // import の意味が同じと証明できない状態で、alias RHS 内の同名型を字面だけで同一視しない。
        if self.old.imports != self.new.imports {
            return false;
        }
        if self.old.function_counts.get(name) != Some(&1)
            || self.new.function_counts.get(name) != Some(&1)
        {
            return false;
        }
        let Some(old_header) = self.old.root_function_headers.get(name) else {
            return false;
        };
        let Some(new_header) = self.new.root_function_headers.get(name) else {
            return false;
        };
        let Some(old) = canonicalize_signature(old_header, &self.old) else {
            return false;
        };
        let Some(new) = canonicalize_signature(new_header, &self.new) else {
            return false;
        };
        if old.function_name != name || new.function_name != name {
            return false;
        }
        if !new.expanded_alias || old.tokens != new.tokens {
            return false;
        }
        // 型・配列長などに現れる名前の root 束縛が変化した場合は、字面の
        // 一致だけで実体型が同じとは言えない。新規 alias 自身だけを除外する。
        old.names.union(&new.names).all(|used| {
            if new.expanded_alias_names.contains(used) && !self.old.root_bindings.contains_key(used)
            {
                return true;
            }
            self.old.root_bindings.get(used) == self.new.root_bindings.get(used)
        })
    }

    #[cfg(test)]
    pub(crate) fn signatures_equivalent(&self, old_signature: &str, new_signature: &str) -> bool {
        let Some(old) = canonicalize_signature(old_signature, &self.old) else {
            return false;
        };
        let Some(new) = canonicalize_signature(new_signature, &self.new) else {
            return false;
        };
        old.function_name == new.function_name && self.prove_function(&old.function_name)
    }
}

impl PreparedRustTypeAliases {
    /// 先に準備した new index と、後から取得した old フルソースを組み合わせる。
    pub(crate) fn with_old_source(self, old_source: &[u8]) -> Option<RustTypeAliasEquivalence> {
        Some(RustTypeAliasEquivalence {
            old: TypeAliasIndex::from_source(old_source)?,
            new: self.new,
        })
    }
}

#[derive(Default)]
struct TypeAliasIndex {
    aliases: HashMap<String, String>,
    blocked: HashSet<String>,
    imports: Vec<String>,
    root_function_headers: HashMap<String, String>,
    function_counts: HashMap<String, usize>,
    root_bindings: HashMap<String, Vec<String>>,
    blocked_root_functions: HashSet<String>,
}

impl TypeAliasIndex {
    fn from_source(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        Self::from_root(tree.root_node(), source)
    }

    fn from_root(root: Node<'_>, source: &[u8]) -> Option<Self> {
        if root.has_error() {
            return None;
        }
        let mut index = Self::default();
        let mut has_pending_attribute = false;
        let mut pending_attributes = String::new();
        let mut ambiguous_root_scope = false;
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if child.kind() == "attribute_item" {
                has_pending_attribute = true;
                pending_attributes.push_str(child.utf8_text(source).ok()?);
                pending_attributes.push('\n');
                continue;
            }
            if child.kind() == "inner_attribute_item" {
                ambiguous_root_scope = true;
                continue;
            }
            if matches!(child.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if matches!(
                child.kind(),
                "macro_invocation" | "extern_crate_declaration"
            ) {
                ambiguous_root_scope = true;
            }
            if child.kind() == "type_item" {
                index.collect_alias(child, source, has_pending_attribute);
            } else if child.kind() == "function_item" {
                index.collect_root_function(child, source, has_pending_attribute);
            } else {
                // スコープ解決は行わない。root 以外の同名 alias / associated type があれば、
                // root alias を誤適用しないよう名前単位で同値証明を諦める。
                index.block_nested_aliases(child, source);
            }

            if child.kind() == "use_declaration" {
                // 条件付き import の有効性は静的に確定できない。old/new の字面一致だけで
                // 名前解決が同じと証明せず、ファイル全体の alias 証明を無効にする。
                if has_pending_attribute {
                    ambiguous_root_scope = true;
                }
                index.imports.push(canonical_token_stream(child, source)?);
            }
            if child.kind() != "function_item" {
                index.collect_root_binding(child, source, &pending_attributes)?;
            }
            has_pending_attribute = false;
            pending_attributes.clear();
        }
        collect_function_counts(root, source, &mut index.function_counts)?;
        if ambiguous_root_scope {
            index.aliases.clear();
        }
        index.imports.sort();
        Some(index)
    }

    fn collect_alias(&mut self, item: Node<'_>, source: &[u8], attributed: bool) {
        let Some(name_node) = item.child_by_field_name("name") else {
            return;
        };
        let Ok(name) = name_node.utf8_text(source) else {
            return;
        };
        let name = name.to_string();
        let unrestricted_public = direct_named_child(item, "visibility_modifier")
            .and_then(|node| node.utf8_text(source).ok())
            .is_some_and(|visibility| !visibility.contains('('));
        let generic = direct_named_child(item, "type_parameters").is_some();
        let rhs = item
            .child_by_field_name("type")
            .and_then(|node| node.utf8_text(source).ok())
            .map(str::to_string);

        if attributed || !unrestricted_public || generic || self.aliases.contains_key(&name) {
            self.aliases.remove(&name);
            self.blocked.insert(name);
            return;
        }
        let Some(rhs) = rhs else {
            self.aliases.remove(&name);
            self.blocked.insert(name);
            return;
        };
        if !self.blocked.contains(&name) {
            self.aliases.insert(name, rhs);
        }
    }

    fn collect_root_function(&mut self, item: Node<'_>, source: &[u8], attributed: bool) {
        let Some(name) = item
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
        else {
            return;
        };
        let header = item
            .child_by_field_name("body")
            .and_then(|body| source.get(item.start_byte()..body.start_byte()))
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        let Some(header) = header else {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
            return;
        };
        if attributed || self.blocked_root_functions.contains(name) {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
            return;
        }
        if self
            .root_function_headers
            .insert(name.to_string(), header.to_string())
            .is_some()
        {
            self.root_function_headers.remove(name);
            self.blocked_root_functions.insert(name.to_string());
        }
    }

    fn collect_root_binding(
        &mut self,
        item: Node<'_>,
        source: &[u8],
        pending_attributes: &str,
    ) -> Option<()> {
        let Some(name) = item.child_by_field_name("name") else {
            return Some(());
        };
        let name = name.utf8_text(source).ok()?.to_string();
        let declaration = format!("{pending_attributes}{}", item.utf8_text(source).ok()?);
        self.root_bindings
            .entry(name)
            .or_default()
            .push(declaration);
        Some(())
    }

    fn block_nested_aliases(&mut self, node: Node<'_>, source: &[u8]) {
        if matches!(node.kind(), "type_item" | "associated_type") {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
            {
                self.aliases.remove(name);
                self.blocked.insert(name.to_string());
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.block_nested_aliases(child, source);
        }
    }
}

fn collect_function_counts(
    node: Node<'_>,
    source: &[u8],
    counts: &mut HashMap<String, usize>,
) -> Option<()> {
    if matches!(node.kind(), "function_item" | "function_signature_item")
        && let Some(name) = node.child_by_field_name("name")
    {
        *counts
            .entry(name.utf8_text(source).ok()?.to_string())
            .or_default() += 1;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_function_counts(child, source, counts)?;
    }
    Some(())
}

struct CanonicalSignature {
    tokens: String,
    expanded_alias: bool,
    expanded_alias_names: HashSet<String>,
    function_name: String,
    names: HashSet<String>,
}

fn canonicalize_signature(signature: &str, aliases: &TypeAliasIndex) -> Option<CanonicalSignature> {
    let (source, original_len) = parseable_signature_source(signature);
    let tree = parser::parse_source(&source, LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let function = find_function_node(tree.root_node())?;
    let function_name = function
        .child_by_field_name("name")?
        .utf8_text(&source)
        .ok()?
        .to_string();
    let mut shadowed = HashSet::new();
    if let Some(type_parameters) = direct_named_child(function, "type_parameters") {
        collect_type_identifiers(type_parameters, &source, &mut shadowed)?;
    }

    let signature_end = function
        .child_by_field_name("body")
        .map(|body| body.start_byte())
        .unwrap_or(original_len)
        .min(original_len);
    let mut canonical = CanonicalSignature {
        tokens: String::new(),
        expanded_alias: false,
        expanded_alias_names: HashSet::new(),
        function_name,
        names: HashSet::new(),
    };
    let mut stack = Vec::new();
    canonicalize_node(
        function,
        &source,
        signature_end,
        aliases,
        &shadowed,
        &mut stack,
        &mut canonical,
    )?;
    Some(canonical)
}

fn canonicalize_node(
    node: Node<'_>,
    source: &[u8],
    end: usize,
    aliases: &TypeAliasIndex,
    shadowed: &HashSet<String>,
    stack: &mut Vec<String>,
    canonical: &mut CanonicalSignature,
) -> Option<()> {
    if node.start_byte() >= end {
        return Some(());
    }
    // 配列長と const generic 引数は値の評価が要る。関数・関連定数・式が
    // 同じ字面でも別の値になり得るため、リテラル以外を同値の証拠にしない。
    if node.kind() == "array_type"
        && node
            .child_by_field_name("length")
            .is_some_and(|length| length.kind() != "integer_literal")
    {
        return None;
    }
    if node.kind() == "type_arguments" {
        let mut cursor = node.walk();
        if node.named_children(&mut cursor).any(|child| {
            matches!(child.kind(), "identifier" | "scoped_identifier" | "block")
                || child.kind().ends_with("_expression")
        }) {
            return None;
        }
    }
    if matches!(node.kind(), "identifier" | "type_identifier") {
        canonical
            .names
            .insert(node.utf8_text(source).ok()?.to_string());
    }
    if node.kind() == "type_identifier" {
        let name = node.utf8_text(source).ok()?;
        if aliases.blocked.contains(name) {
            return None;
        }
        let qualified = node.parent().is_some_and(|parent| {
            matches!(
                parent.kind(),
                "scoped_type_identifier" | "scoped_identifier"
            )
        });
        if aliases.aliases.contains_key(name) && !shadowed.contains(name) && !qualified {
            if !shadowed.is_empty() {
                return None;
            }
            canonical.expanded_alias = true;
            canonical.expanded_alias_names.insert(name.to_string());
            return canonicalize_alias(name, aliases, shadowed, stack, canonical);
        }
    }

    if node.child_count() == 0 {
        if !matches!(node.kind(), "line_comment" | "block_comment") {
            push_token(canonical, node.kind(), node.utf8_text(source).ok()?);
        }
        return Some(());
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        canonicalize_node(child, source, end, aliases, shadowed, stack, canonical)?;
    }
    Some(())
}

fn canonicalize_alias(
    name: &str,
    aliases: &TypeAliasIndex,
    shadowed: &HashSet<String>,
    stack: &mut Vec<String>,
    canonical: &mut CanonicalSignature,
) -> Option<()> {
    if stack.len() >= 32 || stack.iter().any(|active| active == name) {
        return None;
    }
    let rhs = aliases.aliases.get(name)?;
    stack.push(name.to_string());
    let wrapped = format!("type __AstroSightAlias = {rhs};");
    let tree = parser::parse_source(wrapped.as_bytes(), LangId::Rust).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let type_item = direct_named_child(tree.root_node(), "type_item")?;
    let rhs_node = type_item.child_by_field_name("type")?;
    if contains_unsupported_alias_type(rhs_node) {
        return None;
    }
    canonicalize_node(
        rhs_node,
        wrapped.as_bytes(),
        wrapped.len(),
        aliases,
        shadowed,
        stack,
        canonical,
    )?;
    stack.pop();
    Some(())
}

fn contains_unsupported_alias_type(node: Node<'_>) -> bool {
    if matches!(node.kind(), "dynamic_type" | "macro_invocation") {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(contains_unsupported_alias_type)
}

fn canonical_token_stream(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut canonical = CanonicalSignature {
        tokens: String::new(),
        expanded_alias: false,
        expanded_alias_names: HashSet::new(),
        function_name: String::new(),
        names: HashSet::new(),
    };
    canonicalize_node(
        node,
        source,
        node.end_byte(),
        &TypeAliasIndex::default(),
        &HashSet::new(),
        &mut Vec::new(),
        &mut canonical,
    )?;
    Some(canonical.tokens)
}

fn collect_type_identifiers(
    node: Node<'_>,
    source: &[u8],
    names: &mut HashSet<String>,
) -> Option<()> {
    if node.kind() == "type_identifier" {
        names.insert(node.utf8_text(source).ok()?.to_string());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_type_identifiers(child, source, names)?;
    }
    Some(())
}

fn push_token(canonical: &mut CanonicalSignature, kind: &str, text: &str) {
    let _ = write!(
        canonical.tokens,
        "{}:{kind}:{}:{text};",
        kind.len(),
        text.len()
    );
}

fn parseable_signature_source(signature: &str) -> (Vec<u8>, usize) {
    let original_len = signature.len();
    let trimmed = signature.trim_end();
    let mut parseable = signature.as_bytes().to_vec();
    if trimmed.ends_with('{') {
        parseable.extend_from_slice(b" }");
    } else if !trimmed.ends_with(';') && !trimmed.ends_with('}') {
        parseable.extend_from_slice(b" {}");
    }
    (parseable, original_len)
}

/// Rust の通常引数に付く binding-side の `mut` を除いたシグネチャを返す。
///
/// `fn run(mut callback: F)` の `mut` は関数本体で callback 変数を再束縛・可変借用するための
/// 指定で、呼び出し側の型契約ではない。一方、`callback: &mut F` の `mut` は型の一部なので
/// 除外しない。文字列置換では両者を区別できないため、`parameter` の直接子にある
/// `mutable_specifier` だけを AST 位置で除く。
pub(crate) fn normalize_rust_parameter_binding_signature(
    function: Node<'_>,
    source: &[u8],
    start: usize,
    end: usize,
) -> Option<String> {
    let parameters = function
        .child_by_field_name("parameters")
        .or_else(|| direct_named_child(function, "parameters"))?;
    let mut omitted = Vec::new();

    for index in 0..parameters.child_count() {
        let Some(parameter) = parameters.child(index) else {
            continue;
        };
        // `self_parameter` は `mut self` と `&mut self` の grammar shape が異なる版もある。
        // 今回は通常引数の、構造的に型側と区別できる位置だけを扱う。
        if parameter.kind() != "parameter" {
            continue;
        }
        for child_index in 0..parameter.child_count() {
            let Some(child) = parameter.child(child_index) else {
                continue;
            };
            if child.kind() == "mutable_specifier" {
                omitted.push((child.start_byte(), child.end_byte()));
            }
        }
    }

    rebuild_without_ranges(source, start, end, &omitted)
}

/// unified diff の 1 行シグネチャにも同じ AST 正規化を適用する。
/// parse や function node の特定に失敗した場合は `None` を返し、呼び出し側が元文字列で
/// 比較することで fail-closed に倒す。
pub(crate) fn normalize_rust_signature_text(signature: &str) -> Option<String> {
    let (parseable, original_len) = parseable_signature_source(signature);

    let tree = parser::parse_source(&parseable, LangId::Rust).ok()?;
    let function = find_function_node(tree.root_node())?;
    normalize_rust_parameter_binding_signature(
        function,
        &parseable,
        function.start_byte(),
        original_len,
    )
}

fn direct_named_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

fn find_function_node(node: Node<'_>) -> Option<Node<'_>> {
    if matches!(node.kind(), "function_item" | "function_signature_item") {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_function_node(child) {
            return Some(found);
        }
    }
    None
}

fn rebuild_without_ranges(
    source: &[u8],
    start: usize,
    end: usize,
    omitted: &[(usize, usize)],
) -> Option<String> {
    let _ = source.get(start..end)?;
    let mut ranges: Vec<_> = omitted
        .iter()
        .copied()
        .filter(|(range_start, range_end)| start <= *range_start && *range_end <= end)
        .collect();
    ranges.sort_unstable();

    let mut rebuilt = Vec::with_capacity(end.saturating_sub(start));
    let mut cursor = start;
    for (range_start, mut range_end) in ranges {
        if range_start < cursor {
            continue;
        }
        rebuilt.extend_from_slice(source.get(cursor..range_start)?);
        while range_end < end && source[range_end].is_ascii_whitespace() {
            range_end += 1;
        }
        cursor = range_end;
    }
    rebuilt.extend_from_slice(source.get(cursor..end)?);
    Some(normalize_whitespace(&rebuilt))
}

fn normalize_whitespace(bytes: &[u8]) -> String {
    super::normalize_whitespace(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_parameter_binding_mut_but_keeps_type_mutability() {
        let with_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(mut callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        let without_binding_mut =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &mut u32) {")
                .expect("Rust signature should parse");
        assert_eq!(with_binding_mut, without_binding_mut);

        let immutable_type =
            normalize_rust_signature_text("pub fn run<F>(callback: F, value: &u32) {")
                .expect("Rust signature should parse");
        assert_ne!(without_binding_mut, immutable_type);
    }

    #[test]
    fn malformed_signature_fails_closed() {
        assert!(normalize_rust_signature_text("not a Rust function").is_none());
    }

    #[test]
    fn transparent_public_alias_introduction_is_equivalent() {
        let old = b"pub fn collect() -> Vec<(u64, (u8, u8), String)> { vec![] }";
        let new =
            b"pub type Entry = (u64, (u8, u8), String);\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, (u8, u8), String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn new_source_without_candidate_alias_skips_preparation() {
        assert!(
            RustTypeAliasEquivalence::prepare_new_source(
                b"pub fn collect() -> Vec<(u64, String)> { vec![] }"
            )
            .is_none()
        );
    }

    #[test]
    fn changed_generic_imported_private_and_cyclic_aliases_fail_closed() {
        for (label, new_source, new_signature) in [
            (
                "changed rhs",
                "pub type Entry = (u32, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "generic",
                "pub type Entry<T> = (T, String);\npub fn collect() -> Vec<Entry<u64>> { vec![] }",
                "pub fn collect() -> Vec<Entry<u64>>",
            ),
            (
                "imported",
                "use external::Entry;\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "private",
                "type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
            (
                "cycle",
                "pub type Entry = Other;\npub type Other = Entry;\npub fn collect() -> Vec<Entry> { vec![] }",
                "pub fn collect() -> Vec<Entry>",
            ),
        ] {
            let old = b"pub fn collect() -> Vec<(u64, String)> { vec![] }";
            let context = RustTypeAliasEquivalence::new(old, new_source.as_bytes())
                .expect("sources should remain parseable");
            assert!(
                !context
                    .signatures_equivalent("pub fn collect() -> Vec<(u64, String)>", new_signature),
                "{label} は同値と証明せず blocking を維持する"
            );
        }
    }

    #[test]
    fn changed_import_binding_prevents_alias_equivalence() {
        let old = b"use old_crate::External;\npub fn collect() -> Vec<External> { vec![] }";
        let new = b"use new_crate::External;\npub type Entry = External;\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<External>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn nested_same_name_alias_blocks_root_alias_expansion() {
        let old = b"pub type Entry = (u64, String);\npub mod inner { pub fn collect() -> Vec<(u64, String)> { vec![] } }";
        let new = b"pub type Entry = (u64, String);\npub mod inner { pub type Entry = (u32, String); pub fn collect() -> Vec<Entry> { vec![] } }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn associated_type_with_same_name_blocks_root_alias_expansion() {
        let old = b"pub type Entry = (u64, String);\npub trait Source { type Entry; fn collect() -> Vec<(u64, String)>; }";
        let new = b"pub type Entry = (u64, String);\npub trait Source { type Entry; fn collect() -> Vec<Entry>; }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "fn collect() -> Vec<(u64, String)>",
            "fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn nested_import_scope_cannot_reuse_root_alias_proof() {
        let old = b"pub struct ExtA; pub struct ExtB; use crate::ExtA as Ext; pub type Entry = (Ext, String); pub mod inner { use crate::ExtB as Ext; pub fn collect() -> Vec<(Ext, String)> { vec![] } }";
        let new = b"pub struct ExtA; pub struct ExtB; use crate::ExtA as Ext; pub type Entry = (Ext, String); pub mod inner { use crate::ExtB as Ext; use super::Entry; pub fn collect() -> Vec<Entry> { vec![] } }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(Ext, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn attributed_alias_and_function_fail_closed() {
        let old = b"#[cfg(feature = \"x\")] pub fn collect() -> Vec<(u64, String)> { vec![] }";
        let new = b"#[cfg(feature = \"x\")] pub type Entry = (u64, String); #[cfg(feature = \"x\")] pub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid Rust sources");
        assert!(!context.signatures_equivalent(
            "pub fn collect() -> Vec<(u64, String)>",
            "pub fn collect() -> Vec<Entry>"
        ));
    }

    #[test]
    fn complete_headers_and_name_bindings_are_required_for_alias_proof() {
        for (label, old, new) in [
            (
                "multiline return changed",
                "pub fn collect(x: (u64, String),\n) -> u32 { 1 }",
                "pub type Entry = (u64, String);\npub fn collect(x: Entry,\n) -> u64 { 1 }",
            ),
            (
                "same name in nested module",
                "pub fn collect() -> (u64, String) { unreachable!() }\nmod inner { fn collect() {} }",
                "pub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }\nmod inner { fn collect() {} }",
            ),
            (
                "prelude name rebound",
                "pub fn collect() -> (u64, String) { unreachable!() }",
                "pub struct String;\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "root binding attribute changed",
                "pub struct String;\npub fn collect() -> (u64, String) { unreachable!() }",
                "#[cfg(feature = \"extra\")]\npub struct String;\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "const in array length changed",
                "const N: usize = 4;\npub fn collect() -> [u8; N] { [0; N] }",
                "const N: usize = 8;\npub type Entry = [u8; N];\npub fn collect() -> Entry { [0; N] }",
            ),
            (
                "const function in array length changed",
                "pub const fn size() -> usize { 4 }\npub fn collect() -> [u8; size()] { [0; size()] }",
                "pub const fn size() -> usize { 8 }\npub type Entry = [u8; size()];\npub fn collect() -> Entry { [0; size()] }",
            ),
            (
                "associated const in array length changed",
                "pub struct Foo;\nimpl Foo { pub const N: usize = 4; }\npub fn collect() -> [u8; Foo::N] { [0; Foo::N] }",
                "pub struct Foo;\nimpl Foo { pub const N: usize = 8; }\npub type Entry = [u8; Foo::N];\npub fn collect() -> Entry { [0; Foo::N] }",
            ),
            (
                "const generic expression changed",
                "pub struct Bag<const N: usize>;\npub const fn size() -> usize { 4 }\npub fn collect() -> Bag<{ size() }> { unreachable!() }",
                "pub struct Bag<const N: usize>;\npub const fn size() -> usize { 8 }\npub type Entry = Bag<{ size() }>;\npub fn collect() -> Entry { unreachable!() }",
            ),
            (
                "function generic shadows root type",
                "pub struct T;\npub fn collect<T>() -> (T, u8) { unreachable!() }",
                "pub struct T;\npub type Entry = (T, u8);\npub fn collect<T>() -> Entry { unreachable!() }",
            ),
            (
                "trait object lifetime changes",
                "pub trait Tr {}\npub fn collect(x: &dyn Tr) {}",
                "pub trait Tr {}\npub type Obj = dyn Tr;\npub fn collect(x: &Obj) {}",
            ),
            (
                "attribute followed by comment",
                "pub fn collect() -> (u64, String) { unreachable!() }",
                "#[cfg(feature = \"extra\")]\n/// Documentation\npub type Entry = (u64, String);\npub fn collect() -> Entry { unreachable!() }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid syntax trees");
            assert!(!context.prove_function("collect"), "{label}");
        }
    }

    #[test]
    fn unrelated_root_addition_does_not_block_alias_proof() {
        let old = b"pub fn collect() -> Vec<(u64, String)> { vec![] }";
        let new = b"pub struct Unrelated;\npub type Entry = (u64, String);\npub fn collect() -> Vec<Entry> { vec![] }";
        let context = RustTypeAliasEquivalence::new(old, new).expect("valid syntax trees");
        assert!(context.prove_function("collect"));
    }

    #[test]
    fn slices_and_literal_length_arrays_allow_alias_proof() {
        for (old, new) in [
            (
                "pub fn collect(input: &[u8]) -> Vec<(u64, String)> { vec![] }",
                "pub type Entry = (u64, String);\npub fn collect(input: &[u8]) -> Vec<Entry> { vec![] }",
            ),
            (
                "pub fn collect() -> Vec<[u8; 4]> { vec![] }",
                "pub type Arr = [u8; 4];\npub fn collect() -> Vec<Arr> { vec![] }",
            ),
        ] {
            let context = RustTypeAliasEquivalence::new(old.as_bytes(), new.as_bytes())
                .expect("valid syntax trees");
            assert!(context.prove_function("collect"), "{old} -> {new}");
        }
    }
}
