use camino::Utf8Path;
use lru::LruCache;

use crate::engine::parser;
use crate::language::LangId;

use super::{ParsedFile, descendant_for_range};

/// シンボルがテストコンテキスト内にあるか確認する。
///
/// テストシンボルは cross-file 影響を伝播すべきでない：
/// - テスト関数はプロダクションコードから呼ばれない
/// - テストヘルパーの変更はテストモジュールのみに影響する
///
/// 2層のアプローチを使用する：
/// 1. ファイルパスベースの判定（高速、全言語対応）
/// 2. AST ベースの判定（精密、言語固有）
pub(crate) fn is_in_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
    lang_id: LangId,
    path: &str,
) -> bool {
    // ファイルパスベースの判定（全言語共通）
    if is_test_file_path(path) {
        return true;
    }
    // AST ベースの判定（言語別）
    match lang_id {
        LangId::Rust => is_rust_test_context(root, source, symbol_range),
        LangId::Python => is_python_test_context(root, source, symbol_range),
        LangId::Javascript | LangId::Typescript | LangId::Tsx => {
            is_js_test_context(root, source, symbol_range)
        }
        LangId::Java | LangId::Kotlin => is_jvm_test_context(root, source, symbol_range),
        LangId::CSharp => is_csharp_test_context(root, source, symbol_range),
        LangId::Ruby => is_ruby_test_context(root, source, symbol_range),
        _ => false,
    }
}

/// ファイルパスからテストファイルかどうかを判定する。
pub(crate) fn is_test_file_path(path: &str) -> bool {
    let filename = path.rsplit('/').next().unwrap_or(path);
    let filename_lower = filename.to_lowercase();

    // Go: *_test.go
    if filename_lower.ends_with("_test.go") {
        return true;
    }
    // JS/TS: *.test.ts, *.spec.ts, *.test.js, *.spec.js, *.test.tsx, *.spec.tsx
    if filename_lower.contains(".test.") || filename_lower.contains(".spec.") {
        return true;
    }
    // Python: test_*.py, *_test.py, conftest.py
    if filename_lower.ends_with(".py")
        && (filename_lower.starts_with("test_")
            || filename_lower.ends_with("_test.py")
            || filename_lower == "conftest.py")
    {
        return true;
    }
    // Java/Kotlin: *Test.java, *Tests.java, *Test.kt, *Tests.kt
    if filename.ends_with("Test.java")
        || filename.ends_with("Tests.java")
        || filename.ends_with("Test.kt")
        || filename.ends_with("Tests.kt")
    {
        return true;
    }
    // C#: *Test.cs, *Tests.cs
    if filename.ends_with("Test.cs") || filename.ends_with("Tests.cs") {
        return true;
    }
    // Ruby: *_test.rb, *_spec.rb
    if filename_lower.ends_with("_test.rb") || filename_lower.ends_with("_spec.rb") {
        return true;
    }
    // PHP: *Test.php, *Tests.php
    if filename.ends_with("Test.php") || filename.ends_with("Tests.php") {
        return true;
    }
    // C/Cpp: *_test.c, *_test.cpp, *_test.cc
    if filename_lower.ends_with("_test.c")
        || filename_lower.ends_with("_test.cpp")
        || filename_lower.ends_with("_test.cc")
    {
        return true;
    }
    // Bash: *.bats, *_test.sh
    if filename_lower.ends_with(".bats") || filename_lower.ends_with("_test.sh") {
        return true;
    }
    false
}

/// `symbol_range` を覆う最小のノードから根までの祖先 (自身を含む) に、`is_test` を
/// 満たすノードがあるか。
fn any_ancestor(
    root: tree_sitter::Node,
    symbol_range: &crate::models::location::Range,
    is_test: impl FnMut(tree_sitter::Node) -> bool,
) -> bool {
    let Some(node) = descendant_for_range(root, symbol_range) else {
        return false;
    };
    ancestors_inclusive(root, node).into_iter().any(is_test)
}

