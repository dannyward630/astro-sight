//! Shared Rust use-tree expansion for API exposure and impact ownership.

use std::rc::Rc;

/// `pub use` から生成される re-export edge。Named は `source_module::exported_name` が
/// `target_module::target_item` を指す。alias の場合 `exported_name = alias`、`target_item = 元名`。
/// Wildcard は `source_module::* = target_module::*` で名前ごとに伝播する。
#[derive(Clone, Debug)]
pub(crate) enum RustPubUseEdge {
    Named {
        source_module: Vec<String>,
        exported_name: String,
        target_module: Vec<String>,
        target_item: String,
    },
    Wildcard {
        source_module: Vec<String>,
        target_module: Vec<String>,
    },
}

/// 構造的に AST を walk して `pub use` re-export ターゲットを抽出する (whitespace / コメント非依存)。
///
/// `argument` ノードは tree-sitter-rust の以下のいずれかになる:
/// - `identifier`: 単一名 `pub use Foo;` (この crate root の Foo を再エクスポート)
/// - `scoped_identifier`: `path::name` 形式。`path` は field=path、`name` は field=name
/// - `scoped_use_list`: `path::{...}` 形式。`path` は field=path、`list` は field=list (use_list)
/// - `use_list`: `{...}` 形式 (path なし、トップでは稀)
/// - `use_as_clause`: `path as alias` 形式。`path` は field=path、`alias` は field=alias
/// - `use_wildcard`: `path::*` 形式。`path` は field=path (省略あり)
/// - `crate` / `self` / `super`: アンカーキーワード (再帰中に処理)
///
/// 戻り値 `None` で「判定不能」(root を超える super::、解決不能な anchor) — 呼出元は index を `None` にする。
///
/// `a::{b::{c::{…}}}` の入れ子の深さだけ再帰しないよう、明示スタックで前順に展開する。
/// 子は逆順に積むので、edge を出す順序と、失敗で打ち切る位置は子を順に再帰した場合と同じ。
pub(crate) fn expand_rust_use_tree_edges_ast(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
    source_module: &[String],
    alias_override: Option<&str>,
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    let mut pending = vec![UseTreeFrame {
        node,
        path_prefix: Rc::from(path_prefix),
        current_module,
        alias_override,
    }];
    while let Some(frame) = pending.pop() {
        let UseTreeFrame {
            node,
            path_prefix,
            current_module,
            alias_override,
        } = frame;
        match node.kind() {
            "scoped_use_list" => {
                let mut path_node: Option<tree_sitter::Node<'_>> = None;
                let mut list_node: Option<tree_sitter::Node<'_>> = None;
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "use_list" => {
                            list_node = Some(child);
                            break;
                        }
                        _ => path_node = Some(child),
                    }
                }
                let list = list_node?;
                let resolved_prefix = match path_node {
                    Some(pn) => {
                        let (prefix, leaf) =
                            resolve_use_path_node(pn, source, &path_prefix, current_module)?;
                        let mut p = prefix;
                        if let Some(name) = leaf {
                            p.push(name);
                        }
                        Rc::from(p)
                    }
                    None => path_prefix,
                };
                push_use_list_items(&mut pending, list, &resolved_prefix);
            }
            "use_list" => {
                push_use_list_items(&mut pending, node, &path_prefix);
            }
            "use_as_clause" => {
                // [path, alias] 順。alias=`_` は外部非公開なので edge を作らない。
                let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    named.push(child);
                }
                if named.len() != 2 {
                    continue;
                }
                let alias_text = named[1].utf8_text(source).ok()?.trim();
                if alias_text == "_" {
                    continue;
                }
                // use_as_clause の内側 path には alias がさらに適用されるケースは無いので alias_override
                // をここで指定して下流の `scoped_identifier` 経路で edge 化する。
                pending.push(UseTreeFrame {
                    node: named[0],
                    path_prefix,
                    current_module,
                    alias_override: Some(alias_text),
                });
            }
            "use_wildcard" => {
                // named child = [path]
                let mut cursor = node.walk();
                let path_node = node.named_children(&mut cursor).next();
                if let Some(path_node) = path_node {
                    let (resolved_prefix, leaf_name) =
                        resolve_use_path_node(path_node, source, &path_prefix, current_module)?;
                    let mut target_module = resolved_prefix;
                    if let Some(name) = leaf_name {
                        target_module.push(name);
                    }
                    if !target_module.is_empty() {
                        out.push(RustPubUseEdge::Wildcard {
                            source_module: source_module.to_vec(),
                            target_module,
                        });
                    }
                }
            }
            "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
                // path::name 形式の単純 re-export、または anchor 単体。
                let (resolved_prefix, leaf_name) =
                    resolve_use_path_node(node, source, &path_prefix, current_module)?;
                if let Some(item) = leaf_name {
                    // resolved_prefix が空 (crate root 直下の item / module) でも edge を
                    // 生成する。旧実装は silent drop しており、`pub use self::wifi as api;`
                    // のような root 直下モジュールの再エクスポートが API 面判定から漏れて
                    // pub fn の削除・変更が無音になっていた (モジュール到達性は
                    // compute_reexport_reachable_modules 側で固定点計算する)。
                    let exported_name = alias_override
                        .map(str::to_string)
                        .unwrap_or_else(|| item.clone());
                    out.push(RustPubUseEdge::Named {
                        source_module: source_module.to_vec(),
                        exported_name,
                        target_module: resolved_prefix,
                        target_item: item,
                    });
                }
            }
            _ => {
                // 知らない kind は子供を walk (将来の grammar 変更に保守的に対応)。
                let mut cursor = node.walk();
                let children: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|child| child.is_named())
                    .collect();
                for child in children.into_iter().rev() {
                    pending.push(UseTreeFrame {
                        node: child,
                        path_prefix: Rc::clone(&path_prefix),
                        current_module,
                        alias_override,
                    });
                }
            }
        }
    }
    Some(())
}

