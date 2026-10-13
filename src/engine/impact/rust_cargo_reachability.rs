//! Conservative Cargo package reachability for Rust impact candidates.
//!
//! A source file in one package cannot name a declaration from another package
//! unless Cargo can make that package available through a dependency. Unknown
//! manifests, source inclusion, and custom build configuration keep the old
//! impact result. This proof is used only by impact, not refs or dead-code.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tree_sitter::Node;

use crate::engine::bounded_read::{read_input_file, read_input_text_file};
use crate::engine::parser;
use crate::language::LangId;

struct Package {
    directory: PathBuf,
    name: String,
    manifest: toml::Value,
    dependencies: Vec<usize>,
}

struct CargoIndex {
    packages: Vec<Package>,
}

impl CargoIndex {
    fn owner(&self, path: &str) -> Option<usize> {
        let path = Path::new(path);
        if !path.extension()?.to_str()?.eq_ignore_ascii_case("rs") {
            return None;
        }
        self.packages
            .iter()
            .enumerate()
            .filter(|(_, package)| path.starts_with(package.directory.join("src")))
            .max_by_key(|(_, package)| package.directory.components().count())
            .map(|(ix, _)| ix)
    }

    fn may_reach(&self, from: usize, target: usize) -> bool {
        let mut pending = vec![from];
        let mut seen = HashSet::new();
        while let Some(package) = pending.pop() {
            if package == target {
                return true;
            }
            if seen.insert(package) {
                pending.extend(self.packages[package].dependencies.iter().copied());
            }
        }
        false
    }
}

/// One impact request owns one lazy index, shared by all reference workers.
pub(super) struct RustCargoReachability {
    root: PathBuf,
    enabled: bool,
    index: OnceLock<Option<CargoIndex>>,
}

impl RustCargoReachability {
    pub(super) fn new(root: &Path, enabled: bool) -> Self {
        Self {
            root: root.to_owned(),
            enabled,
            index: OnceLock::new(),
        }
    }

    pub(super) fn proves_unreachable(&self, reference: &str, changed: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(index) = self.index.get_or_init(|| scan_workspace(&self.root)) else {
            return false;
        };
        let (Some(reference_owner), Some(changed_owner)) =
            (index.owner(reference), index.owner(changed))
        else {
            return false;
        };
        reference_owner != changed_owner && !index.may_reach(reference_owner, changed_owner)
    }
}

