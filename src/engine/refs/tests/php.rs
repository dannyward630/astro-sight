use super::*;

/// PHP の callable array `[Class::class, 'method']` で string ノードの中身が
/// method ref として返されることを検証 (N3 unit-level)。
#[test]
fn php_callable_array_method_segment_extracts_method_string() {
    let source =
        b"<?php\nclass C {\n    public function h() { $x = [C::class, 'foo']; return $x; }\n}\n";
    let path = camino::Utf8Path::new("dummy.php");
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    let lang_id = LangId::Php;
    let _ = path; // silence unused warning
    // 再帰で array_creation_expression を探す
    fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "array_creation_expression" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_array(child) {
                return Some(found);
            }
        }
        None
    }
    let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
    let seg = php_callable_array_method_segment(arr, source, lang_id);
    assert!(
        seg.is_some(),
        "[C::class, 'foo'] should yield a method segment, got None"
    );
    let (m, _row, _col, confidence) = seg.unwrap();
    assert_eq!(
        confidence,
        crate::models::reference::RefConfidence::ExactOwner
    );
    assert_eq!(m, "foo");
}

/// 第1要素が callable の対象になれないリテラルの場合は ref として認めない (誤検出防止)
#[test]
fn php_callable_array_method_segment_rejects_non_class_const() {
    // [1, 'foo'] や ['foo', 'bar'] は callable array ではない
    let source = b"<?php\nfunction f() { $x = [1, 'foo']; return $x; }\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    let lang_id = LangId::Php;
    fn find_array<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "array_creation_expression" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_array(child) {
                return Some(found);
            }
        }
        None
    }
    let arr = find_array(tree.root_node()).expect("array_creation_expression must exist");
    assert!(php_callable_array_method_segment(arr, source, lang_id).is_none());
}

/// PHP 文字列 callable `'Cls@method'` 形式で method 部分が抽出されることを検証 (N4)。
#[test]
fn php_string_callable_method_segment_extracts_pure_string() {
    let source = b"<?php\nfunction f() { return 'Controller@handle'; }\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(
        n: tree_sitter::Node<'t>,
        target: &str,
        source: &[u8],
    ) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child, target, source) {
                return Some(found);
            }
        }
        None
    }
    let s =
        find_string(tree.root_node(), "'Controller@handle'", source).expect("string must exist");
    let seg = php_string_callable_method_segment(s, source, LangId::Php);
    assert!(
        seg.is_some(),
        "'Controller@handle' should yield method segment"
    );
    let (m, _r, _c) = seg.unwrap();
    assert_eq!(m, "handle");
}

/// PHP `Cls::class . '@method'` concat 右辺 string から method 部分が抽出されることを検証 (N4)。
#[test]
fn php_string_callable_method_segment_extracts_concat_segment() {
    let source = b"<?php\nclass C {}\n$x = C::class . '@handler';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(
        n: tree_sitter::Node<'t>,
        target: &str,
        source: &[u8],
    ) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" && n.utf8_text(source).ok() == Some(target) {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child, target, source) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node(), "'@handler'", source).expect("string must exist");
    let seg = php_string_callable_method_segment(s, source, LangId::Php);
    assert!(seg.is_some(), "Cls::class . '@handler' should match");
    let (m, _r, _c) = seg.unwrap();
    assert_eq!(m, "handler");
}

/// メール風文字列は method ref として抽出しない (誤検出防止)
#[test]
fn php_string_callable_method_segment_rejects_email_like() {
    let source = b"<?php\n$x = 'user@example.com';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// `P@ssw0rd` のようなパスワード風文字列は class 部分が 1 文字で reject される
#[test]
fn php_string_callable_method_segment_rejects_short_class_part() {
    let source = b"<?php\n$x = 'P@ssw0rd';\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// 引数単独の `'@method'` (concat 親ではない) は reject
#[test]
fn php_string_callable_method_segment_rejects_bare_at_method() {
    let source = b"<?php\nfunction f($x) {} f('@handler');\n";
    let tree = parser::parse_source(source, LangId::Php).unwrap();
    fn find_string<'t>(n: tree_sitter::Node<'t>) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "string" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_string(child) {
                return Some(found);
            }
        }
        None
    }
    let s = find_string(tree.root_node()).expect("string must exist");
    assert!(php_string_callable_method_segment(s, source, LangId::Php).is_none());
}

