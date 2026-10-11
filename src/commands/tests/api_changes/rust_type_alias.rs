//! Rust の透過的な type alias 導入を API 契約変更と区別するテスト。

use crate::commands::tests::common::*;
use crate::commands::*;
use std::fs;

#[test]
fn rust_local_type_alias_equivalence_is_proven_conservatively() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib.rs",
                "pub mod consumer;\n\npub fn collect() -> Vec<(u64, String)> { vec![] }\npub fn changed() -> Vec<(u64, String)> { vec![] }\npub fn generic() -> Vec<(u64, String)> { vec![] }\n",
            ),
            (
                "src/consumer.rs",
                "use crate::{changed, collect, generic};\n\npub fn run() {\n    let _ = collect();\n    let _ = changed();\n    let _ = generic();\n}\n",
            ),
        ],
        "initial",
    );

    fs::write(
        repo.join("src/lib.rs"),
        "pub mod consumer;\n\npub type Entry = (u64, String);\npub type ChangedEntry = (u32, String);\npub type GenericEntry<T> = (T, String);\n\npub fn collect() -> Vec<Entry> { vec![] }\npub fn changed() -> Vec<ChangedEntry> { vec![] }\npub fn generic() -> Vec<GenericEntry<u64>> { vec![] }\n",
    )
    .expect("write changed lib.rs");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 9,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api.modified.iter().any(|change| change.name == "collect"),
        "同一の public non-generic alias への置換は api.mod に出すべきでない: {:?}",
        api.modified
    );
    assert!(
        api.compatible_modified.iter().any(|change| {
            change.name == "collect" && change.reason == "transparent_rust_type_alias"
        }),
        "同値と証明した alias 導入は理由付き informational に残すべき: {:?}",
        api.compatible_modified
    );
    assert!(
        api.modified.iter().any(|change| change.name == "changed"),
        "alias RHS が元の型と異なる場合は blocking を維持すべき: {:?}",
        api.modified
    );
    assert!(
        api.modified.iter().any(|change| change.name == "generic"),
        "generic alias は未証明として blocking を維持すべき: {:?}",
        api.modified
    );
}

#[test]
fn rust_nested_alias_shadow_does_not_use_root_alias_for_equivalence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib.rs",
                "pub mod consumer;\npub type Entry = (u64, String);\npub mod inner { pub fn collect() -> Vec<(u64, String)> { vec![] } }\n",
            ),
            (
                "src/consumer.rs",
                "pub fn run() { let _ = crate::inner::collect(); }\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod consumer;\npub type Entry = (u64, String);\npub mod inner { pub type Entry = (u32, String); pub fn collect() -> Vec<Entry> { vec![] } }\n",
    )
    .expect("write changed lib.rs");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 3,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|change| change.name == "collect"),
        "nested scope の同名 alias を root alias で展開して破壊的変更を消してはならない: {:?}",
        api.modified
    );
    assert!(
        !api.compatible_modified
            .iter()
            .any(|change| change.name == "collect"),
        "nested alias shadow は同値変更へ降格してはならない: {:?}",
        api.compatible_modified
    );
}

#[test]
fn rust_nested_import_scope_does_not_use_root_alias_for_equivalence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/lib.rs",
                "pub mod consumer;\npub struct ExtA; pub struct ExtB;\nuse crate::ExtA as Ext;\npub type Entry = (Ext, String);\npub mod inner { use crate::ExtB as Ext; pub fn collect() -> Vec<(Ext, String)> { vec![] } }\n",
            ),
            (
                "src/consumer.rs",
                "pub fn run() { let _ = crate::inner::collect(); }\n",
            ),
        ],
        "initial",
    );
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod consumer;\npub struct ExtA; pub struct ExtB;\nuse crate::ExtA as Ext;\npub type Entry = (Ext, String);\npub mod inner { use crate::ExtB as Ext; use super::Entry; pub fn collect() -> Vec<Entry> { vec![] } }\n",
    )
    .expect("write changed lib.rs");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/lib.rs".to_string(),
        new_path: "src/lib.rs".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|change| change.name == "collect"),
        "nested import が RHS leaf を別型へ束縛する変更は blocking を維持すべき: {:?}",
        api.modified
    );
    assert!(
        !api.compatible_modified
            .iter()
            .any(|change| change.name == "collect"),
        "root alias を nested function へ適用してはならない: {:?}",
        api.compatible_modified
    );
}

#[test]
fn rust_alias_const_expressions_keep_api_mod_blocking() {
    for (label, old_source, new_source) in [
        (
            "const function",
            "pub const fn size() -> usize { 4 }\npub fn collect() -> [u8; size()] { [0; size()] }\n",
            "pub const fn size() -> usize { 8 }\npub type Entry = [u8; size()];\npub fn collect() -> Entry { [0; size()] }\n",
        ),
        (
            "associated const",
            "pub struct Foo;\nimpl Foo { pub const N: usize = 4; }\npub fn collect() -> [u8; Foo::N] { [0; Foo::N] }\n",
            "pub struct Foo;\nimpl Foo { pub const N: usize = 8; }\npub type Entry = [u8; Foo::N];\npub fn collect() -> Entry { [0; Foo::N] }\n",
        ),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path();
        init_git_repo_for_test(repo);
        git_commit_files(
            repo,
            &[
                ("src/lib.rs", old_source),
                (
                    "src/consumer.rs",
                    "pub fn run() { let _ = crate::collect(); }\n",
                ),
            ],
            "initial",
        );
        fs::write(repo.join("src/lib.rs"), new_source).expect("write changed lib.rs");
        let diff_files = vec![crate::models::impact::DiffFile {
            old_path: "src/lib.rs".to_string(),
            new_path: "src/lib.rs".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: old_source.lines().count(),
                new_start: 1,
                new_count: new_source.lines().count(),
            }],
            deleted_old_source: None,
        }];
        let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
        assert!(
            api.modified.iter().any(|change| change.name == "collect"),
            "{label}: value-dependent type must stay blocking: {:?}",
            api.modified
        );
        assert!(
            !api.compatible_modified
                .iter()
                .any(|change| change.name == "collect"),
            "{label}: value-dependent type must not be treated as compatible"
        );
    }
}
