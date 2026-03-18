use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use clap::{Parser, Subcommand};
use getrandom::getrandom;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::core::config::{
    default_artifact_dir, default_cache_dir, effective_artifact_dir, project_storage_key,
    CacheConfig, DeployConfig, OutputConfig, ProjectConfig, SandboxConfig, ScanConfig,
    SecurityConfig, SigningConfig, SourceConfig,
};
use crate::core::BuildConfig;
use crate::engine::BuildEngine;
use crate::runtime::artifacts;
use crate::workspace::engine::{run_workspace_build, run_workspace_deploy, WorkspaceRunOptions};

#[derive(Parser)]
#[command(name = "sendbuilds", about = "send it. build it.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Build {
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
        #[arg(long, value_parser = clap::builder::BoolishValueParser::new())]
        events: Option<bool>,
        #[arg(long)]
        reproducible: bool,
        #[arg(long)]
        in_place: bool,
        #[arg(long)]
        unused_deps: bool,
        #[arg(long)]
        workspace: bool,
        #[arg(long = "packages", value_delimiter = ',')]
        packages: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        affected: bool,
        #[arg(long)]
        git: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        docker: bool,
        #[arg(long)]
        image: Option<String>,
    },
    Deploy {
        repo: Option<String>,
        #[arg(long)]
        local: bool,
        #[arg(long)]
        build: bool,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        docker: bool,
        #[arg(long = "target", value_delimiter = ',')]
        targets: Vec<String>,
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        workspace: bool,
        #[arg(long = "packages", value_delimiter = ',')]
        packages: Vec<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        affected: bool,
        #[arg(long = "dry-run")]
        dry_run: bool,
        #[arg(long)]
        remote: bool,
    },
    Debug {
        build_id: String,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Replay {
        #[arg(value_name = "build-id")]
        build_id: Option<String>,
        #[arg(long = "buildid", visible_alias = "build-id")]
        buildid: Option<String>,
        #[arg(long = "time-machine", visible_alias = "to")]
        time_machine: Option<String>,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Rollback {
        #[arg(value_name = "build-id")]
        build_id: Option<String>,
        #[arg(long = "to")]
        to: Option<String>,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Diff {
        build_a: String,
        build_b: String,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Artifacts {
        #[command(subcommand)]
        cmd: ArtifactsCmd,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Init {
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    Cache {
        #[command(subcommand)]
        cmd: CacheCmd,
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
    },
    Clean {
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
        #[arg(long)]
        all: bool,
        #[arg(long = "cache-only")]
        cache_only: bool,
    },
    Info {
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
        #[arg(long)]
        env: bool,
        #[arg(long)]
        dependencies: bool,
    },
    Rebase {
        #[arg(short, long, default_value = "sendbuild.toml")]
        config: String,
        #[arg(long)]
        git: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long, default_value = ".")]
        context: String,
        #[arg(long)]
        dockerfile: Option<String>,
        #[arg(long)]
        image: Option<String>,
        #[arg(long = "from-image")]
        from_image: Option<String>,
        #[arg(long = "base")]
        base: Option<String>,
        #[arg(long = "platform", value_delimiter = ',')]
        platforms: Vec<String>,
        #[arg(long)]
        push: bool,
    },
}

#[derive(Subcommand)]
enum CacheCmd {
    Save,
    Restore,
    Clear,
    Status,
}

#[derive(Subcommand)]
enum ArtifactsCmd {
    List {
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Prune {
        #[arg(long = "keep-last")]
        keep_last: Option<usize>,
        #[arg(long = "max-age")]
        max_age_days: Option<u64>,
    },
    Download {
        artifact: String,
        #[arg(long)]
        out: Option<String>,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build {
            config,
            events,
            reproducible,
            in_place,
            unused_deps,
            workspace,
            packages,
            all,
            affected,
            git,
            branch,
            docker,
            image,
        } => {
            if git.is_some() || docker {
                return run_quick_build(git, branch, docker, image, in_place, events, reproducible);
            }
            if BuildConfig::exists(&config) {
                let cfg = BuildConfig::from_file(&config)?;
                let mut build_mode = None;
                if all {
                    build_mode = Some("all".to_string());
                } else if affected {
                    build_mode = Some("affected".to_string());
                }
                let opts = WorkspaceRunOptions {
                    force: workspace,
                    packages: if packages.is_empty() {
                        None
                    } else {
                        Some(packages)
                    },
                    build_mode,
                    events,
                    reproducible,
                    unused_deps,
                };
                if run_workspace_build(cfg.clone(), &opts)? {
                    return Ok(());
                }
                prepare_signing_key(cfg.signing.as_ref())?;
                BuildEngine::from_config(cfg)
                    .with_in_place(in_place)
                    .with_events(events)
                    .with_reproducible(reproducible)
                    .with_unused_deps(unused_deps)
                    .run()
            } else {
                println!(
                    "No config file found at '{}'. Running smart local build mode.",
                    config
                );
                let cfg = BuildConfig::for_local_workspace()?;
                BuildEngine::from_config(cfg)
                    .with_in_place(true)
                    .with_events(events)
                    .with_reproducible(reproducible)
                    .with_unused_deps(unused_deps)
                    .run()
            }
        }
        Cmd::Deploy {
            repo,
            local,
            build,
            branch,
            docker,
            targets,
            image,
            workspace,
            packages,
            all,
            affected,
            dry_run,
            remote,
        } => run_deploy(
            repo, local, build, branch, docker, targets, image, workspace, packages, all, affected,
            dry_run, remote,
        ),
        Cmd::Debug { build_id, config } => run_debug(&build_id, &config),
        Cmd::Replay {
            build_id,
            buildid,
            time_machine,
            config,
        } => run_replay(build_id, buildid, time_machine, &config),
        Cmd::Rollback {
            build_id,
            to,
            config,
        } => run_rollback(build_id, to, &config),
        Cmd::Diff {
            build_a,
            build_b,
            config,
        } => run_diff(&build_a, &build_b, &config),
        Cmd::Artifacts { cmd, config } => run_artifacts(cmd, &config),
        Cmd::Init { template, yes } => init_project(template.as_deref(), yes),
        Cmd::Cache { cmd, config } => run_cache(cmd, &config),
        Cmd::Clean {
            config,
            all,
            cache_only,
        } => clean(&config, all, cache_only),
        Cmd::Info {
            config,
            env,
            dependencies,
        } => info(&config, env, dependencies),
        Cmd::Rebase {
            config,
            git,
            repo,
            branch,
            context,
            dockerfile,
            image,
            from_image,
            base,
            platforms,
            push,
        } => run_rebase(
            &config, git, repo, branch, &context, dockerfile, image, from_image, base, platforms,
            push,
        ),
    }
}

fn run_deploy(
    repo: Option<String>,
    local: bool,
    force_build: bool,
    branch: Option<String>,
    docker: bool,
    targets: Vec<String>,
    image: Option<String>,
    workspace: bool,
    packages: Vec<String>,
    all: bool,
    affected: bool,
    dry_run: bool,
    remote: bool,
) -> Result<()> {
    if local && repo.is_some() {
        bail!("use either positional <repo> or --local, not both");
    }
    if branch.is_some() && (local || repo.is_none()) {
        bail!("--branch requires a git repo deploy target");
    }

    if workspace && repo.is_some() {
        bail!("workspace deploy is only supported with --local for now");
    }
    if workspace && !local {
        println!("Workspace deploy requested; forcing --local mode.");
    }

    if BuildConfig::exists("sendbuild.toml") {
        let cfg = BuildConfig::from_file("sendbuild.toml")?;
        let mut build_mode = None;
        if all {
            build_mode = Some("all".to_string());
        } else if affected {
            build_mode = Some("affected".to_string());
        }
        let opts = WorkspaceRunOptions {
            force: workspace,
            packages: if packages.is_empty() {
                None
            } else {
                Some(packages.clone())
            },
            build_mode,
            events: None,
            reproducible: false,
            unused_deps: false,
        };
        if run_workspace_deploy(cfg, &opts, force_build)? {
            return Ok(());
        }
    }

    let git_repo = if local { None } else { repo };
    let project_name = git_repo
        .as_deref()
        .map(project_name_from_repo)
        .unwrap_or_else(local_project_name);
    let docker_available = command_exists("docker");
    let kubectl_available = command_exists("kubectl");

    let explicit_targets = !targets.is_empty();
    let mut normalized_targets = if targets.is_empty() {
        vec!["directory".to_string()]
    } else {
        targets
            .iter()
            .map(|t| normalize_target(t))
            .collect::<Vec<_>>()
    };
    let target_requires_container = normalized_targets
        .iter()
        .any(|t| t == "container_image" || t == "kubernetes");
    let inferred_container = infer_deploy_container_need(git_repo.as_deref())?;
    let auto_container_for_deploy = !docker
        && !explicit_targets
        && (inferred_container || git_repo.is_some() || kubectl_available);
    let mut should_use_container = docker || target_requires_container || auto_container_for_deploy;

    if should_use_container && !docker_available {
        println!("Docker not detected. Falling back to local deploy runtime.");
        normalized_targets.retain(|t| t != "container_image");
        should_use_container = false;
    }

    if normalized_targets.iter().any(|t| t == "kubernetes") && !kubectl_available {
        println!("kubectl not detected. Skipping kubernetes target.");
        normalized_targets.retain(|t| t != "kubernetes");
    }

    if should_use_container && !normalized_targets.iter().any(|t| t == "container_image") {
        normalized_targets.push("container_image".to_string());
    }
    if normalized_targets.is_empty() {
        normalized_targets.push("directory".to_string());
    }

    if dry_run {
        println!("sendbuilds deploy dry-run");
        println!("repo: {}", git_repo.as_deref().unwrap_or("local-workspace"));
        println!("branch: {}", branch.as_deref().unwrap_or("default"));
        println!("remote: {}", if remote { "requested" } else { "disabled" });
        println!("project: {}", project_name);
        let image_tag = image.unwrap_or_else(|| format!("{project_name}:latest"));
        println!("image: {}", image_tag);
        println!("targets: {}", normalized_targets.join(", "));
        println!(
            "container mode: {}",
            if should_use_container {
                "enabled"
            } else {
                "disabled"
            }
        );
        println!(
            "runtime detection: docker={}, kubectl={}",
            if docker_available {
                "available"
            } else {
                "missing"
            },
            if kubectl_available {
                "available"
            } else {
                "missing"
            }
        );
        println!(
            "reuse existing local artifact: {}",
            if !force_build {
                "yes"
            } else {
                "no (forced rebuild)"
            }
        );
        println!("planned steps:");
        for step in [
            "clone repo",
            "detect language/framework",
            "install dependencies",
            "build project",
            "generate SBOM + supply-chain metadata",
            "run vulnerability/security scan",
            "build container image (if target includes container_image)",
            "sign artifacts/provenance",
            "publish artifacts/deploy targets",
        ] {
            println!("- {step}");
        }
        return Ok(());
    }

    if remote {
        println!("--remote requested; cloud workers are not configured yet, running locally.");
    }

    // Local deploy should behave like deploy/start by default:
    // if there is already a built artifact and no container workflow is needed,
    // reuse it and launch instead of rebuilding.
    let local_non_container = git_repo.is_none() && !should_use_container;
    if local_non_container && !force_build {
        let artifact_root = deploy_artifact_root_from_local_config();
        if let Some(latest) = latest_directory_artifact(&artifact_root)? {
            println!(
                "Reusing existing artifact and starting app from {}",
                normalize_display_path(&latest)
            );
            if start_local_artifact(&latest)? {
                return Ok(());
            }
            println!("Could not auto-start existing artifact.");
        }
        let cwd = env::current_dir()?;
        println!(
            "Trying to start current workspace directly from {}",
            normalize_display_path(&cwd)
        );
        if start_local_artifact(&cwd)? {
            return Ok(());
        }
        println!(
            "No reusable local build was started. Building fresh artifacts and deploying locally ..."
        );
    }

    let image_tag = image.unwrap_or_else(|| format!("{project_name}:latest"));
    let in_place = git_repo.is_none();
    run_quick_build_with_options(
        git_repo.clone(),
        branch.clone(),
        should_use_container,
        Some(image_tag.clone()),
        in_place,
        None,
        None,
        Some(false),
        Some(normalized_targets),
        false,
    )
    .with_context(|| {
        format!(
            "deploy build failed (repo={}, branch={}, container_mode={})",
            git_repo.as_deref().unwrap_or("local-workspace"),
            branch.as_deref().unwrap_or("default"),
            should_use_container
        )
    })?;

    if should_use_container {
        start_deployed_container(&image_tag, &project_name)
            .with_context(|| format!("deploy runtime start failed for image `{image_tag}`"))?;
    } else {
        let artifact_root =
            deploy_artifact_root_for_source(git_repo.as_deref(), branch.as_deref())?;
        if let Some(latest) = latest_directory_artifact(&artifact_root)? {
            if start_local_artifact(&latest)? {
                return Ok(());
            }
        }
        println!(
            "Deploy completed with local artifacts at {}",
            normalize_display_path(&artifact_root)
        );
    }
    Ok(())
}

fn run_debug(build_id: &str, config_path: &str) -> Result<()> {
    let artifact_base = resolve_artifact_base(config_path)?;
    let root = resolve_build_root(&artifact_base, build_id)?;
    let metrics_path = root.join("build-metrics.json");
    let lifecycle_path = root.join("cnb").join("lifecycle-metadata.json");

    println!("Build Debug");
    println!("-----------");
    println!("build_id      : {}", build_id);
    println!("artifact_root : {}", normalize_display_path(&root));

    if metrics_path.exists() {
        let raw = fs::read_to_string(&metrics_path)?;
        let json: Value = serde_json::from_str(&raw)?;
        let finished_at = json
            .get("finished_at")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let cache = json.get("cache").cloned().unwrap_or(Value::Null);
        let steps = json
            .get("steps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let failed = steps
            .iter()
            .filter(|s| s.get("status").and_then(Value::as_str) == Some("failed"))
            .count();
        println!("finished_at   : {}", finished_at);
        println!("steps         : {} total, {} failed", steps.len(), failed);
        if !cache.is_null() {
            println!("cache         : {}", cache);
        }
        println!("top steps:");
        for step in steps.iter().take(10) {
            let name = step
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let status = step
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let dur = step.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
            println!("  - {} [{}] {}ms", name, status, dur);
        }
    } else {
        println!("build-metrics : missing");
    }

    if lifecycle_path.exists() {
        let raw = fs::read_to_string(&lifecycle_path)?;
        let json: Value = serde_json::from_str(&raw)?;
        let artifacts = json
            .get("exported_artifacts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        println!("exported artifacts:");
        for item in artifacts.iter().take(20) {
            if let Some(p) = item.as_str() {
                println!("  - {}", p);
            }
        }
    } else {
        println!("cnb metadata  : missing");
    }
    Ok(())
}

fn run_replay(
    build_id_positional: Option<String>,
    build_id_flag: Option<String>,
    time_machine: Option<String>,
    config_path: &str,
) -> Result<()> {
    let selected = select_build_id(
        build_id_positional,
        build_id_flag,
        time_machine,
        config_path,
    )?;
    run_replay_selected(&selected, config_path)
}

fn run_rollback(build_id: Option<String>, to: Option<String>, config_path: &str) -> Result<()> {
    let selected = select_build_id(build_id, None, to, config_path)?;
    println!("Rolling back to build `{}`", selected);
    run_replay_selected(&selected, config_path)
}

fn run_diff(build_a: &str, build_b: &str, config_path: &str) -> Result<()> {
    let artifact_base = resolve_artifact_base(config_path)?;
    let left = load_build_bundle(&artifact_base, build_a)?;
    let right = load_build_bundle(&artifact_base, build_b)?;

    println!("Build Diff");
    println!("----------");
    println!(
        "build_a   : {} ({})",
        left.id,
        normalize_display_path(&left.root)
    );
    println!(
        "build_b   : {} ({})",
        right.id,
        normalize_display_path(&right.root)
    );

    let left_source = source_summary(&left.metrics);
    let right_source = source_summary(&right.metrics);
    println!("source_a  : {left_source}");
    println!("source_b  : {right_source}");

    let (dep_added, dep_removed, dep_changed) =
        compare_dependency_components(&left.sbom, &right.sbom);
    println!();
    println!("Dependencies changed");
    println!(
        "- added={} removed={} version_changed={}",
        dep_added.len(),
        dep_removed.len(),
        dep_changed.len()
    );
    if let Some(sample) = dep_added.first() {
        println!("- sample added: {sample}");
    }
    if let Some(sample) = dep_removed.first() {
        println!("- sample removed: {sample}");
    }
    if let Some(sample) = dep_changed.first() {
        println!("- sample version change: {sample}");
    }

    println!();
    println!("Base image changed");
    let left_base = detect_base_image(&left);
    let right_base = detect_base_image(&right);
    println!(
        "- build_a base: {}",
        left_base.as_deref().unwrap_or("unknown")
    );
    println!(
        "- build_b base: {}",
        right_base.as_deref().unwrap_or("unknown")
    );
    println!(
        "- changed={}",
        if left_base == right_base { "no" } else { "yes" }
    );

    println!();
    println!("Artifact size difference");
    let left_size = payload_size_bytes(&left)?;
    let right_size = payload_size_bytes(&right)?;
    let delta = right_size as i128 - left_size as i128;
    let pct = if left_size == 0 {
        0.0
    } else {
        (delta as f64 / left_size as f64) * 100.0
    };
    println!("- build_a: {} bytes", left_size);
    println!("- build_b: {} bytes", right_size);
    println!("- delta  : {:+} bytes ({:+.2}%)", delta, pct);

    println!();
    println!("SBOM difference");
    let left_components = sbom_component_count(&left.sbom);
    let right_components = sbom_component_count(&right.sbom);
    println!("- components build_a: {}", left_components);
    println!("- components build_b: {}", right_components);
    println!(
        "- delta components: {:+}",
        right_components as i64 - left_components as i64
    );

    println!();
    println!("Security differences");
    print_security_delta("source-scan", &left.security, &right.security);
    let left_container = left
        .security
        .as_ref()
        .and_then(|v| v.get("container_scan"))
        .filter(|v| !v.is_null())
        .cloned();
    let right_container = right
        .security
        .as_ref()
        .and_then(|v| v.get("container_scan"))
        .filter(|v| !v.is_null())
        .cloned();
    print_security_delta("container-scan", &left_container, &right_container);

    Ok(())
}

#[derive(Debug, Clone)]
struct BuildBundle {
    id: String,
    root: PathBuf,
    metrics: Value,
    sbom: Value,
    security: Option<Value>,
    lifecycle: Option<Value>,
    supply_chain: Option<Value>,
    container_image: Option<String>,
}

fn load_build_bundle(artifact_base: &Path, build_id: &str) -> Result<BuildBundle> {
    let root = resolve_build_root(artifact_base, build_id)?;
    let metrics = read_required_json(&root.join("build-metrics.json"))?;
    let sbom = read_optional_json(&root.join("sbom.json")).unwrap_or(Value::Null);
    let security = read_optional_json(&root.join("security-report.json"))
        .or_else(|| metrics.get("security").cloned().filter(|v| !v.is_null()));
    let lifecycle = read_optional_json(&root.join("cnb").join("lifecycle-metadata.json"));
    let supply_chain = read_optional_json(&root.join("supply-chain-metadata.json")).or_else(|| {
        metrics
            .get("supply_chain_metadata")
            .cloned()
            .filter(|v| !v.is_null())
    });
    let container_notes = glob_container_note_files(&root)?;
    let container_image =
        first_container_image_from_notes(&container_notes)?.map(|(image, _path)| image);

    Ok(BuildBundle {
        id: build_id.to_string(),
        root,
        metrics,
        sbom,
        security,
        lifecycle,
        supply_chain,
        container_image,
    })
}

fn read_required_json(path: &Path) -> Result<Value> {
    let raw = fs::read_to_string(path).with_context(|| {
        format!(
            "missing or unreadable json file: {}",
            normalize_display_path(path)
        )
    })?;
    let parsed = serde_json::from_str::<Value>(&raw)
        .with_context(|| format!("invalid json in {}", normalize_display_path(path)))?;
    Ok(parsed)
}

fn read_optional_json(path: &Path) -> Option<Value> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

fn source_summary(metrics: &Value) -> String {
    let source = metrics.get("source").and_then(Value::as_object);
    let repo = source
        .and_then(|s| s.get("repo"))
        .and_then(Value::as_str)
        .unwrap_or("unknown-repo");
    let branch = source
        .and_then(|s| s.get("branch"))
        .and_then(Value::as_str)
        .unwrap_or("unknown-branch");
    let commit = source
        .and_then(|s| s.get("commit"))
        .and_then(Value::as_str)
        .unwrap_or("unknown-commit");
    format!("{repo}@{branch}#{commit}")
}

fn compare_dependency_components(
    left_sbom: &Value,
    right_sbom: &Value,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let left = sbom_component_map(left_sbom);
    let right = sbom_component_map(right_sbom);

    let left_keys = left.keys().cloned().collect::<BTreeSet<_>>();
    let right_keys = right.keys().cloned().collect::<BTreeSet<_>>();

    let added = right_keys
        .difference(&left_keys)
        .map(|k| {
            format!(
                "{k}@{}",
                right
                    .get(k)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string())
            )
        })
        .collect::<Vec<_>>();
    let removed = left_keys
        .difference(&right_keys)
        .map(|k| {
            format!(
                "{k}@{}",
                left.get(k)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string())
            )
        })
        .collect::<Vec<_>>();

    let mut changed = Vec::new();
    for key in left_keys.intersection(&right_keys) {
        let Some(a) = left.get(key) else {
            continue;
        };
        let Some(b) = right.get(key) else {
            continue;
        };
        if a != b {
            changed.push(format!("{key}: {a} -> {b}"));
        }
    }
    (added, removed, changed)
}

fn sbom_component_map(sbom: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(components) = sbom.get("components").and_then(Value::as_array) else {
        return out;
    };

    for item in components {
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let scope = item
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("default");
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("library");
        let key = format!("{kind}:{scope}:{name}");
        let version = item
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        out.insert(key, version);
    }
    out
}

fn detect_base_image(bundle: &BuildBundle) -> Option<String> {
    let from_security = bundle
        .security
        .as_ref()
        .and_then(|v| v.get("distroless"))
        .and_then(Value::as_object)
        .and_then(|d| {
            d.get("to_base")
                .and_then(Value::as_str)
                .or_else(|| d.get("to").and_then(Value::as_str))
                .or_else(|| d.get("from_base").and_then(Value::as_str))
                .or_else(|| d.get("from").and_then(Value::as_str))
        })
        .map(|s| s.to_string());
    if from_security.is_some() {
        return from_security;
    }
    let from_supply = bundle
        .supply_chain
        .as_ref()
        .and_then(|v| v.get("distroless"))
        .and_then(Value::as_object)
        .and_then(|d| {
            d.get("to")
                .and_then(Value::as_str)
                .or_else(|| d.get("from").and_then(Value::as_str))
        })
        .map(|s| s.to_string());
    if from_supply.is_some() {
        return from_supply;
    }
    bundle.container_image.clone()
}

fn payload_size_bytes(bundle: &BuildBundle) -> Result<u64> {
    if let Some(lifecycle) = &bundle.lifecycle {
        if let Some(paths) = lifecycle
            .get("exported_artifacts")
            .and_then(Value::as_array)
        {
            let mut total = 0u64;
            for p in paths {
                let Some(rel) = p.as_str() else {
                    continue;
                };
                let abs = bundle.root.join(rel);
                total = total.saturating_add(path_size_bytes(&abs)?);
            }
            if total > 0 {
                return Ok(total);
            }
        }
    }
    path_size_bytes(&bundle.root)
}

fn path_size_bytes(path: &Path) -> Result<u64> {
    let meta = fs::metadata(path)?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        total = total.saturating_add(path_size_bytes(&entry.path())?);
    }
    Ok(total)
}

fn sbom_component_count(sbom: &Value) -> usize {
    sbom.get("components")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0)
}

fn print_security_delta(label: &str, left: &Option<Value>, right: &Option<Value>) {
    let left_scan = left.as_ref().and_then(Value::as_object);
    let right_scan = right.as_ref().and_then(Value::as_object);
    if left_scan.is_none() && right_scan.is_none() {
        println!("- {label}: unavailable in both builds");
        return;
    }

    let metrics = [
        "total",
        "critical",
        "high",
        "moderate",
        "low",
        "info",
        "misconfigurations",
        "secrets",
    ];
    let mut parts = Vec::new();
    for key in metrics {
        let a = left_scan
            .and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let b = right_scan
            .and_then(|m| m.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let d = b as i128 - a as i128;
        parts.push(format!("{key}:{}->{}/({:+})", a, b, d));
    }
    println!("- {label}: {}", parts.join(", "));
}

fn run_replay_selected(build_id: &str, config_path: &str) -> Result<()> {
    let artifact_base = resolve_artifact_base(config_path)?;
    let root = resolve_build_root(&artifact_base, build_id)?;
    println!(
        "Replaying deploy from artifact {}",
        normalize_display_path(&root)
    );

    let container_notes = glob_container_note_files(&root)?;
    if let Some((image, _note_path)) = first_container_image_from_notes(&container_notes)? {
        let project_hint = project_name_from_repo(&image);
        start_deployed_container(&image, &project_hint)?;
        return Ok(());
    }

    let dir_artifact = root.join("directory");
    if dir_artifact.exists() {
        if start_local_artifact(&dir_artifact)? {
            return Ok(());
        }
        bail!("replay failed: found directory artifact but no runnable start command");
    }
    bail!(
        "replay failed: no runnable artifact found for build-id `{}`",
        build_id
    )
}

fn select_build_id(
    build_id_positional: Option<String>,
    build_id_flag: Option<String>,
    time_machine: Option<String>,
    config_path: &str,
) -> Result<String> {
    let positional = build_id_positional.map(|s| s.trim().to_string());
    let flag = build_id_flag.map(|s| s.trim().to_string());
    let by_id = match (positional, flag) {
        (Some(a), Some(b)) => {
            if a != b {
                bail!(
                    "conflicting build-id values: positional `{}` vs --buildid `{}`",
                    a,
                    b
                );
            }
            Some(a)
        }
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };

    if let Some(id) = by_id {
        if id.is_empty() {
            bail!("build-id cannot be empty");
        }
        if time_machine.is_some() {
            bail!("use either build-id or --time-machine/--to, not both");
        }
        return Ok(id);
    }

    let tm = time_machine
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("missing build target: provide <build-id> or --time-machine/--to")
        })?;
    select_build_id_for_time_machine(config_path, &tm)
}

fn select_build_id_for_time_machine(config_path: &str, time_machine: &str) -> Result<String> {
    let artifact_base = resolve_artifact_base(config_path)?;
    let builds = list_build_dirs(&artifact_base)?;
    if builds.is_empty() {
        bail!(
            "no artifacts found under {}",
            normalize_display_path(&artifact_base)
        );
    }
    let target = parse_time_machine_to_system_time(time_machine)?;
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for build in builds {
        let modified = build
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if modified <= target {
            match &best {
                Some((_, current)) if modified <= *current => {}
                _ => best = Some((build, modified)),
            }
        }
    }
    if let Some((path, _)) = best {
        let id = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("failed to read selected build id"))?;
        println!(
            "Time-machine selected build `{}` for `{}`",
            id, time_machine
        );
        return Ok(id.to_string());
    }
    bail!(
        "no build found at or before `{}` under {}",
        time_machine,
        normalize_display_path(&artifact_base)
    )
}

