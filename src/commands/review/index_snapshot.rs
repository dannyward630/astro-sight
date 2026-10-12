//! staged の AST と参照検索を、同じ index tree の raw blob に揃える。

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::Result;

use crate::commands::git_input::GitBlobBatch;
use crate::error::{AstroError, ErrorCode};
use crate::git_support::{INDEX_SNAPSHOT_MARKER, git_command_for_dir, isolated_git_command};

const MAX_FILES: usize = 50_000;
const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_BLOB_BYTES: usize = 100 * 1024 * 1024;

fn failure(message: impl Into<String>) -> anyhow::Error {
    AstroError::new(ErrorCode::InvalidRequest, message).into()
}

fn git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut child = git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failure("Cannot read the staged snapshot command"))?;
    let bytes = crate::commands::common::read_bytes_limited_and_drain(
        stdout,
        64 * 1024 * 1024,
        "staged snapshot metadata",
    );
    if !child.wait()?.success() {
        return Err(failure("Cannot obtain the staged analysis snapshot"));
    }
    bytes
}

fn git_text(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(dir, args)?)?
        .trim_end_matches('\n')
        .to_owned())
}

fn safe_path(path: &str) -> bool {
    crate::engine::impact::is_safe_diff_path(path)
        && path
            .split('/')
            .all(|part| !part.eq_ignore_ascii_case(".git"))
}

fn create_parents(
    root: &Path,
    relative: &Path,
    directories: &mut HashMap<PathBuf, PathBuf>,
) -> Result<()> {
    let mut logical = PathBuf::new();
    for component in relative.parent().unwrap_or(Path::new("")).components() {
        let Component::Normal(part) = component else {
            return Err(failure("Invalid staged snapshot path"));
        };
        logical.push(part);
        let actual = root.join(&logical);
        fs::create_dir_all(&actual)?;
        let canonical = fs::canonicalize(&actual)?;
        if let Some(previous) = directories.insert(canonical, logical.clone())
            && previous != logical
        {
            return Err(failure("Staged paths collide on this filesystem"));
        }
    }
    Ok(())
}

fn auxiliary_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    matches!(
        name,
        ".gitignore"
            | ".ignore"
            | ".gitattributes"
            | "package.json"
            | "Cargo.toml"
            | "pyproject.toml"
            | "setup.cfg"
            | "go.mod"
            | "go.work"
            | "angular.json"
            | ".angular-cli.json"
            | "composer.json"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "astro-sight.toml"
            | "tsconfig.json"
    ) || matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("html" | "csproj" | "sln")
    )
}

