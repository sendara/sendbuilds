use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::runtime::shell;

#[derive(Debug, Clone, Serialize, Default)]
pub struct UnusedDepsReport {
    pub language: String,
    pub scanner: String,
    pub scanned: bool,
    pub unused: Vec<String>,
    pub notes: Vec<String>,
}

pub fn run(
    language: &str,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<UnusedDepsReport> {
    let normalized = normalize_language(language);
    let mut report = UnusedDepsReport {
        language: normalized.clone(),
        ..Default::default()
    };

    match normalized.as_str() {
        "nodejs" => run_node_depcheck(&mut report, work_dir, env, sandbox)?,
        "python" => run_python_extra_reqs(&mut report, work_dir, env, sandbox)?,
        "ruby" => run_ruby_debride(&mut report, work_dir, env, sandbox)?,
        "go" => run_go_unused(&mut report, work_dir, env, sandbox)?,
        "java" => run_maven_unused(&mut report, work_dir, env, sandbox)?,
        "php" => run_composer_unused(&mut report, work_dir, env, sandbox)?,
        "rust" => run_cargo_udeps(&mut report, work_dir, env, sandbox)?,
        "dotnet" => run_dotnet_unused(&mut report, work_dir, env, sandbox)?,
        "elixir" => run_mix_unused(&mut report, work_dir, env, sandbox)?,
        _ => {
            report.notes.push(format!(
                "unused dependency detection not configured for language={}",
                normalized
            ));
        }
    }

    if report.scanner.is_empty() {
        report.scanner = "none".to_string();
    }
    Ok(report)
}

pub fn to_build_logs(report: &UnusedDepsReport) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "unused-deps summary language={} scanner={} scanned={} unused={}",
        report.language,
        report.scanner,
        report.scanned,
        report.unused.len()
    ));
    if !report.unused.is_empty() {
        out.push(format!(
            "unused-deps packages {}",
            report.unused.iter().take(25).cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    for note in &report.notes {
        out.push(format!("note {note}"));
    }
    out
}

fn run_node_depcheck(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("depcheck", &["--version"]) {
        report.notes.push("depcheck not available; install with npm i -g depcheck or add to devDependencies".to_string());
        return Ok(());
    }
    report.scanner = "depcheck".to_string();
    let run = shell::run_allow_failure("depcheck --json", work_dir, env, sandbox)?;
    report.scanned = run.success;
    if let Some(raw) = collect_json(&run.logs) {
        if let Ok(val) = serde_json::from_str::<Value>(&raw) {
            let mut unused = Vec::new();
            if let Some(arr) = val.get("dependencies").and_then(Value::as_array) {
                for v in arr.iter().filter_map(Value::as_str) {
                    unused.push(v.to_string());
                }
            }
            if let Some(arr) = val.get("devDependencies").and_then(Value::as_array) {
                for v in arr.iter().filter_map(Value::as_str) {
                    unused.push(v.to_string());
                }
            }
            unused.sort();
            unused.dedup();
            report.unused = unused;
        }
    }
    if !run.success && report.unused.is_empty() {
        report.notes.push(format!(
            "depcheck exited with {:?}; no unused dependencies parsed",
            run.exit_code
        ));
    }
    Ok(())
}

fn run_python_extra_reqs(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("pip-extra-reqs", &["--version"]) {
        report
            .notes
            .push("pip-extra-reqs not available; install pip-check-reqs".to_string());
        return Ok(());
    }
    report.scanner = "pip-extra-reqs".to_string();
    let run = shell::run_allow_failure("pip-extra-reqs .", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn run_ruby_debride(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("debride", &["--version"]) {
        report
            .notes
            .push("debride not available; install with gem install debride".to_string());
        return Ok(());
    }
    report.scanner = "debride".to_string();
    let run = shell::run_allow_failure("debride", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn run_go_unused(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if command_available("golangci-lint", &["--version"]) {
        report.scanner = "golangci-lint".to_string();
        let run = shell::run_allow_failure(
            "golangci-lint run --out-format json --disable-all --enable=unused",
            work_dir,
            env,
            sandbox,
        )?;
        report.scanned = run.success;
        report.unused = extract_named_values_from_json(&run.logs, &["Text", "Message"]);
        return Ok(());
    }
    report.notes.push(
        "unused deps for Go requires tooling (e.g. golangci-lint); none detected".to_string(),
    );
    Ok(())
}

fn run_maven_unused(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("mvn", &["-v"]) {
        report.notes.push("mvn not available".to_string());
        return Ok(());
    }
    report.scanner = "mvn dependency:analyze".to_string();
    let run = shell::run_allow_failure(
        "mvn -q -DskipTests dependency:analyze",
        work_dir,
        env,
        sandbox,
    )?;
    report.scanned = run.success;
    report.unused = parse_maven_unused(&run.logs);
    Ok(())
}

fn run_composer_unused(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("composer-unused", &["--version"]) {
        report
            .notes
            .push("composer-unused not available; install composer-unused/composer-unused".to_string());
        return Ok(());
    }
    report.scanner = "composer-unused".to_string();
    let run = shell::run_allow_failure("composer-unused --no-interaction", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn run_cargo_udeps(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("cargo", &["udeps", "--version"]) {
        report
            .notes
            .push("cargo-udeps not available; install with cargo install cargo-udeps".to_string());
        return Ok(());
    }
    report.scanner = "cargo udeps".to_string();
    let run = shell::run_allow_failure("cargo udeps", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn run_dotnet_unused(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("dotnet-unused", &["--version"]) {
        report.notes.push("dotnet-unused not available; install dotnet tool dotnet-unused".to_string());
        return Ok(());
    }
    report.scanner = "dotnet-unused".to_string();
    let run = shell::run_allow_failure("dotnet-unused", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn run_mix_unused(
    report: &mut UnusedDepsReport,
    work_dir: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<()> {
    if !command_available("mix", &["--version"]) {
        report.notes.push("mix not available".to_string());
        return Ok(());
    }
    report.scanner = "mix deps.unlock --unused".to_string();
    let run = shell::run_allow_failure("mix deps.unlock --unused", work_dir, env, sandbox)?;
    report.scanned = run.success;
    report.unused = parse_lines_as_packages(&run.logs);
    Ok(())
}

fn command_available(bin: &str, args: &[&str]) -> bool {
    std::process::Command::new(bin)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn parse_lines_as_packages(logs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for line in logs {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cleaned = line
            .trim_start_matches(|c: char| c == '-' || c == '*' || c.is_whitespace())
            .trim();
        if cleaned.is_empty() {
            continue;
        }
        if cleaned.len() > 2 {
            out.push(cleaned.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn parse_maven_unused(logs: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut capture = false;
    for line in logs {
        let line = line.trim();
        if line.contains("Unused declared dependencies found") {
            capture = true;
            continue;
        }
        if capture {
            if line.is_empty() {
                break;
            }
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 2 {
                out.push(parts[1].to_string());
            } else {
                out.push(line.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn extract_named_values_from_json(logs: &[String], keys: &[&str]) -> Vec<String> {
    let Some(raw) = collect_json(logs) else {
        return Vec::new();
    };
    let Ok(val) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    collect_named_values(&val, keys)
}

fn collect_named_values(root: &Value, keys: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    collect_named_values_recursive(root, keys, &mut out);
    out.sort();
    out.dedup();
    out.into_iter().take(25).collect()
}

fn collect_named_values_recursive(root: &Value, keys: &[&str], out: &mut Vec<String>) {
    match root {
        Value::Object(map) => {
            for (k, v) in map {
                if keys.iter().any(|needle| k.eq_ignore_ascii_case(needle)) {
                    if let Some(s) = v.as_str() {
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                }
                collect_named_values_recursive(v, keys, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_named_values_recursive(item, keys, out);
            }
        }
        _ => {}
    }
}

fn collect_json(logs: &[String]) -> Option<String> {
    let mut body = String::new();
    for line in logs {
        if let Some(rest) = line.strip_prefix("stdout: ") {
            body.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("stderr: ") {
            body.push_str(rest);
        } else {
            body.push_str(line);
        }
        body.push('\n');
    }
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(body[start..=end].to_string())
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
        "dotnet" | ".net" | "net" | "csharp" | "c#" => "dotnet".to_string(),
        "elixir" | "ex" | "exs" => "elixir".to_string(),
        other => other.to_string(),
    }
}
