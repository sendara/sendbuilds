use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::core::config::{effective_artifact_dir, project_storage_key};
use crate::core::{BuildConfig, BuildContext, Step, StepStatus};
use crate::errors::BuildError;
use crate::languages;
use crate::output::{events, logger as log};
use crate::runtime::{artifacts, cnb, git, metrics, scan, security, shell};
use crate::utils::cache::{
    changed_modules, compute_dependency_fingerprint, compute_file_signatures,
    fingerprint_from_signatures, BuildCache, BuildState,
};
use crate::utils::signing;
use crate::workers::parallel::{self, ParallelStepTask};

pub struct BuildEngine {
    config: BuildConfig,
    in_place: bool,
    events_override: Option<bool>,
    reproducible: bool,
    unused_deps: bool,
}

#[derive(Debug, Clone, Default)]
struct ResolvedBuild {
    install_cmd: String,
    build_cmd: String,
    output_dir: Option<String>,
    parallel_build_cmds: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct CacheMetrics {
    hits: u32,
    misses: u32,
}

#[derive(Debug, Clone, Serialize)]
struct StepMetric {
    name: String,
    status: String,
    duration_ms: u64,
    cpu_percent: Option<f32>,
    memory_mb: Option<u64>,
    disk_mb: Option<u64>,
}

impl BuildEngine {
    pub fn from_config(config: BuildConfig) -> Self {
        Self {
            config,
            in_place: false,
            events_override: None,
            reproducible: false,
            unused_deps: false,
        }
    }

    pub fn load(config_path: &str) -> Result<Self> {
        let config = BuildConfig::from_file(config_path)?;
        Ok(Self::from_config(config))
    }

    pub fn with_in_place(mut self, in_place: bool) -> Self {
        self.in_place = in_place;
        self
    }

    pub fn with_events(mut self, events: Option<bool>) -> Self {
        self.events_override = events;
        self
    }

    pub fn with_reproducible(mut self, reproducible: bool) -> Self {
        self.reproducible = reproducible;
        self
    }

    pub fn with_unused_deps(mut self, unused: bool) -> Self {
        self.unused_deps = unused;
        self
    }