fn parse_time_machine_to_system_time(input: &str) -> Result<SystemTime> {
    if let Ok(dt_fixed) = DateTime::parse_from_rfc3339(input) {
        let local = dt_fixed.with_timezone(&Local);
        return Ok(system_time_from_local(local)?);
    }
    if let Ok(dt_local) = NaiveDateTime::parse_from_str(input, "%Y-%m-%d %H:%M:%S") {
        let local = Local
            .from_local_datetime(&dt_local)
            .single()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve local datetime `{}`", input))?;
        return Ok(system_time_from_local(local)?);
    }
    if let Ok(date) = NaiveDate::parse_from_str(input, "%Y-%m-%d") {
        let local = Local
            .with_ymd_and_hms(date.year(), date.month(), date.day(), 23, 59, 59)
            .single()
            .ok_or_else(|| anyhow::anyhow!("failed to resolve local date `{}`", input))?;
        return Ok(system_time_from_local(local)?);
    }
    bail!(
        "invalid --time-machine/--to value `{}`. expected RFC3339, `YYYY-MM-DD HH:MM:SS`, or `YYYY-MM-DD`",
        input
    )
}

fn system_time_from_local(dt: DateTime<Local>) -> Result<SystemTime> {
    let ts = dt.timestamp();
    if ts >= 0 {
        Ok(SystemTime::UNIX_EPOCH + Duration::from_secs(ts as u64))
    } else {
        Ok(SystemTime::UNIX_EPOCH - Duration::from_secs((-ts) as u64))
    }
}

