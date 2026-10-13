//! 深い入れ子の構文木で、解析がスタックを使い切ってプロセスごと落ちないことの統合テスト。
//!
//! 括弧が数万段入れ子になった式や長い `else if` / `||` の連鎖は構文として正しく、
//! 生成コードでは現実に現れる。構文木を子 1 段ごとに再帰で辿る処理があると、こうした
//! ファイル 1 つで CLI・session・MCP サーバーが exit 134 (stack overflow) で落ち、
//! 同じ要求に含まれる他のファイルの結果も失われていた。
//!
//! スタック溢れはプロセスごと abort するので、テストはすべて子プロセスで実行する
//! (テストプロセスは巻き込まれない)。exit 0 だけでなく、深い部分の**後ろ**の参照が
//! 返ること、深いファイルからしか使われないシンボルを dead にしないことまで確かめる。

use std::io::{Read, Write};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::support::*;

/// 子プロセスを期限付きで実行する。stdout / stderr は並行して読み、期限を過ぎたら
/// kill して失敗にする (無限ループの退行でテストが止まらないように)。
fn run_bounded(mut command: Command, stdin: Option<String>) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn astro-sight");
    let mut child_stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || {
        if let Some(input) = stdin {
            // 子が先に落ちた場合の書き込みエラーは、終了状態の検査で検出する。
            let _ = child_stdin.write_all(input.as_bytes());
        }
    });
    let mut stdout = child.stdout.take().expect("stdout");
    let mut stderr = child.stderr.take().expect("stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("astro-sight did not finish within the deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    writer.join().expect("stdin writer");
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

/// MCP サーバーを `cwd` で起動し、initialize の応答を待ってから `requests` を送る。
/// `expected_ids` の応答がそろう (または期限を過ぎる) まで stdin を閉じない。
///
/// 応答を待たずに stdin を閉じると、サーバーは処理中の要求を残したまま終了する。
/// 負荷の高い環境では深い入力の要求が終わる前に閉じてしまい、テストの目的
/// (落ちずに応答し続けること) と関係なく応答が欠けていた。サーバーがスタック溢れで
/// 落ちた場合は出力がその時点で閉じるので、欠けた応答として検出できる。
fn mcp_requests_waiting_for_responses(
    cwd: &std::path::Path,
    requests: &[String],
    expected_ids: &[u64],
) -> String {
    use std::io::{BufRead, BufReader};
    use std::sync::mpsc;

    let mut child = cargo_bin()
        .arg("mcp")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut lines: Vec<String> = Vec::new();
    // 期限内に `id` の応答が届くまで読む。出力が閉じたら (サーバーの終了) false。
    let wait_for = |id: u64, lines: &mut Vec<String>| -> bool {
        let needle = format!("\"id\":{id}");
        loop {
            if lines.iter().any(|line| line.contains(&needle)) {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(line) => lines.push(line),
                Err(_) => return false,
            }
        }
    };

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .expect("write initialize");
    stdin.flush().expect("flush");
    assert!(
        wait_for(1, &mut lines),
        "initialize の応答が必要: {lines:?}"
    );
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .expect("write initialized");
    for request in requests {
        writeln!(stdin, "{request}").expect("write request");
    }
    stdin.flush().expect("flush");
    for &id in expected_ids {
        if !wait_for(id, &mut lines) {
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    reader.join().expect("reader thread");
    lines.extend(rx.try_iter());
    lines.join("\n")
}

/// 正常終了したことを確かめ、stdout を返す。
fn assert_finished(label: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{label}: {:?} stderr={}",
        output.status,
        stderr.chars().take(400).collect::<String>()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn parse_lines(stdout: &str) -> Vec<serde_json::Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("invalid JSON line"))
        .collect()
}

fn nested(open: &str, inner: &str, close: &str, depth: usize) -> String {
    format!("{}{inner}{}", open.repeat(depth), close.repeat(depth))
}

/// 関数の中の深い括弧・12,000 段の `else if`・20,000 項の `||` (F1: 循環的複雑度)。
fn write_deep_functions(repo: &TestRepo) {
    repo.write(
        "fn_parens.js",
        format!(
            "function f() {{ return {}; }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let mut elif = String::from("function g(code) { if (code === 0) { return 0; }");
    for i in 1..12_000 {
        elif.push_str(&format!(" else if (code === {i}) {{ return {i}; }}"));
    }
    elif.push_str(" return -1; }\n");
    repo.write("elif.js", elif);
    let terms: Vec<String> = (0..20_000).map(|i| format!("a === {i}")).collect();
    repo.write(
        "or.js",
        format!("function isKw(a) {{ return {}; }}\n", terms.join(" || ")),
    );
    repo.write("ok.js", "export function okay(x) { return x ? 1 : 2; }\n");
}

/// symbols の `--paths` と `--dir` が深い関数本体で落ちず、循環的複雑度も数えきる。
#[test]
fn symbols_handles_deep_function_bodies() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    let cx_of = |records: &[serde_json::Value], name: &str| -> Option<u64> {
        records
            .iter()
            .flat_map(|r| r["symbols"].as_array().cloned().unwrap_or_default())
            .find(|s| s["name"] == name)
            .and_then(|s| s["cx"].as_u64())
    };

    for file in ["fn_parens.js", "elif.js", "or.js"] {
        let mut command = cargo_bin();
        command.current_dir(repo.root()).args([
            "symbols",
            "--no-cache",
            "--paths",
            &format!("{file},ok.js"),
        ]);
        let stdout = assert_finished(file, &run_bounded(command, None));
        let records = parse_lines(&stdout);
        assert_eq!(records.len(), 2, "{file}: 2 ファイル分の結果: {stdout}");
        assert_eq!(
            cx_of(&records, "okay"),
            Some(2),
            "{file}: 同じ要求の正常なファイル"
        );
    }

    let mut command = cargo_bin();
    command
        .current_dir(repo.root())
        .args(["symbols", "--no-cache", "--dir", "."]);
    let stdout = assert_finished("symbols --dir", &run_bounded(command, None));
    let records = parse_lines(&stdout);
    assert_eq!(
        cx_of(&records, "g"),
        Some(12_001),
        "else if の連鎖を全段数える"
    );
    assert_eq!(cx_of(&records, "f"), Some(1));
    assert_eq!(cx_of(&records, "okay"), Some(2));
}

/// C++ の目印 (コメント中の `::`) を持つ深い `.h` で、言語判定 (C と C++ の
/// parse error の比較) が落ちない (F2)。refs の単一・バッチ検索と dead-code の
/// 件数集計の入口で毎回通る。深い初期化子の後ろの呼び出しも数える。
#[test]
fn refs_and_dead_code_handle_deep_cpp_marked_header() {
    let repo = TestRepo::new();
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\nint after_deep(void) {{ return helper(); }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    repo.write(
        "ok.c",
        "int helper(void) { return 1; }\nint use_deep(void) { return deep_value; }\n",
    );

    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "helper", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs --name", &run_bounded(command, None));
    let refs = parse_lines(&stdout);
    assert!(
        refs[0]["refs"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r["path"] == "deep.h" && r["ln"] == 2)),
        "深い初期化子の後ろの呼び出しを返す: {stdout}"
    );

    let mut command = cargo_bin();
    command
        .args(["refs", "--names", "deep_value,helper", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs --names", &run_bounded(command, None));
    assert_eq!(parse_lines(&stdout).len(), 2, "{stdout}");

    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(repo.root());
    let stdout = assert_finished("dead-code", &run_bounded(command, None));
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
    let dead: Vec<&str> = json["dead_symbols"]
        .as_array()
        .expect("dead_symbols")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !dead.contains(&"helper"),
        "深いヘッダからしか呼ばれない helper は dead ではない: {dead:?}"
    );
}

/// Rust: 検索名が closure の引数に束縛されているとき、ファイル内の宣言を調べる処理が
/// 深い式で落ちない (F3-a)。closure の束縛は数えず、深い式の後ろの呼び出しは数える。
#[test]
fn refs_handles_closure_binding_check_in_deep_rust_file() {
    let repo = TestRepo::new();
    repo.write(
        "lib.rs",
        format!(
            "pub fn tail() -> u8 {{ 1 }}\npub fn run(v: &[u8]) -> usize {{ v.iter().map(|tail| *tail).count() }}\npub const DEEP: u32 = {};\npub fn after() -> u8 {{ tail() }}\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "tail", "--dir"])
        .arg(repo.root());
    let stdout = assert_finished("refs", &run_bounded(command, None));
    let refs: Vec<(u64, String)> = parse_lines(&stdout)[0]["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .map(|r| {
            (
                r["ln"].as_u64().unwrap_or_default(),
                r["kind"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        refs,
        vec![(0, "def".to_string()), (3, "ref".to_string())],
        "定義と深い式の後ろの呼び出しだけ (closure の束縛は数えない)"
    );
}

/// dead-code の member liveness (同名メンバーの帰属) が、深いファイルで落ちない (F3-b / F3-c)。
/// 深いファイルの後ろの呼び出しからしか使われないメンバーを dead にしない。
#[test]
fn dead_code_member_liveness_handles_deep_files() {
    let deep_array = nested("[", "1", "]", 50_000);

    // TS: A.fmt と B.fmt の同名メンバー。B.fmt は深い配列の後ろからだけ呼ばれる。
    let ts = TestRepo::new();
    ts.write("a.ts", "export class A { fmt() { return \"a\"; } }\n");
    ts.write("b.ts", "export class B { fmt() { return \"b\"; } }\n");
    ts.write(
        "c.ts",
        "import { A } from \"./a\";\nexport const out = new A().fmt();\n",
    );
    ts.write(
        "deep.ts",
        format!(
            "import {{ B }} from \"./b\";\nconst D = {deep_array};\nexport function useD() {{ return [D, new B().fmt()]; }}\n"
        ),
    );

    // PHP: 同名の fmt の組があると、型の索引は無関係な深いファイルにも当たる。
    let php = TestRepo::new();
    php.write(
        "A.php",
        "<?php\nclass A { public function fmt() { return 1; } }\n",
    );
    php.write(
        "B.php",
        "<?php\nclass B { public function fmt() { return 2; } }\n",
    );
    php.write("c.php", "<?php\nfunction callA() { return A::fmt(); }\n");
    php.write(
        "deep.php",
        format!("<?php\n$d = {deep_array};\nfunction callB() {{ return B::fmt(); }}\n"),
    );

    for (label, repo, live) in [("ts", &ts, "B.fmt"), ("php", &php, "B.fmt")] {
        let mut command = cargo_bin();
        command.args(["dead-code", "--dir"]).arg(repo.root());
        let stdout = assert_finished(label, &run_bounded(command, None));
        let json: serde_json::Value = serde_json::from_str(&stdout).expect("dead-code JSON");
        let dead: Vec<&str> = json["dead_symbols"]
            .as_array()
            .expect("dead_symbols")
            .iter()
            .filter_map(|s| s["name"].as_str())
            .collect();
        assert!(
            !dead.contains(&live),
            "{label}: 深いファイルの後ろからだけ使う {live} は dead ではない: {dead:?}"
        );
    }
}

/// Angular の ControlValueAccessor 判定が、decorator の深い引数で落ちない。
/// Bash の trap の handler 文字列の深いサブシェルでも refs が落ちない。
#[test]
fn deep_decorator_and_trap_handler_do_not_crash() {
    let ng = TestRepo::new();
    ng.write(
        "c.ts",
        format!(
            "@Component({{ meta: {} }})\nexport class C {{\n  writeValue(v: unknown) {{ return v; }}\n}}\n",
            nested("[", "1", "]", 300_000)
        ),
    );
    ng.write(
        "use.ts",
        "import { C } from \"./c\";\nexport const c = new C();\n",
    );
    let mut command = cargo_bin();
    command.args(["dead-code", "--dir"]).arg(ng.root());
    assert_finished("angular dead-code", &run_bounded(command, None));

    let sh = TestRepo::new();
    sh.write(
        "a.sh",
        format!(
            "cleanup() {{ :; }}\ntrap '{}' INT\nafter() {{ cleanup; }}\n",
            nested("( ", "cleanup", " )", 50_000)
        ),
    );
    let mut command = cargo_bin();
    command
        .args(["refs", "--name", "cleanup", "--dir"])
        .arg(sh.root());
    let stdout = assert_finished("trap refs", &run_bounded(command, None));
    let lines: Vec<u64> = parse_lines(&stdout)[0]["refs"]
        .as_array()
        .expect("refs")
        .iter()
        .filter_map(|r| r["ln"].as_u64())
        .collect();
    assert_eq!(
        lines,
        vec![0, 1, 2],
        "定義・trap の handler・後ろの呼び出し"
    );
}

/// PHP の `$x->m()` の受け手の型推論が、関数本体の深い配列で落ちない (impact / context)。
#[test]
fn impact_handles_deep_php_function_body() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Repo.php",
        "<?php\nclass Repo {\n    public function store(array $row): bool { return true; }\n}\n",
    );
    repo.write(
        "Caller.php",
        format!(
            "<?php\nfunction persist(Repo $repo) {{\n    $d = {};\n    return $repo->store([]);\n}}\n",
            nested("[", "1", "]", 50_000)
        ),
    );
    repo.commit_all("init");
    repo.write(
        "Repo.php",
        "<?php\nclass Repo {\n    public function store(array $row, bool $flush): bool { return true; }\n}\n",
    );
    let mut command = cargo_bin();
    command.args(["context", "--git", "--dir"]).arg(repo.root());
    let stdout = assert_finished("context", &run_bounded(command, None));
    assert!(
        stdout.contains("Caller.php"),
        "深い本体の後ろの呼び出し元を返す: {}",
        stdout.chars().take(600).collect::<String>()
    );
}

/// session は深い入力の要求の後も、同じ接続で後続の要求を処理する。
#[test]
fn session_keeps_serving_after_deep_inputs() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let input = [
        r#"{"command":"symbols","path":"ok.js"}"#,
        r#"{"command":"symbols","path":"fn_parens.js"}"#,
        r#"{"command":"symbols","path":"elif.js"}"#,
        r#"{"command":"refs","name":"deep_value","dir":"."}"#,
        r#"{"command":"symbols","path":"ok.js"}"#,
    ]
    .join("\n")
        + "\n";
    let mut command = cargo_bin();
    command
        .arg("session")
        .env("ASTRO_SIGHT_WORKSPACE", repo.root());
    let stdout = assert_finished("session", &run_bounded(command, Some(input)));
    let responses = parse_lines(&stdout);
    assert_eq!(responses.len(), 5, "5 要求すべてに応答する: {stdout}");
    for response in &responses {
        assert!(response.get("error").is_none(), "{response}");
    }
    assert!(
        responses[3]["refs"]
            .as_array()
            .is_some_and(|r| !r.is_empty())
    );
    assert_eq!(responses[4]["symbols"][0]["name"], "okay");
}

/// MCP サーバーは深い入力のツール呼び出しの後も、同じ接続で後続の要求に応答する。
/// ツールは tokio のワーカースレッドで走るので、そこでも落ちないことを確かめる。
#[test]
fn mcp_keeps_serving_after_deep_inputs() {
    let repo = TestRepo::new();
    write_deep_functions(&repo);
    repo.write(
        "deep.h",
        format!(
            "// see foo::bar\nint deep_value = {};\n",
            nested("(", "1", ")", 50_000)
        ),
    );
    let call = |id: u64, name: &str, arguments: serde_json::Value| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        })
        .to_string()
    };
    let requests = [
        call(
            2,
            "symbols_extract",
            serde_json::json!({ "path": "fn_parens.js" }),
        ),
        call(
            3,
            "symbols_extract",
            serde_json::json!({ "path": "elif.js" }),
        ),
        call(
            4,
            "refs_search",
            serde_json::json!({ "name": "deep_value", "dir": "." }),
        ),
        call(5, "symbols_extract", serde_json::json!({ "path": "ok.js" })),
    ];
    let stdout = mcp_requests_waiting_for_responses(repo.root(), &requests, &[2, 3, 4, 5]);
    for id in 2..=5 {
        let line = stdout
            .lines()
            .find(|line| line.contains(&format!("\"id\":{id}")))
            .unwrap_or_else(|| panic!("id={id} の応答が必要: {stdout}"));
        let json: serde_json::Value = serde_json::from_str(line).expect("valid JSON-RPC");
        assert!(json.get("result").is_some(), "id={id}: {json}");
        assert_ne!(json["result"]["isError"], true, "id={id}: {json}");
    }
}