    pub fn run(self) -> Result<()> {
        let cfg = &self.config;
        let events_enabled = self
            .events_override
            .or_else(|| cfg.output.as_ref().and_then(|o| o.events))
            .unwrap_or(false);
        events::set_enabled(events_enabled);

        let env_map = self.resolve_env();
        let sandbox_enabled = cfg.sandbox.as_ref().and_then(|s| s.enabled).unwrap_or(true);
        let sandbox_strict = if self.reproducible {
            true
        } else {
            cfg.sandbox.as_ref().and_then(|s| s.strict).unwrap_or(false)
        };
        shell::set_sandbox_strict(sandbox_enabled && sandbox_strict);
        let work_dir = if self.in_place && cfg.source.is_none() {
            std::env::current_dir()?
        } else {
            artifacts::make_workdir(&cfg.project.name)?
        };
        let artifact_dir = effective_artifact_dir(cfg);
        let ctx = BuildContext::new(&cfg.project.name, work_dir, artifact_dir, env_map);

        log::header(&format!("sendbuild - {}", cfg.project.name));
        log::kv(
            "buildpack",
            &format!("sendbuilds v{}", env!("CARGO_PKG_VERSION")),
        );
        log::kv("lifecycle", cnb::LIFECYCLE_API);
        log::kv(
            "language",
            cfg.project.language.as_deref().unwrap_or("auto-detect"),
        );
        match &cfg.source {
            Some(source) => log::kv("repo", &source.repo),
            None => log::kv(
                "source",
                if self.in_place {
                    "local workspace (in-place mode)"
                } else {
                    "local workspace (copied to temp)"
                },
            ),
        }
        log::kv("workdir", &ctx.work_dir.display().to_string());
        log::kv(
            "sandbox",
            if sandbox_enabled {
                if sandbox_strict {
                    "enabled (strict)"
                } else {
                    "enabled"
                }
            } else {
                "disabled"
            },
        );
        log::kv(
            "reproducible",
            if self.reproducible {
                "enabled"
            } else {
                "disabled"
            },
        );

        let mut steps: Vec<Step> = Vec::new();
        let mut cache_metrics = CacheMetrics::default();
        let mut publish_result: Option<artifacts::PublishResult> = None;
        let mut security_report: Option<security::SecurityReport> = None;
        let mut security_sbom: Option<Value> = None;
        let mut supply_chain_metadata: Option<Value> = None;
        let unused_deps_enabled = self.unused_deps;

        let cache = self.configure_cache(&ctx)?;
        if let Some(c) = &cache {
            log::kv("cache", &c.root().display().to_string());
        }

        let mut resolved = ResolvedBuild::default();
        let mut previous_state: Option<BuildState> = None;
        let mut current_signatures = BTreeMap::new();
        let mut source_fingerprint = String::new();
        let mut dependency_fingerprint = String::new();
        let mut dep_cache_hit = false;
        let mut dep_dir_cache_allowed = true;
        let mut install_ran = false;
        let mut dep_install_cmd = String::new();
        let mut changed = vec!["all".to_string()];
        let mut resolved_language = String::new();
        let security_enabled = security::enabled(cfg.security.as_ref());

        steps.push(self.execute_step(&ctx, "source", |_e, c, s| self.step_source(c, s))?);
        steps.push(self.execute_step(&ctx, "detect-build-config", |_e, c, s| {
            resolved_language = self.resolve_language(c).ok_or_else(|| {
                anyhow!("unable to infer project language; set [project].language")
            })?;
            languages::validate(&resolved_language)?;
            s.push_log(format!("language={resolved_language}"));
            resolved = self.resolve_build(c, s)?;
            dep_install_cmd = resolved.install_cmd.clone();
            Ok(())
        })?);
        steps.push(self.execute_step(&ctx, "compatibility-check", |_e, c, s| {
            self.step_compat(c, s)
        })?);
        if self.reproducible {
            steps.push(
                self.execute_step(&ctx, "reproducibility-check", |_e, c, s| {
                    self.reproducibility_check(c, &resolved_language, s)
                })?,
            );
        }

        if let Some(c) = &cache {
            steps.push(
                self.execute_step(&ctx, "incremental-prepare", |_e, cctx, step| {
                    previous_state = c.load_state()?;
                    current_signatures = compute_file_signatures(&cctx.work_dir)?;
                    source_fingerprint = fingerprint_from_signatures(&current_signatures);
                    dependency_fingerprint = compute_dependency_fingerprint(&cctx.work_dir)?;
                    changed = changed_modules(previous_state.as_ref(), &current_signatures);

                    dep_cache_hit = previous_state
                        .as_ref()
                        .map(|p| {
                            p.dependency_fingerprint == dependency_fingerprint
                                && c.has_dependency_cache()
                        })
                        .unwrap_or(false);
                    dep_dir_cache_allowed = !cctx.work_dir.join("pnpm-lock.yaml").exists();

                    dep_install_cmd =
                        self.optimize_install_cmd(&dep_install_cmd, &cctx.work_dir, dep_cache_hit);
                    let total_files = current_signatures.len();
                    let updated_files = if changed.iter().any(|v| v == "all") {
                        total_files
                    } else {
                        changed.len().min(total_files)
                    };
                    let reused_files = total_files.saturating_sub(updated_files);
                    step.push_log(format!(
                        "incremental_summary reused={} updated={}",
                        reused_files, updated_files
                    ));
                    log::kv(
                        "Preparing incremental build data",
                        &format!("reused={} updated={}", reused_files, updated_files),
                    );
                    let cache_size = dir_size_bytes(c.root()).unwrap_or_default();
                    step.push_log(format!(
                        "cache_size_mb={:.1}",
                        cache_size as f64 / (1024.0 * 1024.0)
                    ));
                    log::kv(
                        "Cache size",
                        &format!("{:.1}MB", cache_size as f64 / (1024.0 * 1024.0)),
                    );
                    step.push_log(format!("dependency_cache_hit={dep_cache_hit}"));
                    step.push_log(format!(
                        "dependency_dir_cache_allowed={dep_dir_cache_allowed}"
                    ));
                    step.push_log(format!("install_command={dep_install_cmd}"));
                    step.push_log(format!("changed_modules={}", changed.join(",")));
                    Ok(())
                })?,
            );
        }

        steps.push(self.execute_step(&ctx, "install", |_e, cctx, step| {
            if let Some(c) = &cache {
                if dep_cache_hit && dep_dir_cache_allowed {
                    let restored = c.restore_dependencies(&cctx.work_dir)?;
                    if self.validate_dependency_restore(&cctx.work_dir) {
                        cache_metrics.hits += 1;
                        step.push_log(format!(
                            "deps cache hit copied={} skipped={} removed={}",
                            restored.copied_files, restored.skipped_files, restored.removed_files
                        ));
                        return Ok(());
                    }
                }
            }
            cache_metrics.misses += 1;
            let run = self.run_install_with_fallback(
                &dep_install_cmd,
                &cctx.work_dir,
                &cctx.env,
                sandbox_enabled,
                step,
            )?;
            install_ran = true;
            for line in run.logs {
                step.push_log(line.clone());
                log::pipe(&line);
            }
            Ok(())
        })?);
        if unused_deps_enabled {
            let lang = resolved_language.clone();
            steps.push(self.execute_step(&ctx, "unused-deps", |_e, cctx, step| {
                let out = crate::runtime::unused::run(&lang, &cctx.work_dir, &cctx.env, sandbox_enabled)?;
                for line in crate::runtime::unused::to_build_logs(&out) {
                    step.push_log(line.clone());
                    log::pipe(&line);
                }
                Ok(())
            })?);
        }
        if security_enabled {
            let lang = resolved_language.clone();
            let security_cfg = cfg.security.clone();
            let in_place_mode = self.in_place;
            steps.push(self.execute_step(&ctx, "security-first", |_e, cctx, step| {
                let out = security::run(
                    &lang,
                    security_cfg.as_ref(),
                    cfg,
                    &cctx.work_dir,
                    &cctx.env,
                    sandbox_enabled,
                    in_place_mode,
                )?;
                for line in security::to_build_logs(&out.report) {
                    step.push_log(line.clone());
                    log::security(&line);
                }
                security_sbom = Some(out.sbom);
                supply_chain_metadata = Some(out.supply_chain_metadata);
                security_report = Some(out.report);
                Ok(())
            })?);
        }
        let mut post = Vec::new();
        if !security_enabled && scan::enabled(cfg.scan.as_ref()) {
            let lang = resolved_language.clone();
            let scan_cfg = cfg.scan.clone();
            let wd = ctx.work_dir.clone();
            let env = ctx.env.clone();
            post.push(ParallelStepTask::new("security-scan", move |step| {
                let run = scan::run(&lang, scan_cfg.as_ref(), &wd, &env, sandbox_enabled)?;
                for line in run.logs {
                    step.push_log(line.clone());
                    log::pipe(&line);
                }
                Ok(())
            }));
        }
        if let Some(c) = cache.clone() {
            if install_ran && dep_dir_cache_allowed {
                let wd = ctx.work_dir.clone();
                post.push(ParallelStepTask::new("deps-cache-save", move |step| {
                    let saved = c.save_dependencies(&wd)?;
                    step.push_log(format!(
                        "deps cache save copied={} skipped={} removed={} bytes={}",
                        saved.copied_files,
                        saved.skipped_files,
                        saved.removed_files,
                        saved.copied_bytes
                    ));
                    Ok(())
                }));
            }
        }
        if !post.is_empty() {
            let mut done = parallel::run(post)?;
            steps.append(&mut done);
        }

        steps.push(self.execute_step(&ctx, "build", |_e, cctx, step| {
            let output_src = self.output_src(cctx, resolved.output_dir.as_deref());
            let unchanged = previous_state
                .as_ref()
                .map(|p| p.source_fingerprint == source_fingerprint)
                .unwrap_or(false);

            if let Some(c) = &cache {
                if unchanged && c.has_artifact_cache() {
                    let restored = c.restore_artifact(&output_src)?;
                    cache_metrics.hits += 1;
                    step.push_log(format!(
                        "artifact cache hit copied={} skipped={} removed={}",
                        restored.copied_files, restored.skipped_files, restored.removed_files
                    ));
                    return Ok(());
                }
            }
            cache_metrics.misses += 1;

            if !resolved.parallel_build_cmds.is_empty() {
                let mut tasks = Vec::new();
                for (idx, cmd) in resolved.parallel_build_cmds.iter().enumerate() {
                    let command = cmd.clone();
                    let wd = cctx.work_dir.clone();
                    let env = cctx.env.clone();
                    tasks.push(ParallelStepTask::new(
                        format!("build-task-{}", idx + 1),
                        move |_s| {
                            let _ = shell::run(&command, &wd, &env, sandbox_enabled)?;
                            Ok(())
                        },
                    ));
                }
                let _ = parallel::run(tasks)?;
            } else {
                let run = shell::run(
                    &resolved.build_cmd,
                    &cctx.work_dir,
                    &cctx.env,
                    sandbox_enabled,
                )?;
                for line in run.logs {
                    step.push_log(line.clone());
                    log::pipe(&line);
                }
            }

            if let Some(c) = &cache {
                let saved = c.save_artifact(&output_src)?;
                step.push_log(format!(
                    "artifact cache save copied={} skipped={} removed={} bytes={}",
                    saved.copied_files,
                    saved.skipped_files,
                    saved.removed_files,
                    saved.copied_bytes
                ));
            }
            step.push_log(format!("changed_modules={}", changed.join(",")));
            Ok(())
        })?);

        steps.push(self.execute_step(&ctx, "deploy", |_e, cctx, step| {
            publish_result = Some(self.step_deploy(cctx, step, resolved.output_dir.as_deref())?);
            Ok(())
        })?);

        if security_enabled {
            let image = cfg.deploy.container_image.clone();
            let security_cfg = cfg.security.clone();
            steps.push(self.execute_step(&ctx, "container-security-scan", |_e, cctx, step| {
                let mut scans = Vec::new();

                if let Some(img) = image.as_deref() {
                    let image_summary = security::run_container_image_scan(
                        img,
                        security_cfg.as_ref(),
                        &cctx.env,
                        sandbox_enabled,
                    )?;
                    step.push_log(format!(
                        "target=image:{} scanner={} scanned={} total={} critical={} high={} moderate={} low={} info={} misconfigurations={} secrets={}",
                        img,
                        image_summary.scanner,
                        image_summary.scanned,
                        image_summary.total,
                        image_summary.critical,
                        image_summary.high,
                        image_summary.moderate,
                        image_summary.low,
                        image_summary.info,
                        image_summary.misconfigurations,
                        image_summary.secrets
                    ));
                    if let Some(reason) = &image_summary.unavailable_reason {
                        step.push_log(format!("target=image:{img} unavailable_reason={reason}"));
                    }
                    scans.push(image_summary);
                }

                if let Some(p) = &publish_result {
                    for output in &p.outputs {
                        if !is_tar_artifact(output) {
                            continue;
                        }
                        let tar_summary = security::run_container_tar_scan(
                            output,
                            security_cfg.as_ref(),
                            &cctx.env,
                            sandbox_enabled,
                        )?;
                        step.push_log(format!(
                            "target=tar:{} scanner={} scanned={} total={} critical={} high={} moderate={} low={} info={} misconfigurations={} secrets={}",
                            output.display(),
                            tar_summary.scanner,
                            tar_summary.scanned,
                            tar_summary.total,
                            tar_summary.critical,
                            tar_summary.high,
                            tar_summary.moderate,
                            tar_summary.low,
                            tar_summary.info,
                            tar_summary.misconfigurations,
                            tar_summary.secrets
                        ));
                        if let Some(reason) = &tar_summary.unavailable_reason {
                            step.push_log(format!(
                                "target=tar:{} unavailable_reason={reason}",
                                output.display()
                            ));
                        }
                        scans.push(tar_summary);
                    }
                }

                if scans.is_empty() {
                    step.push_log("no image/tar targets available for container scan".to_string());
                    return Ok(());
                }

                let summary = security::merge_scan_summaries(&scans);
                step.push_log(format!(
                    "aggregated scanner={} scanned={} total={} critical={} high={} moderate={} low={} info={} misconfigurations={} secrets={}",
                    summary.scanner,
                    summary.scanned,
                    summary.total,
                    summary.critical,
                    summary.high,
                    summary.moderate,
                    summary.low,
                    summary.info,
                    summary.misconfigurations,
                    summary.secrets
                ));

                if let Some(report) = security_report.as_mut() {
                    report.container_scan = Some(summary.clone());
                }

                let fail_on_critical = security_cfg
                    .as_ref()
                    .and_then(|c| c.fail_on_critical)
                    .unwrap_or(true);
                let critical_threshold = security_cfg
                    .as_ref()
                    .and_then(|c| c.critical_threshold)
                    .unwrap_or(0);
                let fail_on_scanner_unavailable = security_cfg
                    .as_ref()
                    .and_then(|c| c.fail_on_scanner_unavailable)
                    .unwrap_or(false);
                if fail_on_scanner_unavailable && scans.iter().all(|s| !s.scanned) {
                    bail!(
                        "security policy violation: required container/image tar scanner unavailable (attempted: {})",
                        scans
                            .iter()
                            .flat_map(|s| s.scanner_attempts.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if fail_on_critical && summary.critical > critical_threshold {
                    let package_hint = if summary.packages.is_empty() {
                        "none".to_string()
                    } else {
                        summary.packages.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
                    };
                    bail!(
                        "security policy violation: container scan critical vulnerabilities {} exceed threshold {}. packages={}",
                        summary.critical,
                        critical_threshold,
                        package_hint
                    );
                }
                if summary.high > 0 || summary.moderate > 0 {
                    let package_hint = if summary.packages.is_empty() {
                        "none".to_string()
                    } else {
                        summary.packages.iter().take(12).cloned().collect::<Vec<_>>().join(", ")
                    };
                    bail!(
                        "security policy violation: container scan HIGH/MODERATE vulnerabilities are not allowed. observed_high={} observed_moderate={}. packages={}",
                        summary.high,
                        summary.moderate,
                        package_hint
                    );
                }
                Ok(())
            })?);
        }

        steps.push(
            self.execute_step(&ctx, "sign-artifacts", |_e, _cctx, step| {
                let enabled = cfg
                    .signing
                    .as_ref()
                    .and_then(|s| s.enabled)
                    .unwrap_or(false);
                if !enabled {
                    step.push_log("signing disabled".to_string());
                    return Ok(());
                }
                let key_env = cfg
                    .signing
                    .as_ref()
                    .and_then(|s| s.key_env.clone())
                    .unwrap_or_else(|| "SENDBUILD_SIGNING_KEY".to_string());
                if let Some(p) = &publish_result {
                    let provenance_options = signing::ProvenanceOptions {
                        project_name: cfg.project.name.clone(),
                        container_image: cfg.deploy.container_image.clone(),
                        cosign: cfg.signing.as_ref().and_then(|s| s.cosign).unwrap_or(false),
                        cosign_key: cfg.signing.as_ref().and_then(|s| s.cosign_key.clone()),
                        cosign_keyless: cfg
                            .signing
                            .as_ref()
                            .and_then(|s| s.cosign_keyless)
                            .unwrap_or(false),
                        verify_after_sign: cfg
                            .signing
                            .as_ref()
                            .and_then(|s| s.verify_after_sign)
                            .unwrap_or(false),
                        verify_certificate_identity: cfg
                            .signing
                            .as_ref()
                            .and_then(|s| s.verify_certificate_identity.clone()),
                        verify_certificate_oidc_issuer: cfg
                            .signing
                            .as_ref()
                            .and_then(|s| s.verify_certificate_oidc_issuer.clone()),
                    };
                    let (manifest, sig) = signing::sign_outputs(&p.root, &p.outputs, &key_env)?;
                    step.push_log(format!("manifest {}", manifest.display()));
                    step.push_log(format!("signature {}", sig.display()));
                    if let Some((cosign_sig, cosign_cert)) =
                        signing::sign_manifest_with_cosign(&manifest, &provenance_options)?
                    {
                        step.push_log(format!("cosign_blob_signature {}", cosign_sig.display()));
                        step.push_log(format!("cosign_blob_certificate {}", cosign_cert.display()));
                        signing::verify_manifest_with_cosign(
                            &manifest,
                            &cosign_sig,
                            Some(&cosign_cert),
                            &provenance_options,
                        )?;
                        if provenance_options.verify_after_sign {
                            step.push_log("cosign blob verification passed".to_string());
                        }
                    }
                    let generate_provenance = cfg
                        .signing
                        .as_ref()
                        .and_then(|s| s.generate_provenance)
                        .unwrap_or(true);
                    if generate_provenance {
                        let provenance =
                            signing::write_provenance(&p.root, &p.outputs, &provenance_options)?;
                        step.push_log(format!("provenance {}", provenance.display()));
                        if provenance_options.verify_after_sign && provenance_options.cosign {
                            step.push_log("cosign image verification passed".to_string());
                        }
                    }
                }
                Ok(())
            })?,
        );

        if let Some(c) = &cache {
            steps.push(
                self.execute_step(&ctx, "cache-state-save", |_e, _cctx, step| {
                    c.save_state(&BuildState {
                        source_fingerprint: source_fingerprint.clone(),
                        dependency_fingerprint: dependency_fingerprint.clone(),
                        file_signatures: current_signatures.clone(),
                    })?;
                    step.push_log("cache state saved");
                    Ok(())
                })?,
            );
        }

        steps.push(self.execute_step(&ctx, "build-metrics", |_e, _cctx, step| {
            let root = publish_result
                .as_ref()
                .map(|p| p.root.clone())
                .unwrap_or_else(|| ctx.artifact_dir.clone());
            let build_id = root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let source_identity = detect_source_identity(cfg, &ctx.work_dir);
            let metrics_steps = steps
                .iter()
                .map(|s| StepMetric {
                    name: s.name.clone(),
                    status: s.status.as_str().to_string(),
                    duration_ms: (s.duration_secs.unwrap_or_default() * 1000.0).round() as u64,
                    cpu_percent: s.resources.map(|r| r.cpu_percent),
                    memory_mb: s.resources.map(|r| r.memory_mb),
                    disk_mb: s.resources.map(|r| r.disk_mb),
                })
                .collect::<Vec<_>>();
            let report = serde_json::json!({
                "project": cfg.project.name,
                "build_id": build_id,
                "finished_at": chrono::Local::now().to_rfc3339(),
                "source": source_identity,
                "cache": cache_metrics,
                "source_fingerprint": if source_fingerprint.is_empty() { None } else { Some(source_fingerprint.clone()) },
                "dependency_fingerprint": if dependency_fingerprint.is_empty() { None } else { Some(dependency_fingerprint.clone()) },
                "security": &security_report,
                "supply_chain_metadata": &supply_chain_metadata,
                "steps": metrics_steps
            });
            fs::create_dir_all(&root)?;
            if let Some(sbom) = &security_sbom {
                let sbom_out = root.join("sbom.json");
                fs::write(&sbom_out, serde_json::to_vec_pretty(sbom)?)?;
                step.push_log(format!("sbom {}", sbom_out.display()));
            }
            if let Some(supply) = &supply_chain_metadata {
                let supply_out = root.join("supply-chain-metadata.json");
                fs::write(&supply_out, serde_json::to_vec_pretty(supply)?)?;
                step.push_log(format!("supply chain metadata {}", supply_out.display()));
            }
            if let Some(sec_report) = &security_report {
                let security_out = root.join("security-report.json");
                fs::write(&security_out, serde_json::to_vec_pretty(sec_report)?)?;
                step.push_log(format!("security report {}", security_out.display()));
            }
            let out = root.join("build-metrics.json");
            fs::write(&out, serde_json::to_vec_pretty(&report)?)?;
            step.push_log(format!("metrics {}", out.display()));

            let build_info = serde_json::json!({
                "schema_version": "1",
                "project": cfg.project.name,
                "build_id": report.get("build_id").cloned().unwrap_or(serde_json::Value::Null),
                "finished_at": report.get("finished_at").cloned().unwrap_or(serde_json::Value::Null),
                "source": report.get("source").cloned().unwrap_or(serde_json::Value::Null),
                "source_fingerprint": report.get("source_fingerprint").cloned().unwrap_or(serde_json::Value::Null),
                "dependency_fingerprint": report.get("dependency_fingerprint").cloned().unwrap_or(serde_json::Value::Null),
            });
            let build_info_out = root.join("build-info.json");
            fs::write(&build_info_out, serde_json::to_vec_pretty(&build_info)?)?;
            step.push_log(format!("build info {}", build_info_out.display()));
            Ok(())
        })?);

        steps.push(self.execute_step(&ctx, "cnb-lifecycle", |_e, _cctx, step| {
            let root = publish_result
                .as_ref()
                .map(|p| p.root.clone())
                .unwrap_or_else(|| ctx.artifact_dir.clone());
            fs::create_dir_all(&root)?;
            let contract = cnb::write_lifecycle_contract(&root)?;
            let metadata = cnb::write_lifecycle_metadata(
                &root,
                &cfg.project.name,
                ctx.started_at,
                &steps,
                publish_result
                    .as_ref()
                    .map(|p| p.outputs.as_slice())
                    .unwrap_or(&[]),
                publish_result
                    .as_ref()
                    .map(|p| p.warnings.as_slice())
                    .unwrap_or(&[]),
            )?;
            step.push_log(format!("cnb contract {}", contract.display()));
            step.push_log(format!("cnb metadata {}", metadata.display()));
            if let Some(p) = &mut publish_result {
                p.outputs.push(contract);
                p.outputs.push(metadata);
            }
            Ok(())
        })?);

        log::steps_summary(&steps);
        log_build_overview(&steps, ctx.elapsed_secs(), &cache_metrics);
        log::section(&format!("done in {:.1}s", ctx.elapsed_secs()));
        Ok(())
    }

    fn resolve_env(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        if let Some(env) = &self.config.env {
            for (k, v) in env {
                out.insert(k.clone(), v.clone());
            }
        }
        if !self.reproducible {
            if let Some(host) = &self.config.env_from_host {
                for key in host {
                    if let Ok(v) = std::env::var(key) {
                        out.insert(key.clone(), v);
                    }
                }
            }
        } else {
            out.insert("TZ".to_string(), "UTC".to_string());
            out.insert("LANG".to_string(), "C".to_string());
            out.insert("LC_ALL".to_string(), "C".to_string());
            out.insert("PYTHONHASHSEED".to_string(), "0".to_string());
            out.insert("DOTNET_CLI_TELEMETRY_OPTOUT".to_string(), "1".to_string());
            out.insert(
                "SOURCE_DATE_EPOCH".to_string(),
                std::env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| "0".to_string()),
            );
        }
        out
    }

    fn resolve_build(&self, ctx: &BuildContext, step: &mut Step) -> Result<ResolvedBuild> {
        let cfg = self.config.build.as_ref();
        let install_cmd = cfg
            .and_then(|b| b.install_cmd.clone())
            .unwrap_or(self.infer_install_cmd(&ctx.work_dir)?);
        let build_cmd = cfg
            .and_then(|b| b.build_cmd.clone())
            .unwrap_or(self.infer_build_cmd(&ctx.work_dir)?);
        let output_dir = cfg
            .and_then(|b| b.output_dir.clone())
            .or_else(|| self.infer_output_dir(&ctx.work_dir));
        let parallel_build_cmds = cfg
            .and_then(|b| b.parallel_build_cmds.clone())
            .unwrap_or_default();
        step.push_log(format!(
            "install_cmd={}",
            shell::redact_command_for_log(&install_cmd)
        ));
        step.push_log(format!(
            "build_cmd={}",
            shell::redact_command_for_log(&build_cmd)
        ));
        step.push_log(format!(
            "output_dir={}",
            output_dir
                .clone()
                .unwrap_or_else(|| "<repo-root>".to_string())
        ));
        Ok(ResolvedBuild {
            install_cmd,
            build_cmd,
            output_dir,
            parallel_build_cmds,
        })
    }

    fn step_compat(&self, ctx: &BuildContext, step: &mut Step) -> Result<()> {
        if let Some(comp) = &self.config.compatibility {
            if let Some(os) = &comp.target_os {
                let host = std::env::consts::OS;
                if os != host {
                    step.push_log(format!("warning target_os={os} host_os={host}"));
                }
            }
            if let Some(arch) = &comp.target_arch {
                let host = std::env::consts::ARCH;
                if arch != host {
                    step.push_log(format!("warning target_arch={arch} host_arch={host}"));
                }
            }
            if let Some(target_node_major) = comp.target_node_major {
                if let Ok(out) = Command::new("node").arg("--version").output() {
                    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if let Some(major) = parse_node_major(&v) {
                        if major != target_node_major {
                            step.push_log(format!(
                                "warning target_node_major={target_node_major} current={major}"
                            ));
                        }
                    }
                }
            }
        } else {
            step.push_log("compatibility config not set");
        }

        if let Some(pkg) = read_package_json(&ctx.work_dir.join("package.json")) {
            if let Some(engine) = pkg
                .get("engines")
                .and_then(|e| e.get("node"))
                .and_then(Value::as_str)
            {
                step.push_log(format!("package engines.node={engine}"));
            }
        }
        Ok(())
    }

    fn configure_cache(&self, ctx: &BuildContext) -> Result<Option<BuildCache>> {
        if self.reproducible {
            return Ok(None);
        }
        let enabled = self
            .config
            .cache
            .as_ref()
            .and_then(|c| c.enabled)
            .unwrap_or(true);
        if !enabled {
            return Ok(None);
        }
        let base = self
            .config
            .cache
            .as_ref()
            .and_then(|c| c.dir.as_ref())
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.artifact_dir.join(".sendbuild-cache"));
        let key = project_storage_key(&self.config);
        Ok(Some(BuildCache::new(&key, &base)?))
    }

    fn output_src(&self, ctx: &BuildContext, output_dir: Option<&str>) -> PathBuf {
        output_dir
            .map(|d| ctx.work_dir.join(d))
            .unwrap_or_else(|| ctx.work_dir.clone())
    }

    fn optimize_install_cmd(&self, raw: &str, work_dir: &Path, dep_cache_hit: bool) -> String {
        let raw = raw.trim();
        if dep_cache_hit {
            return raw.to_string();
        }
        let has = |name: &str| work_dir.join(name).exists();
        if raw.starts_with("pnpm install") {
            let mut cmd = raw.to_string();
            if has("pnpm-lock.yaml") && !cmd.contains("--frozen-lockfile") {
                cmd.push_str(" --frozen-lockfile");
            }
            if !cmd.contains("--prefer-offline") {
                cmd.push_str(" --prefer-offline");
            }
            return cmd;
        }
        if raw.starts_with("yarn install") && has("yarn.lock") && !raw.contains("--frozen-lockfile")
        {
            return format!("{raw} --frozen-lockfile");
        }
        if raw == "npm install" && has("package-lock.json") {
            return "npm ci --prefer-offline".to_string();
        }
        raw.to_string()
    }

    fn run_install_with_fallback(
        &self,
        cmd: &str,
        wd: &Path,
        env: &HashMap<String, String>,
        sandbox: bool,
        step: &mut Step,
    ) -> Result<shell::ShellRunOutput> {
        if self.reproducible {
            let run = shell::run_allow_failure(cmd, wd, env, sandbox)?;
            for line in &run.logs {
                step.push_log(line.clone());
                log::pipe(line);
            }
            if run.success {
                return Ok(run);
            }
            bail!(
                "install failed in reproducible mode (no fallback allowed). cmd=`{}` exit={:?}",
                shell::redact_command_for_log(cmd),
                run.exit_code
            );
        }
        let candidates = self.install_fallback_candidates(cmd);
        let mut failures = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            step.push_log(format!(
                "install attempt {} {}",
                i + 1,
                shell::redact_command_for_log(c)
            ));
            let run = shell::run_allow_failure(c, wd, env, sandbox)?;
            for line in &run.logs {
                step.push_log(line.clone());
                log::pipe(line);
            }
            if run.success {
                return Ok(run);
            }
            let hint = explain_exit_code(run.exit_code)
                .map(|v| format!(" hint={v}"))
                .unwrap_or_default();
            failures.push(format!(
                "attempt {} cmd=`{}` exit={:?}{}",
                i + 1,
                shell::redact_command_for_log(c),
                run.exit_code,
                hint
            ));
        }
        bail!(
            "install failed after {} attempt(s): {}",
            candidates.len(),
            failures.join(" | ")
        )
    }