/// The subset accepted here is intentionally smaller than Cargo's manifest
/// language. Every unhandled form makes the proof unavailable.
fn scan_workspace(root: &Path) -> Option<CargoIndex> {
    let root_source = read_input_text_file(&root.join("Cargo.toml"))?;
    let root_value = toml::from_str::<toml::Value>(&root_source).ok()?;
    let root_manifest = root_value.get("workspace")?.as_table()?.clone();
    if root_manifest.get("exclude").is_some() {
        return None;
    }
    let members = root_manifest.get("members")?.as_array()?;
    let mut member_directories = HashSet::new();
    for member in members {
        let member = member.as_str()?;
        if member.contains(['*', '?', '[', ']']) {
            return None;
        }
        member_directories.insert(canonical_relative(root, Path::new(""), member)?);
    }

    let mut pending = vec![root.to_owned()];
    let mut manifests = vec![(PathBuf::new(), root_value)];
    let mut rust_files = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            let name = entry.file_name();
            // Nested directories with these names can be ordinary Rust modules.
            // At the root, vendor and node_modules may contain unindexed crates
            // that compile workspace source, so their presence prevents proof.
            if directory == root {
                match name.to_str() {
                    Some(".git" | "target") => continue,
                    Some("vendor" | "node_modules") => return None,
                    _ => {}
                }
            }
            if name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(".cargo"))
            {
                // rustflags can supply an extra --extern and break the manifest proof.
                return None;
            }
            if kind.is_symlink() {
                if std::fs::metadata(&path).is_ok_and(|target| target.is_dir())
                    || path.extension().is_some_and(|ext| match ext.to_str() {
                        Some(ext) => {
                            !ext.is_ascii()
                                || ext.eq_ignore_ascii_case("rs")
                                || ext.eq_ignore_ascii_case("toml")
                        }
                        None => true,
                    })
                {
                    return None;
                }
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                if directory == root {
                    continue;
                }
                let source = read_input_text_file(&path)?;
                let manifest = toml::from_str::<toml::Value>(&source).ok()?;
                let relative = directory.strip_prefix(root).ok()?.to_owned();
                manifests.push((relative, manifest));
            } else if kind.is_file() {
                let Some(extension) = path.extension() else {
                    continue;
                };
                let extension = extension.to_str()?;
                // Case-insensitive filesystems can fold non-ASCII characters
                // (for example long s) into `.rs` during module resolution.
                if !extension.is_ascii() {
                    return None;
                }
                if extension.eq_ignore_ascii_case("rs") {
                    rust_files.push(path.strip_prefix(root).ok()?.to_owned());
                }
            }
        }
    }

    let manifest_directories: HashSet<&Path> = manifests
        .iter()
        .map(|(directory, _)| directory.as_path())
        .collect();
    if member_directories
        .iter()
        .any(|directory| !manifest_directories.contains(directory.as_path()))
    {
        return None;
    }

    let mut packages = Vec::new();
    for (directory, manifest) in manifests {
        if manifest.get("patch").is_some() || manifest.get("replace").is_some() {
            return None;
        }
        if directory.as_os_str().is_empty() {
            if manifest.get("package").is_none() {
                continue;
            }
        } else if !member_directories.contains(&directory) || manifest.get("workspace").is_some() {
            return None;
        }
        let package = manifest.get("package")?;
        if package.get("workspace").is_some() || custom_target_path(&manifest) {
            return None;
        }
        let name = package.get("name")?.as_str()?.to_owned();
        // Sysroot crates can be visible without an ordinary Cargo dependency.
        if matches!(name.as_str(), "std" | "core" | "alloc") {
            return None;
        }
        packages.push(Package {
            directory,
            name,
            manifest,
            dependencies: Vec::new(),
        });
    }
    if packages.len() < 2 {
        return None;
    }
    let mut by_name = HashMap::new();
    let mut by_directory = HashMap::new();
    for (ix, package) in packages.iter().enumerate() {
        if by_name.insert(package.name.clone(), ix).is_some()
            || by_directory.insert(package.directory.clone(), ix).is_some()
        {
            return None;
        }
    }
    // A normal `mod` chain can compile another member's source when that
    // member lives inside this package's tree (including build.rs, tests, and
    // examples), without a macro or #[path].
    for (ix, outer) in packages.iter().enumerate() {
        if packages.iter().enumerate().any(|(other_ix, inner)| {
            other_ix != ix && inner.directory.starts_with(&outer.directory)
        }) {
            return None;
        }
    }

    let workspace_dependencies = root_manifest
        .get("dependencies")
        .and_then(toml::Value::as_table);
    for package in &mut packages {
        let dependencies = package_dependencies(
            root,
            package,
            workspace_dependencies,
            &by_name,
            &by_directory,
        )?;
        package.dependencies = dependencies;
    }
    // A source file may be compiled by a different package through #[path],
    // include!, or a macro. A path-based owner is safe only when no Rust source
    // in the workspace can introduce such a cross-package compilation context.
    for path in rust_files {
        let source = read_input_file(&root.join(&path))?;
        if source_has_expansion(&source) {
            return None;
        }
    }
    Some(CargoIndex { packages })
}

fn canonical_relative(root: &Path, base: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.is_absolute() {
        return None;
    }
    root.join(base)
        .join(relative)
        .canonicalize()
        .ok()?
        .strip_prefix(root.canonicalize().ok()?)
        .ok()
        .map(Path::to_owned)
}

fn custom_target_path(manifest: &toml::Value) -> bool {
    // A custom build script can point directly at a source file owned by a
    // different package, and build-dependencies can make its names available.
    if manifest
        .get("package")
        .and_then(|package| package.get("build"))
        .is_some_and(|build| !build.is_bool())
    {
        return true;
    }
    for key in ["lib", "bin", "example", "test", "bench"] {
        let Some(target) = manifest.get(key) else {
            continue;
        };
        let candidates: Vec<&toml::Value> = match target {
            toml::Value::Array(values) => values.iter().collect(),
            value => vec![value],
        };
        if candidates.iter().any(|target| target.get("path").is_some()) {
            return true;
        }
    }
    false
}

