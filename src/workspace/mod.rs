pub mod engine;

use anyhow::Result;
use serde_json::Value;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceKind {
    Node,
    Rust,
    Go,
    JavaMaven,
    JavaGradle,
    DotNet,
    Python,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    pub path: PathBuf,
    pub kind: WorkspaceKind,
}

#[derive(Debug, Clone)]
pub struct Package {
    pub name: String,
    pub path: PathBuf,
    pub language: Option<String>,
    pub install_cmd: Option<String>,
    pub build_cmd: Option<String>,
    pub output_dir: Option<String>,
    pub start_cmd: Option<String>,
    pub depends_on: Vec<String>,
    pub targets: Option<Vec<String>>,
    pub container_image: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    pub nodes: Vec<String>,
    pub edges: HashMap<String, Vec<String>>,
    pub reverse_edges: HashMap<String, Vec<String>>,
    pub topo_order: Vec<String>,
}

pub fn workspace_storage_key(root: &Path) -> String {
    let identity = root
        .canonicalize()
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|| root.to_string_lossy().replace('\\', "/"));
    let hash = short_hash(&identity);
    format!("workspace-{}", hash)
}

pub fn detect_workspace(root: &Path) -> Option<WorkspaceRoot> {
    if is_node_workspace(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::Node,
        });
    }
    if is_rust_workspace(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::Rust,
        });
    }
    if root.join("go.work").exists() {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::Go,
        });
    }
    if is_gradle_workspace(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::JavaGradle,
        });
    }
    if is_maven_workspace(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::JavaMaven,
        });
    }
    if has_sln(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::DotNet,
        });
    }
    if is_python_workspace(root) {
        return Some(WorkspaceRoot {
            path: root.to_path_buf(),
            kind: WorkspaceKind::Python,
        });
    }
    None
}

pub fn discover_packages(root: &WorkspaceRoot) -> Result<Vec<Package>> {
    match root.kind {
        WorkspaceKind::Node => discover_node_packages(&root.path),
        WorkspaceKind::Rust => discover_rust_packages(&root.path),
        WorkspaceKind::Go => discover_go_packages(&root.path),
        WorkspaceKind::JavaMaven => discover_maven_packages(&root.path),
        WorkspaceKind::JavaGradle => discover_gradle_packages(&root.path),
        WorkspaceKind::DotNet => discover_dotnet_packages(&root.path),
        WorkspaceKind::Python => discover_python_packages(&root.path),
        _ => Ok(Vec::new()),
    }
}

pub fn build_graph(packages: &[Package]) -> DependencyGraph {
    let mut nodes = Vec::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut reverse_edges: HashMap<String, Vec<String>> = HashMap::new();

    for pkg in packages {
        nodes.push(pkg.name.clone());
        for dep in &pkg.depends_on {
            edges.entry(pkg.name.clone()).or_default().push(dep.clone());
            reverse_edges
                .entry(dep.clone())
                .or_default()
                .push(pkg.name.clone());
        }
    }

    let topo_order = topo_sort(&nodes, &edges);
    DependencyGraph {
        nodes,
        edges,
        reverse_edges,
        topo_order,
    }
}

