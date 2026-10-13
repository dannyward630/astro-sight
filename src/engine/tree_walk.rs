//! 構文木の反復走査。
//!
//! 構文木を子 1 段ごとに自己再帰で辿ると、括弧が数万段入れ子になった式や長い
//! `else if` / `||` の連鎖 (いずれも構文として正しい) を含むファイル 1 つでスタックが
//! 溢れ、CLI・session・MCP サーバーがプロセスごと落ちる。走査は 1 つの TreeCursor で
//! 行い、コールスタックを木の深さに依存させない。

use std::ops::ControlFlow;

use tree_sitter::Node;

/// 走査で辿る子の範囲。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Children {
    /// `node.children()` を再帰した場合と同じ (無名ノードも訪れる)。
    All,
    /// `node.named_children()` を再帰した場合と同じ (無名ノードとその子孫は訪れない)。
    Named,
}

/// 訪問したノードの子へ降りるか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Walk {
    /// 子へ降りる。
    Descend,
    /// このノードの子孫は訪れず、次の兄弟へ進む。
    Skip,
}

/// `root` を含む部分木を、再帰版と同じ左から右の前順で訪れる。
///
/// `visit` には訪問したノードと `root` からの深さ (`root` = 0) を渡す。
/// `ControlFlow::Break` を返すとその時点で走査を終え、値をそのまま返す。
/// 走査は `root` の部分木の外へは出ない。
pub(crate) fn walk_preorder<'tree, B>(
    root: Node<'tree>,
    children: Children,
    mut visit: impl FnMut(Node<'tree>, usize) -> ControlFlow<B, Walk>,
) -> ControlFlow<B> {
    let mut cursor = root.walk();
    let mut depth = 0usize;
    loop {
        let node = cursor.node();
        // Named では無名ノードを訪れず、その子孫にも降りない (named_children の再帰と同じ)。
        let action = if depth > 0 && children == Children::Named && !node.is_named() {
            Walk::Skip
        } else {
            visit(node, depth)?
        };

        if action == Walk::Descend && cursor.goto_first_child() {
            depth += 1;
            continue;
        }

        // 次の兄弟、無ければ祖先の次の兄弟へ進む。root まで戻ったら終わり。
        loop {
            if depth == 0 {
                return ControlFlow::Continue(());
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return ControlFlow::Continue(());
            }
            depth -= 1;
        }
    }
}

/// `root` を含む部分木に、`pred` を満たすノードがあるか (前順で最初に見つけた時点で終える)。
pub(crate) fn any_node<'tree>(
    root: Node<'tree>,
    children: Children,
    mut pred: impl FnMut(Node<'tree>) -> bool,
) -> bool {
    walk_preorder(root, children, |node, _| {
        if pred(node) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(Walk::Descend)
        }
    })
    .is_break()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn preorder_recursive<'a>(
        node: Node<'a>,
        children: Children,
        depth: usize,
        skip_kind: &str,
        out: &mut Vec<(usize, u16, usize)>,
    ) {
        out.push((node.id(), node.kind_id(), depth));
        if depth > 0 && node.kind() == skip_kind {
            return;
        }
        let mut cursor = node.walk();
        let kids: Vec<Node<'a>> = match children {
            Children::All => node.children(&mut cursor).collect(),
            Children::Named => node.named_children(&mut cursor).collect(),
        };
        for child in kids {
            preorder_recursive(child, children, depth + 1, skip_kind, out);
        }
    }

    fn preorder_iterative(
        root: Node<'_>,
        children: Children,
        skip_kind: &str,
    ) -> Vec<(usize, u16, usize)> {
        let mut out = Vec::new();
        let _: ControlFlow<()> = walk_preorder(root, children, |node, depth| {
            out.push((node.id(), node.kind_id(), depth));
            if depth > 0 && node.kind() == skip_kind {
                ControlFlow::Continue(Walk::Skip)
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        });
        out
    }

    /// 浅い入力で、訪問の順序・深さ・Skip の効き方が再帰版と一致する。
    /// root を部分木 (関数本体) にしたときも外へ出ないことを含めて確かめる。
    #[test]
    fn walk_preorder_matches_recursive_order() {
        let source = b"function f(a) {\n  // c\n  if (a) { return () => [a, (1 + 2)]; }\n  return g(a, \"s\");\n}\nconst x = 1;\n";
        let tree = parser::parse_source(source, LangId::Javascript).unwrap();
        let root = tree.root_node();
        let body = root
            .named_child(0)
            .and_then(|f| f.child_by_field_name("body"))
            .expect("function body");
        for node in [root, body] {
            for children in [Children::All, Children::Named] {
                for skip_kind in ["arrow_function", "<none>"] {
                    let mut expected = Vec::new();
                    preorder_recursive(node, children, 0, skip_kind, &mut expected);
                    assert_eq!(
                        preorder_iterative(node, children, skip_kind),
                        expected,
                        "{children:?} / skip {skip_kind} / root {}",
                        node.kind()
                    );
                }
            }
        }
    }

    /// Break の値をそのまま返し、見つけた時点で走査を終える。
    #[test]
    fn walk_preorder_breaks_at_first_match() {
        let source = b"a(1); b(2); c(3);\n";
        let tree = parser::parse_source(source, LangId::Javascript).unwrap();
        let mut visited = 0;
        let found = walk_preorder(tree.root_node(), Children::Named, |node, _| {
            visited += 1;
            if node.kind() == "number" {
                ControlFlow::Break(node.utf8_text(source).unwrap().to_string())
            } else {
                ControlFlow::Continue(Walk::Descend)
            }
        });
        assert_eq!(found, ControlFlow::Break("1".to_string()));
        assert!(visited < 10, "最初の一致の後は訪れない: visited={visited}");
        assert!(any_node(tree.root_node(), Children::All, |n| n.kind() == "number"));
        assert!(!any_node(tree.root_node(), Children::All, |n| n.kind() == "string"));
    }
}