fn run_artifacts(cmd: ArtifactsCmd, config_path: &str) -> Result<()> {
    let artifact_base = resolve_artifact_base(config_path)?;
    fs::create_dir_all(&artifact_base)?;

    match cmd {
        ArtifactsCmd::List { all, limit } => {
            let builds = list_build_dirs(&artifact_base)?;
            println!("Artifacts under {}", normalize_display_path(&artifact_base));
            let list: Vec<PathBuf> = if all {
                builds
            } else {
                builds.into_iter().take(limit).collect()
            };
            for path in &list {
                let id = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown");
                let m = path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let age_days = SystemTime::now()
                    .duration_since(m)
                    .ok()
                    .map(|d| d.as_secs() / 86_400)
                    .unwrap_or(0);
                let project_name = detect_project_name_for_build(&artifact_base, path)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("- {} (project={}, age={}d)", id, project_name, age_days);
            }
            if list.is_empty() {
                println!("(no artifacts found)");
            }
        }
        ArtifactsCmd::Prune {
            keep_last,
            max_age_days,
        } => {
            let mut builds = list_build_dirs(&artifact_base)?;
            let keep = keep_last.unwrap_or(20);
            let max_age = max_age_days.unwrap_or(30);
            let now = SystemTime::now();
            let mut removed = 0usize;
            for (idx, dir) in builds.drain(..).enumerate() {
                let modified = dir
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let stale = now
                    .duration_since(modified)
                    .ok()
                    .map(|d| d > Duration::from_secs(max_age * 86_400))
                    .unwrap_or(false);
                if idx >= keep || stale {
                    fs::remove_dir_all(&dir)?;
                    removed += 1;
                }
            }
            println!(
                "Pruned {} artifact build(s) from {}",
                removed,
                normalize_display_path(&artifact_base)
            );
        }
        ArtifactsCmd::Download { artifact, out } => {
            let src = resolve_artifact_reference(&artifact_base, &artifact)?;
            let out_path = out.map(PathBuf::from).unwrap_or_else(|| {
                env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(
                        src.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("artifact"),
                    )
            });
            if src.is_file() {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src, &out_path)?;
            } else if src.is_dir() {
                copy_dir_recursive(&src, &out_path)?;
            } else {
                bail!("artifact not found: {}", normalize_display_path(&src));
            }
            println!(
                "Downloaded artifact to {}",
                normalize_display_path(&out_path)
            );
        }
    }
    Ok(())
}

