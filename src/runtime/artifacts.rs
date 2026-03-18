use crate::core::config::{GarbageCollectionConfig, KubernetesConfig};
use anyhow::{Context, Result};
use chrono::Local;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};
use tar::Builder;
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

#[derive(Debug, Clone)]
pub struct PublishResult {
    pub root: PathBuf,
    pub outputs: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GarbageCollectResult {
    pub removed_dirs: usize,
    pub kept_dirs: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct ContainerBuildResult {
    dockerfile_generated: bool,
    dockerignore_generated: bool,
    layered_dockerfile_generated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ContainerPublishOptions {
    pub platforms: Vec<String>,
    pub push: bool,
    pub registry_cache_ref: Option<String>,
    pub rebase_base: Option<String>,
}

pub fn make_workdir(project: &str) -> Result<PathBuf> {
    let stamp = Local::now().format("%Y%m%d_%H%M%S%3f");
    let dir = std::env::temp_dir()
        .join("sendbuild")
        .join(format!("{project}_{stamp}"));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn copy_workspace(src: &Path, dst: &Path) -> Result<()> {
    copy_workspace_recursive(src, dst)
}

pub fn publish(
    src: &Path,
    container_src: &Path,
    base_dir: &Path,
    project_name: &str,
    targets: &[String],
    container_image: Option<&str>,
    container_options: Option<&ContainerPublishOptions>,
    kubernetes: Option<&KubernetesConfig>,
) -> Result<PublishResult> {
    fs::create_dir_all(base_dir)?;
    let root = create_unique_build_root(base_dir)?;

    let selected = if targets.is_empty() {
        vec!["directory".to_string()]
    } else {
        targets.to_vec()
    };

    let mut outputs = Vec::new();
    let mut warnings = Vec::new();

    for target in selected {
        match target.as_str() {
            "directory" | "static_site" | "static" => {
                let out = root.join("directory");
                fs::create_dir_all(&out)?;
                copy_recursive(src, &out)?;
                outputs.push(out);
            }
            "tarball" => {
                let out = root.join("artifact.tar.gz");
                create_tarball(src, &out)?;
                outputs.push(out);
            }
            "serverless" | "serverless_zip" | "serverless_function" | "zip" => {
                let out = root.join("serverless.zip");
                create_zip(src, &out)?;
                outputs.push(out);
            }
            "container" | "container_image" => {
                let image = container_image.unwrap_or("sendbuild:latest");
                match build_container_image(container_src, image, container_options) {
                    Ok(build_result) => {
                        let out = root.join(format!("container-image-{image}.txt"));
                        let note = format!(
                            "image={image}\ndockerfile_generated={}\nlayered_dockerfile_generated={}\ndockerignore_generated={}\ncontext={}\n",
                            build_result.dockerfile_generated,
                            build_result.layered_dockerfile_generated,
                            build_result.dockerignore_generated,
                            container_src.display()
                        );
                        fs::write(&out, note)?;
                        outputs.push(out);
                    }
                    Err(err) => warnings.push(format!("container image build skipped: {err}")),
                }
            }
            "kubernetes" | "k8s" | "kubernetes_deployment" => {
                let image = container_image.unwrap_or("sendbuild:latest");
                let out = create_kubernetes_manifests(&root, project_name, image, kubernetes)?;
                outputs.push(out);
            }
            other => warnings.push(format!("unknown output target: {other}")),
        }
    }

    Ok(PublishResult {
        root,
        outputs,
        warnings,
    })
}

fn create_unique_build_root(base_dir: &Path) -> Result<PathBuf> {
    for attempt in 0..1000u32 {
        let stamp = Local::now().format("%Y%m%d_%H%M%S%3f").to_string();
        let id = if attempt == 0 {
            stamp
        } else {
            format!("{stamp}-{attempt}")
        };
        let root = base_dir.join(id);
        match fs::create_dir(&root) {
            Ok(()) => return Ok(root),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).with_context(|| format!("creating {}", root.display())),
        }
    }
    anyhow::bail!("unable to create unique build id after 1000 attempts")
}

pub fn garbage_collect_artifacts(
    base_dir: &Path,
    current_root: &Path,
    settings: Option<&GarbageCollectionConfig>,
) -> Result<GarbageCollectResult> {
    let enabled = settings.and_then(|g| g.enabled).unwrap_or(false);
    if !enabled || !base_dir.exists() {
        return Ok(GarbageCollectResult::default());
    }

    let keep_last = settings.and_then(|g| g.keep_last).unwrap_or(5);
    let max_age_days = settings.and_then(|g| g.max_age_days);
    let now = SystemTime::now();

    let mut dirs = Vec::new();
    for entry in fs::read_dir(base_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path();
            if path != current_root {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                dirs.push((path, modified));
            }
        }
    }

    dirs.sort_by(|a, b| b.1.cmp(&a.1));

    let mut removed_dirs = 0usize;
    let mut kept_dirs = 0usize;

    for (idx, (path, modified)) in dirs.iter().enumerate() {
        let stale = max_age_days
            .and_then(|days| {
                now.duration_since(*modified)
                    .ok()
                    .map(|d| d > Duration::from_secs(days * 86_400))
            })
            .unwrap_or(false);
        let over_limit = idx >= keep_last;
        if stale || over_limit {
            fs::remove_dir_all(path).with_context(|| {
                format!("failed to remove old artifact directory {}", path.display())
            })?;
            removed_dirs += 1;
        } else {
            kept_dirs += 1;
        }
    }

    Ok(GarbageCollectResult {
        removed_dirs,
        kept_dirs,
    })
}

fn create_tarball(src: &Path, out: &Path) -> Result<()> {
    let file = fs::File::create(out)?;
    let gz = GzEncoder::new(file, Compression::default());
    let mut tar = Builder::new(gz);
    tar.append_dir_all(".", src)?;
    tar.finish()?;
    Ok(())
}

fn create_zip(src: &Path, out: &Path) -> Result<()> {
    let file = fs::File::create(out)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip_dir(src, src, &mut zip, options)?;
    zip.finish()?;
    Ok(())
}

fn zip_dir(
    base: &Path,
    current: &Path,
    zip: &mut ZipWriter<fs::File>,
    options: SimpleFileOptions,
) -> Result<()> {
    for entry in
        fs::read_dir(current).with_context(|| format!("cant read {}", current.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let ty = entry.file_type()?;
        if ty.is_dir() {
            let dir_name = if rel.ends_with('/') {
                rel.clone()
            } else {
                format!("{rel}/")
            };
            zip.add_directory(dir_name, options)?;
            zip_dir(base, &path, zip, options)?;
        } else if ty.is_file() {
            zip.start_file(rel, options)?;
            let data = fs::read(path)?;
            zip.write_all(&data)?;
        }
    }
    Ok(())
}

fn build_container_image(
    src: &Path,
    image: &str,
    options: Option<&ContainerPublishOptions>,
) -> Result<ContainerBuildResult> {
    let docker_available = Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !docker_available {
        anyhow::bail!("docker not available");
    }

    let dockerfile_path = src.join("Dockerfile");
    let layered_dockerfile_path = src.join("Dockerfile.sendbuild.layered");
    let mut generated_dockerfile = false;
    let mut generated_layered_dockerfile = false;
    let generated_dockerignore = ensure_dockerignore(src)?;
    if !dockerfile_path.exists() {
        let dockerfile = build_generated_dockerfile(src)?;
        fs::write(&dockerfile_path, dockerfile)?;
        generated_dockerfile = true;
    } else {
        let existing = fs::read_to_string(&dockerfile_path).unwrap_or_default();
        if should_regenerate_generated_dockerfile(&existing) {
            let dockerfile = build_generated_dockerfile(src)?;
            fs::write(&dockerfile_path, dockerfile)?;
            generated_dockerfile = true;
        }
    }

    let opts = options.cloned().unwrap_or_default();
    let use_layered = generated_dockerfile
        || should_regenerate_generated_dockerfile(
            &fs::read_to_string(&dockerfile_path).unwrap_or_default(),
        );
    let dockerfile_for_build = if use_layered {
        let layered = build_layered_dockerfile(src, opts.rebase_base.as_deref())?;
        fs::write(&layered_dockerfile_path, layered)?;
        generated_layered_dockerfile = true;
        layered_dockerfile_path.clone()
    } else {
        dockerfile_path.clone()
    };

    let buildx_available = Command::new("docker")
        .args(["buildx", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let platforms = opts
        .platforms
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    let use_buildx = buildx_available
        && (!platforms.is_empty() || opts.registry_cache_ref.is_some() || opts.push);
    if !platforms.is_empty() && !opts.push {
        anyhow::bail!("multi-arch container build requires [deploy].push_container = true");
    }

    let status = if use_buildx {
        let mut cmd = Command::new("docker");
        cmd.arg("buildx")
            .arg("build")
            .arg("-t")
            .arg(image)
            .arg("--file")
            .arg(&dockerfile_for_build)
            .arg("--provenance=mode=max")
            .arg("--sbom=true");
        if !platforms.is_empty() {
            cmd.arg("--platform").arg(platforms.join(","));
        }
        if let Some(cache_ref) = opts.registry_cache_ref.as_deref() {
            cmd.arg("--cache-from")
                .arg(format!("type=registry,ref={cache_ref}"));
            cmd.arg("--cache-to")
                .arg(format!("type=registry,ref={cache_ref},mode=max"));
        }
        if opts.push {
            cmd.arg("--push");
        } else {
            cmd.arg("--load");
        }
        cmd.arg(".");
        cmd.current_dir(src).status()?
    } else {
        if !platforms.is_empty() || opts.registry_cache_ref.is_some() || opts.push {
            anyhow::bail!(
                "docker buildx is required for multi-arch/cache/push container options; install buildx"
            );
        }
        Command::new("docker")
            .arg("build")
            .arg("-t")
            .arg(image)
            .arg("--file")
            .arg(&dockerfile_for_build)
            .arg(".")
            .current_dir(src)
            .status()?
    };
    if !status.success() {
        anyhow::bail!("docker build failed");
    }

    if use_layered {
        write_rebase_plan(src, image, opts.rebase_base.as_deref(), &platforms)?;
    }

    Ok(ContainerBuildResult {
        dockerfile_generated: generated_dockerfile,
        dockerignore_generated: generated_dockerignore,
        layered_dockerfile_generated: generated_layered_dockerfile,
    })
}

fn build_layered_dockerfile(src: &Path, rebase_base: Option<&str>) -> Result<String> {
    if src.join("package.json").exists() {
        return build_layered_node_dockerfile(src, rebase_base);
    }
    let runtime_base = rebase_base.unwrap_or(infer_runtime_base(src));
    let (cmd, port) = infer_container_start(src)?;
    let cmd_json = cmd
        .iter()
        .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![
        "# sendbuilds: layered rebase-ready dockerfile".to_string(),
        format!("ARG RUNTIME_BASE={runtime_base}"),
        "FROM ${RUNTIME_BASE} AS launch".to_string(),
        "WORKDIR /app".to_string(),
        "COPY --chown=65532:65532 . /app".to_string(),
    ];
    if let Some(p) = port {
        lines.push(format!("EXPOSE {p}"));
    }
    lines.push("USER 65532:65532".to_string());
    lines.push(format!("CMD [{cmd_json}]"));
    Ok(lines.join("\n") + "\n")
}

fn build_layered_node_dockerfile(src: &Path, rebase_base: Option<&str>) -> Result<String> {
    let runtime_base = rebase_base.unwrap_or("node:20-alpine");
    let pm = detect_node_package_manager(src);
    let has_build = has_package_script(src, "build");
    let start_cmd = infer_node_start_command(src)?;
    let start_cmd_json = start_cmd
        .iter()
        .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![
        "# sendbuilds: layered rebase-ready dockerfile".to_string(),
        format!("ARG RUNTIME_BASE={runtime_base}"),
        "FROM ${RUNTIME_BASE} AS deps".to_string(),
        "WORKDIR /app".to_string(),
        "COPY package.json ./".to_string(),
    ];
    if src.join("package-lock.json").exists() {
        lines.push("COPY package-lock.json ./".to_string());
    }
    if src.join("yarn.lock").exists() {
        lines.push("COPY yarn.lock ./".to_string());
    }
    if src.join("pnpm-lock.yaml").exists() {
        lines.push("COPY pnpm-lock.yaml ./".to_string());
    }
    lines.push("RUN corepack enable".to_string());
    lines.push(format!("RUN {}", install_with_fallback_command(pm, src)));
    lines.push("FROM deps AS build".to_string());
    lines.push("COPY . /app".to_string());
    if has_build {
        lines.push(format!("RUN {}", build_with_fallback_command(pm)));
    }
    lines.push("FROM ${RUNTIME_BASE} AS launch".to_string());
    lines.push("WORKDIR /app".to_string());
    lines.push("COPY --from=build --chown=node:node /app /app".to_string());
    lines.push("EXPOSE 3000".to_string());
    lines.push("USER node".to_string());
    lines.push(format!("CMD [{start_cmd_json}]"));
    Ok(lines.join("\n") + "\n")
}

fn write_rebase_plan(
    src: &Path,
    image: &str,
    rebase_base: Option<&str>,
    platforms: &[String],
) -> Result<()> {
    let plan = serde_json::json!({
        "schema_version": "1",
        "image": image,
        "runtime_base": rebase_base.unwrap_or("auto"),
        "platforms": platforms,
        "strategy": "layered-buildx-rebase-ready",
        "note": "rebuild launch stage with new runtime_base while reusing cache layers",
    });
    fs::write(
        src.join(".sendbuild-rebase-plan.json"),
        serde_json::to_vec_pretty(&plan)?,
    )?;
    Ok(())
}

fn infer_runtime_base(src: &Path) -> &'static str {
    if src.join("deno.json").exists() || src.join("deno.jsonc").exists() {
        return "denoland/deno:alpine";
    }
    if src.join("mix.exs").exists() {
        return "hexpm/elixir:1.17.3-erlang-27-alpine";
    }
    if src.join("gleam.toml").exists() {
        return "ghcr.io/gleam-lang/gleam:latest";
    }
    if src.join("package.json").exists() {
        return "gcr.io/distroless/nodejs20-debian12";
    }
    if src.join("requirements.txt").exists() || src.join("pyproject.toml").exists() {
        return "gcr.io/distroless/python3-debian12";
    }
    if src.join("go.mod").exists()
        || src.join("Cargo.toml").exists()
        || src.join("CMakeLists.txt").exists()
    {
        return "gcr.io/distroless/static-debian12";
    }
    if src.join("pom.xml").exists()
        || src.join("build.gradle").exists()
        || src.join("build.gradle.kts").exists()
    {
        return "gcr.io/distroless/java21-debian12";
    }
    if src.join("global.json").exists() || has_glob_ext(src, "csproj") {
        return "mcr.microsoft.com/dotnet/aspnet:8.0";
    }
    if src.join("composer.json").exists() || src.join("artisan").exists() {
        return "php:8.3-cli-alpine";
    }
    if src.join("Gemfile").exists() {
        return "ruby:3.3-alpine";
    }
    if src.join("index.html").exists() {
        return "python:3.12-alpine";
    }
    if has_glob_ext(src, "sh") {
        return "alpine:3.20";
    }
    "gcr.io/distroless/static-debian12"
}

fn build_generated_dockerfile(src: &Path) -> Result<String> {
    if src.join("package.json").exists() {
        return build_generated_node_dockerfile(src);
    }

    let base = infer_runtime_base(src);
    let (cmd, port) = infer_container_start(src)?;
    let cmd_json = cmd
        .iter()
        .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![
        "# sendbuilds: auto-generated dockerfile".to_string(),
        format!("FROM {base}"),
        "WORKDIR /app".to_string(),
        "COPY --chown=65532:65532 . /app".to_string(),
    ];
    if let Some(p) = port {
        lines.push(format!("EXPOSE {p}"));
    }
    lines.push("USER 65532:65532".to_string());
    lines.push(format!("CMD [{cmd_json}]"));
    Ok(lines.join("\n") + "\n")
}

fn build_generated_node_dockerfile(src: &Path) -> Result<String> {
    let package_manager = detect_node_package_manager(src);
    let has_build = has_package_script(src, "build");
    let start_cmd = infer_node_start_command(src)?;
    let start_cmd_json = start_cmd
        .iter()
        .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec![
        "# sendbuilds: auto-generated dockerfile".to_string(),
        "FROM node:20-alpine".to_string(),
        "WORKDIR /app".to_string(),
        "COPY --chown=node:node . /app".to_string(),
        "RUN corepack enable".to_string(),
        format!(
            "RUN {}",
            install_with_fallback_command(package_manager, src)
        ),
    ];
    if has_build {
        lines.push(format!(
            "RUN {}",
            build_with_fallback_command(package_manager)
        ));
    }
    lines.push("EXPOSE 3000".to_string());
    lines.push("USER node".to_string());
    lines.push(format!("CMD [{start_cmd_json}]"));
    Ok(lines.join("\n") + "\n")
}

fn ensure_dockerignore(src: &Path) -> Result<bool> {
    let dockerignore = src.join(".dockerignore");
    if dockerignore.exists() {
        return Ok(false);
    }
    let rules = [
        ".git",
        ".gitignore",
        ".env",
        ".env.*",
        "*.pem",
        "*.key",
        "*.p12",
        "*.kubeconfig",
        "node_modules",
        "target",
        "artifacts",
        ".sendbuild-cache",
        ".venv",
        "venv",
        "__pycache__",
    ];
    fs::write(dockerignore, rules.join("\n") + "\n")?;
    Ok(true)
}

fn infer_node_start_command(src: &Path) -> Result<Vec<String>> {
    let package_manager = detect_node_package_manager(src);
    if let Some((cmd, _port)) = infer_node_start_from_package_json(src) {
        return Ok(cmd);
    }
    if has_package_script(src, "start") {
        return Ok(match package_manager {
            "pnpm" => vec!["pnpm".to_string(), "run".to_string(), "start".to_string()],
            "yarn" => vec!["yarn".to_string(), "start".to_string()],
            _ => vec!["npm".to_string(), "run".to_string(), "start".to_string()],
        });
    }
    if has_next_dependency(src)
        || src.join("next.config.js").exists()
        || src.join("next.config.mjs").exists()
        || src.join("next.config.ts").exists()
        || src.join(".next").exists()
    {
        return Ok(match package_manager {
            "pnpm" => vec![
                "pnpm".to_string(),
                "exec".to_string(),
                "next".to_string(),
                "start".to_string(),
                "-p".to_string(),
                "3000".to_string(),
            ],
            "yarn" => vec![
                "yarn".to_string(),
                "next".to_string(),
                "start".to_string(),
                "-p".to_string(),
                "3000".to_string(),
            ],
            _ => vec![
                "npx".to_string(),
                "next".to_string(),
                "start".to_string(),
                "-p".to_string(),
                "3000".to_string(),
            ],
        });
    }
    if has_package_script(src, "serve") {
        return Ok(match package_manager {
            "pnpm" => vec!["pnpm".to_string(), "run".to_string(), "serve".to_string()],
            "yarn" => vec!["yarn".to_string(), "serve".to_string()],
            _ => vec!["npm".to_string(), "run".to_string(), "serve".to_string()],
        });
    }
    if has_package_script(src, "dev") {
        return Ok(match package_manager {
            "pnpm" => vec![
                "pnpm".to_string(),
                "run".to_string(),
                "dev".to_string(),
                "--".to_string(),
                "--host".to_string(),
                "0.0.0.0".to_string(),
                "--port".to_string(),
                "3000".to_string(),
            ],
            "yarn" => vec![
                "yarn".to_string(),
                "dev".to_string(),
                "--host".to_string(),
                "0.0.0.0".to_string(),
                "--port".to_string(),
                "3000".to_string(),
            ],
            _ => vec![
                "npm".to_string(),
                "run".to_string(),
                "dev".to_string(),
                "--".to_string(),
                "--host".to_string(),
                "0.0.0.0".to_string(),
                "--port".to_string(),
                "3000".to_string(),
            ],
        });
    }
    if src
        .join(".next")
        .join("standalone")
        .join("server.js")
        .exists()
    {
        return Ok(vec![
            "node".to_string(),
            ".next/standalone/server.js".to_string(),
        ]);
    }
    for candidate in ["server.js", "dist/server.js", "build/server.js", "index.js"] {
        if src.join(candidate).exists() {
            return Ok(vec!["node".to_string(), candidate.to_string()]);
        }
    }
    anyhow::bail!(
        "cannot infer Node start command. add scripts.start to package.json or provide Dockerfile"
    );
}

fn infer_container_start(src: &Path) -> Result<(Vec<String>, Option<u16>)> {
    if src.join("deno.json").exists() || src.join("deno.jsonc").exists() {
        for candidate in ["main.ts", "mod.ts", "server.ts", "index.ts"] {
            if src.join(candidate).exists() {
                return Ok((
                    vec![
                        "deno".to_string(),
                        "run".to_string(),
                        "-A".to_string(),
                        candidate.to_string(),
                    ],
                    Some(8000),
                ));
            }
        }
        anyhow::bail!(
            "cannot infer Deno start command. add a Dockerfile or include main.ts/mod.ts/server.ts/index.ts"
        );
    }
    if src.join("mix.exs").exists() {
        if src.join("config").join("runtime.exs").exists()
            || file_contains(&src.join("mix.exs"), "phoenix")
        {
            return Ok((
                vec![
                    "mix".to_string(),
                    "phx.server".to_string(),
                    "--no-halt".to_string(),
                ],
                Some(4000),
            ));
        }
        return Ok((
            vec![
                "mix".to_string(),
                "run".to_string(),
                "--no-halt".to_string(),
            ],
            Some(4000),
        ));
    }
    if src.join("gleam.toml").exists() {
        return Ok((vec!["gleam".to_string(), "run".to_string()], Some(8000)));
    }
    if src.join("package.json").exists() {
        if src
            .join(".next")
            .join("standalone")
            .join("server.js")
            .exists()
        {
            return Ok((vec![".next/standalone/server.js".to_string()], Some(3000)));
        }
        if src.join(".next").exists() {
            return Ok((
                vec!["npm".to_string(), "run".to_string(), "start".to_string()],
                Some(3000),
            ));
        }
        if let Some((cmd, port)) = infer_node_start_from_package_json(src) {
            return Ok((cmd, port));
        }
        for candidate in [
            "server.js",
            "dist/server.js",
            "build/server.js",
            "index.js",
            ".output/server/index.mjs",
        ] {
            if src.join(candidate).exists() {
                return Ok((vec![candidate.to_string()], Some(3000)));
            }
        }
        anyhow::bail!(
            "cannot infer Node.js start command. add a Dockerfile or ensure server entry file exists (e.g. server.js, dist/server.js, .next/standalone/server.js)"
        );
    }
    if src.join("requirements.txt").exists()
        || src.join("pyproject.toml").exists()
        || src.join("app.py").exists()
    {
        if src.join("manage.py").exists() {
            return Ok((
                vec![
                    "python".to_string(),
                    "manage.py".to_string(),
                    "runserver".to_string(),
                    "0.0.0.0:8000".to_string(),
                ],
                Some(8000),
            ));
        }
        if src.join("wsgi.py").exists() {
            return Ok((
                vec![
                    "python".to_string(),
                    "-m".to_string(),
                    "gunicorn".to_string(),
                    "wsgi:app".to_string(),
                    "--bind".to_string(),
                    "0.0.0.0:8000".to_string(),
                ],
                Some(8000),
            ));
        }
        if file_contains(&src.join("requirements.txt"), "flask")
            || file_contains(&src.join("pyproject.toml"), "flask")
        {
            if src.join("app.py").exists() {
                return Ok((vec!["python".to_string(), "app.py".to_string()], Some(8000)));
            }
            if src.join("main.py").exists() {
                return Ok((
                    vec!["python".to_string(), "main.py".to_string()],
                    Some(8000),
                ));
            }
        }
        if src.join("app.py").exists() {
            return Ok((vec!["python".to_string(), "app.py".to_string()], Some(8000)));
        }
        if src.join("main.py").exists() {
            return Ok((
                vec!["python".to_string(), "main.py".to_string()],
                Some(8000),
            ));
        }
        anyhow::bail!(
            "cannot infer Python start command. add a Dockerfile or include app.py/main.py at repository root"
        );
    }
    if src.join("pom.xml").exists()
        || src.join("build.gradle").exists()
        || src.join("build.gradle.kts").exists()
    {
        if let Some(jar) = find_first_jar(src) {
            return Ok((
                vec!["java".to_string(), "-jar".to_string(), jar],
                Some(8080),
            ));
        }
        anyhow::bail!(
            "cannot infer Java start command. add a Dockerfile or ensure built .jar exists (target/ or build/libs/)"
        );
    }
    if src.join("go.mod").exists() {
        if let Some(bin) = find_first_executable(src, &["bin", ".", "dist"]) {
            return Ok((vec![format!("./{bin}")], Some(8080)));
        }
        anyhow::bail!(
            "cannot infer Go start command. add a Dockerfile or ensure a built binary exists in ./bin, ./dist, or repo root"
        );
    }
    if src.join("Cargo.toml").exists() {
        if let Some(bin) = find_first_executable(src, &["target/release", "bin", "."]) {
            return Ok((vec![format!("./{bin}")], Some(8080)));
        }
        anyhow::bail!(
            "cannot infer Rust start command. add a Dockerfile or ensure a release binary exists in target/release/"
        );
    }
    if src.join("global.json").exists() || has_glob_ext(src, "csproj") {
        if let Some(dll) = find_first_by_ext(src, &["bin/Release", "."], "dll") {
            return Ok((vec!["dotnet".to_string(), dll], Some(8080)));
        }
        anyhow::bail!(
            "cannot infer .NET start command. add a Dockerfile or ensure a publish/build .dll exists under bin/Release/"
        );
    }
    if src.join("composer.json").exists() || src.join("artisan").exists() {
        if src.join("artisan").exists() {
            return Ok((
                vec![
                    "php".to_string(),
                    "artisan".to_string(),
                    "serve".to_string(),
                    "--host=0.0.0.0".to_string(),
                    "--port=8080".to_string(),
                ],
                Some(8080),
            ));
        }
        if src.join("public").join("index.php").exists() {
            return Ok((
                vec![
                    "php".to_string(),
                    "-S".to_string(),
                    "0.0.0.0:8080".to_string(),
                    "-t".to_string(),
                    "public".to_string(),
                ],
                Some(8080),
            ));
        }
        if src.join("index.php").exists() {
            return Ok((
                vec![
                    "php".to_string(),
                    "-S".to_string(),
                    "0.0.0.0:8080".to_string(),
                    "index.php".to_string(),
                ],
                Some(8080),
            ));
        }
        anyhow::bail!(
            "cannot infer PHP start command. add a Dockerfile or ensure artisan/public/index.php exists"
        );
    }
    if src.join("Gemfile").exists() {
        if file_contains(&src.join("Gemfile"), "rails") && src.join("bin").join("rails").exists() {
            return Ok((
                vec![
                    "bundle".to_string(),
                    "exec".to_string(),
                    "rails".to_string(),
                    "server".to_string(),
                    "-b".to_string(),
                    "0.0.0.0".to_string(),
                    "-p".to_string(),
                    "3000".to_string(),
                ],
                Some(3000),
            ));
        }
        if src.join("config.ru").exists() {
            return Ok((
                vec![
                    "bundle".to_string(),
                    "exec".to_string(),
                    "rackup".to_string(),
                    "-o".to_string(),
                    "0.0.0.0".to_string(),
                    "-p".to_string(),
                    "9292".to_string(),
                ],
                Some(9292),
            ));
        }
        if src.join("app.rb").exists() {
            return Ok((vec!["ruby".to_string(), "app.rb".to_string()], Some(9292)));
        }
        anyhow::bail!(
            "cannot infer Ruby start command. add a Dockerfile or ensure config.ru/app.rb exists"
        );
    }
    if src.join("index.html").exists() {
        return Ok((
            vec![
                "python".to_string(),
                "-m".to_string(),
                "http.server".to_string(),
                "8080".to_string(),
            ],
            Some(8080),
        ));
    }
    if has_glob_ext(src, "sh") {
        for candidate in ["start.sh", "run.sh", "entrypoint.sh", "server.sh"] {
            if src.join(candidate).exists() {
                return Ok((vec!["sh".to_string(), candidate.to_string()], Some(8080)));
            }
        }
        anyhow::bail!(
            "cannot infer shell-script start command. add a Dockerfile or include start.sh/run.sh/entrypoint.sh"
        );
    }
    if has_glob_ext(src, "c")
        || has_glob_ext(src, "cpp")
        || has_glob_ext(src, "cc")
        || has_glob_ext(src, "cxx")
    {
        if let Some(bin) = find_first_executable(src, &["build", "bin", "."]) {
            return Ok((vec![format!("./{bin}")], Some(8080)));
        }
        anyhow::bail!(
            "cannot infer C/C++ start command. add a Dockerfile or ensure compiled binary exists in build/ or bin/"
        );
    }
    anyhow::bail!(
        "cannot infer container start command for this project. add a Dockerfile with explicit CMD/ENTRYPOINT"
    );
}

pub fn infer_local_start_command(src: &Path) -> Result<Vec<String>> {
    let (mut cmd, _port) = infer_container_start(src)?;
    if cmd.is_empty() {
        anyhow::bail!("cannot infer local start command (empty)");
    }
    if needs_node_prefix(&cmd) {
        cmd.insert(0, "node".to_string());
    }
    Ok(cmd)
}

fn find_first_jar(src: &Path) -> Option<String> {
    let candidates = [
        src.join("target"),
        src.join("build").join("libs"),
        src.to_path_buf(),
    ];
    for root in candidates {
        if !root.exists() || !root.is_dir() {
            continue;
        }
        if let Some(found) = find_first_jar_recursive(&root) {
            let rel = found
                .strip_prefix(src)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            return Some(rel);
        }
    }
    None
}

fn needs_node_prefix(cmd: &[String]) -> bool {
    if cmd.is_empty() {
        return false;
    }
    let first = cmd[0].trim().to_lowercase();
    if matches!(
        first.as_str(),
        "node" | "npm" | "pnpm" | "yarn" | "deno" | "bun"
    ) {
        return false;
    }
    first.ends_with(".js")
        || first.ends_with(".mjs")
        || first.ends_with(".cjs")
        || first.ends_with(".ts")
}

fn find_first_jar_recursive(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_first_jar_recursive(&path) {
                return Some(found);
            }
            continue;
        }
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("jar"))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

fn find_first_executable(src: &Path, roots: &[&str]) -> Option<String> {
    for root_rel in roots {
        let root = src.join(root_rel);
        if !root.exists() || !root.is_dir() {
            continue;
        }
        if let Some(found) = find_first_executable_recursive(&root) {
            let rel = found
                .strip_prefix(src)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            return Some(rel);
        }
    }
    None
}

fn find_first_executable_recursive(root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_first_executable_recursive(&path) {
                return Some(found);
            }
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".dll")
            || name.ends_with(".jar")
            || name.ends_with(".d")
            || name.ends_with(".rlib")
            || name.ends_with(".a")
            || name.ends_with(".o")
            || name.ends_with(".obj")
            || name.ends_with(".pdb")
            || name.ends_with(".map")
        {
            continue;
        }
        if path.extension().is_none() || name.ends_with(".exe") {
            return Some(path);
        }
    }
    None
}

fn find_first_by_ext(src: &Path, roots: &[&str], ext: &str) -> Option<String> {
    for root_rel in roots {
        let root = src.join(root_rel);
        if !root.exists() || !root.is_dir() {
            continue;
        }
        if let Some(found) = find_first_by_ext_recursive(&root, ext) {
            let rel = found
                .strip_prefix(src)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            return Some(rel);
        }
    }
    None
}

fn find_first_by_ext_recursive(root: &Path, ext: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_first_by_ext_recursive(&path, ext) {
                return Some(found);
            }
            continue;
        }
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case(ext))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