fn package_dependencies(
    root: &Path,
    package: &Package,
    workspace_dependencies: Option<&toml::map::Map<String, toml::Value>>,
    by_name: &HashMap<String, usize>,
    by_directory: &HashMap<PathBuf, usize>,
) -> Option<Vec<usize>> {
    let mut edges = HashSet::new();
    for key in [
        "dependencies",
        "dev-dependencies",
        "dev_dependencies",
        "build-dependencies",
        "build_dependencies",
    ] {
        if let Some(table) = package.manifest.get(key) {
            collect_dependencies(
                table,
                root,
                &package.directory,
                workspace_dependencies,
                by_name,
                by_directory,
                &mut edges,
            )?;
        }
    }
    if let Some(targets) = package.manifest.get("target") {
        for target in targets.as_table()?.values() {
            for (key, table) in target.as_table()? {
                if !matches!(
                    key.as_str(),
                    "dependencies"
                        | "dev-dependencies"
                        | "dev_dependencies"
                        | "build-dependencies"
                        | "build_dependencies"
                ) {
                    return None;
                }
                collect_dependencies(
                    table,
                    root,
                    &package.directory,
                    workspace_dependencies,
                    by_name,
                    by_directory,
                    &mut edges,
                )?;
            }
        }
    }
    let mut edges: Vec<usize> = edges.into_iter().collect();
    edges.sort_unstable();
    Some(edges)
}

#[allow(clippy::too_many_arguments)]
fn collect_dependencies(
    table: &toml::Value,
    root: &Path,
    package_directory: &Path,
    workspace_dependencies: Option<&toml::map::Map<String, toml::Value>>,
    by_name: &HashMap<String, usize>,
    by_directory: &HashMap<PathBuf, usize>,
    edges: &mut HashSet<usize>,
) -> Option<()> {
    for (key, value) in table.as_table()? {
        let (value, base) = match value.get("workspace") {
            Some(toml::Value::Boolean(true)) => {
                if value.get("package").is_some() || value.get("path").is_some() {
                    return None;
                }
                (workspace_dependencies?.get(key)?, Path::new(""))
            }
            Some(_) => return None,
            None => (value, package_directory),
        };
        let (name, path) = match value {
            toml::Value::String(_) => (key.as_str(), None),
            toml::Value::Table(_) => {
                let name = match value.get("package") {
                    Some(value) => value.as_str()?,
                    None => key,
                };
                let path = match value.get("path") {
                    Some(value) => Some(value.as_str()?),
                    None => None,
                };
                (name, path)
            }
            _ => return None,
        };
        if let Some(path) = path {
            let directory = canonical_relative(root, base, path)?;
            edges.insert(*by_directory.get(&directory)?);
        }
        if let Some(&owner) = by_name.get(name) {
            edges.insert(owner);
        }
    }
    Some(())
}

fn source_has_expansion(source: &[u8]) -> bool {
    if memchr::memchr2(b'#', b'!', source).is_none() {
        return false;
    }
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return true;
    };
    let root = tree.root_node();
    if root.has_error() {
        return true;
    }
    has_expansion_node(root, source)
}