/// `expand_rust_use_tree_edges_ast` の展開待ちの 1 要素。
struct UseTreeFrame<'tree, 'a> {
    node: tree_sitter::Node<'tree>,
    path_prefix: Rc<[String]>,
    current_module: &'a [String],
    alias_override: Option<&'a str>,
}

/// `use_list` ノード (`{ ... }`) の各要素 (`,` 区切り) を展開待ちに積む。
/// group 内では `current_module` は継承しない (空)。
fn push_use_list_items<'tree>(
    pending: &mut Vec<UseTreeFrame<'tree, '_>>,
    list: tree_sitter::Node<'tree>,
    path_prefix: &Rc<[String]>,
) {
    let mut cursor = list.walk();
    let items: Vec<_> = list
        .children(&mut cursor)
        .filter(|child| child.is_named())
        .collect();
    for item in items.into_iter().rev() {
        pending.push(UseTreeFrame {
            node: item,
            path_prefix: Rc::clone(path_prefix),
            current_module: &[],
            alias_override: None,
        });
    }
}

/// use path の 1 セグメント。先頭から順に平坦化して保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
enum UsePathSegment {
    /// `crate` (クレートルート起点)
    Crate,
    /// `self` (現 module 起点)
    SelfMod,
    /// `super` (1 階層上)
    Super,
    /// 通常の識別子
    Name(String),
}