fn has_glob_ext(root: &Path, ext: &str) -> bool {
    let Ok(entries) = fs::read_dir(root) else {
        return false;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_file() {
            if p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case(ext))
                .unwrap_or(false)
            {
                return true;
            }
            if ext == "csproj"
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".csproj"))
                    .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

fn file_contains(path: &Path, needle: &str) -> bool {
    if !path.exists() {
        return false;
    }
    fs::read_to_string(path)
        .map(|v| v.to_lowercase().contains(&needle.to_lowercase()))
        .unwrap_or(false)
}

fn infer_node_start_from_package_json(src: &Path) -> Option<(Vec<String>, Option<u16>)> {
    let pkg = src.join("package.json");
    let raw = fs::read_to_string(pkg).ok()?;
    let parsed = serde_json::from_str::<Value>(&raw).ok()?;

    let start_script = parsed
        .get("scripts")
        .and_then(|s| s.get("start"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    if !start_script.is_empty() {
        let parts = start_script
            .split_whitespace()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        if parts.len() >= 2
            && parts[0].eq_ignore_ascii_case("next")
            && parts[1].eq_ignore_ascii_case("start")
        {
            let mut cmd = vec![
                "node_modules/next/dist/bin/next".to_string(),
                "start".to_string(),
            ];
            cmd.extend(parts.into_iter().skip(2));
            return Some((cmd, Some(3000)));
        }
        if let Some(rest) = start_script.strip_prefix("node ") {
            let parts = rest
                .split_whitespace()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                return Some((parts, Some(3000)));
            }
        }
    }

    let main = parsed.get("main").and_then(Value::as_str).map(str::trim);
    if let Some(main_file) = main {
        if !main_file.is_empty() && src.join(main_file).exists() {
            return Some((vec![main_file.to_string()], Some(3000)));
        }
    }
    None
}

fn has_package_script(src: &Path, script: &str) -> bool {
    let pkg = src.join("package.json");
    let Ok(raw) = fs::read_to_string(pkg) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    parsed
        .get("scripts")
        .and_then(|s| s.get(script))
        .and_then(Value::as_str)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

fn detect_node_package_manager(src: &Path) -> &'static str {
    if let Some(pm) = package_manager_from_package_json(src) {
        return pm;
    }
    if src.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if src.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    }
}