fn resolve_artifact_base(config_path: &str) -> Result<PathBuf> {
    if BuildConfig::exists(config_path) {
        let cfg = BuildConfig::from_file(config_path)?;
        return Ok(effective_artifact_dir(&cfg));
    }
    Ok(default_artifact_dir())
}

fn resolve_build_root(artifact_base: &Path, build_id: &str) -> Result<PathBuf> {
    let direct = artifact_base.join(build_id);
    if direct.exists() {
        return Ok(direct);
    }
    let candidates = list_build_dirs(artifact_base)?;
    if let Some(found) = candidates.into_iter().find(|p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n == build_id)
            .unwrap_or(false)
    }) {
        return Ok(found);
    }
    bail!(
        "build-id `{}` not found under {}",
        build_id,
        normalize_display_path(artifact_base)
    )
}

fn list_build_dirs(artifact_base: &Path) -> Result<Vec<PathBuf>> {
    if !artifact_base.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(artifact_base)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if is_build_id_dir_name(&name) {
                dirs.push(entry.path());
            } else {
                for sub in fs::read_dir(entry.path())? {
                    let sub = sub?;
                    if !sub.file_type()?.is_dir() {
                        continue;
                    }
                    let sub_name = sub.file_name();
                    let sub_name = sub_name.to_string_lossy();
                    if is_build_id_dir_name(&sub_name) {
                        dirs.push(sub.path());
                    }
                }
            }
        }
    }
    dirs.sort_by(|a, b| {
        let am = a
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let bm = b
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        bm.cmp(&am)
    });
    Ok(dirs)
}