/// `scoped_identifier` の入れ子を先頭から順の平坦なセグメント列へ畳む。
///
/// **path 位置だけを anchor として扱ってはいけない**: tree-sitter-rust は
/// `super::super::x` の 2 つ目の `super` を `scoped_identifier` の **name 位置**へ置く。
/// name 位置のテキストを無条件に識別子として扱う実装だと、`"super"` が文字列として
/// モジュールパスに積まれ、2 段以上の `super` を含む再エクスポートの解決が壊れる。
/// ノード kind で分類してから解決すれば、path / name のどちらに来ても同じ結果になる。
///
/// `a::b::…` の段数だけ再帰しないよう、明示スタックで前順に辿る。知らない kind は子を
/// 順に試し、最初に成功した子を採る (試した子が失敗したら `out` をその子の開始時点へ戻す)。
fn flatten_use_path(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut Vec<UsePathSegment>,
) -> Option<()> {
    enum Task<'tree> {
        Visit(tree_sitter::Node<'tree>),
        /// 知らない kind の子を順に試している途中。`next` は次に試す子、`checkpoint` は
        /// 試す前の `out` の長さ。この要素まで戻ってきたら、試していた子が成功している。
        Attempt {
            children: Vec<tree_sitter::Node<'tree>>,
            next: usize,
            checkpoint: usize,
        },
    }

    let mut tasks = vec![Task::Visit(node)];
    while let Some(task) = tasks.pop() {
        let Task::Visit(node) = task else {
            // 試していた子が最後まで成功した。残りの子は試さない。
            continue;
        };
        let succeeded = match node.kind() {
            "crate" => {
                out.push(UsePathSegment::Crate);
                true
            }
            "self" => {
                out.push(UsePathSegment::SelfMod);
                true
            }
            "super" => {
                out.push(UsePathSegment::Super);
                true
            }
            "identifier" => match node.utf8_text(source) {
                Ok(text) => {
                    out.push(UsePathSegment::Name(text.trim().to_string()));
                    true
                }
                Err(_) => false,
            },
            "scoped_identifier" => {
                // tree-sitter-rust grammar は scoped_identifier で path/name の field 名を出さない。
                // named children は最大 2 つ: [path, name] または [name] のみ
                // (path が省略された場合は crate root レベルの単一 identifier 扱い)。
                let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    named.push(child);
                }
                match named.as_slice() {
                    [only] => {
                        tasks.push(Task::Visit(*only));
                        true
                    }
                    [path_node, name_node] => {
                        tasks.push(Task::Visit(*name_node));
                        tasks.push(Task::Visit(*path_node));
                        true
                    }
                    _ => false, // 想定外の named children 数
                }
            }
            _ => {
                // 知らない kind は子供を walk して可能な解決を試みる (従来の保守姿勢を維持)。
                let mut cursor = node.walk();
                let children: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|child| child.is_named())
                    .collect();
                match children.first() {
                    Some(&first) => {
                        tasks.push(Task::Attempt {
                            children,
                            next: 1,
                            checkpoint: out.len(),
                        });
                        tasks.push(Task::Visit(first));
                        true
                    }
                    None => false,
                }
            }
        };
        if succeeded {
            continue;
        }
        // 失敗した。直近の試行まで戻り、その子の残りの作業と出力を捨てて次の子を試す。
        // 試行が無ければ全体の失敗、子を試し尽くした試行はさらに外側へ失敗を伝える。
        loop {
            match tasks.pop() {
                None => return None,
                Some(Task::Visit(_)) => {}
                Some(Task::Attempt {
                    children,
                    next,
                    checkpoint,
                }) => {
                    out.truncate(checkpoint);
                    if let Some(&child) = children.get(next) {
                        tasks.push(Task::Attempt {
                            children,
                            next: next + 1,
                            checkpoint,
                        });
                        tasks.push(Task::Visit(child));
                        break;
                    }
                }
            }
        }
    }
    Some(())
}