fn diff_contract(diff: &str) -> String {
    diff.lines()
        .filter(|line| !line.starts_with("index "))
        .map(|line| {
            if line.starts_with("@@ ") {
                line.find(" @@").map_or(line, |end| &line[..end + 3])
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) struct IndexSnapshot {
    _temporary: tempfile::TempDir,
    analysis_dir: PathBuf,
}

impl IndexSnapshot {
    pub(super) fn prepare(dir: &str, base: &str, expected_diff: &str) -> Result<Self> {
        let original = Path::new(dir);
        // hook の index は読むだけにし、tree の作成は一時 index で行う。
        let index = git_text(original, &["rev-parse", "--git-path", "index"])?;
        let shared_index = git_text(original, &["rev-parse", "--shared-index-path"])?;
        let object_format = git_text(original, &["rev-parse", "--show-object-format"])?;
        if !matches!(object_format.as_str(), "sha1" | "sha256") {
            return Err(failure("Unsupported Git object format"));
        }
        let original_root =
            fs::canonicalize(git_text(original, &["rev-parse", "--show-toplevel"])?)?;
        let canonical_dir = fs::canonicalize(original)?;
        let prefix = canonical_dir.strip_prefix(&original_root)?;
        let objects = git_text(original, &["rev-parse", "--git-path", "objects"])?;
        let common = git_text(original, &["rev-parse", "--git-common-dir"])?;
        let absolute = |path: &str| {
            let path = Path::new(path);
            fs::canonicalize(if path.is_absolute() {
                path.to_path_buf()
            } else {
                original.join(path)
            })
        };
        let object_paths = [absolute(&objects)?, absolute(&common)?.join("objects")];
        let temporary = tempfile::Builder::new()
            .prefix("astro-sight-index-")
            .tempdir()?;
        let root = temporary.path();
        let init = isolated_git_command()
            .args(["init", "--quiet", "--template="])
            .arg(format!("--object-format={object_format}"))
            .arg(root)
            .output()?;
        if !init.status.success() {
            return Err(failure("Cannot initialize the staged analysis snapshot"));
        }
        fs::write(root.join(".git").join(INDEX_SNAPSHOT_MARKER), "")?;
        let mut alternates = String::new();
        for path in object_paths {
            let text = path
                .to_str()
                .ok_or_else(|| failure("Non-UTF-8 Git object directory"))?;
            if text.contains(['\n', '\r']) {
                return Err(failure("Unsupported Git object directory"));
            }
            alternates.push_str(text);
            alternates.push('\n');
        }
        fs::write(root.join(".git/objects/info/alternates"), alternates)?;
        let index = Path::new(&index);
        let index = if index.is_absolute() {
            index.to_path_buf()
        } else {
            original.join(index)
        };
        fs::copy(index, root.join(".git/index"))?;
        if !shared_index.is_empty() {
            let shared = Path::new(&shared_index);
            let shared = if shared.is_absolute() {
                shared.to_path_buf()
            } else {
                original.join(shared)
            };
            let name = shared
                .file_name()
                .ok_or_else(|| failure("Invalid shared index path"))?;
            fs::copy(&shared, root.join(".git").join(name))?;
        }
        let tree = git_text(root, &["write-tree"])?;
        if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(failure("Invalid staged tree identifier"));
        }
        fs::write(root.join(".git").join(INDEX_SNAPSHOT_MARKER), &tree)?;
        // 独立した一時リポジトリなので、元の index / refs / worktree 登録は変えない。
        git_bytes(root, &["update-ref", "HEAD", base])?;
        git_bytes(root, &["read-tree", &tree])?;
        let listing = git_bytes(root, &["ls-tree", "-r", "-l", "-z", &tree])?;
        let root_text = root
            .to_str()
            .ok_or_else(|| failure("Non-UTF-8 snapshot directory"))?;
        let blobs = GitBlobBatch::new(root_text, &tree);
        let mut directories = HashMap::new();
        let mut count = 0usize;
        let mut total = 0usize;
        for raw in listing.split(|b| *b == 0).filter(|entry| !entry.is_empty()) {
            let entry = match std::str::from_utf8(raw) {
                Ok(entry) => entry,
                Err(_) => {
                    let path = raw
                        .splitn(2, |byte| *byte == b'\t')
                        .nth(1)
                        .ok_or_else(|| failure("Invalid staged tree entry"))?;
                    let lossy = String::from_utf8_lossy(path);
                    if crate::language::LangId::from_path(camino::Utf8Path::new(&lossy)).is_ok()
                        || auxiliary_file(Path::new(lossy.as_ref()))
                        || Path::new(lossy.as_ref()).extension().is_none()
                    {
                        return Err(failure("Non-UTF-8 staged source path"));
                    }
                    continue;
                }
            };
            let (metadata, path) = entry
                .split_once('\t')
                .ok_or_else(|| failure("Invalid staged tree entry"))?;
            if !safe_path(path) {
                return Err(failure("Unsafe staged snapshot path"));
            }
            let fields: Vec<_> = metadata.split_whitespace().collect();
            let [mode, kind, _oid, size] = fields.as_slice() else {
                return Err(failure("Invalid staged tree metadata"));
            };
            let relative = Path::new(path);
            if *mode == "160000" && *kind == "commit" {
                // サブモジュールは親 index にコードを持たないため展開しない。
                create_parents(root, relative, &mut directories)?;
                fs::create_dir(root.join(relative))?;
                let canonical = fs::canonicalize(root.join(relative))?;
                if let Some(previous) = directories.insert(canonical, relative.to_path_buf())
                    && previous != relative
                {
                    return Err(failure("Staged directory paths collide on this filesystem"));
                }
                continue;
            }
            // 共通 walker はリンクを辿らない。外部・dangling を含め展開しない。
            if *mode == "120000" {
                continue;
            }
            if *kind != "blob" || !matches!(*mode, "100644" | "100755" | "120000") {
                return Err(failure("Unsupported staged tree entry"));
            }
            let size: usize = size.parse()?;
            let known = crate::language::LangId::from_path(camino::Utf8Path::new(path)).is_ok();
            let auxiliary = auxiliary_file(relative);
            if !known && !auxiliary && relative.extension().is_some() {
                continue;
            }
            count += 1;
            if count > MAX_FILES {
                return Err(failure("Staged snapshot exceeds the file limit"));
            }
            total = total
                .checked_add(size)
                .ok_or_else(|| failure("Staged snapshot size overflow"))?;
            if total > MAX_TOTAL_BYTES || size > MAX_BLOB_BYTES {
                return Err(failure("Staged snapshot exceeds the byte limit"));
            }
            let content = blobs
                .read_limited(path, MAX_BLOB_BYTES)
                .ok_or_else(|| failure("Cannot read a staged snapshot blob"))?;
            if content.len() != size {
                return Err(failure("Staged snapshot blob size mismatch"));
            }
            if !known
                && !auxiliary
                && crate::engine::parser::detect_lang(camino::Utf8Path::new(path), &content)
                    .is_err()
            {
                continue;
            }
            create_parents(root, relative, &mut directories)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(relative))
                .map_err(|_| failure("Staged file paths collide or cannot be written"))?;
            file.write_all(&content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(if *mode == "100755" {
                    0o755
                } else {
                    0o644
                }))?;
            }
        }
        let analysis_dir = root.join(prefix);
        fs::create_dir_all(&analysis_dir)?;
        let analysis_text = analysis_dir
            .to_str()
            .ok_or_else(|| failure("Non-UTF-8 staged analysis path"))?;
        if diff_contract(&crate::commands::git_input::run_git_diff(
            analysis_text,
            base,
            false,
        )?) != diff_contract(expected_diff)
        {
            return Err(failure(
                "Staged input changed while creating the analysis snapshot",
            ));
        }
        drop(blobs);
        Ok(Self {
            _temporary: temporary,
            analysis_dir,
        })
    }

    pub(super) fn dir(&self) -> Result<&str> {
        self.analysis_dir
            .to_str()
            .ok_or_else(|| failure("Non-UTF-8 staged analysis path"))
    }
}