    fn install_fallback_candidates(&self, initial: &str) -> Vec<String> {
        let mut out = vec![initial.to_string()];
        if initial.starts_with("pnpm install") {
            let no_offline = remove_flag(initial, "--prefer-offline");
            push_unique(&mut out, no_offline.clone());
            push_unique(&mut out, remove_flag(&no_offline, "--frozen-lockfile"));
        } else if initial.starts_with("yarn install") {
            push_unique(&mut out, remove_flag(initial, "--frozen-lockfile"));
        } else if initial.starts_with("npm ci") {
            push_unique(&mut out, initial.replacen("npm ci", "npm install", 1));
        }
        out
    }

    fn validate_dependency_restore(&self, work_dir: &Path) -> bool {
        let nm = work_dir.join("node_modules");
        if !nm.exists() {
            return false;
        }
        if let Some(pkg) = read_package_json(&work_dir.join("package.json")) {
            if has_next_dependency(&pkg) {
                return nm
                    .join("next")
                    .join("dist")
                    .join("bin")
                    .join("next")
                    .exists();
            }
        }
        true
    }

    fn reproducibility_check(
        &self,
        ctx: &BuildContext,
        language: &str,
        step: &mut Step,
    ) -> Result<()> {
        let wd = &ctx.work_dir;
        if wd.join("package.json").exists() {
            let node_locks = ["pnpm-lock.yaml", "yarn.lock", "package-lock.json"]
                .iter()
                .filter(|n| wd.join(n).exists())
                .count();
            if node_locks != 1 {
                bail!(
                    "reproducible mode requires exactly one Node lockfile (pnpm-lock.yaml, yarn.lock, or package-lock.json)"
                );
            }
            step.push_log("node lockfile check passed".to_string());
        }

        if wd.join("Cargo.toml").exists() && !wd.join("Cargo.lock").exists() {
            bail!("reproducible mode requires Cargo.lock for Rust projects");
        }
        if wd.join("go.mod").exists() && !wd.join("go.sum").exists() {
            bail!("reproducible mode requires go.sum for Go projects");
        }
        if wd.join("composer.json").exists() && !wd.join("composer.lock").exists() {
            bail!("reproducible mode requires composer.lock for PHP projects");
        }
        if wd.join("Gemfile").exists() && !wd.join("Gemfile.lock").exists() {
            bail!("reproducible mode requires Gemfile.lock for Ruby projects");
        }
        if wd.join("pyproject.toml").exists()
            && !wd.join("poetry.lock").exists()
            && !wd.join("requirements.txt").exists()
        {
            bail!(
                "reproducible mode requires poetry.lock or requirements.txt when pyproject.toml is present"
            );
        }

        if language == "dotnet" && !wd.join("global.json").exists() {
            bail!("reproducible mode requires global.json for .NET SDK pinning");
        }

        step.push_log("reproducibility checks passed".to_string());
        Ok(())
    }