/// `node` から根までの祖先 (`node` 自身を含む) を内側から順に返す。
///
/// `Node::parent` は根から辿り直すので深さに比例し、1 段ずつ親をたどると深さの 2 乗の
/// 時間になる (深い use ツリーの中の参照で数十秒かかっていた)。根から `node` へ 1 回
/// だけ降りて道筋を記録する。道筋を一意に決められない場合 (幅 0 のノード、`root` が
/// 木の根でない場合) は従来どおり `Node::parent` でたどる。
fn ancestors_inclusive<'tree>(
    root: tree_sitter::Node<'tree>,
    node: tree_sitter::Node<'tree>,
) -> Vec<tree_sitter::Node<'tree>> {
    if let Some(mut path) = path_from_root(root, node) {
        path.reverse();
        return path;
    }
    let mut chain = Vec::new();
    let mut current = Some(node);
    while let Some(n) = current {
        chain.push(n);
        current = n.parent();
    }
    chain
}

/// 木の根 `root` から `target` までの道筋 (両端を含み、外側から)。
///
/// 兄弟の範囲は重ならないので、幅が正の `target` の範囲を含む子は各段で高々 1 つで、
/// それが `target` の祖先になる。`target` に着かなければ `None`。
fn path_from_root<'tree>(
    root: tree_sitter::Node<'tree>,
    target: tree_sitter::Node<'tree>,
) -> Option<Vec<tree_sitter::Node<'tree>>> {
    let (start, end) = (target.start_byte(), target.end_byte());
    if start >= end || root.parent().is_some() {
        return None;
    }
    let mut path = vec![root];
    let mut cursor = root.walk();
    while cursor.node().id() != target.id() {
        if !cursor.goto_first_child() {
            return None;
        }
        loop {
            let child = cursor.node();
            if child.start_byte() <= start && end <= child.end_byte() {
                break;
            }
            if !cursor.goto_next_sibling() {
                return None;
            }
        }
        path.push(cursor.node());
    }
    Some(path)
}

/// Rust: `#[test]` attribute / `#[cfg(test)]` module
fn is_rust_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_rust_test_item(n, source))
}

fn is_rust_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    (n.kind() == "function_item" && has_attribute_text(n, source, "test"))
        || (n.kind() == "mod_item" && has_attribute_text(n, source, "cfg(test)"))
}

/// Python: `test_*` 関数名 / `TestCase` サブクラス内
fn is_python_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_python_test_item(n, source))
}

fn is_python_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    if n.kind() == "function_definition"
        && let Some(name) = n.child_by_field_name("name")
        && let Ok(text) = name.utf8_text(source)
        && text.starts_with("test_")
    {
        return true;
    }
    // TestCase サブクラス内
    if n.kind() == "class_definition"
        && let Some(bases) = n.child_by_field_name("superclasses")
        && let Ok(text) = bases.utf8_text(source)
        && text.contains("TestCase")
    {
        return true;
    }
    // pytest decorator
    if n.kind() == "decorated_definition" {
        let mut cursor = n.walk();
        for child in n.children(&mut cursor) {
            if child.kind() == "decorator"
                && let Ok(text) = child.utf8_text(source)
                && (text.contains("pytest.fixture") || text.contains("pytest.mark"))
            {
                return true;
            }
        }
    }
    false
}

/// JS/TS/TSX: `test()`/`it()`/`describe()` call 内
fn is_js_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_js_test_item(n, source))
}

fn is_js_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    n.kind() == "call_expression"
        && n.child_by_field_name("function")
            .and_then(|func| func.utf8_text(source).ok())
            .is_some_and(|name| {
                matches!(
                    name,
                    "test"
                        | "it"
                        | "describe"
                        | "beforeEach"
                        | "afterEach"
                        | "beforeAll"
                        | "afterAll"
                )
            })
}

/// Java/Kotlin: `@Test` アノテーション
fn is_jvm_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_jvm_test_item(n, source))
}

fn is_jvm_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    let is_method = matches!(
        n.kind(),
        "method_declaration" | "function_declaration" | "constructor_declaration"
    );
    is_method && has_jvm_annotation(n, source, "Test")
}

/// Java/Kotlin のアノテーション（`@Test` 等）の存在チェック。
fn has_jvm_annotation(node: tree_sitter::Node, source: &[u8], name: &str) -> bool {
    // Java: modifiers > marker_annotation > identifier
    // Kotlin: modifiers > annotation > ... > simple_identifier
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "modifiers" | "marker_annotation" | "annotation"
        ) && let Ok(text) = child.utf8_text(source)
            && text.contains(name)
        {
            return true;
        }
    }
    // prev sibling check (annotation が method の前に独立ノードとして存在する場合)
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        if matches!(p.kind(), "marker_annotation" | "annotation") {
            if let Ok(text) = p.utf8_text(source)
                && text.contains(name)
            {
                return true;
            }
            prev = p.prev_named_sibling();
        } else if p.kind().contains("comment") {
            prev = p.prev_named_sibling();
        } else {
            break;
        }
    }
    false
}

