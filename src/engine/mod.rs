pub mod angular_template_refs;
pub(crate) mod bash_parse_recovery;
pub mod bash_trap_refs;
pub(crate) mod bounded_read;
pub mod calls;
pub mod cochange;
pub mod diff;
pub mod extractor;
pub mod generated;
pub mod gitattributes;
pub mod impact;
pub mod imports;
pub(crate) mod js_binding_pattern;
pub mod lexer;
pub(crate) mod lexical_binding;
pub mod lint;
pub mod parser;
pub mod phpunit_refs;
pub(crate) mod python_callable;
pub(crate) mod python_scope;
pub(crate) mod python_signature;
pub mod query_cache;
pub mod refs;
pub(crate) mod rust_cfg;
pub(crate) mod rust_macro_name;
pub(crate) mod rust_scope;
pub(crate) mod rust_signature;
pub(crate) mod rust_targets;
pub(crate) mod rust_use_tree;
pub mod sequence;
pub mod snippet;
pub mod symbols;
pub mod xml_refs;

/// signature 文字列の改行・タブ・連続空白を単一 space に正規化する。
pub(crate) fn normalize_whitespace(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