/// 明示 diff の hunk を、offset の移動を許さず new 側のソースと照合する。
pub(super) fn verify_explicit_new_side(
    dir: &str,
    diff: &str,
    files: &[crate::models::impact::DiffFile],
) -> Result<()> {
    let sections = crate::engine::diff::FileSections::split(diff);
    for file in files {
        let path = if file.new_path == "/dev/null" {
            &file.old_path
        } else {
            &file.new_path
        };
        if !safe_path(path) {
            return Err(failure("Unsafe explicit diff path"));
        }
        let full = Path::new(dir).join(path);
        let utf8 = camino::Utf8Path::new(
            full.to_str()
                .ok_or_else(|| failure("Non-UTF-8 analysis path"))?,
        );
        let known = crate::language::LangId::from_path(camino::Utf8Path::new(path)).is_ok();
        if file.new_path == "/dev/null" {
            if known && full.try_exists()? {
                return Err(failure(format!(
                    "Explicit diff deletes a source file that still exists: {path}"
                )));
            }
            continue;
        }
        let source = match crate::engine::parser::read_file(utf8) {
            Ok(source) => source,
            Err(_) if !known => continue,
            Err(_) => {
                return Err(failure(format!(
                    "Cannot verify explicit diff source: {path}"
                )));
            }
        };
        if crate::engine::parser::detect_lang(utf8, &source).is_err() {
            continue;
        }
        if !file.hunks.is_empty()
            && crate::engine::diff::reconstruct_old_source(&sections.get(path), path, &source)
                .is_none()
        {
            return Err(failure(format!(
                "Explicit diff does not match the source at its declared line offsets: {path}"
            )));
        }
    }
    Ok(())
}