/// PHP のメソッド呼び出しは case-insensitive に解決される。
/// 定義 `isFooBar` と呼び出し `isFoobar` (case 違い) は同一メソッドに解決される。
#[test]
fn find_references_php_method_call_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Vo.php"),
        "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Caller.php"),
        "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
    )
    .unwrap();

    let refs = find_references("isFooBar", dir.path(), None).unwrap();
    let defs = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .count();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(defs, 1, "definition should resolve, got refs={refs:?}");
    assert_eq!(
        non_defs, 1,
        "case-different method call must resolve as reference, got refs={refs:?}"
    );
}

/// 追加ファイルなし (`extra_files` 空) の場合は通常の workspace walk と同じ件数になる。
#[test]
fn count_non_definition_refs_split_without_extra_files_keeps_existing_api() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("sample.rs"),
        "pub fn helper() {}\nfn run() { helper(); }\n",
    )
    .unwrap();

    let counts = count_non_definition_refs_split_with_extra_files(
        &["helper".to_string()],
        dir.path(),
        None,
        &[],
        |_| false,
    )
    .unwrap()
    .counts;

    assert_eq!(counts.get("helper"), Some(&(1, 0)));
}

/// PHP の静的メソッド呼び出し (`Foo::bar()`) も case-insensitive に解決される。
#[test]
fn find_references_php_static_method_call_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass Svc {\n    public static function doIt(): void {}\n    public function run(): void { Svc::DOIT(); }\n}\n",
    )
    .unwrap();

    let refs = find_references("doIt", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 1,
        "case-different static call must resolve, got refs={refs:?}"
    );
}

/// PHP のクラス名は case-insensitive。`new FOO()` / `new Foo()` が定義 `class Foo` に解決される。
#[test]
fn find_references_php_class_name_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Foo.php"),
        "<?php\nclass Foo {\n    public function go(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("use.php"),
        "<?php\nfunction make() {\n    $a = new FOO();\n    $b = new Foo();\n    return $a;\n}\n",
    )
    .unwrap();

    let refs = find_references("Foo", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 2,
        "class name `new FOO`/`new Foo` must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP のプロパティ名は case-sensitive。大小違いの検索は member_access に一致しない。
#[test]
fn find_references_php_property_access_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass C {\n    public int $myProp = 0;\n    public function f(): int { return $this->myProp; }\n}\n",
    )
    .unwrap();

    // case-fold していれば "MYPROP" が "myProp" に誤マッチするが、プロパティは case-sensitive。
    let refs = find_references("MYPROP", dir.path(), None).unwrap();
    assert!(
        refs.is_empty(),
        "property access is case-sensitive; uppercase search must not match, got refs={refs:?}"
    );
}

/// PHP のクラス定数は case-sensitive。大小違いの検索は class_constant_access に一致しない。
#[test]
fn find_references_php_class_constant_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nclass C {\n    const MyConst = 1;\n    public function f(): int { return self::MyConst; }\n}\n",
    )
    .unwrap();

    let refs = find_references("MYCONST", dir.path(), None).unwrap();
    assert!(
        refs.is_empty(),
        "class constant is case-sensitive; uppercase search must not match, got refs={refs:?}"
    );
}

/// バッチ参照検索 (dead-code / api が使う経路) でも PHP メソッドの case 違いが解決される。
#[test]
fn find_references_batch_php_method_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Vo.php"),
        "<?php\nclass Vo {\n    public function isFooBar(): bool { return true; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Caller.php"),
        "<?php\nclass Caller {\n    public function check(Vo $vo): bool { return $vo->isFoobar(); }\n}\n",
    )
    .unwrap();

    let map = find_references_batch(&["isFooBar".to_string()], dir.path(), None).unwrap();
    let refs = map.get("isFooBar").cloned().unwrap_or_default();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert_eq!(
        non_defs, 1,
        "batch path must resolve case-different call, got refs={refs:?}"
    );
}