fn package_manager_from_package_json(src: &Path) -> Option<&'static str> {
    let pkg = src.join("package.json");
    let raw = fs::read_to_string(pkg).ok()?;
    let parsed = serde_json::from_str::<Value>(&raw).ok()?;
    let declared = parsed.get("packageManager").and_then(Value::as_str)?;
    let lower = declared.to_lowercase();
    if lower.starts_with("pnpm@") {
        return Some("pnpm");
    }
    if lower.starts_with("yarn@") {
        return Some("yarn");
    }
    if lower.starts_with("npm@") {
        return Some("npm");
    }
    None
}

fn install_with_fallback_command(preferred: &str, src: &Path) -> String {
    let npm_install = if src.join("package-lock.json").exists() {
        "npm ci --include=dev || npm install --include=dev"
    } else {
        "npm install --include=dev"
    };
    match preferred {
        "pnpm" => format!(
            "(pnpm install --frozen-lockfile --prod=false || pnpm install --no-frozen-lockfile --prod=false || pnpm install --prod=false) || ({npm_install}) || (yarn install --production=false)"
        ),
        "yarn" => format!(
            "(yarn install --frozen-lockfile --production=false || yarn install --production=false) || ({npm_install}) || (pnpm install --prod=false)"
        ),
        _ => format!(
            "({npm_install}) || (pnpm install --frozen-lockfile --prod=false || pnpm install --prod=false) || (yarn install --production=false)"
        ),
    }
}

