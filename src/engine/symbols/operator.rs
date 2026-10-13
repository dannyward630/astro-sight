//! 言語の構文から暗黙に呼ばれるメソッドの判定。
//!
//! Kotlin の `operator fun` は演算子の構文 (`a + b` → `plus`、`a[i]` → `get`、`a()` →
//! `invoke`、`x in a` → `contains`、分割代入 → `componentN`、`by` の委譲 → `getValue` など)
//! から、Swift の `callAsFunction` は `a()` から呼ばれる。呼び出し側に名前が現れないので、
//! リポジトリ内の参照は 0 件が正常。dead-code (実行時入口を除外する経路) がこれを
//! 「未参照」と報告しないために使う。
//!
//! API 差分には持ち込まない (`is_program_entrypoint` と同じ境界)。`operator` の付け外しは
//! 呼び出し方の契約の変更なので、API 差分では通常どおり比較する。

use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;
use crate::models::symbol::SymbolKind;

use super::{enclosing_of_kind, node_for_symbol_range};

/// `symbol_range` の関数・メソッドが、言語の構文から暗黙に呼ばれるものか。
pub fn is_syntax_invoked_method(
    root: Node,
    source: &[u8],
    lang_id: LangId,
    kind: SymbolKind,
    name: &str,
    symbol_range: &Range,
) -> bool {
    if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        return false;
    }
    match lang_id {
        LangId::Kotlin => node_for_symbol_range(root, symbol_range)
            .and_then(|node| enclosing_of_kind(node, &["function_declaration"]))
            .is_some_and(|decl| kotlin_has_operator_modifier(decl, source)),
        // SE-0253: `callAsFunction` を持つ型の値は `a()` の形で呼べる。
        LangId::Swift => name == "callAsFunction",
        _ => false,
    }
}

/// Kotlin の関数宣言の `modifiers` に `operator` (`function_modifier`) があるか。
fn kotlin_has_operator_modifier(decl: Node, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    decl.children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .any(|modifiers| {
            let mut inner = modifiers.walk();
            modifiers.children(&mut inner).any(|modifier| {
                modifier.kind() == "function_modifier"
                    && modifier.utf8_text(source) == Ok("operator")
            })
        })
}