/// PHP の名前空間付きクラス参照 (`use App\Foo` / 型ヒント / `new \App\Foo()`) も
/// case-insensitive に解決される (qualified_name の末尾 name)。
#[test]
fn find_references_php_namespaced_class_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Foo.php"),
        "<?php\nnamespace App\\Repo;\nclass UserRepository {\n    public function go(): void {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("use.php"),
        "<?php\nuse App\\Repo\\USERREPOSITORY;\nfunction make(\\App\\Repo\\Userrepository $r) {\n    return new \\App\\Repo\\userRepository();\n}\n",
    )
    .unwrap();

    // 参照: use の USERREPOSITORY / 型ヒント Userrepository / new userRepository (全て case 違い)。
    let refs = find_references("UserRepository", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert!(
        non_defs >= 3,
        "namespaced class refs must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP の trait adaptation (`insteadof` / `as`) のメソッド名も case-insensitive に解決される。
#[test]
fn find_references_php_trait_adaptation_method_case_insensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("t.php"),
        "<?php\ntrait A {\n    public function work(): void {}\n}\ntrait B {\n    public function work(): void {}\n}\nclass C {\n    use A, B {\n        B::WORK insteadof A;\n        A::Work as legacyWork;\n    }\n}\n",
    )
    .unwrap();

    // B::WORK (insteadof) と A::Work (as) は trait メソッド work の case 違い参照。
    let refs = find_references("work", dir.path(), None).unwrap();
    let non_defs = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .count();
    assert!(
        non_defs >= 2,
        "trait adaptation method refs must resolve case-insensitively, got refs={refs:?}"
    );
}

/// PHP の `use const` (定数 import) は case-sensitive、`use` (クラス import) は case-insensitive。
#[test]
fn find_references_php_use_const_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nnamespace App;\nuse const App\\Config\\MAX_SIZE;\nuse App\\Repo\\UserRepo;\n",
    )
    .unwrap();

    // use const の定数名は case-sensitive (大小違いは一致しない)。
    assert!(
        find_references("max_size", dir.path(), None)
            .unwrap()
            .is_empty(),
        "use const must be case-sensitive"
    );
    // 対照: use (クラス import) は case-insensitive。
    assert!(
        !find_references("USERREPO", dir.path(), None)
            .unwrap()
            .is_empty(),
        "use class import must be case-insensitive"
    );
}

/// PHP の group use 内でも const は case-sensitive、クラス / 関数は case-insensitive。
#[test]
fn find_references_php_group_use_const_is_case_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.php"),
        "<?php\nnamespace App;\nuse App\\{ Foo, const MAX_LEN, function helper };\n",
    )
    .unwrap();

    assert!(
        find_references("max_len", dir.path(), None)
            .unwrap()
            .is_empty(),
        "group use const must be case-sensitive"
    );
    assert!(
        !find_references("FOO", dir.path(), None).unwrap().is_empty(),
        "group use class must be case-insensitive"
    );
    assert!(
        !find_references("HELPER", dir.path(), None)
            .unwrap()
            .is_empty(),
        "group use function must be case-insensitive"
    );
}