fn build_with_fallback_command(preferred: &str) -> String {
    match preferred {
        "pnpm" => "(pnpm run build || npm run build || yarn build)".to_string(),
        "yarn" => "(yarn build || npm run build || pnpm run build)".to_string(),
        _ => "(npm run build || pnpm run build || yarn build)".to_string(),
    }
}

fn has_next_dependency(src: &Path) -> bool {
    let pkg = src.join("package.json");
    let Ok(raw) = fs::read_to_string(pkg) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    for field in ["dependencies", "devDependencies"] {
        if parsed
            .get(field)
            .and_then(Value::as_object)
            .map(|m| m.contains_key("next"))
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn should_regenerate_generated_dockerfile(existing: &str) -> bool {
    let lower = existing.to_lowercase();
    if lower.contains("# sendbuilds: auto-generated dockerfile") {
        return true;
    }
    // Legacy auto-generated Next.js command that fails when deps are absent at runtime.
    if lower.contains("node_modules/next/dist/bin/next") {
        return true;
    }
    false
}

fn create_kubernetes_manifests(
    root: &Path,
    project_name: &str,
    container_image: &str,
    kubernetes: Option<&KubernetesConfig>,
) -> Result<PathBuf> {
    let enabled = kubernetes.and_then(|k| k.enabled).unwrap_or(true);
    if !enabled {
        let disabled = root.join("kubernetes-disabled.txt");
        fs::write(
            &disabled,
            "kubernetes manifest generation disabled by config\n",
        )?;
        return Ok(disabled);
    }

    let name = sanitize_k8s_name(project_name);
    let namespace = kubernetes
        .and_then(|k| k.namespace.as_deref())
        .unwrap_or("default");
    let replicas = kubernetes.and_then(|k| k.replicas).unwrap_or(1);
    let container_port = kubernetes.and_then(|k| k.container_port).unwrap_or(8080);
    let service_port = kubernetes.and_then(|k| k.service_port).unwrap_or(80);
    let image_pull_policy = kubernetes
        .and_then(|k| k.image_pull_policy.as_deref())
        .unwrap_or("IfNotPresent");

    let out_dir = root.join("kubernetes");
    fs::create_dir_all(&out_dir)?;

    let deployment = format!(
        "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {name}\n  namespace: {namespace}\nspec:\n  replicas: {replicas}\n  selector:\n    matchLabels:\n      app: {name}\n  template:\n    metadata:\n      labels:\n        app: {name}\n    spec:\n      containers:\n        - name: {name}\n          image: {container_image}\n          imagePullPolicy: {image_pull_policy}\n          ports:\n            - containerPort: {container_port}\n"
    );

    let service = format!(
        "apiVersion: v1\nkind: Service\nmetadata:\n  name: {name}\n  namespace: {namespace}\nspec:\n  selector:\n    app: {name}\n  ports:\n    - protocol: TCP\n      port: {service_port}\n      targetPort: {container_port}\n  type: ClusterIP\n"
    );

    fs::write(out_dir.join("deployment.yaml"), deployment)?;
    fs::write(out_dir.join("service.yaml"), service)?;

    Ok(out_dir)
}

fn sanitize_k8s_name(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if c == '-' || c == '_' || c == ' ' || c == '.' {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "sendbuilds-app".to_string()
    } else {
        trimmed
    }
}

fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("cant read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());

        if ty.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_recursive(&entry.path(), &dest_path)?;
        } else {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

fn copy_workspace_recursive(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("cant read {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().to_string();

        if ty.is_dir() && should_skip_workspace_dir(&name) {
            continue;
        }

        let dest_path = dst.join(&name);
        if ty.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_workspace_recursive(&entry.path(), &dest_path)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), dest_path)?;
        }
    }
    Ok(())
}

fn should_skip_workspace_dir(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".next"
            | "node_modules"
            | "target"
            | "artifacts"
            | ".sendbuild-cache"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".gradle"
            | "build"
            | ".idea"
            | ".vscode"
    )
}
