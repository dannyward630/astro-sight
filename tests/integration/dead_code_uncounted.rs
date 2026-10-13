//! 参照を数えられなかったファイル (読み込み上限の超過・権限・構文解析の失敗) があるとき、
//! dead-code がそのファイルから参照されうる候補を dead と断定せず、申告することの統合テスト。
//!
//! 旧実装は件数集計で読み込みエラーを黙って捨てていた。唯一の呼び出しが 100MB を超える
//! ファイルにある関数を dead と報告し (90MB なら dead にならない)、申告も出なかった。

#[allow(unused_imports)]
use super::support::*;

fn names_of(json: &serde_json::Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = json[key]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn truncations_of(json: &serde_json::Value) -> Vec<(Option<String>, String, String)> {
    json["truncations"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|t| {
                    (
                        t["path"].as_str().map(str::to_string),
                        t["reason"].as_str().unwrap_or_default().to_string(),
                        t["message"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const A_TS: &str = "export function onlyUsedInBig(): number {\n  return 1;\n}\n\nexport function unusedHelper(): number {\n  return 2;\n}\n";
const CALLER: &str = "import { onlyUsedInBig } from \"./a\";\nconsole.log(onlyUsedInBig());\n";

/// 唯一の呼び出しが読み込み上限 (100MB) を超えるファイルにある関数は、dead ではなく
/// `unverified_symbols` に回し、ファイルを `source_analysis_failed` で申告する。
/// 上限を超えたファイルはメモリを上限付きのまま走査し、現れない名前 (`unusedHelper`) は
/// 従来どおり dead と報告する。review の hook でも保留した候補で止めず、申告を出す。
///
/// 対照: 同じ呼び出しを読めるファイルに置けば数えて live にし、申告も出さない。
#[test]
fn dead_code_withholds_candidates_referenced_only_from_oversized_file() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/a.ts", A_TS);
    let line = format!("// {}\n", "x".repeat(96));
    let mut big = line.repeat(110 * 1024 * 1024 / line.len() + 1);
    big.push_str(CALLER);
    repo.write("src/big.ts", big);

    let json = repo.run_json("dead-code", &[]);
    assert_eq!(
        names_of(&json, "dead_symbols"),
        vec!["unusedHelper"],
        "{json}"
    );
    assert_eq!(
        names_of(&json, "unverified_symbols"),
        vec!["onlyUsedInBig"],
        "{json}"
    );
    let truncations = truncations_of(&json);
    assert_eq!(truncations.len(), 1, "{truncations:?}");
    let (path, reason, message) = &truncations[0];
    assert_eq!(path.as_deref(), Some("src/big.ts"));
    assert_eq!(reason, "source_analysis_failed");
    assert!(message.contains("exceeds the read limit"), "{message}");
    assert!(
        !message.contains(repo.root().to_string_lossy().as_ref()),
        "申告に絶対パスを含めない: {message}"
    );

    // review の hook: onlyUsedInBig の宣言行に触れる (旧実装なら touched な dead で止まる)。
    repo.commit_all("init");
    repo.write(
        "src/a.ts",
        A_TS.replace(
            "export function onlyUsedInBig(): number {",
            "export function onlyUsedInBig(): number { // touched",
        ),
    );
    let output = cargo_bin()
        .args(["review", "--dir"])
        .arg(repo.root())
        .args(["--git", "--hook"])
        .output()
        .expect("failed to run review");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let hook: serde_json::Value =
        serde_json::from_str(stderr.lines().next().unwrap_or_default()).expect("hook JSON");
    let dead = hook["dead"].as_array().cloned().unwrap_or_default();
    assert!(
        !dead.iter().any(|d| d["n"] == "onlyUsedInBig"),
        "保留した候補で止めない: {hook}"
    );
    let trunc = hook["trunc"].as_array().cloned().unwrap_or_default();
    assert!(
        trunc
            .iter()
            .any(|t| t["r"] == "source_analysis_failed" && t["f"] == "src/big.ts"),
        "数えられなかったファイルを申告する: {hook}"
    );

    // 対照: 読めるファイルに置けば数える
    repo.remove_file("src/big.ts");
    repo.write("src/small.ts", CALLER);
    let json = repo.run_json("dead-code", &[]);
    assert_eq!(
        names_of(&json, "dead_symbols"),
        vec!["unusedHelper"],
        "{json}"
    );
    assert!(names_of(&json, "unverified_symbols").is_empty(), "{json}");
    assert!(truncations_of(&json).is_empty(), "{json}");
}

/// 開けないファイル (権限なし) は中身を確かめられないので、どの候補も dead と断定しない。
/// root で動いていて読めてしまう環境では確かめない (git の管理外に置く。追跡中のファイルが
/// 読めないと `git diff` 自体が失敗するため、ここでは dead-code だけを見る)。
#[cfg(unix)]
#[test]
fn dead_code_withholds_every_candidate_when_a_source_cannot_be_opened() {
    use std::os::unix::fs::PermissionsExt;

    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write("src/a.ts", A_TS);
    repo.write("src/locked.ts", CALLER);
    let locked = repo.path("src/locked.ts");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read(&locked).is_ok();
    let json = (!readable).then(|| repo.run_json("dead-code", &[]));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    let Some(json) = json else {
        return;
    };

    assert!(names_of(&json, "dead_symbols").is_empty(), "{json}");
    assert_eq!(
        names_of(&json, "unverified_symbols"),
        vec!["onlyUsedInBig", "unusedHelper"],
        "{json}"
    );
    let truncations = truncations_of(&json);
    assert_eq!(truncations.len(), 1, "{truncations:?}");
    assert_eq!(truncations[0].0.as_deref(), Some("src/locked.ts"));
    assert!(
        truncations[0].2.contains("could not be opened"),
        "{:?}",
        truncations[0]
    );

    // 対照: 読めるようにすれば数え、unusedHelper だけが dead
    let json = repo.run_json("dead-code", &[]);
    assert_eq!(
        names_of(&json, "dead_symbols"),
        vec!["unusedHelper"],
        "{json}"
    );
    assert!(names_of(&json, "unverified_symbols").is_empty(), "{json}");
}