/// PHP の batch 経路 (IndexedMatcher) が single 経路 (SingleMatcher) と完全に一致すること。
///
/// 旧実装は `build_name_to_ix` が case-insensitive 用の折りたたみキー (小文字) と原文キーを
/// **同じ map** に混在させていたため、case-sensitive 文脈 (`$this->foo` のプロパティ名) の
/// lookup がシンボル `Foo` の折りたたみキーへ誤ヒットしていた。single 経路は
/// `php_name_is_case_insensitive(node)` を都度見るので正しく、同じ入力で結果が食い違っていた。
/// `find_references_batch` は `ApiRefIndex` の土台なので、汚染は api.rm / api.mod /
/// refs_internal / dead-code (class が live に化ける fail-open) まで波及する。
///
/// 対照ケースを同一 fixture に内蔵する: case-insensitive 文脈 (`FOO::bar()`) では
/// 大小違いのシンボルへ**引き続き**ヒットしなければならない (抑制しすぎない)。
#[test]
fn php_batch_refs_match_single_refs_across_case_domains() {
    // `Foo` (クラス) と `foo` (関数/プロパティ) が同居し、case-sensitive 文脈と
    // case-insensitive 文脈の両方から参照される fixture。
    let source: &[u8] = b"<?php\n\
class Foo {\n\
    public $foo = 1;\n\
    public function bar() { return $this->foo; }\n\
}\n\
function foo(): int { return 2; }\n\
$Foo = new Foo();\n\
$foo = FOO::bar();\n\
echo foo();\n";
    let lang_id = LangId::Php;
    let tree = parser::parse_source(source, lang_id).unwrap();
    let root = tree.root_node();
    let defs = definition_node_kinds(lang_id);
    let names = vec!["Foo".to_string(), "foo".to_string()];

    let batch = collect_batch_refs_for_test(root, source, &names, "t.php", defs, lang_id);
    for (ix, name) in names.iter().enumerate() {
        let single = collect_single_refs_for_test(root, source, name, "t.php", defs, lang_id);
        assert_eq!(
            ref_fingerprints(&single),
            ref_fingerprints(&batch[ix]),
            "single と batch は同一入力に対して同一結果を返すこと (name={name})"
        );
    }

    // 症状の直接固定: `$this->foo` (プロパティ = case-sensitive) の行は `Foo` の参照でない。
    // ここは修正前に batch 側だけがヒットしていた。
    let this_foo_line = 3; // 0-origin: `public function bar() { return $this->foo; }`
    assert!(
        !batch[0].iter().any(|r| r.line == this_foo_line),
        "case-sensitive なプロパティ `$this->foo` を `Foo` の参照に数えないこと: {:?}",
        batch[0]
    );
    // 対照: case-insensitive なクラス参照 `FOO::bar()` は大小違いでも `Foo` にヒットする。
    let foo_scope_line = 7; // 0-origin: `$foo = FOO::bar();`
    assert!(
        batch[0].iter().any(|r| r.line == foo_scope_line),
        "case-insensitive な `FOO::bar()` は `Foo` の参照として残ること: {:?}",
        batch[0]
    );
    // 対照: プロパティ `$this->foo` はシンボル `foo` 側では引き続き参照として数える。
    assert!(
        batch[1].iter().any(|r| r.line == this_foo_line),
        "`$this->foo` はシンボル `foo` の参照として数えること: {:?}",
        batch[1]
    );
}

/// `instanceof` の右辺と属性 `#[...]` のクラス名も、ほかのクラス名の文脈 (型ヒント・`new`・
/// `catch`) と同じく大文字小文字を無視して照合する。
///
/// 旧実装は許可リストにこの 2 つの文脈が無く完全一致に落ちていたため、`$a instanceof widget`
/// や `#[TRACKED]` を `refs --name Widget` / `Tracked` が返さず、`instanceof gadget` からだけ
/// 使う `class Gadget` を dead と報告していた。
///
/// **対照ケースを内蔵する**: `$a + WIDGET` の右辺 (定数、case-sensitive) と、属性の名前付き
/// 引数 (`name: 'x'`) は完全一致のまま。単一検索とバッチ検索は同じ結果を返す。
#[test]
fn php_instanceof_and_attribute_class_names_are_case_insensitive() {
    let source: &[u8] = b"<?php\n\
class Widget {}\n\
#[\\Attribute] class Tracked {}\n\
$d1 = $a instanceof Widget;\n\
$d2 = $a instanceof widget;\n\
$d3 = $a instanceof \\App\\WIDGET;\n\
$c = $a + WIDGET;\n\
#[TRACKED]\n\
class UsesAttr {}\n\
#[\\App\\tracked(name: 'widget')]\n\
class UsesAttr2 {}\n";
    let lang_id = LangId::Php;
    let tree = parser::parse_source(source, lang_id).unwrap();
    let root = tree.root_node();
    let defs = definition_node_kinds(lang_id);
    let names = vec!["Widget".to_string(), "Tracked".to_string()];
    let batch = collect_batch_refs_for_test(root, source, &names, "t.php", defs, lang_id);
    for (ix, name) in names.iter().enumerate() {
        let single = collect_single_refs_for_test(root, source, name, "t.php", defs, lang_id);
        assert_eq!(
            ref_fingerprints(&single),
            ref_fingerprints(&batch[ix]),
            "single と batch は同じ結果 (name={name})"
        );
    }
    let lines = |refs: &[SymbolReference]| -> Vec<usize> {
        let mut out: Vec<usize> = refs.iter().map(|r| r.line).collect();
        out.sort_unstable();
        out
    };
    // Widget: 定義 (1)・instanceof の 3 形 (3, 4, 5)。`$a + WIDGET` (6) の定数と
    // 名前付き引数の値 'widget' (9) は数えない。
    assert_eq!(lines(&batch[0]), vec![1, 3, 4, 5], "{:?}", batch[0]);
    // Tracked: 定義 (2)・`#[TRACKED]` (7)・`#[\App\tracked(...)]` (9)。
    assert_eq!(lines(&batch[1]), vec![2, 7, 9], "{:?}", batch[1]);
}

