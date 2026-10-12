//! Named and star barrels must carry impact to the final use site.

use super::support::*;

fn paths(value: &serde_json::Value, bucket: &str) -> Vec<String> {
    value["changes"][0][bucket]
        .as_array()
        .map(|callers| {
            callers
                .iter()
                .filter_map(|caller| caller["path"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn ts_barrels_propagate_only_proven_origins() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/lib/storage");
    repo.write(
        "tsconfig.json",
        "{\n // project alias\n \"compilerOptions\": {\"paths\": {\"@/*\": [\"src/*\", // fallback\n ],},},\n}\n",
    );
    repo.write("src/lib/storage/impl.ts", "export const LIMIT = 10;\n");
    repo.write(
        "src/lib/storage/index.ts",
        "export { LIMIT } from './impl';\n",
    );
    repo.write("src/lib/empty.ts", "export const OTHER = 1;\n");
    repo.write(
        "src/lib/index.ts",
        "export * from './empty';\nexport * from './storage';\n",
    );
    repo.write(
        "src/a_rel.ts",
        "import { LIMIT } from './lib/storage';\nexport const a = LIMIT + 1;\n",
    );
    repo.write(
        "src/b_alias.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const b = LIMIT + 1;\n",
    );
    repo.write(
        "src/c_multi.ts",
        "import { LIMIT } from '@/lib';\nexport const c = LIMIT + 1;\n",
    );
    repo.write("src/other.ts", "export const LIMIT = 99;\n");
    repo.write(
        "src/z_unrelated.ts",
        "import { LIMIT } from './other';\nexport const z = LIMIT + 1;\n",
    );
    repo.write(
        "src/property.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const property = object.LIMIT;\n",
    );
    repo.create_dir_all("apps/web/src/lib/storage");
    repo.write(
        "apps/web/tsconfig.json",
        "{\"compilerOptions\":{\"paths\":{\"@/*\":[\"./src/*\"]}}}",
    );
    repo.write(
        "apps/web/src/lib/storage/index.ts",
        "export const LIMIT = 3;\n",
    );
    repo.write(
        "apps/web/src/page.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const page = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/lib/storage/impl.ts", "export const LIMIT = 20;\n");

    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"][0]["path"], "src/lib/storage/impl.ts");
    let impacted = paths(&context, "impacted_callers");
    for expected in ["src/a_rel.ts", "src/b_alias.ts", "src/c_multi.ts"] {
        assert!(
            impacted.contains(&expected.to_owned()),
            "{expected}: {context}"
        );
    }
    assert!(
        !impacted.contains(&"src/z_unrelated.ts".to_owned()),
        "{context}"
    );
    assert!(
        !impacted.contains(&"apps/web/src/page.ts".to_owned()),
        "{context}"
    );
    assert!(
        !impacted.contains(&"src/property.ts".to_owned()),
        "{context}"
    );
    assert!(
        paths(&context, "low_confidence_callers").contains(&"src/z_unrelated.ts".to_owned()),
        "{context}"
    );
    assert!(
        paths(&context, "informational_callers").contains(&"src/lib/storage/index.ts".to_owned()),
        "{context}"
    );
    let impact = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(
        !impact.status.success(),
        "{}",
        String::from_utf8_lossy(&impact.stderr)
    );
}

#[test]
fn ts_root_barrel_resolves_relative_paths_without_a_parent_directory() {
    let repo = TestRepo::new();
    repo.write("impl.ts", "export const LIMIT = 10;\n");
    repo.write("index.ts", "export { LIMIT } from './impl.js';\n");
    repo.write(
        "consumer.ts",
        "import { LIMIT } from './index';\nexport const result = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("impl.ts", "export const LIMIT = 20;\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        paths(&context, "impacted_callers").contains(&"consumer.ts".to_owned()),
        "{context}"
    );
}

#[test]
fn ts_barrel_ambiguity_and_local_export_do_not_prove_origin() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "tsconfig.json",
        "{\"compilerOptions\":{\"paths\":{\"@/storage\":[\"src/other.ts\"],\"@/*\":[\"src/*\"]}}}",
    );
    repo.write(
        "src/impl.ts",
        "export const LIMIT = 10;\nexport const OTHER = 5;\n",
    );
    repo.write("src/other.ts", "export const LIMIT = 99;\n");
    repo.write("src/storage.ts", "export { LIMIT } from './impl';\n");
    repo.write(
        "src/ambiguous.ts",
        "export * from './impl';\nexport * from './other';\n",
    );
    repo.write(
        "src/overridden.ts",
        "export * from './impl';\nexport class LIMIT {}\n",
    );
    repo.write(
        "src/unresolved.ts",
        "export * from './impl';\nexport * from './missing';\n",
    );
    repo.write(
        "src/namespace.ts",
        "export * from './impl';\nexport * as LIMIT from './other';\n",
    );
    repo.write(
        "src/destructured.ts",
        "export * from './impl';\nexport const { LIMIT } = source;\n",
    );
    repo.create_dir_all("src/bad");
    repo.write("src/bad.ts", "export const LIMIT = ;\n");
    repo.write("src/bad/index.ts", "export * from '../impl';\n");
    repo.write(
        "src/renamed.ts",
        "export { OTHER as LIMIT } from './impl';\n",
    );
    repo.write(
        "src/a.ts",
        "import { LIMIT } from './ambiguous';\nexport const a = LIMIT;\n",
    );
    repo.write(
        "src/b.ts",
        "import { LIMIT } from './overridden';\nexport const b = LIMIT;\n",
    );
    repo.write(
        "src/c.ts",
        "import { LIMIT } from './unresolved';\nexport const c = LIMIT;\n",
    );
    repo.write(
        "src/d.ts",
        "import { LIMIT } from './namespace';\nexport const d = LIMIT;\n",
    );
    repo.write(
        "src/e.ts",
        "import { LIMIT } from './renamed';\nexport const e = LIMIT;\n",
    );
    repo.write(
        "src/f.ts",
        "import { LIMIT } from './bad';\nexport const f = LIMIT;\n",
    );
    repo.write(
        "src/g.ts",
        "import { LIMIT } from '@/storage';\nexport const g = LIMIT;\n",
    );
    repo.write(
        "src/h.ts",
        "import { LIMIT } from './destructured';\nexport const h = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/impl.ts",
        "export const LIMIT = 20;\nexport const OTHER = 5;\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let impacted = paths(&context, "impacted_callers");
    assert!(!impacted.contains(&"src/a.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/b.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/c.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/d.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/e.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/f.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/g.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/h.ts".to_owned()), "{context}");
}