    fn infer_install_cmd(&self, wd: &Path) -> Result<String> {
        if wd.join("pnpm-lock.yaml").exists() {
            return Ok("pnpm install --frozen-lockfile --prefer-offline".to_string());
        }
        if wd.join("yarn.lock").exists() {
            return Ok("yarn install --frozen-lockfile".to_string());
        }
        if wd.join("package-lock.json").exists() {
            return Ok("npm ci --prefer-offline".to_string());
        }
        if wd.join("package.json").exists() {
            return Ok("npm install --prefer-offline".to_string());
        }
        if wd.join("Gemfile").exists() {
            return Ok("bundle install".to_string());
        }
        if wd.join("composer.lock").exists() || wd.join("composer.json").exists() {
            return Ok("composer install --no-interaction --prefer-dist".to_string());
        }
        if wd.join("go.mod").exists() {
            return Ok("go mod download".to_string());
        }
        if wd.join("pom.xml").exists() {
            return Ok("mvn -q -DskipTests dependency:resolve".to_string());
        }
        if wd.join("build.gradle").exists() || wd.join("build.gradle.kts").exists() {
            return Ok("./gradlew dependencies".to_string());
        }
        if wd.join("Cargo.toml").exists() {
            return Ok("cargo fetch".to_string());
        }
        if wd.join("deno.json").exists() || wd.join("deno.jsonc").exists() {
            return Ok("deno cache main.ts".to_string());
        }
        if wd.join("mix.exs").exists() {
            return Ok("mix deps.get".to_string());
        }
        if wd.join("gleam.toml").exists() {
            return Ok("gleam deps download".to_string());
        }
        if has_glob_ext(wd, "csproj") || wd.join("global.json").exists() {
            return Ok("dotnet restore".to_string());
        }
        if wd.join("CMakeLists.txt").exists() {
            return Ok("cmake -S . -B build".to_string());
        }
        if has_glob_ext(wd, "c")
            || has_glob_ext(wd, "cpp")
            || has_glob_ext(wd, "cc")
            || has_glob_ext(wd, "cxx")
        {
            return Ok("echo no dependency install step for c/c++".to_string());
        }
        if has_glob_ext(wd, "sh") {
            return Ok("echo no dependency install step for shell scripts".to_string());
        }
        if wd.join("index.html").exists() {
            return Ok("echo no dependency install step for static site".to_string());
        }
        if wd.join("requirements.txt").exists() {
            return Ok("pip install -r requirements.txt".to_string());
        }
        if wd.join("poetry.lock").exists() || wd.join("pyproject.toml").exists() {
            return Ok("pip install -e .".to_string());
        }
        bail!("unable to infer install command; set [build].install_cmd")
    }

