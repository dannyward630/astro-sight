//! パス境界・サンドボックス・diff 入力検証の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

// ---- フェーズ 2: セキュリティテスト ----

#[test]
fn sandboxed_service_rejects_path_traversal() {
    // AppService::sandboxed はワークスペース外のパスを拒否する。
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // ワークスペース外に実在するファイルを指定する (/etc/hosts は Windows に無く、境界の判定の前に
    // FILE_NOT_FOUND になってしまう)。一時ディレクトリはワークスペース (リポジトリ) の外にある。
    let outside = tempfile::Builder::new()
        .suffix(".rs")
        .tempfile()
        .expect("create a file outside the workspace");
    let params = astro_sight::service::AstParams {
        path: outside.path().to_str().expect("utf-8 temp path"),
        line: None,
        col: None,
        end_line: None,
        end_col: None,
        depth: 3,
        context_lines: 3,
    };
    let result = service.extract_ast(&params);
    assert!(result.is_err(), "ワークスペース外のパスは拒否されるべき");

    let err_msg = match result {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        err_msg.contains("outside workspace") || err_msg.contains("PATH_OUT_OF_BOUNDS"),
        "エラーにワークスペース境界が含まれるべき: {err_msg}"
    );
}

/// cochange の起点に git の pathspec magic (`:/x` / `:(top)x`) を渡しても、ワークスペース外の
/// ファイルの履歴を読ませない。
///
/// `..` は字句検証で PATH_OUT_OF_BOUNDS になるが、magic は素通りして git がリポジトリの
/// ルートから解決していた。ワークスペース外のファイルと共変更が取れ、`commits_analyzed`
/// の 0 / 非 0 がファイルの存在の手掛かりにもなっていた。**対照**: ワークスペース内の
/// 起点は従来どおり履歴を読む。
#[test]
fn sandboxed_cochange_treats_pathspec_magic_literally() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("ws");
    repo.create_dir_all("secret");
    for round in 0..5 {
        repo.write("ws/a.rs", format!("pub fn a() -> u32 {{ {round} }}\n"));
        repo.write(
            "secret/hidden.rs",
            format!("pub fn h() -> u32 {{ {round} }}\n"),
        );
        repo.commit_all(&format!("round {round}"));
    }
    let workspace = std::fs::canonicalize(repo.path("ws")).unwrap();
    let service = astro_sight::service::AppService::sandboxed(workspace).unwrap();
    let analyze = |source: &str| {
        let opts = astro_sight::models::cochange::CoChangeOptions {
            source_files: vec![source.to_string()],
            min_confidence: 0.0,
            min_samples: 1,
            ..Default::default()
        };
        service
            .analyze_cochange(".", &opts)
            .unwrap_or_else(|e| panic!("{source}: {e}"))
    };

    for magic in [":/secret/hidden.rs", ":(top)secret/hidden.rs"] {
        let result = analyze(magic);
        assert_eq!(
            result.commits_analyzed, 0,
            "{magic}: ワークスペース外の履歴を読まないこと"
        );
        assert!(result.entries.is_empty(), "{magic}: {:?}", result.entries);
    }
    assert!(
        analyze("a.rs").commits_analyzed > 0,
        "ワークスペース内の起点は履歴を読むこと"
    );
}

/// サンドボックス外のパスは、存在の有無や種類に関係なく PATH_OUT_OF_BOUNDS になる。
///
/// 旧実装は境界の確認より先に「存在しない (FILE_NOT_FOUND)」「ディレクトリではない
/// (INVALID_REQUEST)」を返していたため、エラーコードの違いからワークスペース外の
/// ファイルの有無と種類が分かった。**対照**: ワークスペース内の欠けたファイルやファイルを
/// ディレクトリとして渡した場合は、従来どおり FILE_NOT_FOUND / INVALID_REQUEST。
#[test]
fn sandboxed_service_does_not_reveal_outside_paths_through_error_codes() {
    use astro_sight::error::{AstroError, ErrorCode};

    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("ws");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "fn inside() {}\n").unwrap();
    std::fs::write(outside.join("secret.rs"), "fn secret() {}\n").unwrap();
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let service = astro_sight::service::AppService::sandboxed(workspace).unwrap();

    let code_of = |result: anyhow::Result<()>| -> ErrorCode {
        result
            .expect_err("エラーになること")
            .downcast_ref::<AstroError>()
            .map(|e| e.code)
            .expect("AstroError であること")
    };
    let symbols = |path: &str| code_of(service.extract_symbols(path).map(|_| ()));
    let refs_in = |dir: &str| code_of(service.find_references("inside", dir, None).map(|_| ()));

    for path in [
        "../outside/secret.rs",
        "../outside/missing.rs",
        "../missing-dir/x.rs",
    ] {
        assert_eq!(symbols(path), ErrorCode::PathOutOfBounds, "{path}");
    }
    for dir in ["../outside/secret.rs", "../outside", "../outside/missing"] {
        assert_eq!(refs_in(dir), ErrorCode::PathOutOfBounds, "{dir}");
    }
    assert_eq!(symbols("src/missing.rs"), ErrorCode::FileNotFound);
    assert_eq!(refs_in("src/lib.rs"), ErrorCode::InvalidRequest);
    assert_eq!(refs_in("missing"), ErrorCode::FileNotFound);
}

