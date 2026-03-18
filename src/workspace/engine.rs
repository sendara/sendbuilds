use anyhow::{bail, Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::core::config::{default_artifact_dir, default_cache_dir, PackageConfig, WorkspaceConfig};
use crate::core::{BuildConfig, BuildStepConfig, ProjectConfig};
use crate::engine::BuildEngine;
use crate::utils::cache::compute_file_signatures;
use crate::workspace::{
    build_graph, detect_workspace, discover_packages, workspace_storage_key, DependencyGraph,
    Package, WorkspaceRoot,
};

#[derive(Debug, Clone, Default)]
pub struct WorkspaceRunOptions {
    pub force: bool,
    pub packages: Option<Vec<String>>,
    pub build_mode: Option<String>,
    pub events: Option<bool>,
    pub reproducible: bool,
    pub unused_deps: bool,
}

pub fn run_workspace_build(cfg: BuildConfig, opts: &WorkspaceRunOptions) -> Result<bool> {
    let workspace_cfg = cfg.workspace.clone();
    if !should_run_workspace(&workspace_cfg, opts, &cfg.packages) {
        return Ok(false);
    }

    let root = resolve_root(&workspace_cfg)?;
    let (workspace, mut packages) = resolve_packages(&root, &workspace_cfg, &cfg.packages, opts)?;
    if packages.is_empty() {
        bail!("workspace mode enabled but no packages were discovered");
    }

    apply_package_filter(&mut packages, opts.packages.clone(), workspace_cfg.as_ref());
    if packages.is_empty() {
        bail!("workspace package filter removed all packages");
    }

    let graph = build_graph(&packages);
    let build_mode = resolve_build_mode(&workspace_cfg, opts);
    let cache_root = workspace_cache_root(&cfg, &workspace)?;
    let affected = compute_affected_packages(
        &root,
        &packages,
        &graph,
        &cache_root,
        &build_mode,
    )?;

    let mut selected = order_packages(&packages, &graph, &affected);
    if selected.is_empty() {
        println!("No affected packages detected. Skipping build.");
        return Ok(true);
    }

    let cwd = env::current_dir().with_context(|| "cant resolve current directory")?;
    let _guard = DirGuard::new(&cwd)?;
    for pkg in selected.iter_mut() {
        let pkg_cfg = build_config_for_package(&cfg, &workspace, pkg)?;
        env::set_current_dir(&pkg.path)?;
        BuildEngine::from_config(pkg_cfg)
            .with_in_place(true)
            .with_events(opts.events)
            .with_reproducible(opts.reproducible)
            .with_unused_deps(opts.unused_deps)
            .run()?;
    }

    save_workspace_state(&root, &cache_root)?;
    Ok(true)
}

pub fn run_workspace_deploy(
    cfg: BuildConfig,
    opts: &WorkspaceRunOptions,
    force_build: bool,
) -> Result<bool> {
    let workspace_cfg = cfg.workspace.clone();
    if !should_run_workspace(&workspace_cfg, opts, &cfg.packages) {
        return Ok(false);
    }
    let root = resolve_root(&workspace_cfg)?;
    let (workspace, mut packages) = resolve_packages(&root, &workspace_cfg, &cfg.packages, opts)?;
    if packages.is_empty() {
        bail!("workspace mode enabled but no packages were discovered");
    }
    apply_package_filter(&mut packages, opts.packages.clone(), workspace_cfg.as_ref());
    if packages.is_empty() {
        bail!("workspace package filter removed all packages");
    }

    let graph = build_graph(&packages);
    let build_mode = resolve_build_mode(&workspace_cfg, opts);
    let cache_root = workspace_cache_root(&cfg, &workspace)?;
    let affected = if force_build {
        packages.iter().map(|p| p.name.clone()).collect()
    } else {
        compute_affected_packages(&root, &packages, &graph, &cache_root, &build_mode)?
    };
    let selected = order_packages(&packages, &graph, &affected);
    if selected.is_empty() {
        println!("No affected packages detected. Skipping build.");
        return Ok(true);
    }

    let cwd = env::current_dir().with_context(|| "cant resolve current directory")?;
    let _guard = DirGuard::new(&cwd)?;
    for pkg in selected.iter() {
        let pkg_cfg = build_config_for_package(&cfg, &workspace, pkg)?;
        env::set_current_dir(&pkg.path)?;
        BuildEngine::from_config(pkg_cfg)
            .with_in_place(true)
            .with_events(opts.events)
            .with_reproducible(opts.reproducible)
            .with_unused_deps(opts.unused_deps)
            .run()?;
    }

    write_workspace_manifest(&cfg, &workspace, &packages)?;
    save_workspace_state(&root, &cache_root)?;
    Ok(true)
}

fn should_run_workspace(
    workspace_cfg: &Option<WorkspaceConfig>,
    opts: &WorkspaceRunOptions,
    packages_cfg: &Option<Vec<PackageConfig>>,
) -> bool {
    if opts.force {
        return true;
    }
    if let Some(cfg) = workspace_cfg {
        if cfg.enabled.unwrap_or(false) {
            return true;
        }
        if cfg.mode.as_deref() == Some("explicit") {
            return true;
        }
        if cfg.packages.as_ref().map(|p| !p.is_empty()).unwrap_or(false) {
            return true;
        }
    }
    packages_cfg.as_ref().map(|p| !p.is_empty()).unwrap_or(false)
}

fn resolve_root(workspace_cfg: &Option<WorkspaceConfig>) -> Result<PathBuf> {
    if let Some(cfg) = workspace_cfg {
        if let Some(root) = cfg.root.as_ref() {
            return Ok(PathBuf::from(root));
        }
    }
    env::current_dir().with_context(|| "cant resolve current directory")
}

fn resolve_packages(
    root: &Path,
    workspace_cfg: &Option<WorkspaceConfig>,
    packages_cfg: &Option<Vec<PackageConfig>>,
    opts: &WorkspaceRunOptions,
) -> Result<(WorkspaceRoot, Vec<Package>)> {
    if let Some(cfg) = workspace_cfg {
        if cfg.mode.as_deref() == Some("explicit") {
            let explicit = packages_from_config(root, packages_cfg)?;
            return Ok((
                WorkspaceRoot {
                    path: root.to_path_buf(),
                    kind: crate::workspace::WorkspaceKind::Unknown,
                },
                explicit,
            ));
        }
    }

    let detected = detect_workspace(root);
    if detected.is_none() && opts.force {
        return Ok((
            WorkspaceRoot {
                path: root.to_path_buf(),
                kind: crate::workspace::WorkspaceKind::Unknown,
            },
            packages_from_config(root, packages_cfg)?,
        ));
    }
    let Some(workspace) = detected else {
        return Ok((
            WorkspaceRoot {
                path: root.to_path_buf(),
                kind: crate::workspace::WorkspaceKind::Unknown,
            },
            Vec::new(),
        ));
    };
    let packages = discover_packages(&workspace)?;
    Ok((workspace, packages))
}

fn packages_from_config(
    root: &Path,
    packages_cfg: &Option<Vec<PackageConfig>>,
) -> Result<Vec<Package>> {
    let Some(packages) = packages_cfg else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for pkg in packages {
        out.push(Package {
            name: pkg.name.clone(),
            path: root.join(&pkg.path),
            language: pkg.language.clone(),
            install_cmd: pkg.install_cmd.clone(),
            build_cmd: pkg.build_cmd.clone(),
            output_dir: pkg.output_dir.clone(),
            start_cmd: pkg.start_cmd.clone(),
            depends_on: pkg.depends_on.clone().unwrap_or_default(),
            targets: pkg.targets.clone(),
            container_image: pkg.container_image.clone(),
        });
    }
    Ok(out)
}

fn apply_package_filter(
    packages: &mut Vec<Package>,
    cli_filter: Option<Vec<String>>,
    workspace_cfg: Option<&WorkspaceConfig>,
) {
    let mut allow = BTreeSet::new();
    if let Some(list) = workspace_cfg.and_then(|c| c.packages.clone()) {
        for n in list {
            allow.insert(n);
        }
    }
    if let Some(list) = cli_filter {
        for n in list {
            allow.insert(n);
        }
    }
    if allow.is_empty() {
        return;
    }
    packages.retain(|p| allow.contains(&p.name));
}

fn resolve_build_mode(workspace_cfg: &Option<WorkspaceConfig>, opts: &WorkspaceRunOptions) -> String {
    if let Some(mode) = opts.build_mode.as_ref() {
        return mode.clone();
    }
    workspace_cfg
        .as_ref()
        .and_then(|c| c.build.clone())
        .unwrap_or_else(|| "affected".to_string())
}

fn workspace_cache_root(cfg: &BuildConfig, workspace: &WorkspaceRoot) -> Result<PathBuf> {
    let base = cfg
        .cache
        .as_ref()
        .and_then(|c| c.dir.clone())
        .map(PathBuf::from)
        .unwrap_or_else(default_cache_dir);
    let key = workspace_storage_key(&workspace.path);
    Ok(base.join(key))
}

fn compute_affected_packages(
    root: &Path,
    packages: &[Package],
    graph: &DependencyGraph,
    cache_root: &Path,
    build_mode: &str,
) -> Result<HashSet<String>> {
    if build_mode == "all" {
        return Ok(packages.iter().map(|p| p.name.clone()).collect());
    }
    if build_mode == "list" {
        return Ok(packages.iter().map(|p| p.name.clone()).collect());
    }
    let prev = load_workspace_state(cache_root)?;
    let current = compute_file_signatures(root)?;
    let mut changed = HashSet::new();
    if prev.is_none() {
        for pkg in packages {
            changed.insert(pkg.name.clone());
        }
    } else if let Some(prev) = prev {
        for (path, sig) in &current {
            if prev.get(path) != Some(sig) {
                mark_changed_by_path(path, root, packages, &mut changed);
            }
        }
        for path in prev.keys() {
            if !current.contains_key(path) {
                mark_changed_by_path(path, root, packages, &mut changed);
            }
        }
    }
    if changed.is_empty() {
        return Ok(HashSet::new());
    }
    if changed.contains("__root__") {
        return Ok(packages.iter().map(|p| p.name.clone()).collect());
    }
    let mut affected = changed.clone();
    let mut queue: Vec<String> = changed.into_iter().collect();
    while let Some(name) = queue.pop() {
        if let Some(deps) = graph.reverse_edges.get(&name) {
            for dep in deps {
                if affected.insert(dep.clone()) {
                    queue.push(dep.clone());
                }
            }
        }
    }
    Ok(affected)
}

fn mark_changed_by_path(
    rel_path: &str,
    root: &Path,
    packages: &[Package],
    out: &mut HashSet<String>,
) {
    let rel = rel_path.replace('\\', "/");
    for pkg in packages {
        let pkg_rel = pkg
            .path
            .strip_prefix(root)
            .unwrap_or(&pkg.path)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(&pkg_rel) {
            out.insert(pkg.name.clone());
            return;
        }
    }
    out.insert("__root__".to_string());
}

fn order_packages(
    packages: &[Package],
    graph: &DependencyGraph,
    affected: &HashSet<String>,
) -> Vec<Package> {
    let mut map: HashMap<String, Package> =
        packages.iter().map(|p| (p.name.clone(), p.clone())).collect();
    let mut out = Vec::new();
    for name in &graph.topo_order {
        if affected.contains(name) {
            if let Some(pkg) = map.remove(name) {
                out.push(pkg);
            }
        }
    }
    for (name, pkg) in map {
        if affected.contains(&name) {
            out.push(pkg);
        }
    }
    out
}

fn build_config_for_package(
    base: &BuildConfig,
    workspace: &WorkspaceRoot,
    pkg: &Package,
) -> Result<BuildConfig> {
    let mut cfg = BuildConfig {
        project: ProjectConfig {
            name: pkg.name.clone(),
            language: pkg.language.clone().or_else(|| base.project.language.clone()),
        },
        workspace: base.workspace.clone(),
        packages: base.packages.clone(),
        source: None,
        build: base.build.clone(),
        deploy: base.deploy.clone(),
        output: base.output.clone(),
        cache: base.cache.clone(),
        scan: base.scan.clone(),
        security: base.security.clone(),
        env: base.env.clone(),
        env_from_host: base.env_from_host.clone(),
        sandbox: base.sandbox.clone(),
        signing: base.signing.clone(),
        compatibility: base.compatibility.clone(),
    };

    if pkg.install_cmd.is_some()
        || pkg.build_cmd.is_some()
        || pkg.output_dir.is_some()
        || cfg.build.is_some()
    {
        let mut build = cfg.build.unwrap_or(BuildStepConfig {
            install_cmd: None,
            build_cmd: None,
            parallel_build_cmds: None,
            output_dir: None,
        });
        if pkg.install_cmd.is_some() {
            build.install_cmd = pkg.install_cmd.clone();
        }
        if pkg.build_cmd.is_some() {
            build.build_cmd = pkg.build_cmd.clone();
        }
        if pkg.output_dir.is_some() {
            build.output_dir = pkg.output_dir.clone();
        }
        cfg.build = Some(build);
    }

    let workspace_key = workspace_storage_key(&workspace.path);
    let artifact_dir = workspace_artifact_dir(&cfg, &workspace_key, &pkg.name);
    cfg.deploy.artifact_dir = artifact_dir.display().to_string();
    if let Some(targets) = pkg.targets.clone() {
        cfg.deploy.targets = Some(targets);
    }
    if let Some(image) = pkg.container_image.clone() {
        cfg.deploy.container_image = Some(image);
    }

    if let Some(cache) = cfg.cache.as_mut() {
        let base_cache = cache
            .dir
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(default_cache_dir);
        cache.dir = Some(base_cache.join(workspace_key).join(&pkg.name).display().to_string());
    }
    Ok(cfg)
}

fn workspace_artifact_dir(cfg: &BuildConfig, workspace_key: &str, package: &str) -> PathBuf {
    let configured = PathBuf::from(&cfg.deploy.artifact_dir);
    let default_dir = default_artifact_dir();
    if configured == default_dir {
        return default_dir.join(workspace_key).join(package);
    }
    configured.join(package)
}

fn load_workspace_state(cache_root: &Path) -> Result<Option<HashMap<String, u64>>> {
    let path = cache_root.join("workspace_state.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let map: HashMap<String, u64> = serde_json::from_str(&raw)?;
    Ok(Some(map))
}

fn save_workspace_state(root: &Path, cache_root: &Path) -> Result<()> {
    fs::create_dir_all(cache_root)?;
    let path = cache_root.join("workspace_state.json");
    let current = compute_file_signatures(root)?;
    let raw = serde_json::to_string_pretty(&current)?;
    fs::write(path, raw)?;
    Ok(())
}

fn write_workspace_manifest(
    cfg: &BuildConfig,
    workspace: &WorkspaceRoot,
    packages: &[Package],
) -> Result<()> {
    let workspace_key = workspace_storage_key(&workspace.path);
    let mut entries = Vec::new();
    for pkg in packages {
        let artifact_dir = workspace_artifact_dir(cfg, &workspace_key, &pkg.name);
        let latest = find_latest_build_dir(&artifact_dir);
        entries.push(serde_json::json!({
            "name": pkg.name,
            "path": pkg.path.to_string_lossy(),
            "artifact_dir": artifact_dir.to_string_lossy(),
            "latest_build": latest.map(|p| p.to_string_lossy().to_string()),
        }));
    }
    let manifest_root = workspace_artifact_dir(cfg, &workspace_key, "index")
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| workspace.path.join("artifacts"));
    fs::create_dir_all(&manifest_root)?;
    let manifest_path = manifest_root.join("index.json");
    let raw = serde_json::to_string_pretty(&entries)?;
    fs::write(manifest_path, raw)?;
    Ok(())
}

fn find_latest_build_dir(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &best {
            None => best = Some((path, modified)),
            Some((_, prev)) if modified > *prev => best = Some((path, modified)),
            _ => {}
        }
    }
    best.map(|(p, _)| p)
}

struct DirGuard {
    prev: PathBuf,
}

impl DirGuard {
    fn new(prev: &Path) -> Result<Self> {
        Ok(Self {
            prev: prev.to_path_buf(),
        })
    }
}

impl Drop for DirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.prev);
    }
}
