use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
pub struct BuildConfig {
    pub project: ProjectConfig,
    pub workspace: Option<WorkspaceConfig>,
    pub packages: Option<Vec<PackageConfig>>,
    pub source: Option<SourceConfig>,
    pub build: Option<BuildStepConfig>,
    pub deploy: DeployConfig,
    pub output: Option<OutputConfig>,
    pub cache: Option<CacheConfig>,
    pub scan: Option<ScanConfig>,
    pub security: Option<SecurityConfig>,
    pub env: Option<HashMap<String, String>>,
    pub env_from_host: Option<Vec<String>>,
    pub sandbox: Option<SandboxConfig>,
    pub signing: Option<SigningConfig>,
    pub compatibility: Option<CompatibilityConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputConfig {
    pub events: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProjectConfig {
    pub name: String,
    pub language: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceConfig {
    pub enabled: Option<bool>,
    pub root: Option<String>,
    pub mode: Option<String>,
    pub packages: Option<Vec<String>>,
    pub build: Option<String>,
    pub graph_output: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PackageConfig {
    pub name: String,
    pub path: String,
    pub language: Option<String>,
    pub install_cmd: Option<String>,
    pub build_cmd: Option<String>,
    pub output_dir: Option<String>,
    pub start_cmd: Option<String>,
    pub depends_on: Option<Vec<String>>,
    pub targets: Option<Vec<String>>,
    pub container_image: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SourceConfig {
    pub repo: String,
    pub branch: Option<String>,
    pub commit: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BuildStepConfig {
    pub install_cmd: Option<String>,
    pub build_cmd: Option<String>,
    pub parallel_build_cmds: Option<Vec<String>>,
    pub output_dir: Option<String>,
    pub prefer_offline: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DeployConfig {
    pub artifact_dir: String,
    pub targets: Option<Vec<String>>,
    pub container_image: Option<String>,
    pub push_container_image: Option<String>,
    pub verify_container_image: Option<String>,
    pub container_platforms: Option<Vec<String>>,
    pub push_container: Option<bool>,
    pub container_backend: Option<String>,
    pub verify_container_push: Option<bool>,
    pub fail_if_container_unavailable: Option<bool>,
    pub rebase_base: Option<String>,
    pub kubernetes: Option<KubernetesConfig>,
    pub gc: Option<GarbageCollectionConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KubernetesConfig {
    pub enabled: Option<bool>,
    pub namespace: Option<String>,
    pub replicas: Option<u32>,
    pub container_port: Option<u16>,
    pub service_port: Option<u16>,
    pub image_pull_policy: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GarbageCollectionConfig {
    pub enabled: Option<bool>,
    pub keep_last: Option<usize>,
    pub max_age_days: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CacheConfig {
    pub enabled: Option<bool>,
    pub dir: Option<String>,
    pub registry_ref: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScanConfig {
    pub enabled: Option<bool>,
    pub command: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecurityConfig {
    pub enabled: Option<bool>,
    pub fail_on_critical: Option<bool>,
    pub critical_threshold: Option<u32>,
    pub fail_on_scanner_unavailable: Option<bool>,
    pub generate_sbom: Option<bool>,
    pub auto_distroless: Option<bool>,
    pub distroless_base: Option<String>,
    pub rewrite_dockerfile_in_place: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SandboxConfig {
    pub enabled: Option<bool>,
    pub strict: Option<bool>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SigningConfig {
    pub enabled: Option<bool>,
    pub key_env: Option<String>,
    pub auto_generate_key: Option<bool>,
    pub key_file: Option<String>,
    pub generate_provenance: Option<bool>,
    pub cosign: Option<bool>,
    pub cosign_key: Option<String>,
    pub cosign_keyless: Option<bool>,
    pub verify_after_sign: Option<bool>,
    pub verify_certificate_identity: Option<String>,
    pub verify_certificate_oidc_issuer: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CompatibilityConfig {
    pub target_os: Option<String>,
    pub target_arch: Option<String>,
    pub target_node_major: Option<u32>,
}

impl BuildConfig {
    pub fn from_file(path: &str) -> Result<Self> {
        let raw = fs::read_to_string(path).with_context(|| format!("cant read config: {path}"))?;

        toml::from_str(&raw).with_context(|| "config parse failed")
    }

    pub fn for_local_workspace() -> Result<Self> {
        let cwd = std::env::current_dir().with_context(|| "cant resolve current directory")?;
        let name = cwd
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("local-app")
            .to_string();

        Ok(Self {
            project: ProjectConfig {
                name,
                language: None,
            },
            workspace: None,
            packages: None,
            source: None,
            build: None,
            deploy: DeployConfig {
                artifact_dir: default_artifact_dir().display().to_string(),
                targets: Some(vec!["directory".to_string()]),
                container_image: None,
                push_container_image: None,
                verify_container_image: None,
                container_platforms: None,
                push_container: None,
                container_backend: None,
                verify_container_push: None,
                fail_if_container_unavailable: None,
                rebase_base: None,
                kubernetes: None,
                gc: None,
            },
            output: None,
            cache: None,
            scan: None,
            security: None,
            env: None,
            env_from_host: None,
            sandbox: None,
            signing: None,
            compatibility: None,
        })
    }

    pub fn exists(path: &str) -> bool {
        Path::new(path).exists()
    }
}

pub fn default_data_root() -> PathBuf {
    if cfg!(target_os = "windows") {
        return env::var_os("LOCALAPPDATA")
            .or_else(|| env::var_os("APPDATA"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("sendbuilds");
    }
    if cfg!(target_os = "macos") {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return home
            .join("Library")
            .join("Application Support")
            .join("sendbuilds");
    }
    let xdg_data = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|h| h.join(".local").join("share"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    xdg_data.join("sendbuilds")
}

pub fn default_artifact_dir() -> PathBuf {
    default_data_root().join("artifacts")
}

pub fn default_cache_dir() -> PathBuf {
    default_data_root().join("cache")
}

pub fn project_storage_key(cfg: &BuildConfig) -> String {
    let base_name = sanitize_segment(&cfg.project.name);
    let identity = if let Some(source) = cfg.source.as_ref() {
        format!("repo:{}", source.repo)
    } else {
        let cwd = env::current_dir()
            .ok()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        format!("path:{}", cwd.display())
    };
    let hash = short_hash(&identity);
    format!("{base_name}-{hash}")
}

pub fn effective_artifact_dir(cfg: &BuildConfig) -> PathBuf {
    let configured = PathBuf::from(&cfg.deploy.artifact_dir);
    if paths_equivalent(&configured, &default_artifact_dir()) {
        return default_artifact_dir().join(project_storage_key(cfg));
    }
    configured
}

fn sanitize_segment(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' || ch == '.' {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed
    }
}

fn short_hash(input: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut hasher);
    format!("{:08x}", hasher.finish() as u32)
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    let sa = normalize_path_for_compare(a);
    let sb = normalize_path_for_compare(b);
    if cfg!(target_os = "windows") {
        sa.eq_ignore_ascii_case(&sb)
    } else {
        sa == sb
    }
}

fn normalize_path_for_compare(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}