/// use path のノードを (モジュール prefix, 末尾の item 名) へ解決する。
///
/// 先頭から続く `crate` / `self` / `super` を module anchor として消費し、残りの
/// 識別子列のうち末尾を item、それ以外を module パスとして扱う。
/// クレートルートを越える `super`、anchor が先頭以外に現れる不正な path、
/// 解決できないノードはすべて `None` (fail-closed)。
pub(crate) fn resolve_use_path_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
) -> Option<(Vec<String>, Option<String>)> {
    let mut segments = Vec::new();
    flatten_use_path(node, source, &mut segments)?;
    if segments.is_empty() {
        return None;
    }

    let mut prefix: Vec<String> = path_prefix.to_vec();
    let mut names: Vec<String> = Vec::new();
    for (idx, segment) in segments.iter().enumerate() {
        match segment {
            UsePathSegment::Crate => {
                // 先頭以外の `crate` は Rust として不正。
                if idx != 0 {
                    return None;
                }
                prefix = Vec::new();
            }
            UsePathSegment::SelfMod => {
                if idx != 0 {
                    return None;
                }
                if prefix.is_empty() {
                    prefix = current_module.to_vec();
                }
            }
            UsePathSegment::Super => {
                // `super` は先頭から連続する場合のみ有効 (`a::super::b` は不正)。
                if !names.is_empty() {
                    return None;
                }
                if idx == 0 && prefix.is_empty() {
                    prefix = current_module.to_vec();
                }
                // クレートルートを越える `super` は解決不能。
                prefix.pop()?;
            }
            UsePathSegment::Name(name) => names.push(name.clone()),
        }
    }

    // grouped 形式 (`use a::b::{self};`) では、list 要素として**単独の `self`** が
    // path_prefix = ["a", "b"] を積んだ状態で届く (`expand_use_list_edges` が list の各要素へ
    // 解決済み prefix を渡すため)。直接形 `use a::b::self;` と同値なので、prefix の末尾を
    // item へ畳んで同じ edge を作る。畳まないと `leaf_name` が None になり
    // `expand_rust_use_tree_edges_ast` が Named edge を生成せず、private module 配下の
    // 公開 module をこの形で再エクスポートした場合に配下の API 削除を見逃す。
    //
    // path_prefix が空のとき (`use self::{a, b};` の path 位置に来る `self` など) は
    // module anchor としての `self` なので畳まない。
    if names.is_empty()
        && segments.as_slice() == [UsePathSegment::SelfMod]
        && !path_prefix.is_empty()
    {
        let item = prefix.pop()?;
        return Some((prefix, Some(item)));
    }

    // 末尾の `self` (`use a::b::self;` = `use a::b::{self};`) は「その module 自身」の
    // 再エクスポート。`self` は予約語なので item 名にはなり得ず、この位置に現れたら
    // 必ず trailing self 形式。直前の名前を item にする。
    //
    // tree-sitter は末尾の `self` を kind `self` ではなく `identifier` として返すため
    // (path 位置の `self` だけが kind `self` になる)、ここは `Name("self")` として届く。
    // 畳まないと「`self` という名前の item」という実在しない edge を作る。
    if names.last().is_some_and(|n| n == "self") {
        names.pop();
        if names.is_empty() {
            // `crate::self` のような、直前の名前が無い形は解決不能 (fail-closed)。
            return None;
        }
    }
    // 末尾以外に現れる `self` は Rust として不正なので解決しない。
    if names.iter().any(|n| n == "self") {
        return None;
    }

    let leaf = names.pop();
    prefix.extend(names);
    Some((prefix, leaf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。
    fn expand_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        path_prefix: &[String],
        current_module: &[String],
        source_module: &[String],
        alias_override: Option<&str>,
        out: &mut Vec<RustPubUseEdge>,
    ) -> Option<()> {
        match node.kind() {
            "scoped_use_list" => {
                let mut path_node = None;
                let mut list_node = None;
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "use_list" => {
                            list_node = Some(child);
                            break;
                        }
                        _ => path_node = Some(child),
                    }
                }
                let list = list_node?;
                let resolved_prefix = match path_node {
                    Some(pn) => {
                        let (mut prefix, leaf) =
                            resolve_use_path_node(pn, source, path_prefix, current_module)?;
                        if let Some(name) = leaf {
                            prefix.push(name);
                        }
                        prefix
                    }
                    None => path_prefix.to_vec(),
                };
                expand_list_recursive(list, source, &resolved_prefix, source_module, out)?;
            }
            "use_list" => expand_list_recursive(node, source, path_prefix, source_module, out)?,
            "use_as_clause" => {
                let mut cursor = node.walk();
                let named: Vec<_> = node.named_children(&mut cursor).collect();
                if named.len() != 2 {
                    return Some(());
                }
                let alias_text = named[1].utf8_text(source).ok()?.trim();
                if alias_text == "_" {
                    return Some(());
                }
                expand_recursive(
                    named[0],
                    source,
                    path_prefix,
                    current_module,
                    source_module,
                    Some(alias_text),
                    out,
                )?;
            }
            "use_wildcard" => {
                let mut cursor = node.walk();
                if let Some(path_node) = node.named_children(&mut cursor).next() {
                    let (mut target_module, leaf_name) =
                        resolve_use_path_node(path_node, source, path_prefix, current_module)?;
                    if let Some(name) = leaf_name {
                        target_module.push(name);
                    }
                    if !target_module.is_empty() {
                        out.push(RustPubUseEdge::Wildcard {
                            source_module: source_module.to_vec(),
                            target_module,
                        });
                    }
                }
            }
            "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
                let (resolved_prefix, leaf_name) =
                    resolve_use_path_node(node, source, path_prefix, current_module)?;
                if let Some(item) = leaf_name {
                    out.push(RustPubUseEdge::Named {
                        source_module: source_module.to_vec(),
                        exported_name: alias_override
                            .map(str::to_string)
                            .unwrap_or_else(|| item.clone()),
                        target_module: resolved_prefix,
                        target_item: item,
                    });
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        expand_recursive(
                            child,
                            source,
                            path_prefix,
                            current_module,
                            source_module,
                            alias_override,
                            out,
                        )?;
                    }
                }
            }
        }
        Some(())
    }

    fn expand_list_recursive(
        list: tree_sitter::Node<'_>,
        source: &[u8],
        path_prefix: &[String],
        source_module: &[String],
        out: &mut Vec<RustPubUseEdge>,
    ) -> Option<()> {
        let mut cursor = list.walk();
        for child in list.children(&mut cursor) {
            if child.is_named() {
                expand_recursive(child, source, path_prefix, &[], source_module, None, out)?;
            }
        }
        Some(())
    }

    fn flatten_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        out: &mut Vec<UsePathSegment>,
    ) -> Option<()> {
        match node.kind() {
            "crate" => out.push(UsePathSegment::Crate),
            "self" => out.push(UsePathSegment::SelfMod),
            "super" => out.push(UsePathSegment::Super),
            "identifier" => out.push(UsePathSegment::Name(
                node.utf8_text(source).ok()?.trim().to_string(),
            )),
            "scoped_identifier" => {
                let mut cursor = node.walk();
                let named: Vec<_> = node.named_children(&mut cursor).collect();
                return match named.as_slice() {
                    [only] => flatten_recursive(*only, source, out),
                    [path_node, name_node] => {
                        flatten_recursive(*path_node, source, out)?;
                        flatten_recursive(*name_node, source, out)
                    }
                    _ => None,
                };
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        let mut sub = Vec::new();
                        if flatten_recursive(child, source, &mut sub).is_some() {
                            out.extend(sub);
                            return Some(());
                        }
                    }
                }
                return None;
            }
        }
        Some(())
    }

    fn all_nodes(root: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
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

    /// 反復に書き換えた use ツリーの展開と path の平坦化が、浅い入力で修正前の再帰版と
    /// 同じ結果 (edge の内容と順序、失敗の有無、失敗した時点までの出力) を返す。
    /// 平坦化は木の全ノードに当て、知らない kind の子を順に試す分岐 (先の子が失敗して
    /// 後の子で成功する、すべて失敗する) も通す。
    #[test]
    fn iterative_use_tree_walk_matches_recursive_version() {
        let source = b"pub use crate::a::b;\npub use self::c as d;\npub use super::super::e::{self, f as _, g::{h, i::*}};\npub use j::{k::{l, m}, n as o, self};\npub use crate::self;\npub use ::std::fmt;\npub use a::b::c::{self};\npub use super::{x, y::z};\npub use super::super::super::too::far;\npub use a::super::b;\npub use {p, q::r};\npub use crate::*;\nfn t() { let _ = [1, a]; let _ = (1, 2); let _ = a::b::c; }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let mut checked_edges = 0;
        for node in all_nodes(tree.root_node()) {
            let (mut iterative, mut recursive) = (Vec::new(), Vec::new());
            assert_eq!(
                (flatten_use_path(node, source, &mut iterative), &iterative),
                (flatten_recursive(node, source, &mut recursive), &recursive),
                "flatten {}: {}",
                node.kind(),
                node.utf8_text(source).unwrap()
            );
            if node.kind() != "use_declaration" {
                continue;
            }
            let argument = node.child_by_field_name("argument").unwrap();
            let module = |names: &[&str]| {
                names
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
            };
            for (prefix, current) in [
                (module(&[]), module(&["m", "n"])),
                (module(&["p"]), module(&["q"])),
                (module(&[]), module(&[])),
            ] {
                for alias in [None, Some("renamed")] {
                    let (mut iterative, mut recursive) = (Vec::new(), Vec::new());
                    let left = expand_rust_use_tree_edges_ast(
                        argument,
                        source,
                        &prefix,
                        &current,
                        &module(&["src"]),
                        alias,
                        &mut iterative,
                    );
                    let right = expand_recursive(
                        argument,
                        source,
                        &prefix,
                        &current,
                        &module(&["src"]),
                        alias,
                        &mut recursive,
                    );
                    assert_eq!(
                        (left, format!("{iterative:?}")),
                        (right, format!("{recursive:?}")),
                        "expand {}",
                        node.utf8_text(source).unwrap()
                    );
                    checked_edges += iterative.len();
                }
            }
        }
        assert!(checked_edges > 20, "edge を出す入力を含む: {checked_edges}");
    }
}