fn is_build_id_dir_name(name: &str) -> bool {
    // legacy: YYYYMMDD_HHMMSS
    // current: YYYYMMDD_HHMMSSmmm[-N]
    if name.len() < 15 {
        return false;
    }
    let bytes = name.as_bytes();
    for (idx, b) in bytes.iter().enumerate().take(15) {
        if idx == 8 && *b != b'_' {
            return false;
        }
        if idx != 8 && !b.is_ascii_digit() {
            return false;
        }
    }
    if name.len() == 15 {
        return true;
    }
    let suffix = &name[15..];
    if suffix.len() < 3 {
        return false;
    }
    let mut seen_dash = false;
    for ch in suffix.chars() {
        if ch == '-' {
            if seen_dash {
                return false;
            }
            seen_dash = true;
            continue;
        }
        if !ch.is_ascii_digit() {
            return false;
        }
    }
    true
}

fn detect_project_name_for_build(artifact_base: &Path, build_dir: &Path) -> Option<String> {
    let metrics_path = build_dir.join("build-metrics.json");
    if metrics_path.exists() {
        if let Ok(raw) = fs::read_to_string(&metrics_path) {
            if let Ok(json) = serde_json::from_str::<Value>(&raw) {
                if let Some(name) = json.get("project").and_then(Value::as_str) {
                    if !name.trim().is_empty() {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    let parent = build_dir.parent()?;
    if parent != artifact_base {
        if let Some(storage_key) = parent.file_name().and_then(|n| n.to_str()) {
            return Some(project_name_from_storage_key(storage_key));
        }
    }
    None
}

fn project_name_from_storage_key(storage_key: &str) -> String {
    let bytes = storage_key.as_bytes();
    if bytes.len() > 9 && bytes[bytes.len() - 9] == b'-' {
        let suffix = &storage_key[bytes.len() - 8..];
        if suffix.chars().all(|c| c.is_ascii_hexdigit()) {
            return storage_key[..bytes.len() - 9].to_string();
        }
    }
    storage_key.to_string()
}

fn resolve_artifact_reference(artifact_base: &Path, artifact: &str) -> Result<PathBuf> {
    let raw = PathBuf::from(artifact);
    if raw.is_absolute() && raw.exists() {
        return Ok(raw);
    }
    let combined = artifact_base.join(artifact);
    if combined.exists() {
        return Ok(combined);
    }
    let by_build = artifact_base.join(artifact);
    if by_build.exists() {
        return Ok(by_build);
    }
    bail!(
        "artifact `{}` not found under {}",
        artifact,
        normalize_display_path(artifact_base)
    )
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if entry.file_type()?.is_file() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn glob_container_note_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("container-image-") && name.ends_with(".txt") {
            out.push(entry.path());
        }
    }
    Ok(out)
}

fn first_container_image_from_notes(notes: &[PathBuf]) -> Result<Option<(String, PathBuf)>> {
    for note in notes {
        let raw = fs::read_to_string(note)?;
        for line in raw.lines() {
            if let Some(image) = line.strip_prefix("image=") {
                let value = image.trim().to_string();
                if !value.is_empty() {
                    return Ok(Some((value, note.clone())));
                }
            }
        }
    }
    Ok(None)
}

fn deploy_artifact_root_from_local_config() -> PathBuf {
    BuildConfig::from_file("sendbuild.toml")
        .map(|c| effective_artifact_dir(&c))
        .or_else(|_| BuildConfig::for_local_workspace().map(|c| effective_artifact_dir(&c)))
        .unwrap_or_else(|_| default_artifact_dir())
}

fn deploy_artifact_root_for_source(
    git_repo: Option<&str>,
    git_branch: Option<&str>,
) -> Result<PathBuf> {
    if git_repo.is_none() {
        return Ok(deploy_artifact_root_from_local_config());
    }
    let name = git_repo
        .map(project_name_from_repo)
        .unwrap_or_else(local_project_name);
    let cfg = BuildConfig {
        project: ProjectConfig {
            name,
            language: None,
        },
        workspace: None,
        packages: None,
        source: git_repo.map(|repo| SourceConfig {
            repo: repo.to_string(),
            branch: git_branch.map(|b| b.to_string()),
            commit: None,
        }),
        build: None,
        deploy: DeployConfig {
            artifact_dir: normalize_display_path(&default_artifact_dir()),
            targets: Some(vec!["directory".to_string()]),
            container_image: None,
            container_platforms: None,
            push_container: Some(false),
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
    };
    Ok(effective_artifact_dir(&cfg))
}

fn latest_directory_artifact(artifact_root: &Path) -> Result<Option<PathBuf>> {
    if !artifact_root.exists() {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(artifact_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let ts_dir = entry.path();
        let out = ts_dir.join("directory");
        if out.exists() {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            candidates.push((out, modified));
        }
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(candidates.into_iter().map(|(p, _)| p).next())
}

fn start_local_artifact(dir: &Path) -> Result<bool> {
    if !dir.exists() {
        return Ok(false);
    }
    // Built server entrypoints first (Next/Nuxt/custom)
    if dir
        .join(".next")
        .join("standalone")
        .join("server.js")
        .exists()
        && command_exists("node")
    {
        println!("Starting Next.js standalone server ...");
        let status = Command::new("node")
            .arg(".next/standalone/server.js")
            .current_dir(dir)
            .status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }
    if dir
        .join(".output")
        .join("server")
        .join("index.mjs")
        .exists()
        && command_exists("node")
    {
        println!("Starting `.output/server/index.mjs` ...");
        let status = Command::new("node")
            .arg(".output/server/index.mjs")
            .current_dir(dir)
            .status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }
    if dir.join("server.js").exists() && command_exists("node") {
        println!("Starting `server.js` ...");
        let status = Command::new("node")
            .arg("server.js")
            .current_dir(dir)
            .status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }

    // Node runtime apps
    if dir.join("package.json").exists() {
        let commands: Vec<(&str, Vec<&str>)> = if dir.join("pnpm-lock.yaml").exists() {
            vec![
                ("pnpm", vec!["run", "start"]),
                ("npm", vec!["run", "start"]),
            ]
        } else if dir.join("yarn.lock").exists() {
            vec![("yarn", vec!["start"]), ("npm", vec!["run", "start"])]
        } else {
            vec![("npm", vec!["run", "start"])]
        };
        for (bin, args) in commands {
            if command_exists(bin) {
                println!(
                    "Starting local artifact with `{}` ...",
                    format!("{bin} {}", args.join(" "))
                );
                let status = Command::new(bin).args(args).current_dir(dir).status();
                if status.as_ref().map(|s| s.success()).unwrap_or(false) {
                    return Ok(true);
                }
            }
        }
    }

    // Python web apps
    if dir.join("manage.py").exists() && (command_exists("python") || command_exists("python3")) {
        let py = if command_exists("python") {
            "python"
        } else {
            "python3"
        };
        println!("Starting local artifact with `{py} manage.py runserver 0.0.0.0:8000` ...");
        let status = Command::new(py)
            .args(["manage.py", "runserver", "0.0.0.0:8000"])
            .current_dir(dir)
            .status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }
    if dir.join("app.py").exists() && (command_exists("python") || command_exists("python3")) {
        let py = if command_exists("python") {
            "python"
        } else {
            "python3"
        };
        println!("Starting local artifact with `{py} app.py` ...");
        let status = Command::new(py).arg("app.py").current_dir(dir).status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }

    // Static site fallback
    if dir.join("index.html").exists() && (command_exists("python") || command_exists("python3")) {
        let py = if command_exists("python") {
            "python"
        } else {
            "python3"
        };
        println!("Starting static artifact server on http://127.0.0.1:4173 ...");
        let status = Command::new(py)
            .args(["-m", "http.server", "4173"])
            .current_dir(dir)
            .status();
        if status.as_ref().map(|s| s.success()).unwrap_or(false) {
            return Ok(true);
        }
    }
    // Generic fallback: infer a runnable command for non-Node/Python stacks.
    if let Ok(cmd) = artifacts::infer_local_start_command(dir) {
        if let Some((bin, args)) = cmd.split_first() {
            if !looks_like_path(bin) && !command_exists(bin) {
                println!(
                    "Skipping inferred start command `{}` (runtime not available).",
                    bin
                );
                return Ok(false);
            }
            println!("Starting local artifact with `{}` ...", cmd.join(" "));
            let status = Command::new(bin).args(args).current_dir(dir).status();
            if status.as_ref().map(|s| s.success()).unwrap_or(false) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn command_exists(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn looks_like_path(bin: &str) -> bool {
    bin.starts_with('.') || bin.contains('\\') || bin.contains('/') || bin.contains(':')
}

fn start_deployed_container(image: &str, project_name: &str) -> Result<()> {
    if !command_exists("docker") {
        bail!("docker is required to start deployed container, but it was not found");
    }
    ensure_docker_daemon_ready()?;

    let container_name = format!("{project_name}-deploy");
    let _ = Command::new("docker")
        .args(["rm", "-f", &container_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    if !docker_image_exists(image)? {
        bail!(
            "image `{}` is not present locally. the container build may have been skipped or failed earlier. \
run `sendbuilds build --docker --image {}` and retry deploy",
            image,
            image
        );
    }

    println!(
        "Starting deployed container `{}` from image `{}` ...",
        container_name, image
    );
    let run = Command::new("docker")
        .args(["run", "-d", "--name", &container_name, "-P", image])
        .output()?;
    if !run.status.success() {
        let stderr = render_output_stream(&run.stderr);
        let stdout = render_output_stream(&run.stdout);
        let state =
            docker_container_state(&container_name).unwrap_or_else(|| "unavailable".to_string());
        let logs =
            docker_container_logs(&container_name).unwrap_or_else(|| "unavailable".to_string());
        bail!(
            "failed to start container `{}` from image `{}`. docker_run_exit={:?}; docker_run_stderr={}; docker_run_stdout={}; container_state={}; container_logs={}; hints: verify Dockerfile CMD/ENTRYPOINT, required env vars, and exposed ports",
            container_name,
            image,
            run.status.code(),
            stderr,
            stdout,
            state,
            logs
        );
    }

    let container_id = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let ports = Command::new("docker")
        .args(["port", &container_name])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "no published ports reported".to_string());

    println!("Container started.");
    println!("container_name={}", container_name);
    println!("container_id={}", container_id);
    println!("image={}", image);
    println!("ports={}", ports.replace('\n', ", "));
    let running = Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &container_name])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("running={}", running);
    Ok(())
}

fn ensure_docker_daemon_ready() -> Result<()> {
    let out = Command::new("docker").arg("info").output();
    match out {
        Ok(res) if res.status.success() => Ok(()),
        Ok(res) => bail!(
            "docker daemon is not ready (docker info exit={:?}). stderr={} stdout={}. \
hints: start Docker Desktop (or dockerd) and retry",
            res.status.code(),
            render_output_stream(&res.stderr),
            render_output_stream(&res.stdout)
        ),
        Err(err) => bail!("failed to run `docker info`: {}", err),
    }
}

fn docker_image_exists(image: &str) -> Result<bool> {
    let out = Command::new("docker")
        .args(["image", "inspect", image])
        .output()?;
    Ok(out.status.success())
}

fn docker_container_state(container_name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{.State.Status}}|{{.State.Error}}|{{.State.ExitCode}}",
            container_name,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn docker_container_logs(container_name: &str) -> Option<String> {
    let out = Command::new("docker")
        .args(["logs", "--tail", "80", container_name])
        .output()
        .ok()?;
    let logs = if out.stdout.is_empty() && out.stderr.is_empty() {
        String::new()
    } else if out.stderr.is_empty() {
        render_output_stream(&out.stdout)
    } else if out.stdout.is_empty() {
        render_output_stream(&out.stderr)
    } else {
        format!(
            "{} | {}",
            render_output_stream(&out.stdout),
            render_output_stream(&out.stderr)
        )
    };
    if logs.is_empty() {
        None
    } else {
        Some(logs)
    }
}

fn render_output_stream(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw).replace('\n', " | ");
    let trimmed = text.trim().to_string();
    if trimmed.len() > 600 {
        let mut clipped = trimmed.chars().take(600).collect::<String>();
        clipped.push_str(" ...");
        clipped
    } else {
        trimmed
    }
}

fn infer_deploy_container_need(git_repo: Option<&str>) -> Result<bool> {
    if let Ok(cfg) = BuildConfig::from_file("sendbuild.toml") {
        let from_cfg = cfg
            .deploy
            .targets
            .as_ref()
            .map(|targets| {
                targets.iter().any(|t| {
                    let n = normalize_target(t);
                    n == "container_image" || n == "kubernetes"
                })
            })
            .unwrap_or(false);
        if from_cfg {
            return Ok(true);
        }
    }

    if git_repo.is_none() {
        let cwd = env::current_dir()?;
        if cwd.join("Dockerfile").exists() || cwd.join("docker-compose.yml").exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn run_quick_build(
    git_repo: Option<String>,
    git_branch: Option<String>,
    docker: bool,
    image: Option<String>,
    in_place: bool,
    events: Option<bool>,
    reproducible: bool,
) -> Result<()> {
    run_quick_build_with_options(
        git_repo,
        git_branch,
        docker,
        image,
        in_place,
        events,
        None,
        None,
        None,
        reproducible,
    )
}

fn run_quick_build_with_options(
    git_repo: Option<String>,
    git_branch: Option<String>,
    docker: bool,
    image: Option<String>,
    in_place: bool,
    events: Option<bool>,
    rebase_base: Option<String>,
    fail_on_scanner_unavailable: Option<bool>,
    explicit_targets: Option<Vec<String>>,
    reproducible: bool,
) -> Result<()> {
    let has_git = git_repo.is_some();
    let name = git_repo
        .as_deref()
        .map(project_name_from_repo)
        .unwrap_or_else(|| local_project_name());
    let mut targets = explicit_targets.unwrap_or_else(|| vec!["directory".to_string()]);
    if docker && !targets.iter().any(|t| t == "container_image") {
        targets.push("container_image".to_string());
    }
    let wants_container = targets.iter().any(|t| t == "container_image");
    let image_tag = image.unwrap_or_else(|| format!("{name}:latest"));

    let cfg = BuildConfig {
        project: ProjectConfig {
            name: name.clone(),
            language: None,
        },
        workspace: None,
        packages: None,
        source: git_repo.map(|repo| SourceConfig {
            repo,
            branch: git_branch,
            commit: None,
        }),
        build: None,
        deploy: DeployConfig {
            artifact_dir: normalize_display_path(&default_artifact_dir()),
            targets: Some(targets),
            container_image: if wants_container {
                Some(image_tag)
            } else {
                None
            },
            container_platforms: None,
            // quick builds ofc
            push_container: Some(false),
            rebase_base,
            kubernetes: None,
            gc: None,
        },
        output: Some(OutputConfig { events }),
        cache: Some(CacheConfig {
            enabled: Some(true),
            dir: None,
            registry_ref: None,
        }),
        scan: Some(ScanConfig {
            enabled: Some(false),
            command: None,
        }),
        security: Some(SecurityConfig {
            enabled: Some(true),
            fail_on_critical: Some(true),
            critical_threshold: Some(0),
            fail_on_scanner_unavailable: Some(fail_on_scanner_unavailable.unwrap_or(true)),
            generate_sbom: Some(true),
            auto_distroless: Some(true),
            distroless_base: None,
            rewrite_dockerfile_in_place: Some(false),
        }),
        env: None,
        env_from_host: None,
        sandbox: Some(SandboxConfig {
            enabled: Some(true),
            strict: Some(true),
        }),
        signing: Some(SigningConfig {
            enabled: Some(true),
            key_env: Some("SENDBUILD_SIGNING_KEY".to_string()),
            auto_generate_key: Some(true),
            key_file: Some(".sendbuild/signing.key".to_string()),
            generate_provenance: Some(true),
            cosign: Some(false),
            cosign_key: None,
            cosign_keyless: None,
            verify_after_sign: None,
            verify_certificate_identity: None,
            verify_certificate_oidc_issuer: None,
        }),
        compatibility: None,
    };

    prepare_signing_key(cfg.signing.as_ref())?;

    BuildEngine::from_config(cfg)
        .with_in_place(in_place || !has_git)
        .with_events(events)
        .with_reproducible(reproducible)
        .run()
}

fn local_project_name() -> String {
    env::current_dir()
        .ok()
        .and_then(|cwd| {
            cwd.file_name()
                .and_then(|n| n.to_str().map(ToString::to_string))
        })
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "local-app".to_string())
}

fn project_name_from_repo(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/').trim();
    let last = trimmed.rsplit('/').next().unwrap_or("app");
    let no_git = last.strip_suffix(".git").unwrap_or(last);
    let mut out = String::new();
    for ch in no_git.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' || ch == '.' {
            out.push('-');
        }
    }
    let normalized = out.trim_matches('-').to_string();
    if normalized.is_empty() {
        "app".to_string()
    } else {
        normalized
    }
}

fn normalize_target(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "docker" | "container" | "container-image" => "container_image".to_string(),
        "k8s" => "kubernetes".to_string(),
        "zip" | "serverless" => "serverless_zip".to_string(),
        "dir" => "directory".to_string(),
        other => other.to_string(),
    }
}

fn prepare_signing_key(signing: Option<&SigningConfig>) -> Result<()> {
    let Some(cfg) = signing else {
        return Ok(());
    };
    if !cfg.enabled.unwrap_or(false) {
        return Ok(());
    }

    let key_env = cfg
        .key_env
        .as_deref()
        .unwrap_or("SENDBUILD_SIGNING_KEY")
        .to_string();

    match env::var(&key_env) {
        Ok(value) => {
            validate_signing_key(&value, &key_env)?;
            return Ok(());
        }
        Err(env::VarError::NotUnicode(_)) => {
            bail!("signing key env `{key_env}` is not valid unicode")
        }
        Err(env::VarError::NotPresent) => {}
    }

    if !cfg.auto_generate_key.unwrap_or(true) {
        bail!(
            "missing required signing key env: {key_env}. set it or enable [signing].auto_generate_key = true"
        );
    }

    let key_file = cfg
        .key_file
        .as_deref()
        .unwrap_or(".sendbuild/signing.key")
        .to_string();
    let key_path = PathBuf::from(&key_file);
    let key = if key_path.exists() {
        let existing = fs::read_to_string(&key_path)
            .with_context(|| format!("failed to read signing key file: {}", key_path.display()))?;
        let trimmed = existing.trim().to_string();
        if trimmed.len() >= 32 {
            trimmed
        } else {
            generate_and_store_signing_key(&key_path)?
        }
    } else {
        generate_and_store_signing_key(&key_path)?
    };
    validate_signing_key(&key, &key_env)?;
    env::set_var(&key_env, key);
    Ok(())
}

fn validate_signing_key(value: &str, key_env: &str) -> Result<()> {
    if value.trim().len() < 32 {
        bail!("signing key env `{key_env}` must be at least 32 characters");
    }
    Ok(())
}

fn generate_and_store_signing_key(path: &Path) -> Result<String> {
    let mut key_bytes = [0u8; 32];
    getrandom(&mut key_bytes)
        .map_err(|e| anyhow::anyhow!("failed to gather secure randomness: {e}"))?;
    let key = hex::encode(key_bytes);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create signing key directory: {}",
                    parent.display()
                )
            })?;
        }
    }
    write_signing_key(path, &key)?;

    Ok(key)
}

fn write_signing_key(path: &Path, key: &str) -> Result<()> {
    let content = format!("{key}\n");

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("failed to write signing key file: {}", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write signing key file: {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush signing key file: {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).with_context(|| {
            format!(
                "failed to set secure permissions on signing key file: {}",
                path.display()
            )
        })?;
        return Ok(());
    }

    #[cfg(not(unix))]
    {
        fs::write(path, content)
            .with_context(|| format!("failed to write signing key file: {}", path.display()))?;
        Ok(())
    }
}

fn init_project(template: Option<&str>, _yes: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("my-app")
        .to_string();
    let repo_default = "https://github.com/you/repo".to_string();
    let artifact_dir_default = normalize_display_path(&default_artifact_dir());

    let framework = template
        .map(|v| v.to_string())
        .unwrap_or_else(|| detect_framework(&cwd).unwrap_or_else(|| "generic".to_string()));

    let config = match framework.as_str() {
        "nextjs" => format!(
            r#"[project]
name = "{project_name}"
# language = "nodejs" # optional override

[source]
repo = "{repo_default}"
branch = "main"

[build]
output_dir = ".next"

[deploy]
artifact_dir = "{artifact_dir_default}"
targets = ["static_site", "tarball", "kubernetes"]
container_image = "{project_name}:latest"

[deploy.kubernetes]
enabled = true
namespace = "default"
replicas = 2
container_port = 3000
service_port = 80

[deploy.gc]
enabled = true
keep_last = 5
max_age_days = 14
"#
        ),
        "rails" => format!(
            r#"[project]
name = "{project_name}"
# language = "ruby"

[source]
repo = "{repo_default}"
branch = "main"

[deploy]
artifact_dir = "{artifact_dir_default}"
targets = ["tarball", "container_image", "kubernetes"]
container_image = "{project_name}:latest"

[deploy.kubernetes]
enabled = true
namespace = "default"
replicas = 2
container_port = 3000
service_port = 80

[deploy.gc]
enabled = true
keep_last = 5
max_age_days = 14
"#
        ),
        "django" => format!(
            r#"[project]
name = "{project_name}"
# language = "python"

[source]
repo = "{repo_default}"
branch = "main"

[build]
output_dir = "staticfiles"

[deploy]
artifact_dir = "{artifact_dir_default}"
targets = ["static_site", "serverless_function", "kubernetes"]
container_image = "{project_name}:latest"

[deploy.kubernetes]
enabled = true
namespace = "default"
replicas = 2
container_port = 8000
service_port = 80

[deploy.gc]
enabled = true
keep_last = 5
max_age_days = 14
"#
        ),
        _ => format!(
            r#"[project]
name = "{project_name}"
# language = "nodejs" # optional override

[source]
repo = "{repo_default}"
branch = "main"

[deploy]
artifact_dir = "{artifact_dir_default}"
targets = ["directory", "kubernetes"]
container_image = "{project_name}:latest"

[deploy.kubernetes]
enabled = true
namespace = "default"
replicas = 1
container_port = 8080
service_port = 80

[deploy.gc]
enabled = true
keep_last = 5
max_age_days = 14
"#
        ),
    };

    fs::write("sendbuild.toml", config)?;
    println!("Initialized sendbuild.toml (template={framework})");
    Ok(())
}

fn run_cache(cmd: CacheCmd, config_path: &str) -> Result<()> {
    let cfg = BuildConfig::from_file(config_path)?;
    let cache_root = resolve_cache_root(&cfg);
    let project_cache = cache_root.join(project_storage_key(&cfg));
    let deps = project_cache.join("deps");
    let artifact = project_cache.join("artifact");

    match cmd {
        CacheCmd::Save => {
            fs::create_dir_all(&project_cache)?;
            println!("Cache save placeholder at {}", project_cache.display());
        }
        CacheCmd::Restore => {
            if project_cache.exists() {
                println!("Cache restore available at {}", project_cache.display());
            } else {
                println!("No cache found at {}", project_cache.display());
            }
        }
        CacheCmd::Clear => {
            if project_cache.exists() {
                fs::remove_dir_all(&project_cache)?;
            }
            println!("Cache cleared: {}", project_cache.display());
        }
        CacheCmd::Status => {
            println!("Cache root: {}", project_cache.display());
            println!(
                "deps: {}",
                if deps.exists() { "present" } else { "missing" }
            );
            println!(
                "artifact: {}",
                if artifact.exists() {
                    "present"
                } else {
                    "missing"
                }
            );
            println!(
                "state: {}",
                if project_cache.join("state.txt").exists() {
                    "present"
                } else {
                    "missing"
                }
            );
        }
    }

    Ok(())
}

fn clean(config_path: &str, all: bool, cache_only: bool) -> Result<()> {
    let cfg = BuildConfig::from_file(config_path)?;
    let artifact_dir = effective_artifact_dir(&cfg);
    let cache_root = resolve_cache_root(&cfg);
    let project_cache = cache_root.join(project_storage_key(&cfg));

    if cache_only {
        if project_cache.exists() {
            fs::remove_dir_all(&project_cache)?;
        }
        println!("Cleaned cache only: {}", project_cache.display());
        return Ok(());
    }

    if artifact_dir.exists() {
        fs::remove_dir_all(&artifact_dir)?;
    }
    println!("Cleaned artifacts: {}", artifact_dir.display());

    if all {
        if project_cache.exists() {
            fs::remove_dir_all(&project_cache)?;
        }
        let temp = env::temp_dir().join("sendbuild");
        if temp.exists() {
            fs::remove_dir_all(&temp)?;
        }
        println!("Cleaned cache: {}", project_cache.display());
        println!("Cleaned temp workdirs: {}", temp.display());
    }

    Ok(())
}

fn info(config_path: &str, show_env: bool, show_deps: bool) -> Result<()> {
    let cfg = BuildConfig::from_file(config_path).ok();
    println!("sendbuilds version {}", env!("CARGO_PKG_VERSION"));
    println!(
        "executable: {}",
        normalize_display_path(&env::current_exe().unwrap_or_else(|_| PathBuf::from("unknown")))
    );
    println!(
        "build_identity: {}@{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    if let Some(c) = &cfg {
        println!("project: {}", c.project.name);
        match &c.source {
            Some(s) => println!("repo: {}", s.repo),
            None => println!("source: local workspace"),
        }
        println!(
            "artifact_dir: {}",
            normalize_display_path(&effective_artifact_dir(c))
        );
        println!("storage_key: {}", project_storage_key(c));
    }

    if show_env {
        println!("os: {}", env::consts::OS);
        println!("arch: {}", env::consts::ARCH);
        println!("cwd: {}", env::current_dir()?.display());
    }

    if show_deps {
        for (name, args) in [
            ("node", vec!["--version"]),
            ("python", vec!["--version"]),
            ("ruby", vec!["--version"]),
            ("go", vec!["version"]),
            ("java", vec!["-version"]),
            ("php", vec!["--version"]),
            ("cargo", vec!["--version"]),
            ("dotnet", vec!["--version"]),
            ("deno", vec!["--version"]),
            ("gleam", vec!["--version"]),
            ("elixir", vec!["--version"]),
        ] {
            let out = Command::new(name).args(&args).output();
            match out {
                Ok(o) if o.status.success() => {
                    let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
                    let val = if !stdout.is_empty() { stdout } else { stderr };
                    println!("{name}: {}", val.lines().next().unwrap_or("ok"));
                }
                _ => println!("{name}: not found"),
            }
        }
    }

    Ok(())
}

fn run_rebase(
    config_path: &str,
    git: bool,
    repo: Option<String>,
    branch: Option<String>,
    context: &str,
    dockerfile: Option<String>,
    image: Option<String>,
    from_image: Option<String>,
    base: Option<String>,
    platforms: Vec<String>,
    push: bool,
) -> Result<()> {
    let cfg = BuildConfig::from_file(config_path).ok();

    if git || repo.is_some() {
        let repo_ref = repo
            .or_else(|| cfg.as_ref().and_then(|c| c.source.as_ref().map(|s| s.repo.clone())))
            .or_else(local_git_remote_url)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "rebase --git requires a repository source. set --repo, [source].repo, or configure git remote.origin.url"
                )
            })?;
        let branch_ref = branch.or_else(|| {
            cfg.as_ref()
                .and_then(|c| c.source.as_ref().and_then(|s| s.branch.clone()))
        });
        let target_image = image
            .or_else(|| cfg.as_ref().and_then(|c| c.deploy.container_image.clone()))
            .unwrap_or_else(|| format!("{}:latest", project_name_from_repo(&repo_ref)));
        let runtime_base = base.or_else(|| cfg.as_ref().and_then(|c| c.deploy.rebase_base.clone()));

        ensure_local_image_cache(&target_image);

        println!(
            "Rebase git mode: rebuilding full image from `{}` as `{}`.",
            repo_ref, target_image
        );
        return run_quick_build_with_options(
            Some(repo_ref),
            branch_ref,
            true,
            Some(target_image),
            false,
            None,
            runtime_base,
            Some(false),
            None,
            false,
        );
    }

    let context_path = PathBuf::from(context);
    if !context_path.exists() {
        bail!("rebase context does not exist: {}", context_path.display());
    }

    let target_image = image
        .or_else(|| cfg.as_ref().and_then(|c| c.deploy.container_image.clone()))
        .unwrap_or_else(|| format!("{}:rebased", local_project_name()));
    let cache_from = from_image.or_else(|| Some(target_image.clone()));
    let runtime_base = base
        .or_else(|| cfg.as_ref().and_then(|c| c.deploy.rebase_base.clone()))
        .or_else(|| read_runtime_base_from_plan(&context_path))
        .filter(|v| !v.trim().is_empty() && v != "auto")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing runtime base. set --base <image>, [deploy].rebase_base, or .sendbuild-rebase-plan.json"
            )
        })?;

    let dockerfile_path = resolve_rebase_dockerfile(&context_path, dockerfile.as_deref())?;

    let docker_available = Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_available {
        bail!("docker not available");
    }

    let normalized_platforms = platforms
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();

    let buildx_available = Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let status = if buildx_available {
        if normalized_platforms.len() > 1 && !push {
            bail!("multi-arch rebase requires --push (buildx cannot --load multiple platforms)");
        }
        let mut cmd = Command::new("docker");
        cmd.arg("buildx")
            .arg("build")
            .arg("--target")
            .arg("launch")
            .arg("--build-arg")
            .arg(format!("RUNTIME_BASE={runtime_base}"))
            .arg("-t")
            .arg(&target_image)
            .arg("--file")
            .arg(&dockerfile_path);
        if let Some(cache) = cache_from.as_deref() {
            cmd.arg("--cache-from").arg(cache);
        }
        if !normalized_platforms.is_empty() {
            cmd.arg("--platform").arg(normalized_platforms.join(","));
        }
        if push {
            cmd.arg("--push");
        } else {
            cmd.arg("--load");
        }
        cmd.arg(&context_path).status()?
    } else {
        if !normalized_platforms.is_empty() || push {
            bail!("docker buildx is required for --platform/--push rebase options");
        }
        let mut cmd = Command::new("docker");
        cmd.arg("build")
            .arg("--target")
            .arg("launch")
            .arg("--build-arg")
            .arg(format!("RUNTIME_BASE={runtime_base}"))
            .arg("-t")
            .arg(&target_image)
            .arg("--file")
            .arg(&dockerfile_path);
        if let Some(cache) = cache_from.as_deref() {
            cmd.arg("--cache-from").arg(cache);
        }
        cmd.arg(&context_path).status()?
    };

    if !status.success() {
        bail!("docker rebase build failed");
    }

    println!(
        "Rebased image `{}` using runtime base `{}` (cache-from: {}).",
        target_image,
        runtime_base,
        cache_from.as_deref().unwrap_or("none")
    );
    Ok(())
}