    fn infer_build_cmd(&self, wd: &Path) -> Result<String> {
        let pkg = wd.join("package.json");
        if pkg.exists() {
            if let Some(json) = read_package_json(&pkg) {
                if has_build_script(&json) {
                    if wd.join("pnpm-lock.yaml").exists() {
                        return Ok("pnpm run build".to_string());
                    }
                    if wd.join("yarn.lock").exists() {
                        return Ok("yarn build".to_string());
                    }
                    return Ok("npm run build".to_string());
                }
                if has_next_dependency(&json) {
                    if wd.join("pnpm-lock.yaml").exists() {
                        return Ok("pnpm next build".to_string());
                    }
                    return Ok("npx next build".to_string());
                }
            }
        }
        if wd.join("Gemfile").exists() && file_contains(&wd.join("Gemfile"), "rails") {
            return Ok("bundle exec rails assets:precompile".to_string());
        }
        if wd.join("Rakefile").exists() {
            return Ok("bundle exec rake build".to_string());
        }
        if wd.join("manage.py").exists() {
            return Ok("python manage.py collectstatic --noinput".to_string());
        }
        if wd.join("app.py").exists() || wd.join("wsgi.py").exists() {
            return Ok("python -m flask --app app routes".to_string());
        }
        if wd.join("pom.xml").exists() {
            return Ok("mvn -DskipTests package".to_string());
        }
        if wd.join("build.gradle").exists() || wd.join("build.gradle.kts").exists() {
            return Ok("./gradlew build -x test".to_string());
        }
        if wd.join("artisan").exists()
            || file_contains(&wd.join("composer.json"), "laravel/framework")
        {
            return Ok("php artisan config:cache && php artisan route:cache".to_string());
        }
        if wd.join("go.mod").exists() {
            return Ok("go build ./...".to_string());
        }
        if wd.join("Cargo.toml").exists() {
            return Ok("cargo build --release".to_string());
        }
        if wd.join("deno.json").exists() || wd.join("deno.jsonc").exists() {
            return Ok("deno check .".to_string());
        }
        if wd.join("mix.exs").exists() {
            return Ok("mix compile".to_string());
        }
        if wd.join("gleam.toml").exists() {
            return Ok("gleam build".to_string());
        }
        if has_glob_ext(wd, "csproj") || wd.join("global.json").exists() {
            return Ok("dotnet build -c Release".to_string());
        }
        if wd.join("CMakeLists.txt").exists() {
            return Ok("cmake -S . -B build && cmake --build build --config Release".to_string());
        }
        if has_glob_ext(wd, "c")
            || has_glob_ext(wd, "cpp")
            || has_glob_ext(wd, "cc")
            || has_glob_ext(wd, "cxx")
        {
            return Ok("echo set [build].build_cmd for c/c++ project".to_string());
        }
        if has_glob_ext(wd, "sh") {
            return Ok("echo shell scripts project - no compile build".to_string());
        }
        if wd.join("index.html").exists() {
            return Ok("echo static site - no compile build".to_string());
        }
        if wd.join("pyproject.toml").exists() {
            return Ok("python -m build".to_string());
        }
        bail!("unable to infer build command; set [build].build_cmd")
    }