/// PHP 8 の nullsafe 呼び出し `$g?->Fmtx()` のメソッド名も case-insensitive に照合する。
///
/// `php_ref_context_is_case_insensitive` が `member_call_expression` しか見ておらず、
/// `nullsafe_member_call_expression` の name は厳密一致に落ちていたため、定義 `Fmtx` に対する
/// `refs --name fmtx` が `$g?->Fmtx()` を返さなかった (`->` 版は返す非対称)。
/// 対照: nullsafe のプロパティアクセス `$g?->myProp` は `->` 版と同じく case-sensitive のまま。
#[test]
fn find_references_php_nullsafe_method_call_case_insensitive() {
    let source: &[u8] = b"<?php\n\
class Gamma {\n\
    public $myProp = 1;\n\
    public function Fmtx() { return 1; }\n\
}\n\
function run(?Gamma $g) {\n\
    $g?->fmtx();\n\
    $g->FMTX();\n\
    return $g?->myProp;\n\
}\n";
    let lang_id = LangId::Php;
    let tree = parser::parse_source(source, lang_id).unwrap();
    let root = tree.root_node();
    let defs = definition_node_kinds(lang_id);

    let refs = collect_single_refs_for_test(root, source, "Fmtx", "t.php", defs, lang_id);
    let ref_lines: Vec<usize> = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Reference))
        .map(|r| r.line)
        .collect();
    assert_eq!(
        ref_lines,
        [6, 7],
        "nullsafe 呼び出し (6 行目) と通常呼び出し (7 行目) の両方を大小無視で返すこと: {refs:?}"
    );

    // 対照: プロパティは case-sensitive なので大小違いの検索に一致しない
    let props = collect_single_refs_for_test(root, source, "MYPROP", "t.php", defs, lang_id);
    assert!(
        props.is_empty(),
        "nullsafe のプロパティアクセスは case-sensitive のまま: {props:?}"
    );

    // single と batch が同じ結果を返すこと
    let names = vec!["Fmtx".to_string(), "MYPROP".to_string()];
    let batch = collect_batch_refs_for_test(root, source, &names, "t.php", defs, lang_id);
    assert_eq!(ref_fingerprints(&refs), ref_fingerprints(&batch[0]));
    assert_eq!(ref_fingerprints(&props), ref_fingerprints(&batch[1]));
}

