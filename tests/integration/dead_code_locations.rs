//! dead-code が報告するシンボルの宣言行と、テストからの参照の振り分けが、リポジトリの
//! 外の事情 (同名の別メソッド、親ディレクトリの名前) に左右されないことの統合テスト。

#[allow(unused_imports)]
use super::support::*;

fn symbols_with_lines(json: &serde_json::Value, key: &str) -> Vec<(String, Option<u64>)> {
    let mut out: Vec<(String, Option<u64>)> = json[key]
        .as_array()
        .unwrap_or_else(|| panic!("{key} の配列が必要: {json}"))
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap_or_default().to_string(),
                s["line"].as_u64(),
            )
        })
        .collect();
    out.sort();
    out
}

const SHAPES: &str = "export class Alpha {\n  fmt(): string { return \"a\"; }\n}\n\nexport class Beta {\n  label = \"b\";\n  fmt(): string { return \"b\"; }\n}\n";

/// 同じファイルに同名のメソッド (`Alpha.fmt` と `Beta.fmt`) があっても、dead の宣言行は
/// そのメソッド自身の行を報告し、`--dead-scope touched-symbols` もその行で判定する。
///
/// 旧実装は行の表を末尾の名前 (`fmt`) だけで引き、先に出た `Alpha.fmt` の行を `Beta.fmt` の
/// 行として報告していた。そのため `Beta.fmt` だけを変えると dead が消え、`Alpha.fmt` だけを
/// 変えると触っていない `Beta.fmt` の dead で hook が止まっていた。
#[test]
fn dead_lines_and_touched_scope_follow_the_qualified_member() {
    let repo = TestRepo::new();
    repo.write("shapes.ts", SHAPES);
    repo.write(
        "use.ts",
        "import { Alpha, Beta } from \"./shapes\";\nexport const out = new Alpha().fmt() + new Beta().label;\n",
    );
    let dead = symbols_with_lines(&repo.run_json("dead-code", &[]), "dead_symbols");
    assert!(
        dead.contains(&("Beta.fmt".to_string(), Some(6))),
        "Beta.fmt は 6 行目 (0 始まり): {dead:?}"
    );

    repo.init_git();
    repo.commit_all("init");
    let touched = |repo: &TestRepo| -> Vec<(String, Option<u64>)> {
        symbols_with_lines(
            &repo.run_json("review", &["--git", "--dead-scope", "touched-symbols"]),
            "dead_symbols",
        )
    };

    // Beta.fmt の行だけを変える → 触った dead として残る
    repo.write("shapes.ts", SHAPES.replace("return \"b\"", "return \"B\""));
    let after_beta = touched(&repo);
    assert!(
        after_beta.contains(&("Beta.fmt".to_string(), Some(6))),
        "触った Beta.fmt の dead を残す: {after_beta:?}"
    );

    // Alpha.fmt の行だけを変える → 触っていない Beta.fmt の dead は出ない
    repo.write("shapes.ts", SHAPES.replace("return \"a\"", "return \"A\""));
    let after_alpha = touched(&repo);
    assert!(
        !after_alpha.iter().any(|(name, _)| name == "Beta.fmt"),
        "触っていない Beta.fmt の dead は出さない: {after_alpha:?}"
    );
}

/// テストからの参照かどうかは `--dir` からの相対パスで判定する。リポジトリの親ディレクトリが
/// `test` などでも、本番のファイルからの参照をテストからの参照と数えない。
///
/// 旧実装は走査したファイルの絶対パスをそのまま判定に渡していたため、`…/test/myrepo` では
/// 本番で使われている `used` / `v` を `test_only_symbols` と報告していた
/// (対照: 同じ内容を `…/work/myrepo` に置くと空)。
#[test]
fn parent_directory_named_test_does_not_make_references_test_only() {
    let base = TestRepo::new();
    for parent in ["test", "work"] {
        let root = base.path(format!("{parent}/myrepo"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/a.ts"),
            "export function used(): number { return 1; }\nexport function unused(): number { return 2; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/b.ts"),
            "import { used } from \"./a\";\nexport const v = used();\n",
        )
        .unwrap();
        std::fs::write(
            root.join("main.ts"),
            "import { v } from \"./src/b\";\nconsole.log(v);\n",
        )
        .unwrap();
        // 対照: 本当にテストからしか使われないシンボル
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/c.ts"),
            "export function onlyTested(): number { return 3; }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/c.test.ts"),
            "import { onlyTested } from \"../src/c\";\nonlyTested();\n",
        )
        .unwrap();

        let output = cargo_bin()
            .args(["dead-code", "--dir"])
            .arg(&root)
            .output()
            .expect("failed to run dead-code");
        assert!(output.status.success());
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
        let dead: Vec<String> = symbols_with_lines(&json, "dead_symbols")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        let test_only: Vec<String> = symbols_with_lines(&json, "test_only_symbols")
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(dead, vec!["unused".to_string()], "{parent}: {json}");
        assert_eq!(
            test_only,
            vec!["onlyTested".to_string()],
            "{parent}: {json}"
        );
    }
}