fn discover_node_packages(root: &Path) -> Result<Vec<Package>> {
    let mut patterns = Vec::new();
    if let Some(mut ws) = node_workspace_patterns_from_package_json(root)? {
        patterns.append(&mut ws);
    }
    if let Some(mut ws) = node_workspace_patterns_from_pnpm(root)? {
        patterns.append(&mut ws);
    }
    if patterns.is_empty() {
        return Ok(Vec::new());
    }

    let dirs = expand_workspace_patterns(root, &patterns)?;
    let mut out = Vec::new();
    for dir in dirs {
        let pkg_json = dir.join("package.json");
        if !pkg_json.exists() {
            continue;
        }
        let name = read_package_name(&pkg_json).unwrap_or_else(|| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("package")
                .to_string()
        });
        out.push(Package {
            name,
            path: dir,
            language: Some("nodejs".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    add_node_internal_deps(out)
}

fn discover_rust_packages(root: &Path) -> Result<Vec<Package>> {
    let members = rust_workspace_members(root)?;
    if members.is_empty() {
        return Ok(Vec::new());
    }
    let dirs = expand_workspace_patterns(root, &members)?;
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.join("Cargo.toml").exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("crate")
            .to_string();
        out.push(Package {
            name,
            path: dir,
            language: Some("rust".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    add_rust_internal_deps(out)
}

fn discover_go_packages(root: &Path) -> Result<Vec<Package>> {
    let uses = go_work_uses(root)?;
    if uses.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for rel in uses {
        let dir = root.join(&rel);
        if !dir.join("go.mod").exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("go-module")
            .to_string();
        out.push(Package {
            name,
            path: dir,
            language: Some("go".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    Ok(out)
}

fn discover_maven_packages(root: &Path) -> Result<Vec<Package>> {
    let modules = maven_modules(root)?;
    if modules.is_empty() {
        return Ok(Vec::new());
    }
    let dirs = expand_workspace_patterns(root, &modules)?;
    let mut out = Vec::new();
    for dir in dirs {
        if !dir.join("pom.xml").exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("module")
            .to_string();
        out.push(Package {
            name,
            path: dir,
            language: Some("java".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    Ok(out)
}

fn discover_gradle_packages(root: &Path) -> Result<Vec<Package>> {
    let modules = gradle_includes(root)?;
    if modules.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for rel in modules {
        let dir = root.join(&rel);
        if !dir.exists() {
            continue;
        }
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("module")
            .to_string();
        out.push(Package {
            name,
            path: dir,
            language: Some("java".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    Ok(out)
}

fn discover_dotnet_packages(root: &Path) -> Result<Vec<Package>> {
    let projects = dotnet_projects_from_sln(root)?;
    if projects.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for rel in projects {
        let dir = root.join(&rel);
        if !dir.exists() {
            continue;
        }
        let name = dir
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string();
        out.push(Package {
            name,
            path: dir
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| root.to_path_buf()),
            language: Some("dotnet".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
    }
    Ok(out)
}

fn discover_python_packages(root: &Path) -> Result<Vec<Package>> {
    let mut out = Vec::new();
    if root.join("pyproject.toml").exists() {
        out.push(Package {
            name: root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("python")
                .to_string(),
            path: root.to_path_buf(),
            language: Some("python".to_string()),
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if path.join("pyproject.toml").exists() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("python")
                .to_string();
            out.push(Package {
                name,
                path,
                language: Some("python".to_string()),
                install_cmd: None,
                build_cmd: None,
                output_dir: None,
                start_cmd: None,
                depends_on: Vec::new(),
                targets: None,
                container_image: None,
            });
        }
    }
    Ok(out)
}

fn is_node_workspace(root: &Path) -> bool {
    root.join("pnpm-workspace.yaml").exists()
        || root.join("nx.json").exists()
        || root.join("turbo.json").exists()
        || package_json_has_workspaces(&root.join("package.json"))
}

fn is_rust_workspace(root: &Path) -> bool {
    let cargo = root.join("Cargo.toml");
    if !cargo.exists() {
        return false;
    }
    fs::read_to_string(&cargo)
        .map(|v| v.contains("[workspace]"))
        .unwrap_or(false)
}

fn is_gradle_workspace(root: &Path) -> bool {
    root.join("settings.gradle").exists() || root.join("settings.gradle.kts").exists()
}

fn is_maven_workspace(root: &Path) -> bool {
    let pom = root.join("pom.xml");
    if !pom.exists() {
        return false;
    }
    fs::read_to_string(&pom)
        .map(|v| v.contains("<modules>"))
        .unwrap_or(false)
}

fn has_sln(root: &Path) -> bool {
    fs::read_dir(root)
        .ok()
        .map(|mut it| it.any(|e| e.ok().map(|x| has_ext(&x.path(), "sln")).unwrap_or(false)))
        .unwrap_or(false)
}

fn is_python_workspace(root: &Path) -> bool {
    let pyproject = root.join("pyproject.toml");
    if !pyproject.exists() {
        return false;
    }
    fs::read_to_string(&pyproject)
        .map(|v| v.contains("[tool.poetry]"))
        .unwrap_or(false)
}

fn package_json_has_workspaces(path: &Path) -> bool {
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    json.get("workspaces").is_some()
}

fn node_workspace_patterns_from_package_json(root: &Path) -> Result<Option<Vec<String>>> {
    let pkg = root.join("package.json");
    if !pkg.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&pkg)?;
    let json: Value = serde_json::from_str(&raw)?;
    let Some(workspaces) = json.get("workspaces") else {
        return Ok(None);
    };
    if let Some(arr) = workspaces.as_array() {
        return Ok(Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
        ));
    }
    if let Some(obj) = workspaces.as_object() {
        if let Some(pkgs) = obj.get("packages").and_then(Value::as_array) {
            return Ok(Some(
                pkgs.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect(),
            ));
        }
    }
    Ok(None)
}

fn node_workspace_patterns_from_pnpm(root: &Path) -> Result<Option<Vec<String>>> {
    let path = root.join("pnpm-workspace.yaml");
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("- ") {
            let pattern = rest.trim_matches('"').trim_matches('\'').trim();
            if !pattern.is_empty() {
                out.push(pattern.to_string());
            }
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn rust_workspace_members(root: &Path) -> Result<Vec<String>> {
    let cargo = root.join("Cargo.toml");
    if !cargo.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&cargo)?;
    let parsed: toml::Value = toml::from_str(&raw)?;
    let Some(ws) = parsed.get("workspace") else {
        return Ok(Vec::new());
    };
    let Some(members) = ws.get("members").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    Ok(members
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect())
}

fn go_work_uses(root: &Path) -> Result<Vec<String>> {
    let path = root.join("go.work");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("use ") {
            let value = rest.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                out.push(value.to_string());
            }
        }
    }
    Ok(out)
}

fn maven_modules(root: &Path) -> Result<Vec<String>> {
    let path = root.join("pom.xml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(start) = line.find("<module>") {
            if let Some(end) = line.find("</module>") {
                let value = line[start + 8..end].trim();
                if !value.is_empty() {
                    out.push(value.to_string());
                }
            }
        }
    }
    Ok(out)
}

fn gradle_includes(root: &Path) -> Result<Vec<String>> {
    let mut path = root.join("settings.gradle");
    if !path.exists() {
        path = root.join("settings.gradle.kts");
    }
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("include(") {
            let inner = rest.trim_end_matches(')').trim();
            for part in inner.split(',') {
                let p = part.trim().trim_matches('"').trim_matches('\'');
                if !p.is_empty() {
                    out.push(p.replace(':', "/").trim_start_matches('/').to_string());
                }
            }
        }
    }
    Ok(out)
}

fn dotnet_projects_from_sln(root: &Path) -> Result<Vec<String>> {
    let mut sln_path = None;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if has_ext(&path, "sln") {
            sln_path = Some(path);
            break;
        }
    }
    let Some(path) = sln_path else {
        return Ok(Vec::new());
    };
    let raw = fs::read_to_string(&path)?;
    let mut out = Vec::new();
    for line in raw.lines() {
        if let Some(idx) = line.find(".csproj") {
            let start = line[..idx]
                .rfind('"')
                .map(|v| v + 1)
                .unwrap_or(0);
            let value = line[start..idx + 7].trim_matches('"');
            if !value.is_empty() {
                out.push(value.replace('\\', "/"));
            }
        }
    }
    Ok(out)
}

fn read_package_name(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    json.get("name").and_then(Value::as_str).map(ToString::to_string)
}

fn add_node_internal_deps(mut pkgs: Vec<Package>) -> Result<Vec<Package>> {
    let names: BTreeSet<String> = pkgs.iter().map(|p| p.name.clone()).collect();
    for pkg in &mut pkgs {
        let pkg_json = pkg.path.join("package.json");
        if let Ok(raw) = fs::read_to_string(&pkg_json) {
            if let Ok(json) = serde_json::from_str::<Value>(&raw) {
                let mut deps = Vec::new();
                for field in ["dependencies", "devDependencies", "peerDependencies"] {
                    if let Some(obj) = json.get(field).and_then(Value::as_object) {
                        for name in obj.keys() {
                            if names.contains(name) {
                                deps.push(name.to_string());
                            }
                        }
                    }
                }
                deps.sort();
                deps.dedup();
                pkg.depends_on = deps;
            }
        }
    }
    Ok(pkgs)
}

fn add_rust_internal_deps(mut pkgs: Vec<Package>) -> Result<Vec<Package>> {
    let mut path_map: HashMap<String, String> = HashMap::new();
    for pkg in &pkgs {
        let rel = pkg
            .path
            .to_string_lossy()
            .replace('\\', "/")
            .to_string();
        path_map.insert(rel, pkg.name.clone());
    }
    for pkg in &mut pkgs {
        let cargo = pkg.path.join("Cargo.toml");
        let Ok(raw) = fs::read_to_string(&cargo) else {
            continue;
        };
        let Ok(parsed) = toml::from_str::<toml::Value>(&raw) else {
            continue;
        };
        let mut deps = Vec::new();
        if let Some(tbl) = parsed.get("dependencies").and_then(|v| v.as_table()) {
            for (_name, entry) in tbl {
                if let Some(obj) = entry.as_table() {
                    if let Some(path) = obj.get("path").and_then(|v| v.as_str()) {
                        let full = pkg
                            .path
                            .join(path)
                            .canonicalize()
                            .ok()
                            .map(|p| p.to_string_lossy().replace('\\', "/"));
                        if let Some(full) = full {
                            for (k, v) in &path_map {
                                if full.starts_with(k) {
                                    deps.push(v.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
        deps.sort();
        deps.dedup();
        pkg.depends_on = deps;
    }
    Ok(pkgs)
}

fn expand_workspace_patterns(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut matches = BTreeSet::new();
    for pattern in patterns {
        let norm = pattern.replace('\\', "/");
        let parts = norm.split('/').filter(|p| !p.is_empty()).collect::<Vec<_>>();
        walk_match(root, root, &parts, &mut matches)?;
    }
    Ok(matches.into_iter().collect())
}

fn walk_match(
    root: &Path,
    cursor: &Path,
    pattern: &[&str],
    out: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if pattern.is_empty() {
        if cursor.is_dir() {
            out.insert(cursor.to_path_buf());
        }
        return Ok(());
    }
    let seg = pattern[0];
    if seg == "**" {
        walk_match(root, cursor, &pattern[1..], out)?;
        for entry in fs::read_dir(cursor)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                walk_match(root, &entry.path(), pattern, out)?;
            }
        }
        return Ok(());
    }
    for entry in fs::read_dir(cursor)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if seg == "*" || seg == name {
            walk_match(root, &entry.path(), &pattern[1..], out)?;
        }
    }
    Ok(())
}

fn topo_sort(nodes: &[String], edges: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut indegree: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        indegree.entry(n.clone()).or_insert(0);
    }
    for (from, deps) in edges {
        indegree.entry(from.clone()).or_insert(0);
        for dep in deps {
            *indegree.entry(dep.clone()).or_insert(0) += 1;
        }
    }
    let mut queue: VecDeque<String> = indegree
        .iter()
        .filter_map(|(k, v)| if *v == 0 { Some(k.clone()) } else { None })
        .collect();
    let mut out = Vec::new();
    while let Some(n) = queue.pop_front() {
        out.push(n.clone());
        if let Some(deps) = edges.get(&n) {
            for dep in deps {
                if let Some(v) = indegree.get_mut(dep) {
                    *v = v.saturating_sub(1);
                    if *v == 0 {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }
    }
    out
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false)
}

fn short_hash(input: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("sendbuilds_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_node_workspace_from_package_json() {
        let root = temp_dir("node_ws");
        fs::write(
            root.join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let ws = detect_workspace(&root).unwrap();
        assert_eq!(ws.kind, WorkspaceKind::Node);
    }

    #[test]
    fn discovers_node_packages() {
        let root = temp_dir("node_pkgs");
        fs::write(
            root.join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("packages").join("a")).unwrap();
        fs::create_dir_all(root.join("packages").join("b")).unwrap();
        fs::write(
            root.join("packages").join("a").join("package.json"),
            r#"{"name":"pkg-a"}"#,
        )
        .unwrap();
        fs::write(
            root.join("packages").join("b").join("package.json"),
            r#"{"name":"pkg-b"}"#,
        )
        .unwrap();
        let ws = detect_workspace(&root).unwrap();
        let pkgs = discover_packages(&ws).unwrap();
        let names: BTreeSet<String> = pkgs.iter().map(|p| p.name.clone()).collect();
        assert!(names.contains("pkg-a"));
        assert!(names.contains("pkg-b"));
    }

    #[test]
    fn graph_topo_sort_contains_nodes() {
        let mut pkgs = Vec::new();
        pkgs.push(Package {
            name: "a".to_string(),
            path: PathBuf::from("a"),
            language: None,
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: vec!["b".to_string()],
            targets: None,
            container_image: None,
        });
        pkgs.push(Package {
            name: "b".to_string(),
            path: PathBuf::from("b"),
            language: None,
            install_cmd: None,
            build_cmd: None,
            output_dir: None,
            start_cmd: None,
            depends_on: Vec::new(),
            targets: None,
            container_image: None,
        });
        let graph = build_graph(&pkgs);
        assert!(graph.topo_order.contains(&"a".to_string()));
        assert!(graph.topo_order.contains(&"b".to_string()));
    }
}