/// 拡張子なしの PHP スクリプト (`bin/console` の shebang) でも single と batch が一致すること。
///
/// single 経路の事前フィルタが拡張子だけで言語を決めていたため、shebang で PHP と判定される
/// ファイルに大小区別の memmem が掛かり、大小違いの関数呼び出し `Bootstrap_App()` を
/// `refs --name` だけが落としていた (batch の AC は常に ASCII CI なので返す)。
/// 対照: 拡張子 `.php` のファイルは従来どおり両経路で返る。
#[test]
fn shebang_php_file_single_and_batch_agree_on_case_insensitive_call() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("bin")).unwrap();
    std::fs::write(
        dir.path().join("boot.php"),
        "<?php\nfunction bootstrap_app() { return 1; }\nBOOTSTRAP_APP();\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("bin/console"),
        "#!/usr/bin/env php\n<?php\nBootstrap_App();\n",
    )
    .unwrap();

    let single = find_references("bootstrap_app", dir.path(), None).unwrap();
    let batch = find_references_batch(&["bootstrap_app".to_string()], dir.path(), None)
        .unwrap()
        .remove("bootstrap_app")
        .unwrap_or_default();
    let key = |refs: &[SymbolReference]| -> Vec<(String, usize, usize)> {
        refs.iter()
            .map(|r| (r.path.clone(), r.line, r.column))
            .collect()
    };
    assert_eq!(
        key(&single),
        key(&batch),
        "single と batch は同じ参照集合を返すこと"
    );
    assert!(
        single
            .iter()
            .any(|r| r.path.ends_with("console") && r.line == 2),
        "shebang で PHP と判定されるファイルの大小違い呼び出しを返すこと: {single:?}"
    );
    assert!(
        single
            .iter()
            .any(|r| r.path.ends_with("boot.php") && r.line == 2),
        "対照: .php の大小違い呼び出しは従来どおり返ること: {single:?}"
    );
}