    fn infer_output_dir(&self, wd: &Path) -> Option<String> {
        if [
            "next.config.js",
            "next.config.mjs",
            "next.config.cjs",
            "next.config.ts",
        ]
        .iter()
        .any(|f| wd.join(f).exists())
        {
            return Some(".next".to_string());
        }
        if wd.join("nuxt.config.ts").exists() || wd.join("nuxt.config.js").exists() {
            return Some(".output".to_string());
        }
        if wd.join("manage.py").exists() {
            return Some("staticfiles".to_string());
        }
        if wd.join("artisan").exists() {
            return Some("public".to_string());
        }
        if wd.join("target").exists() && wd.join("Cargo.toml").exists() {
            return Some("target/release".to_string());
        }
        if wd.join("target").exists() && wd.join("pom.xml").exists() {
            return Some("target".to_string());
        }
        if wd.join("build").exists()
            && (wd.join("build.gradle").exists() || wd.join("build.gradle.kts").exists())
        {
            return Some("build/libs".to_string());
        }
        if wd.join("bin").exists() && wd.join("go.mod").exists() {
            return Some("bin".to_string());
        }
        if has_glob_ext(wd, "csproj") {
            return Some("bin/Release".to_string());
        }
        if wd.join("CMakeLists.txt").exists() {
            return Some("build".to_string());
        }
        if has_glob_ext(wd, "sh") || wd.join("index.html").exists() {
            return Some(".".to_string());
        }
        if wd.join("dist").exists() {
            return Some("dist".to_string());
        }
        None
    }

