use anyhow::{Context, Result};
use std::collections::{hash_map::DefaultHasher, BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct FileSignature {
    pub hash: u64,
    pub quick_sig: u64,
}

impl FileSignature {
    pub fn new(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default();
        let size = metadata.len();
        let quick_sig = modified.wrapping_mul(31) ^ size;
        let hash = hash_file(path)?;
        Ok(Self { hash, quick_sig })
    }
}

#[derive(Debug, Clone)]
pub struct BuildState {
    pub source_fingerprint: String,
    pub dependency_fingerprint: String,
    pub file_signatures: BTreeMap<String, FileSignature>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncStats {
    pub copied_files: u64,
    pub removed_files: u64,
    pub skipped_files: u64,
    pub copied_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct BuildCache {
    root: PathBuf,
    deps_dir: PathBuf,
    artifact_dir: PathBuf,
    state_path: PathBuf,
}

impl BuildCache {
    pub fn new(project: &str, base_dir: &Path) -> Result<Self> {
        let root = base_dir.join(project);
        let deps_dir = root.join("deps");
        let artifact_dir = root.join("artifact");
        let state_path = root.join("state.txt");

        fs::create_dir_all(&root)?;

        Ok(Self {
            root,
            deps_dir,
            artifact_dir,
            state_path,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_state(&self) -> Result<Option<BuildState>> {
        if !self.state_path.exists() {
            return Ok(None);
        }

        let file = fs::File::open(&self.state_path)?;
        let reader = BufReader::new(file);

        let mut source_fingerprint = String::new();
        let mut dependency_fingerprint = String::new();
        let mut file_signatures = BTreeMap::new();

        for line in reader.lines().map_while(Result::ok) {
            if let Some(value) = line.strip_prefix("source_fingerprint=") {
                source_fingerprint = value.to_string();
                continue;
            }
            if let Some(value) = line.strip_prefix("dependency_fingerprint=") {
                dependency_fingerprint = value.to_string();
                continue;
            }
            if let Some(value) = line.strip_prefix("file\t") {
                let mut parts = value.split('\t');
                let path = parts.next().unwrap_or_default();
                let hash = parts.next().unwrap_or_default();
                let quick = parts.next();
                if let Ok(hash) = hash.parse::<u64>() {
                    let quick_sig = quick
                        .and_then(|val| val.parse::<u64>().ok())
                        .unwrap_or(hash);
                    file_signatures.insert(
                        path.to_string(),
                        FileSignature {
                            hash,
                            quick_sig,
                        },
                    );
                }
            }
        }

        if source_fingerprint.is_empty() {
            return Ok(None);
        }

        Ok(Some(BuildState {
            source_fingerprint,
            dependency_fingerprint,
            file_signatures,
        }))
    }

    pub fn save_state(&self, state: &BuildState) -> Result<()> {
        fs::create_dir_all(self.state_path.parent().unwrap_or(&self.root))?;
        let mut file = fs::File::create(&self.state_path)?;
        writeln!(file, "source_fingerprint={}", state.source_fingerprint)?;
        writeln!(
            file,
            "dependency_fingerprint={}",
            state.dependency_fingerprint
        )?;
        for (path, sig) in &state.file_signatures {
            writeln!(file, "file\t{}\t{}\t{}", path, sig.hash, sig.quick_sig)?;
        }
        Ok(())
    }

    pub fn has_dependency_cache(&self) -> bool {
        self.deps_dir.exists()
    }

    pub fn restore_dependencies(&self, work_dir: &Path) -> Result<SyncStats> {
        if !self.deps_dir.exists() {
            return Ok(SyncStats::default());
        }
        let target = work_dir.join("node_modules");
        sync_recursive(&self.deps_dir, &target, true)
    }

    pub fn save_dependencies(&self, work_dir: &Path) -> Result<SyncStats> {
        let src = work_dir.join("node_modules");
        if !src.exists() {
            return Ok(SyncStats::default());
        }
        sync_recursive(&src, &self.deps_dir, true)
    }

    pub fn has_artifact_cache(&self) -> bool {
        self.artifact_dir.exists()
    }

    pub fn restore_artifact(&self, output_dir: &Path) -> Result<SyncStats> {
        if !self.artifact_dir.exists() {
            return Ok(SyncStats::default());
        }
        sync_recursive(&self.artifact_dir, output_dir, true)
    }

    pub fn save_artifact(&self, output_dir: &Path) -> Result<SyncStats> {
        if !output_dir.exists() {
            return Ok(SyncStats::default());
        }
        sync_recursive(output_dir, &self.artifact_dir, true)
    }
}

pub fn compute_file_signatures(root: &Path) -> Result<BTreeMap<String, u64>> {
    let mut signatures = BTreeMap::new();
    collect_file_signatures(root, root, &mut signatures)?;
    Ok(signatures)
}

pub fn compute_file_signatures_incremental(
    root: &Path,
    previous: Option<&BuildState>,
) -> Result<BTreeMap<String, u64>> {
    let mut signatures = BTreeMap::new();
    collect_file_signatures_incremental(root, root, &mut signatures, previous)?;
    Ok(signatures)
}

pub fn fingerprint_from_signatures(signatures: &BTreeMap<String, u64>) -> String {
    let mut hasher = DefaultHasher::new();
    for (path, sig) in signatures {
        path.hash(&mut hasher);
        sig.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

pub fn compute_dependency_fingerprint(root: &Path) -> Result<String> {
    let lockfiles = [
        "pnpm-lock.yaml",
        "package-lock.json",
        "yarn.lock",
        "bun.lockb",
        "bun.lock",
        "Gemfile.lock",
        "poetry.lock",
        "Pipfile.lock",
    ];

    let mut selected = Vec::new();
    for file in lockfiles {
        let path = root.join(file);
        if path.exists() {
            selected.push((file, path));
        }
    }

    // If lockfiles exist, use only lockfiles for dependency identity.
    // package.json often changes for non-dependency reasons (scripts/metadata).
    if selected.is_empty() {
        let fallback = root.join("requirements.txt");
        if fallback.exists() {
            selected.push(("requirements.txt", fallback));
        } else {
            let manifest = root.join("package.json");
            if manifest.exists() {
                selected.push(("package.json", manifest));
            }
        }
    }

    if selected.is_empty() {
        return Ok("none".to_string());
    }

    let mut hasher = DefaultHasher::new();
    for (name, path) in selected {
        name.hash(&mut hasher);
        hash_file(&path)?.hash(&mut hasher);
    }
    Ok(format!("{:016x}", hasher.finish()))
}

pub fn changed_modules(
    previous: Option<&BuildState>,
    current: &BTreeMap<String, u64>,
) -> Vec<String> {
    let Some(prev) = previous else {
        return vec!["all".to_string()];
    };

    let mut changed = BTreeSet::new();

    for (path, sig) in current {
        if prev
            .file_signatures
            .get(path)
            .map(|signature| signature.hash)
            != Some(*sig)
        {
            changed.insert(module_name(path));
        }
    }

    for path in prev.file_signatures.keys() {
        if !current.contains_key(path) {
            changed.insert(module_name(path));
        }
    }

    if changed.is_empty() {
        vec!["none".to_string()]
    } else {
        changed.into_iter().collect()
    }
}

fn module_name(path: &str) -> String {
    path.split('/').next().unwrap_or("unknown").to_string()
}

fn collect_file_signatures(
    root: &Path,
    cursor: &Path,
    out: &mut BTreeMap<String, u64>,
) -> Result<()> {
    for entry in fs::read_dir(cursor).with_context(|| format!("cant read {}", cursor.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if ignored_dir(&name) {
                continue;
            }
            collect_file_signatures(root, &path, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        let signature = FileSignature::new(&path)?;
        out.insert(rel, signature.hash);
    }
    Ok(())
}

fn collect_file_signatures_incremental(
    root: &Path,
    cursor: &Path,
    out: &mut BTreeMap<String, u64>,
    previous: Option<&BuildState>,
) -> Result<()> {
    for entry in fs::read_dir(cursor).with_context(|| format!("cant read {}", cursor.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if file_type.is_dir() {
            if ignored_dir(&name) {
                continue;
            }
            collect_file_signatures_incremental(root, &path, out, previous)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");

        if let Some(prev) = previous {
            if let Some(prev_sig) = prev.file_signatures.get(&rel) {
                let current_sig = FileSignature::new(&path)?;
                if current_sig.quick_sig == prev_sig.quick_sig {
                    out.insert(rel, prev_sig.hash);
                    continue;
                }
                out.insert(rel, current_sig.hash);
                continue;
            }
        }

        let signature = FileSignature::new(&path)?;
        out.insert(rel, signature.hash);
    }
    Ok(())
}

fn ignored_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | "artifacts" | ".next"
    )
}

fn hash_file(path: &Path) -> Result<u64> {
    let mut file = fs::File::open(path)?;
    let mut hasher = DefaultHasher::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        buffer[..n].hash(&mut hasher);
    }
    Ok(hasher.finish())
}

fn sync_recursive(src: &Path, dst: &Path, prune: bool) -> Result<SyncStats> {
    fs::create_dir_all(dst)?;
    let mut stats = SyncStats::default();

    sync_dir(src, dst, prune, &mut stats)?;
    Ok(stats)
}

fn sync_dir(src: &Path, dst: &Path, prune: bool, stats: &mut SyncStats) -> Result<()> {
    fs::create_dir_all(dst)?;
    let mut src_names = HashSet::new();

    for entry in fs::read_dir(src).with_context(|| format!("cant read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let from = entry.path();
        let name = entry.file_name();
        let to = dst.join(&name);
        src_names.insert(name.to_string_lossy().to_string());

        if ty.is_dir() {
            sync_dir(&from, &to, prune, stats)?;
            continue;
        }
        if !ty.is_file() {
            continue;
        }

        if should_copy(&from, &to)? {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            // Try hardlink first for faster copying
            let copied = if fs::hard_link(&from, &to).is_ok() {
                // Hardlink succeeded
                let metadata = fs::metadata(&from)?;
                metadata.len()
            } else {
                // Fall back to copy
                fs::copy(&from, &to)?
            };
            stats.copied_files += 1;
            stats.copied_bytes += copied;
        } else {
            stats.skipped_files += 1;
        }
    }

    if prune {
        for entry in fs::read_dir(dst).with_context(|| format!("cant read {}", dst.display()))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if src_names.contains(&name) {
                continue;
            }
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
            stats.removed_files += 1;
        }
    }

    Ok(())
}

fn should_copy(src: &Path, dst: &Path) -> Result<bool> {
    if !dst.exists() {
        return Ok(true);
    }

    let src_meta = fs::metadata(src)?;
    let dst_meta = fs::metadata(dst)?;

    if src_meta.len() != dst_meta.len() {
        return Ok(true);
    }

    let src_mtime = mtime(src_meta.modified().ok());
    let dst_mtime = mtime(dst_meta.modified().ok());

    Ok(src_mtime > dst_mtime)
}

fn mtime(value: Option<SystemTime>) -> u128 {
    value
        .and_then(|v| v.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