#[test]
fn sandboxed_service_allows_workspace_paths() {
    // AppService::sandboxed はワークスペース内のパスを許可する。
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // src/lib.rs はワークスペース内にある。
    let result = service.extract_symbols("src/lib.rs");
    assert!(result.is_ok(), "ワークスペース内のパスは許可されるべき");
}

#[test]
fn sandboxed_service_rejects_file_workspace_root() {
    let dir = tempfile::TempDir::new().unwrap();
    let file_path = dir.path().join("workspace.txt");
    std::fs::write(&file_path, "not a directory").unwrap();

    let result = astro_sight::service::AppService::sandboxed(file_path.clone());
    assert!(
        result.is_err(),
        "ファイルをワークスペースルートにしてはいけない"
    );

    let err_msg = match result {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        err_msg.contains("directory"),
        "エラーにディレクトリ要件が含まれるべき: {err_msg}"
    );
}

// ===========================================================================
// 境界値・異常系・エッジケーステスト
// ===========================================================================

// ---- diff パーサー境界値テスト ----

#[test]
fn diff_empty_input_produces_no_files() {
    // 空の diff 入力は空の結果を返すべき
    let files = astro_sight::engine::diff::parse_unified_diff("");
    assert!(files.is_empty(), "空の diff は空の結果を返すべき");
}

#[test]
fn diff_deleted_file_with_dev_null() {
    // ファイル削除（+++ /dev/null）を正しくパースすること
    let diff = r#"--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-fn old_fn() {}
-fn another() {}
-// end
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].old_path, "src/old.rs");
    assert_eq!(files[0].new_path, "/dev/null");
    assert_eq!(files[0].hunks[0].new_count, 0);
}

#[test]
fn diff_hunk_header_only_no_content_lines() {
    // ハンクヘッダのみで内容行がない diff
    let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,3 @@
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].hunks.len(), 1);
}

#[test]
fn diff_missing_hunk_header_produces_no_file() {
    // ハンクヘッダがない場合はファイルとして認識しない（hunks が空）
    let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
"#;
    let files = astro_sight::engine::diff::parse_unified_diff(diff);
    assert!(
        files.is_empty(),
        "ハンクなしの diff はファイルを生成しないべき"
    );
}

// ---- AppService 入力サイズ制限テスト ----

#[test]
fn sandboxed_service_validates_input_size() {
    let cwd = std::env::current_dir().unwrap();
    let cwd = std::fs::canonicalize(cwd).unwrap();
    let service = astro_sight::service::AppService::sandboxed(cwd).unwrap();

    // sandboxed は max_input_size = 100MB に設定される。
    // analyze_context で validate_input_size が呼ばれるため、
    // 小さい入力は通ること、巨大入力は別のテスト環境で確認。
    let result = service.analyze_context(
        "",
        ".",
        &astro_sight::models::impact::ContextAnalysisOptions::default(),
    );
    assert!(result.is_ok(), "空の diff 入力はサイズ制限を通過するべき");
}

#[test]
fn git_repo_invalid_base_still_errors() {
    // 正常 repo (プロジェクト自身) で不正 base → exit 1 を維持 (R4)。
    let output = cargo_bin()
        .args([
            "review",
            "--dir",
            ".",
            "--git",
            "--base",
            "no-such-ref-xyz-astro",
        ])
        .output()
        .expect("run");
    assert!(!output.status.success(), "不正 base は exit 1 維持");
}

#[test]
fn git_repo_dash_base_rejected() {
    // 先頭 '-' の base は git 管理下でも入力契約違反で exit 1。
    let output = cargo_bin()
        .args(["review", "--dir", ".", "--git", "--base=-x"])
        .output()
        .expect("run");
    assert!(!output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(json["error"]["code"], "INVALID_REQUEST");
}