    fn execute_step<F>(&self, ctx: &BuildContext, name: &str, run: F) -> Result<Step>
    where
        F: FnOnce(&Self, &BuildContext, &mut Step) -> Result<()>,
    {
        let mut step = Step::new(name);
        step.status = StepStatus::Running;
        events::step_started(&step);
        log::step_started(name);

        let before = metrics::sample(&ctx.work_dir).ok();
        let started = Instant::now();
        let result = run(self, ctx, &mut step);
        step.duration_secs = Some(started.elapsed().as_secs_f32());
        let after = metrics::sample(&ctx.work_dir).ok();
        if let (Some(a), Some(b)) = (before, after) {
            step.resources = Some(metrics::to_step_resources(a, b));
        }

        match result {
            Ok(()) => {
                step.status = StepStatus::Completed;
                events::step_completed(&step);
                log::step_completed(&step);
                Ok(step)
            }
            Err(err) => {
                step.status = StepStatus::Failed;
                step.push_log(format!("error: {err:#}"));
                events::step_failed(&step, &err.to_string());
                log::step_failed(&step);
                Err(err)
            }
        }
    }

    fn step_source(&self, ctx: &BuildContext, step: &mut Step) -> Result<()> {
        if let Some(src) = &self.config.source {
            step.push_log(format!(
                "clone repo {}",
                shell::redact_command_for_log(&src.repo)
            ));
            git::clone(&src.repo, &ctx.work_dir)?;
            if let Some(commit) = &src.commit {
                git::fetch_and_checkout(&ctx.work_dir, commit)?;
            } else if let Some(branch) = &src.branch {
                git::checkout(&ctx.work_dir, branch)?;
            }
        } else if self.in_place {
            step.push_log(format!(
                "using current workspace in place {}",
                ctx.work_dir.display()
            ));
        } else {
            let cwd = std::env::current_dir()?;
            step.push_log(format!("copy local workspace {}", cwd.display()));
            artifacts::copy_workspace(&cwd, &ctx.work_dir)?;
        }
        Ok(())
    }