fn has_expansion_node(root: Node<'_>, source: &[u8]) -> bool {
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        let attribute = matches!(node.kind(), "attribute_item" | "inner_attribute_item");
        let derive = attribute
            && node
                .named_child(0)
                .and_then(|attribute| attribute.named_child(0))
                .and_then(|name| name.utf8_text(source).ok())
                == Some("derive");
        if node.kind() == "macro_invocation"
            || attribute
                && (derive || super::rust_std_origin::safe_attribute(node, source).is_none())
        {
            return true;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::RustCargoReachability;

    fn proves_unreachable(
        workspace_extra: &str,
        b_manifest_extra: &str,
        c_manifest_extra: &str,
        b_source: &str,
        enabled: bool,
    ) -> bool {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for package in ["a", "b", "c"] {
            std::fs::create_dir_all(root.join(package).join("src")).unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            format!("[workspace]\nmembers = [\"a\", \"b\", \"c\"]\n{workspace_extra}"),
        )
        .unwrap();
        for (package, extra) in [("a", ""), ("b", b_manifest_extra), ("c", c_manifest_extra)] {
            std::fs::write(
                root.join(package).join("Cargo.toml"),
                format!(
                    "[package]\nname = \"sample-{package}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{extra}"
                ),
            )
            .unwrap();
            std::fs::write(
                root.join(package).join("src/lib.rs"),
                if package == "b" {
                    b_source
                } else {
                    "pub struct Result;\n"
                },
            )
            .unwrap();
        }
        RustCargoReachability::new(root, enabled).proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
    }

    #[test]
    fn unrelated_package_is_excluded_but_all_dependency_kinds_remain() {
        let ordinary = "pub fn unrelated() {}\n";
        assert!(proves_unreachable("", "", "", ordinary, true));
        for dependency in [
            "[dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dependencies]\nrenamed = { package = \"sample-a\", path = \"../a\" }\n",
            "[dev-dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dev_dependencies]\nsample-a = { path = \"../a\" }\n",
            "[build-dependencies]\nsample-a = { path = \"../a\" }\n",
            "[build_dependencies]\nsample-a = { path = \"../a\" }\n",
            "[target.'cfg(unix)'.dependencies]\nsample-a = { path = \"../a\" }\n",
            "[dependencies]\nsample-a = { path = \"../a\", optional = true }\n",
            "[dependencies]\nsample-a.workspace = true\n",
            "[dependencies]\nsample-c = { path = \"../c\" }\n",
        ] {
            let workspace = if dependency.contains("workspace = true") {
                "[workspace.dependencies]\nsample-a = { path = \"a\" }\n"
            } else {
                ""
            };
            let c = if dependency.contains("sample-c") {
                "[dependencies]\nsample-a = { path = \"../a\" }\n"
            } else {
                ""
            };
            assert!(
                !proves_unreachable(workspace, dependency, c, ordinary, true),
                "{dependency}"
            );
        }
    }

    #[test]
    fn uncertain_manifest_or_source_keeps_existing_impact() {
        let ordinary = "pub fn unrelated() {}\n";
        assert!(!proves_unreachable("", "", "", ordinary, false));
        assert!(!proves_unreachable(
            "[patch.crates-io]\nother = { path = \"c\" }\n",
            "",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "[dependencies]\nexternal = { path = \"../../outside\" }\n",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "",
            "",
            "include!(\"shared.rs\");\n",
            true
        ));
        assert!(!proves_unreachable(
            "",
            "",
            "",
            "#[path = \"shared.rs\"] mod shared;\n",
            true
        ));
        // An imported derive can shadow a built-in name such as Clone.
        assert!(!proves_unreachable(
            "",
            "[dependencies]\ncustom = \"1\"\n",
            "",
            "use custom::Clone;\n#[derive(Clone)] pub struct Value;\n",
            true
        ));
        assert!(!proves_unreachable(
            "",
            "[lib]\npath = \"src/lib.rs\"\n",
            "",
            ordinary,
            true
        ));
        assert!(!proves_unreachable(
            "",
            "build = \"../a/src/lib.rs\"\n",
            "",
            ordinary,
            true
        ));
    }

    #[test]
    fn source_inclusion_in_a_transitive_dependency_keeps_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\n",
        )
        .unwrap();
        for (directory, name, dependency, source) in [
            ("a", "sample-a", "", "pub struct Result;\n"),
            (
                "b",
                "sample-b",
                "[dependencies]\nsample-c = { path = \"../c\" }\n",
                "use sample_c::Result;\n",
            ),
            (
                "c",
                "sample-c",
                "",
                "#[path = \"../../a/src/lib.rs\"] mod shared; pub use shared::Result;\n",
            ),
        ] {
            std::fs::create_dir_all(root.join(directory).join("src")).unwrap();
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n{dependency}"),
            )
            .unwrap();
            std::fs::write(root.join(directory).join("src/lib.rs"), source).unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn skipped_root_directory_with_workspace_member_disables_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\", \"target/c\"]\n",
        )
        .unwrap();
        for (path, name) in [
            ("a", "sample-a"),
            ("b", "sample-b"),
            ("target/c", "sample-c"),
        ] {
            std::fs::create_dir_all(root.join(path).join("src")).unwrap();
            std::fs::write(
                root.join(path).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(root.join(path).join("src/lib.rs"), "pub struct Result;\n").unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn package_nested_in_another_package_disables_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"d\", \"d/b\"]\n",
        )
        .unwrap();
        for (path, name) in [("a", "sample-a"), ("d", "sample-d"), ("d/b", "sample-b")] {
            std::fs::create_dir_all(root.join(path).join("src")).unwrap();
            std::fs::write(
                root.join(path).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(root.join(path).join("src/lib.rs"), "pub struct Result;\n").unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("d/b/src/lib.rs", "a/src/lib.rs")
        );
    }

    #[test]
    fn sysroot_named_package_never_proves_disconnection() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for package in ["a", "b"] {
            std::fs::create_dir_all(root.join(package).join("src")).unwrap();
        }
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"a\", \"b\"]\n",
        )
        .unwrap();
        for (directory, name) in [("a", "core"), ("b", "sample-b")] {
            std::fs::write(
                root.join(directory).join("Cargo.toml"),
                format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
            )
            .unwrap();
            std::fs::write(
                root.join(directory).join("src/lib.rs"),
                "pub struct Item;\n",
            )
            .unwrap();
        }
        assert!(
            !RustCargoReachability::new(root, true)
                .proves_unreachable("b/src/lib.rs", "a/src/lib.rs")
        );
    }
}
