//! 同じ識別子の別の書き方 (Rust の raw identifier、Swift / Kotlin の backtick) と、
//! UTF-8 でない行を含むスクリプトの参照を、refs / dead-code / impact が数えることの統合テスト。

#[allow(unused_imports)]
use super::support::*;

fn hook(repo: &TestRepo, command: &str) -> std::process::Output {
    cargo_bin()
        .args([
            command,
            "--dir",
            repo.root().to_str().unwrap(),
            "--git",
            "--hook",
        ])
        .output()
        .expect("failed to run astro-sight")
}

fn ref_lines(json: &serde_json::Value) -> Vec<(String, u64, String)> {
    let mut out: Vec<(String, u64, String)> = json["refs"]
        .as_array()
        .map(|refs| {
            refs.iter()
                .map(|r| {
                    (
                        r["path"].as_str().unwrap_or_default().to_string(),
                        r["ln"].as_u64().unwrap_or_default(),
                        r["kind"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn dead_names(json: &serde_json::Value) -> Vec<String> {
    let mut names: Vec<String> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols")
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// Rust の `r#gen` と `gen` は同じ識別子。Rust 2024 で予約語になった `gen` を edition 2021 の
/// クレートから呼ぶときは `a::r#gen()` と書く。
///
/// 旧実装はノードのテキストを文字列のまま比べ、`refs --name gen` が呼び出しを返さず、
/// dead-code が `gen` を dead と報告し、`gen` の引数を変えても impact の hook が exit 0
/// (review の hook は `no_callers` を付けた) になっていた。
/// 対照: 呼び出しの無い `unused_fn` は dead のまま。
#[test]
fn rust_raw_identifier_matches_plain_name() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    );
    repo.create_dir_all("a/src");
    repo.create_dir_all("b/src");
    repo.write(
        "a/Cargo.toml",
        "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.write(
        "b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n",
    );
    repo.write(
        "a/src/lib.rs",
        "pub fn gen() -> u8 {\n    4\n}\n\npub fn unused_fn() -> u8 {\n    0\n}\n",
    );
    repo.write("b/src/lib.rs", "pub fn run() -> u8 {\n    a::r#gen()\n}\n");

    let call = ("b/src/lib.rs".to_string(), 1, "ref".to_string());
    let definition = ("a/src/lib.rs".to_string(), 0, "def".to_string());
    for name in ["gen", "r#gen"] {
        let refs = ref_lines(&repo.run_json("refs", &["--name", name]));
        assert!(
            refs.contains(&call) && refs.contains(&definition),
            "{name}: 定義と `a::r#gen()` の呼び出しを返す: {refs:?}"
        );
    }
    let batch = cargo_bin()
        .args(["refs", "--names", "gen,r#gen", "--dir"])
        .arg(repo.root())
        .output()
        .expect("failed to run refs --names");
    assert!(batch.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&batch.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("NDJSON line"))
        .collect();
    assert_eq!(lines.len(), 2);
    for json in &lines {
        assert!(ref_lines(json).contains(&call), "--names: {json}");
    }

    let dead = dead_names(&repo.run_json("dead-code", &[]));
    assert!(!dead.contains(&"gen".to_string()), "{dead:?}");
    assert!(dead.contains(&"unused_fn".to_string()), "対照: {dead:?}");

    repo.commit_all("init");
    repo.write(
        "a/src/lib.rs",
        "pub fn gen(seed: u8) -> u8 {\n    seed\n}\n\npub fn unused_fn() -> u8 {\n    0\n}\n",
    );
    let impact = hook(&repo, "impact");
    let stderr = String::from_utf8_lossy(&impact.stderr);
    assert_eq!(impact.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("b/src/lib.rs"), "{stderr}");

    // review の hook は stderr の 1 行目に JSON を出す。api.mod の `gen` には呼び出し元があり
    // (`no_callers` を付けない)、dead にも `gen` を出さない。
    let review = hook(&repo, "review");
    let stderr = String::from_utf8_lossy(&review.stderr);
    let json: serde_json::Value =
        serde_json::from_str(stderr.lines().next().unwrap_or_default()).expect("hook JSON");
    let modified = json["api"]["mod"].as_array().cloned().unwrap_or_default();
    assert!(
        modified
            .iter()
            .any(|m| m["n"] == "gen" && m.get("no_callers").is_none()),
        "{json}"
    );
    let dead = json["dead"].as_array().cloned().unwrap_or_default();
    assert!(
        !dead.iter().any(|d| d["n"] == "gen"),
        "gen は dead ではない: {json}"
    );
}

/// Swift で予約語を名前にするときは宣言に backtick が要り、ドットの後の参照には要らない
/// (`static func `default`()` と `Registry.default()`)。Kotlin でも backtick は名前の一部ではない。
///
/// 旧実装は backtick 込みの宣言名と backtick なしの呼び出しを別の名前として扱い、使われている
/// メソッドを dead と報告し、引数を変えても impact の hook が exit 0 になっていた。
/// 対照: 使われていない `unusedS` / `unusedK` は dead のまま。
#[test]
fn swift_and_kotlin_backtick_names_match_plain_calls() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Settings.swift",
        "public struct Settings {\n    public let level: Int\n    public static func `for`(level: Int) -> Settings {\n        Settings(level: level)\n    }\n}\n\npublic final class Registry {\n    public static func `default`() -> Registry {\n        Registry()\n    }\n    public func unusedS() -> Int {\n        0\n    }\n}\n",
    );
    repo.write(
        "App.swift",
        "let t = Settings.for(level: 2)\nlet r = Registry.default()\n",
    );
    repo.write(
        "K.kt",
        "package k\nclass K {\n    fun `foo`(): Int = 1\n    fun unusedK(): Int = 0\n}\n",
    );
    repo.write("Use.kt", "package k\nfun run(k: K): Int = k.foo()\n");

    let dead = dead_names(&repo.run_json("dead-code", &[]));
    for live in ["Settings.`for`", "Registry.`default`", "K.`foo`"] {
        assert!(!dead.contains(&live.to_string()), "{live}: {dead:?}");
    }
    for unused in ["Registry.unusedS", "K.unusedK"] {
        assert!(
            dead.contains(&unused.to_string()),
            "対照 {unused}: {dead:?}"
        );
    }
    for name in ["default", "`default`"] {
        let refs = ref_lines(&repo.run_json("refs", &["--name", name]));
        assert!(
            refs.contains(&("App.swift".to_string(), 1, "ref".to_string()))
                && refs.contains(&("Settings.swift".to_string(), 8, "def".to_string())),
            "{name}: 宣言と呼び出しを両方返す: {refs:?}"
        );
    }

    repo.commit_all("init");
    repo.write(
        "Settings.swift",
        "public struct Settings {\n    public let level: Int\n    public static func `for`(level: Int) -> Settings {\n        Settings(level: level)\n    }\n}\n\npublic final class Registry {\n    public static func `default`(name: String) -> Registry {\n        Registry()\n    }\n    public func unusedS() -> Int {\n        0\n    }\n}\n",
    );
    let impact = hook(&repo, "impact");
    let stderr = String::from_utf8_lossy(&impact.stderr);
    assert_eq!(impact.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("App.swift"), "{stderr}");
}

/// 拡張子なしのスクリプトの shebang は先頭行だけで判定する。2 行目以降に Shift_JIS の
/// コメントがあっても、そのスクリプトからの参照を数える。
///
/// 旧実装はファイル全体を UTF-8 として検証してから先頭行を取り出していたため、走査対象には
/// 選ばれる (先頭行だけを見る) のに解析で失敗し、refs が呼び出しを黙って落とし、dead-code が
/// 呼ばれている関数を dead と報告していた。対照: ASCII だけのスクリプトは従来どおり。
#[test]
fn shebang_script_with_non_utf8_comment_is_analyzed() {
    let repo = TestRepo::new();
    repo.create_dir_all("bin");
    repo.write(
        "lib.sh",
        "deploy_app() {\n    echo deploy\n}\n\nunused_helper() {\n    :\n}\n\nok_helper() {\n    :\n}\n",
    );
    let mut deploy = b"#!/bin/bash\n. ./lib.sh\n# ".to_vec();
    deploy.extend_from_slice(&[0x83, 0x66, 0x83, 0x76, 0x83, 0x8d, 0x83, 0x43]);
    deploy.extend_from_slice(b"\ndeploy_app\n");
    repo.write("bin/deploy", deploy);
    repo.write(
        "bin/ok",
        "#!/bin/bash\n. ./lib.sh\n# plain comment\nok_helper\n",
    );

    let refs = ref_lines(&repo.run_json("refs", &["--name", "deploy_app"]));
    assert!(
        refs.contains(&("bin/deploy".to_string(), 3, "ref".to_string())),
        "Shift_JIS のコメントを含むスクリプトからの呼び出し: {refs:?}"
    );
    let ok = ref_lines(&repo.run_json("refs", &["--name", "ok_helper"]));
    assert!(
        ok.contains(&("bin/ok".to_string(), 3, "ref".to_string())),
        "対照: {ok:?}"
    );

    let dead = dead_names(&repo.run_json("dead-code", &[]));
    assert!(!dead.contains(&"deploy_app".to_string()), "{dead:?}");
    assert!(
        dead.contains(&"unused_helper".to_string()),
        "対照: {dead:?}"
    );

    let symbols = cargo_bin()
        .args(["symbols", "--no-cache", "--path"])
        .arg(repo.path("bin/deploy"))
        .output()
        .expect("failed to run symbols");
    assert!(
        symbols.status.success(),
        "{}",
        String::from_utf8_lossy(&symbols.stdout)
    );
    let json: serde_json::Value = serde_json::from_slice(&symbols.stdout).expect("JSON");
    assert_eq!(json["lang"], "bash", "{json}");
}

/// PHP の型付き引数で受けた `$repo->save()` は、受け手の型を推論できる呼び出し
/// (InferredOwner) として impact の blocking に残す。
///
/// 旧実装は型の手掛かりを関数の本体の中だけで探し、本体の外にある引数リストを見なかった。
/// `save` のような汎用名は型不明の呼び出しを低確信度に回すので、`save` に引数を足しても
/// 呼び出し元が hook から外れ、exit 0 になっていた。対照: `new` で作った受け手は従来どおり。
#[test]
fn php_typed_parameter_receiver_is_a_blocking_caller() {
    for (label, caller) in [
        (
            "typed parameter",
            "<?php\nfunction persistParam(Repo $repo): bool {\n    return $repo->save([]);\n}\n",
        ),
        (
            "new (control)",
            "<?php\nfunction persistNew(): bool {\n    $repo = new Repo();\n    return $repo->save([]);\n}\n",
        ),
    ] {
        let repo = TestRepo::new();
        repo.init_git();
        repo.write(
            "Repo.php",
            "<?php\nclass Repo {\n    public function save(array $row): bool { return true; }\n}\n",
        );
        repo.write("Caller.php", caller);
        repo.commit_all("init");
        repo.write(
            "Repo.php",
            "<?php\nclass Repo {\n    public function save(array $row, bool $flush): bool { return true; }\n}\n",
        );
        let output = hook(&repo, "impact");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{label}: {stderr}");
        assert!(stderr.contains("Caller.php"), "{label}: {stderr}");
    }
}