fn resolve_rebase_dockerfile(context: &Path, provided: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = provided {
        let path = PathBuf::from(path);
        if !path.exists() {
            bail!("dockerfile not found: {}", path.display());
        }
        return Ok(path);
    }

    let layered = context.join("Dockerfile.sendbuild.layered");
    if layered.exists() {
        return Ok(layered);
    }

    let dockerfile = context.join("Dockerfile");
    if dockerfile.exists() {
        let data = fs::read_to_string(&dockerfile).unwrap_or_default();
        if data
            .to_lowercase()
            .contains("# sendbuilds: layered rebase-ready dockerfile")
        {
            return Ok(dockerfile);
        }
    }

    bail!(
        "could not find a sendbuilds layered dockerfile in `{}`. run `sendbuilds build --docker` first or pass --dockerfile",
        context.display()
    )
}

fn read_runtime_base_from_plan(context: &Path) -> Option<String> {
    let plan_path = context.join(".sendbuild-rebase-plan.json");
    let plan = fs::read_to_string(plan_path).ok()?;
    let json: Value = serde_json::from_str(&plan).ok()?;
    json.get("runtime_base")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn local_git_remote_url() -> Option<String> {
    let out = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

fn ensure_local_image_cache(image: &str) {
    let local_exists = Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if local_exists {
        return;
    }
    let _ = Command::new("docker").args(["pull", image]).status();
}

fn detect_framework(cwd: &Path) -> Option<String> {
    if cwd.join("next.config.js").exists()
        || cwd.join("next.config.mjs").exists()
        || cwd.join("next.config.ts").exists()
    {
        return Some("nextjs".to_string());
    }
    if cwd.join("Gemfile").exists() && file_contains(&cwd.join("Gemfile"), "rails") {
        return Some("rails".to_string());
    }
    if cwd.join("manage.py").exists() {
        return Some("django".to_string());
    }
    None
}

fn resolve_cache_root(cfg: &BuildConfig) -> PathBuf {
    cfg.cache
        .as_ref()
        .and_then(|c| c.dir.as_ref())
        .map(PathBuf::from)
        .unwrap_or_else(default_cache_dir)
}

fn file_contains(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path)
        .map(|v| v.to_lowercase().contains(&needle.to_lowercase()))
        .unwrap_or(false)
}

fn normalize_display_path(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}