/// C#: `[Test]`/`[Fact]`/`[TestMethod]` アトリビュート
fn is_csharp_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_csharp_test_item(n, source))
}

fn is_csharp_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    n.kind() == "method_declaration"
        && has_csharp_attribute(n, source, &["Test", "TestMethod", "Fact", "Theory"])
}

/// C# のアトリビュート（`[Test]` 等）の存在チェック。
fn has_csharp_attribute(node: tree_sitter::Node, source: &[u8], names: &[&str]) -> bool {
    // attribute_list が method_declaration の子ノードとして存在
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list"
            && let Ok(text) = child.utf8_text(source)
        {
            for name in names {
                if text.contains(name) {
                    return true;
                }
            }
        }
    }
    false
}

/// Ruby: `describe`/`it` call 内 / `test_*` メソッド名
fn is_ruby_test_context(
    root: tree_sitter::Node,
    source: &[u8],
    symbol_range: &crate::models::location::Range,
) -> bool {
    any_ancestor(root, symbol_range, |n| is_ruby_test_item(n, source))
}

fn is_ruby_test_item(n: tree_sitter::Node, source: &[u8]) -> bool {
    // RSpec: describe/it/before/after call
    if n.kind() == "call"
        && let Some(method) = n.child_by_field_name("method")
        && let Ok(name) = method.utf8_text(source)
        && matches!(name, "describe" | "context" | "it" | "before" | "after")
    {
        return true;
    }
    // Minitest: test_* メソッド
    n.kind() == "method"
        && n.child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .is_some_and(|text| text.starts_with("test_") || text == "setup" || text == "teardown")
}

/// ノードの前に指定テキストを含む attribute_item 兄弟が存在するか確認する。
pub(crate) fn has_attribute_text(node: tree_sitter::Node, source: &[u8], pattern: &str) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(p) = prev {
        match p.kind() {
            "attribute_item" => {
                if let Ok(text) = p.utf8_text(source)
                    && text.contains(pattern)
                {
                    return true;
                }
                prev = p.prev_named_sibling();
            }
            "line_comment" | "block_comment" => {
                prev = p.prev_named_sibling();
            }
            _ => break,
        }
    }
    false
}

/// `LangId` が AST ベースのテストコンテキスト判定に対応しているか。
/// 非対応言語ではファイル内容を読まずに即 false を返すことで、
/// 大規模な PHP/HTML/CSS リポジトリで Tree/mmap が溜まるのを防ぐ。
fn supports_ast_test_context(lang_id: LangId) -> bool {
    matches!(
        lang_id,
        LangId::Rust
            | LangId::Python
            | LangId::Javascript
            | LangId::Typescript
            | LangId::Tsx
            | LangId::Java
            | LangId::Kotlin
            | LangId::CSharp
            | LangId::Ruby
    )
}