/// `$x->save()` の名前ノードを前順で `nth` 番目から取り出す。
fn php_member_call_name_nodes<'t>(
    root: tree_sitter::Node<'t>,
    source: &[u8],
    method: &str,
) -> Vec<tree_sitter::Node<'t>> {
    let mut out = Vec::new();
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::All,
        |node, _| {
            if node.kind() == "member_call_expression"
                && let Some(name) = node.child_by_field_name("name")
                && name.utf8_text(source) == Ok(method)
            {
                out.push(name);
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    out
}

/// 関数自身の型付き引数で受けた `$repo->save()` は InferredOwner になる。
///
/// 型の手掛かりを関数の本体の中だけで探していたため、本体の外にある引数リストを見ず、
/// `function f(Repo $repo) { $repo->save(); }` が常に BareNameOnly になっていた。
/// impact は BareNameOnly かつ汎用名 (`save` / `find` など) の参照を低確信度に回すので、
/// 呼び出し元が hook の blocking から外れていた。
///
/// **対照ケースを内蔵する**: 型の無い引数・別名の型付き引数は従来どおり BareNameOnly、
/// `new` の代入と `@var` コメントは従来どおり InferredOwner。
#[test]
fn php_typed_parameter_receiver_is_inferred_owner() {
    use crate::models::reference::RefConfidence;
    let cases: &[(&str, &str, RefConfidence)] = &[
        (
            "typed function parameter",
            "<?php\nfunction persistParam(Repo $repo) { return $repo->save([]); }\n",
            RefConfidence::InferredOwner,
        ),
        (
            "typed method parameter",
            "<?php\nclass S { public function m(Repo $repo) { return $repo->save([]); } }\n",
            RefConfidence::InferredOwner,
        ),
        (
            "typed arrow function parameter",
            "<?php\n$f = fn(Repo $repo) => $repo->save([]);\n",
            RefConfidence::InferredOwner,
        ),
        (
            "typed closure parameter",
            "<?php\n$g = function (Repo $repo) use ($x) { return $repo->save([]); };\n",
            RefConfidence::InferredOwner,
        ),
        (
            "promoted constructor parameter",
            "<?php\nclass S { public function __construct(private Repo $repo) { $repo->save([]); } }\n",
            RefConfidence::InferredOwner,
        ),
        (
            "untyped parameter (control)",
            "<?php\nfunction persist($repo) { return $repo->save([]); }\n",
            RefConfidence::BareNameOnly,
        ),
        (
            "typed parameter with another name (control)",
            "<?php\nfunction persist(Repo $other, $repo) { return $repo->save([]); }\n",
            RefConfidence::BareNameOnly,
        ),
        (
            "outer typed parameter is not the closure's (control)",
            "<?php\nfunction outer(Repo $repo) { return function () use ($repo) { return $repo->save([]); }; }\n",
            RefConfidence::BareNameOnly,
        ),
        (
            "new assignment (control)",
            "<?php\nfunction persist() { $repo = new Repo(); return $repo->save([]); }\n",
            RefConfidence::InferredOwner,
        ),
        (
            "@var comment (control)",
            "<?php\nfunction persist($repo) {\n  /** @var Repo $repo */\n  return $repo->save([]);\n}\n",
            RefConfidence::InferredOwner,
        ),
    ];
    for (label, source, expected) in cases {
        let tree = parser::parse_source(source.as_bytes(), LangId::Php).unwrap();
        let names = php_member_call_name_nodes(tree.root_node(), source.as_bytes(), "save");
        assert_eq!(names.len(), 1, "{label}");
        assert_eq!(
            classify_method_ref_confidence(names[0], source.as_bytes(), LangId::Php, false),
            *expected,
            "{label}"
        );
    }
}

/// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
fn php_scope_has_inferable_var_type_recursive(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    var_marker: &str,
) -> bool {
    match node.kind() {
        "simple_parameter" | "property_promotion_parameter" => {
            let has_type = node.child_by_field_name("type").is_some();
            let name_match = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            if has_type && name_match {
                return true;
            }
        }
        "assignment_expression" => {
            let lhs_match = node
                .child_by_field_name("left")
                .and_then(|n| n.utf8_text(source).ok())
                .is_some_and(|t| t == var_marker);
            let rhs_is_object_creation = node
                .child_by_field_name("right")
                .is_some_and(|n| n.kind() == "object_creation_expression");
            if lhs_match && rhs_is_object_creation {
                return true;
            }
        }
        "comment" => {
            if let Ok(text) = node.utf8_text(source)
                && text.contains("@var")
                && text.contains(var_marker)
            {
                return true;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if php_scope_has_inferable_var_type_recursive(child, source, var_marker) {
            return true;
        }
    }
    false
}

/// 型推論材料の探索は、どのノードを根にしても反復版と再帰版で一致する。
#[test]
fn php_scope_has_inferable_var_type_matches_recursive_version() {
    let source = "<?php\nclass S {\n  public function __construct(private Repo $p) {}\n  public function m(Repo $a, $b) {\n    /** @var Repo $c */\n    $d = new Repo();\n    $e = make();\n    $f = fn(Repo $g) => $g->save([]);\n    return [$a, $b, $c, $d, $e, $f];\n  }\n}\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Php).unwrap();
    let markers = ["$a", "$b", "$c", "$d", "$e", "$f", "$g", "$p", "$missing"];
    let mut checked = 0;
    let mut found = 0;
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        tree.root_node(),
        crate::engine::tree_walk::Children::All,
        |node, _| {
            for marker in markers {
                let expected =
                    php_scope_has_inferable_var_type_recursive(node, source.as_bytes(), marker);
                assert_eq!(
                    php_scope_has_inferable_var_type(node, source.as_bytes(), marker),
                    expected,
                    "{marker} under {}",
                    node.kind()
                );
                found += usize::from(expected);
            }
            checked += 1;
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    assert!(checked > 30 && found > 0, "checked={checked} found={found}");
}

/// callable 配列 `[<target>, 'method']` と静的 callable 文字列 `'Class::method'` の
/// `method` を参照として返す。
///
/// 旧実装は第1要素が `X::class` の配列と `Class@method` の文字列しか受け付けず、
/// `add_action('init', [$this, 'init'])` / `usort($a, ['Plugin', 'cmp'])` /
/// `[new Plugin(), 'm']` / `call_user_func('Plugin::m')` でしか渡さないメソッドを
/// dead と報告していた。
///
/// **対照ケースを内蔵する**: 数値・真偽値・null・配列を第1要素に持つ配列、クラス名の形で
/// ない文字列 (`['required', 'email']`)、キー付き・3 要素の配列、識別子の形でない第2要素は
/// 従来どおり参照にしない。
#[test]
fn php_callable_forms_are_method_refs() {
    use crate::models::reference::RefConfidence::{BareNameOnly, ExactOwner, InferredOwner};
    // 確信度は同じ受け手での直接呼び出し (`$x->m()`) と同じ: クラスを名指しすれば ExactOwner、
    // `$this` と型の手掛かりのある変数は InferredOwner、受け手の型が分からなければ BareNameOnly。
    let cases: &[(
        &str,
        Option<(&str, crate::models::reference::RefConfidence)>,
    )] = &[
        ("[$this, 'viaThis']", Some(("viaThis", InferredOwner))),
        (
            "['Plugin', 'viaClassString']",
            Some(("viaClassString", ExactOwner)),
        ),
        (
            "['\\App\\Plugin', 'viaQualifiedString']",
            Some(("viaQualifiedString", ExactOwner)),
        ),
        ("[new Plugin(), 'viaNew']", Some(("viaNew", BareNameOnly))),
        (
            "[$this->handler, 'viaMember']",
            Some(("viaMember", BareNameOnly)),
        ),
        ("[$obj, 'viaVariable']", Some(("viaVariable", BareNameOnly))),
        (
            "[self::class, 'viaSelfClass']",
            Some(("viaSelfClass", ExactOwner)),
        ),
        (
            "[Plugin::class, 'viaClassConst']",
            Some(("viaClassConst", ExactOwner)),
        ),
        (
            "[Plugin::HANDLER, 'viaConst']",
            Some(("viaConst", BareNameOnly)),
        ),
        (
            "[$this, \"viaDoubleQuoted\"]",
            Some(("viaDoubleQuoted", InferredOwner)),
        ),
        // 対照: callable の対象になれない第1要素
        ("[1, 'foo']", None),
        ("[true, 'foo']", None),
        ("[null, 'foo']", None),
        ("[['x'], 'foo']", None),
        ("['required', 'email']", None),
        // 対照: callable の形ではない配列
        ("['k' => $this, 'm' => 'foo']", None),
        ("[$this, 'foo', 'bar']", None),
        ("[$this, 'not an identifier']", None),
    ];
    for (array, expected) in cases {
        let source = format!("<?php\nfunction f($obj) {{ return {array}; }}\n");
        let tree = parser::parse_source(source.as_bytes(), LangId::Php).unwrap();
        let mut found = None;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                if found.is_none() && node.kind() == "array_creation_expression" {
                    found = Some(node);
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        let array_node = found.expect("array_creation_expression");
        let segment = php_callable_array_method_segment(array_node, source.as_bytes(), LangId::Php)
            .map(|(name, _, _, confidence)| (name, confidence));
        assert_eq!(segment, *expected, "{array}");
    }
    // 型付き引数の変数は `$x->m()` と同じく InferredOwner
    let typed = "<?php\nfunction f(Repo $repo) { return [$repo, 'save']; }\n";
    let tree = parser::parse_source(typed.as_bytes(), LangId::Php).unwrap();
    let mut typed_array = None;
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        tree.root_node(),
        crate::engine::tree_walk::Children::All,
        |node, _| {
            if typed_array.is_none() && node.kind() == "array_creation_expression" {
                typed_array = Some(node);
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
    assert_eq!(
        php_callable_array_method_segment(
            typed_array.expect("array"),
            typed.as_bytes(),
            LangId::Php
        )
        .map(|(name, _, _, confidence)| (name, confidence)),
        Some(("save", InferredOwner))
    );

    let strings: &[(&str, Option<&str>)] = &[
        ("'Plugin::viaStaticString'", Some("viaStaticString")),
        ("'\\App\\Plugin::boot'", Some("boot")),
        ("'Plugin@handle'", Some("handle")),
        // 対照: クラス名の形でない・メソッド名の形でない・クラス名の無い `::`
        ("'http://example.com'", None),
        ("'a::b'", None),
        ("'Plugin::'", None),
        ("'::boot'", None),
        ("'Plugin::Boot'", None),
    ];
    for (literal, expected) in strings {
        let source = format!("<?php\ncall_user_func({literal});\n");
        let tree = parser::parse_source(source.as_bytes(), LangId::Php).unwrap();
        let mut found = None;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                if found.is_none() && node.kind() == "string" {
                    found = Some(node);
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        let string_node = found.expect("string");
        let segment =
            php_string_callable_method_segment(string_node, source.as_bytes(), LangId::Php);
        assert_eq!(segment.map(|(name, _, _)| name), *expected, "{literal}");
        if let Some((name, row, column)) = segment {
            let line = source.lines().nth(row).unwrap();
            assert!(line[column..].starts_with(name), "{literal}: 位置 {column}");
        }
    }
}