    fn step_deploy(
        &self,
        ctx: &BuildContext,
        step: &mut Step,
        output_dir: Option<&str>,
    ) -> Result<artifacts::PublishResult> {
        let output_src = self.output_src(ctx, output_dir);
        if !output_src.exists() {
            return Err(BuildError::MissingOutput(output_src.display().to_string()).into());
        }
        let targets = self
            .config
            .deploy
            .targets
            .clone()
            .unwrap_or_else(|| vec!["directory".to_string()]);
        let registry_cache_ref = self
            .config
            .cache
            .as_ref()
            .and_then(|c| c.registry_ref.clone());
        let container_options = artifacts::ContainerPublishOptions {
            platforms: self
                .config
                .deploy
                .container_platforms
                .clone()
                .unwrap_or_default(),
            push: self.config.deploy.push_container.unwrap_or(false),
            registry_cache_ref,
            rebase_base: self.config.deploy.rebase_base.clone(),
        };
        let published = artifacts::publish(
            &output_src,
            &ctx.work_dir,
            &ctx.artifact_dir,
            &self.config.project.name,
            &targets,
            self.config.deploy.container_image.as_deref(),
            Some(&container_options),
            self.config.deploy.kubernetes.as_ref(),
        )?;
        step.push_log(format!("artifact root {}", published.root.display()));
        for w in &published.warnings {
            step.push_log(format!("warning {w}"));
        }
        for out in &published.outputs {
            let size_bytes = path_size_bytes(out).unwrap_or_default();
            let label = if out.is_dir() {
                "Generated bundle"
            } else {
                "Generated artifact"
            };
            step.push_log(format!(
                "{}: {} ({})",
                label,
                out.display(),
                human_size(size_bytes)
            ));
            log::kv(
                label,
                &format!("{} ({})", out.display(), human_size(size_bytes)),
            );
        }
        let gc = artifacts::garbage_collect_artifacts(
            &ctx.artifact_dir,
            &published.root,
            self.config.deploy.gc.as_ref(),
        )?;
        if self
            .config
            .deploy
            .gc
            .as_ref()
            .and_then(|g| g.enabled)
            .unwrap_or(false)
        {
            step.push_log(format!(
                "garbage collection removed={} kept={}",
                gc.removed_dirs, gc.kept_dirs
            ));
        }
        Ok(published)
    }
}

fn detect_source_identity(cfg: &BuildConfig, work_dir: &Path) -> Value {
    let configured_repo = cfg.source.as_ref().map(|s| s.repo.clone());
    let configured_branch = cfg.source.as_ref().and_then(|s| s.branch.clone());
    let configured_commit = cfg.source.as_ref().and_then(|s| s.commit.clone());

    let cwd = std::env::current_dir().ok();
    let git_repo = git_output(work_dir, &["remote", "get-url", "origin"]).or_else(|| {
        cwd.as_ref()
            .and_then(|p| git_output(p, &["remote", "get-url", "origin"]))
    });
    let git_branch = git_output(work_dir, &["rev-parse", "--abbrev-ref", "HEAD"]).or_else(|| {
        cwd.as_ref()
            .and_then(|p| git_output(p, &["rev-parse", "--abbrev-ref", "HEAD"]))
    });
    let git_commit = git_output(work_dir, &["rev-parse", "HEAD"]).or_else(|| {
        cwd.as_ref()
            .and_then(|p| git_output(p, &["rev-parse", "HEAD"]))
    });

    let source_type = if cfg.source.is_some() {
        "git"
    } else if git_repo.is_some() {
        "local-git"
    } else {
        "local"
    };

    serde_json::json!({
        "type": source_type,
        "repo": configured_repo.or(git_repo),
        "branch": configured_branch.or(git_branch),
        "commit": configured_commit.or(git_commit),
    })
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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

fn is_tar_artifact(path: &Path) -> bool {
    let normalized = path.to_string_lossy().to_lowercase();
    normalized.ends_with(".tar") || normalized.ends_with(".tar.gz") || normalized.ends_with(".tgz")
}

fn log_build_overview(steps: &[Step], total_secs: f32, cache_metrics: &CacheMetrics) {
    let mut peak_cpu = 0.0f32;
    let mut peak_mem = 0u64;
    let mut disk_written = 0u64;
    for step in steps {
        if let Some(r) = step.resources {
            if r.cpu_percent > peak_cpu {
                peak_cpu = r.cpu_percent;
            }
            if r.memory_mb > peak_mem {
                peak_mem = r.memory_mb;
            }
            disk_written = disk_written.saturating_add(r.disk_mb);
        }
    }

    let cache_restored = if cache_metrics.hits > 0 { "yes" } else { "no" };
    let layer_total = cache_metrics.hits + cache_metrics.misses;
    let layers_reused = if layer_total > 0 {
        format!("{}/{}", cache_metrics.hits, layer_total)
    } else {
        "0/0".to_string()
    };

    log::section("Build Overview");
    log::kv("Total build duration", &format!("{total_secs:.1}s"));
    log::kv("Cache restored", cache_restored);
    log::kv("Layers reused", &layers_reused);
    if let Some((reused, updated)) = incremental_counts_from_steps(steps) {
        log::kv(
            "Preparing incremental build data",
            &format!("reused={} updated={}", reused, updated),
        );
    }
    if let Some(cache_size_mb) = cache_size_from_steps(steps) {
        log::kv("Cache size", &format!("{cache_size_mb:.1}MB"));
    }
    log::kv("Peak memory", &format!("{peak_mem}MB"));
    log::kv("Peak CPU", &format!("{peak_cpu:.1}%"));
    log::kv("Disk written", &format!("{disk_written}MB"));
}

fn incremental_counts_from_steps(steps: &[Step]) -> Option<(u64, u64)> {
    for step in steps {
        for line in &step.logs {
            if let Some(rest) = line.strip_prefix("incremental_summary ") {
                let mut reused = None;
                let mut updated = None;
                for token in rest.split_whitespace() {
                    if let Some(v) = token.strip_prefix("reused=") {
                        reused = v.parse::<u64>().ok();
                    } else if let Some(v) = token.strip_prefix("updated=") {
                        updated = v.parse::<u64>().ok();
                    }
                }
                if let (Some(r), Some(u)) = (reused, updated) {
                    return Some((r, u));
                }
            }
        }
    }
    None
}

fn cache_size_from_steps(steps: &[Step]) -> Option<f64> {
    for step in steps {
        for line in &step.logs {
            if let Some(v) = line.strip_prefix("cache_size_mb=") {
                if let Ok(parsed) = v.parse::<f64>() {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

fn path_size_bytes(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    if path.is_file() {
        return Ok(fs::metadata(path)?.len());
    }
    dir_size_bytes(path)
}

fn dir_size_bytes(root: &Path) -> Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1}GB", b / GB)
    } else if b >= MB {
        format!("{:.1}MB", b / MB)
    } else if b >= KB {
        format!("{:.1}KB", b / KB)
    } else {
        format!("{bytes}B")
    }
}

fn parse_node_major(v: &str) -> Option<u32> {
    v.trim()
        .trim_start_matches('v')
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn read_package_json(path: &Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

fn has_build_script(pkg: &Value) -> bool {
    pkg.get("scripts")
        .and_then(|s| s.get("build"))
        .and_then(Value::as_str)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

fn has_next_dependency(pkg: &Value) -> bool {
    for field in ["dependencies", "devDependencies"] {
        if pkg
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

fn remove_flag(cmd: &str, flag: &str) -> String {
    cmd.split_whitespace()
        .filter(|t| *t != flag)
        .collect::<Vec<_>>()
        .join(" ")
}

fn push_unique(values: &mut Vec<String>, candidate: String) {
    let n = candidate.trim().to_string();
    if !n.is_empty() && !values.iter().any(|v| v == &n) {
        values.push(n);
    }
}

fn file_contains(path: &Path, needle: &str) -> bool {
    if !path.exists() {
        return false;
    }
    fs::read_to_string(path)
        .map(|v| v.to_lowercase().contains(&needle.to_lowercase()))
        .unwrap_or(false)
}

impl BuildEngine {
    fn resolve_language(&self, ctx: &BuildContext) -> Option<String> {
        if let Some(lang) = &self.config.project.language {
            return Some(normalize_language(lang));
        }

        let wd = &ctx.work_dir;
        if wd.join("package.json").exists()
            || wd.join("pnpm-lock.yaml").exists()
            || wd.join("yarn.lock").exists()
            || wd.join("package-lock.json").exists()
        {
            return Some("nodejs".to_string());
        }
        if wd.join("requirements.txt").exists()
            || wd.join("pyproject.toml").exists()
            || wd.join("manage.py").exists()
        {
            return Some("python".to_string());
        }
        if wd.join("Gemfile").exists() || wd.join("Rakefile").exists() {
            return Some("ruby".to_string());
        }
        if wd.join("go.mod").exists() {
            return Some("go".to_string());
        }
        if wd.join("pom.xml").exists()
            || wd.join("build.gradle").exists()
            || wd.join("build.gradle.kts").exists()
        {
            return Some("java".to_string());
        }
        if wd.join("composer.json").exists() {
            return Some("php".to_string());
        }
        if wd.join("Cargo.toml").exists() {
            return Some("rust".to_string());
        }
        if wd.join("deno.json").exists() || wd.join("deno.jsonc").exists() {
            return Some("deno".to_string());
        }
        if wd.join("mix.exs").exists() {
            return Some("elixir".to_string());
        }
        if wd.join("gleam.toml").exists() {
            return Some("gleam".to_string());
        }
        if wd.join("global.json").exists()
            || wd.join(".csproj").exists()
            || has_glob_ext(wd, "csproj")
        {
            return Some("dotnet".to_string());
        }
        if wd.join("CMakeLists.txt").exists()
            || has_glob_ext(wd, "c")
            || has_glob_ext(wd, "cpp")
            || has_glob_ext(wd, "cc")
            || has_glob_ext(wd, "cxx")
        {
            return Some("c_cpp".to_string());
        }
        if has_glob_ext(wd, "sh") {
            return Some("shell".to_string());
        }
        if wd.join("index.html").exists() && !wd.join("package.json").exists() {
            return Some("static".to_string());
        }
        None
    }
}

fn normalize_language(language: &str) -> String {
    match language.to_lowercase().as_str() {
        "node" | "nodejs" => "nodejs".to_string(),
        "python" | "py" => "python".to_string(),
        "ruby" | "rb" => "ruby".to_string(),
        "go" | "golang" => "go".to_string(),
        "java" | "jvm" => "java".to_string(),
        "php" => "php".to_string(),
        "rust" | "rs" => "rust".to_string(),
        "shell" | "sh" | "bash" => "shell".to_string(),
        "c" | "cpp" | "c++" | "c_cpp" | "cc" => "c_cpp".to_string(),
        "gleam" => "gleam".to_string(),
        "elixir" | "ex" | "exs" => "elixir".to_string(),
        "deno" => "deno".to_string(),
        "dotnet" | ".net" | "net" | "csharp" | "c#" => "dotnet".to_string(),
        "static" | "static_site" => "static".to_string(),
        other => other.to_string(),
    }
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

fn explain_exit_code(exit: Option<i32>) -> Option<&'static str> {
    match exit? {
        -1073740940 => Some(
            "windows STATUS_HEAP_CORRUPTION (0xC0000374): process crashed; often tool/runtime corruption, native addon mismatch, or bad cache",
        ),
        -1073741819 => Some(
            "windows STATUS_ACCESS_VIOLATION (0xC0000005): process crashed with invalid memory access",
        ),
        -1073741515 => Some(
            "windows STATUS_DLL_NOT_FOUND (0xC0000135): missing runtime DLL/dependency",
        ),
        -1073740791 => Some(
            "windows STATUS_STACK_BUFFER_OVERRUN (0xC0000409): process aborted due to stack corruption",
        ),
        137 => Some("process killed (likely OOM or SIGKILL)"),
        139 => Some("segmentation fault (SIGSEGV)"),
        _ => None,
    }
}