/// ターゲットファイルの指定行/列の参照がテストコンテキスト内にあるか確認する。
///
/// 3 段階で判定し、必要最小限のファイルだけをパースする:
///   1. パス判定（ファイル読み込み不要、全言語共通）
///   2. 拡張子から言語を判定し、AST 判定非対応の言語は即 false（PHP 等）
///   3. 対応言語のみパースし、再パースを避けるため LRU キャッシュに保持する
///
/// LRU 上限でキャッシュを有界にすることで、数千〜数万ファイル級のリポジトリでも
/// Tree + SourceBuf が残留しても RSS ピークが定数に収束する。
pub(crate) fn is_ref_in_target_test_context(
    path: &str,
    line: usize,
    column: usize,
    cache: &mut LruCache<String, Option<ParsedFile>>,
) -> bool {
    // 1. パスベース判定（ファイル読み込み不要）
    if is_test_file_path(path) {
        return true;
    }

    // 2. 言語判定: AST 判定非対応なら parse せずに即 false
    let utf8_path = Utf8Path::new(path);
    let Ok(lang_id) = LangId::from_path(utf8_path) else {
        return false;
    };
    if !supports_ast_test_context(lang_id) {
        return false;
    }

    // 3. 対応言語のみ parse して LRU にキャッシュ
    if cache.get(path).is_none() {
        let parsed = parser::read_file(utf8_path).ok().and_then(|source| {
            let (tree, parsed_lang_id) = parser::parse_file(utf8_path, &source).ok()?;
            // mmap 経路を維持するため SourceBuf をそのままキャッシュする
            Some((tree, source, parsed_lang_id))
        });
        cache.put(path.to_string(), parsed);
    }

    let Some(Some((tree, source, lang_id))) = cache.get(path) else {
        return false;
    };

    let range = crate::models::location::Range {
        start: crate::models::location::Point { line, column },
        end: crate::models::location::Point { line, column },
    };

    is_in_test_context(tree.root_node(), source.as_bytes(), &range, *lang_id, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Go のテストファイルパターン (*_test.go) を検出する
    #[test]
    fn is_test_file_path_go() {
        assert!(is_test_file_path("pkg/handler_test.go"));
    }

    // JS/TS の spec ファイルパターン (*.spec.ts) を検出する
    #[test]
    fn is_test_file_path_js_spec() {
        assert!(is_test_file_path("src/app.spec.ts"));
    }

    // Python のテストファイルパターン (test_*.py) を検出する
    #[test]
    fn is_test_file_path_python() {
        assert!(is_test_file_path("tests/test_main.py"));
    }

    // Ruby の spec ファイルパターン (*_spec.rb) を検出する
    #[test]
    fn is_test_file_path_ruby() {
        assert!(is_test_file_path("spec/model_spec.rb"));
    }

    // 通常のソースファイルはテストファイルと判定されない
    #[test]
    fn is_test_file_path_normal() {
        assert!(!is_test_file_path("src/main.rs"));
        assert!(!is_test_file_path("lib/handler.py"));
    }

    /// 修正前の祖先の辿り方 (`Node::parent` を 1 段ずつ。浅い入力で比べるためだけに残す)。
    fn parent_walk(node: tree_sitter::Node) -> Vec<tree_sitter::Node> {
        let mut chain = Vec::new();
        let mut current = Some(node);
        while let Some(n) = current {
            chain.push(n);
            current = n.parent();
        }
        chain
    }

    fn all_nodes(root: tree_sitter::Node) -> Vec<tree_sitter::Node> {
        let mut cursor = root.walk();
        let mut nodes = vec![root];
        let mut next = 0;
        while next < nodes.len() {
            let node = nodes[next];
            nodes.extend(node.children(&mut cursor));
            next += 1;
        }
        nodes
    }

    fn range_of(node: tree_sitter::Node) -> crate::models::location::Range {
        let point = |p: tree_sitter::Point| crate::models::location::Point {
            line: p.row,
            column: p.column,
        };
        crate::models::location::Range {
            start: point(node.start_position()),
            end: point(node.end_position()),
        }
    }

    type TestItem = fn(tree_sitter::Node, &[u8]) -> bool;

    fn samples() -> Vec<(LangId, &'static str, TestItem)> {
        vec![
            (
                LangId::Rust,
                "fn prod() { helper(); }\n#[test]\nfn it_works() { let x = helper(); }\n#[cfg(test)]\nmod tests {\n    // c\n    fn util() { if true { helper() } else { 2 } }\n}\nfn broken() { let y = (1 + ; }\n",
                is_rust_test_item,
            ),
            (
                LangId::Python,
                "def prod():\n    helper()\n\ndef test_one():\n    helper()\n\nclass T(unittest.TestCase):\n    def run(self):\n        return [helper(x) for x in y]\n\n@pytest.fixture\ndef fx():\n    return helper(\n",
                is_python_test_item,
            ),
            (
                LangId::Javascript,
                "function prod() { helper(); }\ndescribe('x', () => { it('y', () => { helper(1, [2]); }); });\nconst v = g(h(helper(\n",
                is_js_test_item,
            ),
            (
                LangId::Typescript,
                "export function prod(): void { helper(); }\ntest('t', async () => { await helper<number>(1); });\n",
                is_js_test_item,
            ),
            (
                LangId::Tsx,
                "export const C = () => <div>{helper()}</div>;\nit('r', () => { render(<C />); });\n",
                is_js_test_item,
            ),
            (
                LangId::Java,
                "class A {\n  void prod() { helper(); }\n  @Test\n  void t() { helper(); }\n  A() { helper(; }\n}\n",
                is_jvm_test_item,
            ),
            (
                LangId::Kotlin,
                "class A {\n  fun prod() { helper() }\n  @Test fun t() { helper() }\n}\n",
                is_jvm_test_item,
            ),
            (
                LangId::CSharp,
                "class A {\n  void Prod() { Helper(); }\n  [Fact]\n  public void T() { Helper(); }\n}\n",
                is_csharp_test_item,
            ),
            (
                LangId::Ruby,
                "def prod\n  helper\nend\ndescribe 'x' do\n  it 'y' do\n    helper(1)\n  end\nend\ndef test_one\n  helper\nend\n",
                is_ruby_test_item,
            ),
        ]
    }

    /// 根から 1 回だけ降りて作る祖先の列が、修正前の `Node::parent` を 1 段ずつ
    /// たどった列と、浅い入力の全ノード (無名・幅 0・ERROR を含む) で一致する。
    /// 各言語のテスト判定は祖先の列への述語なので、判定結果も修正前と一致する。
    #[test]
    fn ancestors_and_test_context_match_parent_walk() {
        for (lang, source, is_test_item) in samples() {
            let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
            let root = tree.root_node();
            let mut fast_paths = 0;
            let mut test_ranges = 0;
            for node in all_nodes(root) {
                let ids = |chain: Vec<tree_sitter::Node>| {
                    chain.iter().map(|n| n.id()).collect::<Vec<_>>()
                };
                assert_eq!(
                    ids(ancestors_inclusive(root, node)),
                    ids(parent_walk(node)),
                    "{lang:?} {} {:?}",
                    node.kind(),
                    node.byte_range()
                );
                fast_paths += usize::from(path_from_root(root, node).is_some());

                let range = range_of(node);
                let expected = descendant_for_range(root, &range).is_some_and(|target| {
                    parent_walk(target)
                        .into_iter()
                        .any(|n| is_test_item(n, source.as_bytes()))
                });
                assert_eq!(
                    any_ancestor(root, &range, |n| is_test_item(n, source.as_bytes())),
                    expected,
                    "{lang:?} {} {:?}",
                    node.kind(),
                    node.byte_range()
                );
                test_ranges += usize::from(expected);
            }
            assert!(fast_paths > 0, "{lang:?}: 根から降りる経路を通る");
            assert!(test_ranges > 0, "{lang:?}: テスト内と判定される範囲を含む");
        }
    }

    /// 深い入れ子の中の参照も、深さに比例する時間で判定できる。修正前は祖先 1 段ごとに
    /// `Node::parent` が根から辿り直すため、4 万段の use ツリーで数分かかっていた。
    #[test]
    fn deep_reference_is_judged_within_deadline() {
        let handle = std::thread::spawn(|| {
            let depth = 40_000;
            let deep_use = format!(
                "use crate::{{{}f{}}};\n",
                "m::{".repeat(depth),
                "}".repeat(depth)
            );
            let mut results = Vec::new();
            for (lang, source, path) in [
                (
                    LangId::Rust,
                    format!("fn prod() {{}}\n{deep_use}"),
                    "src/c.rs",
                ),
                (
                    LangId::Rust,
                    format!("#[cfg(test)]\nmod tests {{\n{deep_use}}}\n"),
                    "src/c.rs",
                ),
                (
                    LangId::Javascript,
                    format!(
                        "describe('d', () => {{ v = {}f(1){}; }});\n",
                        "g(".repeat(depth),
                        ")".repeat(depth)
                    ),
                    "src/c.js",
                ),
            ] {
                let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
                let root = tree.root_node();
                let offset = source.find("f}").or_else(|| source.find("f(1)")).unwrap();
                let line = source[..offset].matches('\n').count();
                let column = offset - source[..offset].rfind('\n').map_or(0, |i| i + 1);
                let point = crate::models::location::Point { line, column };
                let range = crate::models::location::Range {
                    start: point,
                    end: point,
                };
                let target = descendant_for_range(root, &range).unwrap();
                assert!(
                    path_from_root(root, target).is_some(),
                    "{lang:?}: 根から降りる"
                );
                results.push(is_in_test_context(
                    root,
                    source.as_bytes(),
                    &range,
                    lang,
                    path,
                ));
            }
            results
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !handle.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "深い入力の判定が 60 秒で終わらない"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(handle.join().unwrap(), vec![false, true, true]);
    }
}
