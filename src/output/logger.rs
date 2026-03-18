use chrono::Local;
use colored::Colorize;

use crate::core::{Step, StepStatus};

const SLOW_STEP_SECS: f32 = 10.0;

const BANNER: &str = r#" _____  ______ _   _ _____  ____  _    _ _____ _      _____  
/ ____|/ ____| \ | |  __ \|  _ \| |  | |_   _| |    |  __ \ 
| (___ | |    |  \| | |  | | |_) | |  | | | | | |    | |  | |
 \___ \| |    | . ` | |  | |  _ <| |  | | | | | |    | |  | |
 ____) | |____| |\  | |__| | |_) | |__| |_| |_| |____| |__| |
|_____/ \_____|_| \_|_____/|____/ \____/|_____|______|_____/ "#;

fn ts() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn emit(level: &str, msg: &str) {
    let level_padded = format!("{level:<5}");
    let styled_level = match level {
        "INFO" => level_padded.blue().bold(),
        "WARN" => level_padded.yellow().bold(),
        "ERROR" => level_padded.red().bold(),
        _ => level_padded.normal(),
    };
    println!("[{}] {} | {}", ts().dimmed(), styled_level, msg);
}

fn emit_security(level: &str, msg: &str) {
    let level_padded = format!("{level:<7}");
    let styled_level = match level {
        "ALERT" => level_padded.red().bold(),
        "WARN" => level_padded.yellow().bold(),
        _ => level_padded.cyan().bold(),
    };
    println!(
        "[{}] | SECURITY | {} | {}",
        ts().dimmed(),
        styled_level,
        msg
    );
}

fn classify_line(line: &str) -> &'static str {
    let lower = line.trim().to_lowercase();
    if lower.contains("severity: critical")
        || lower.contains(" security policy violation")
        || lower.starts_with("error")
        || lower.starts_with("fatal")
        || lower.contains(" failed")
        || lower.contains(" failure")
    {
        "ERROR"
    } else if lower.contains("severity: high")
        || lower.starts_with("warn")
        || lower.contains(" warning")
        || lower.contains("deprecated")
        || lower.contains("vulnerable")
    {
        "WARN"
    } else {
        "INFO"
    }
}

pub fn header(msg: &str) {
    println!("{}", BANNER.cyan().bold());
    emit("INFO", msg);
    emit("INFO", "Build started");
}

pub fn section(msg: &str) {
    emit("INFO", msg);
}

pub fn pipe(line: &str) {
    for segment in line.lines() {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        emit(classify_line(trimmed), trimmed);
    }
}

pub fn security(line: &str) {
    for segment in line.lines() {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        let level = if lower.contains("severity: critical")
            || lower.contains("severity: high")
            || lower.contains("severity: moderate")
            || lower.contains("security policy result=failed")
            || lower.contains("security policy violation")
        {
            "ALERT"
        } else if lower.contains("vulnerabilities:")
            || lower.starts_with("- ")
            || lower.contains("security finding")
        {
            "WARN"
        } else {
            "INFO"
        };
        emit_security(level, trimmed);
    }
}

pub fn kv(key: &str, val: &str) {
    emit("INFO", &format!("{key}: {val}"));
}

pub fn ok(msg: &str) {
    emit("INFO", msg);
}

pub fn fail(msg: &str) {
    emit("ERROR", msg);
}

pub fn step_started(name: &str) {
    emit("INFO", &format!("{}...", friendly_step(name)));
}

pub fn step_completed(step: &Step) {
    let secs = step.duration_secs.unwrap_or_default();
    ok(&format!(
        "{} complete ({secs:.1}s)",
        friendly_step(&step.name)
    ));
}

pub fn step_failed(step: &Step) {
    let secs = step.duration_secs.unwrap_or_default();
    fail(&format!(
        "{} failed ({secs:.1}s)",
        friendly_step(&step.name)
    ));
}

pub fn steps_summary(steps: &[Step]) {
    section("Build Summary");
    section("────────────────────────────────");
    for step_data in steps {
        let duration = step_data.duration_secs.unwrap_or_default();
        let slow = if duration >= SLOW_STEP_SECS {
            " [slow]"
        } else {
            ""
        };
        let label = friendly_step(&step_data.name);
        let dots = if label.len() < 34 {
            ".".repeat(34 - label.len())
        } else {
            ".".to_string()
        };
        let msg = format!(
            "{label}{dots} {} ({duration:.1}s){slow}",
            step_data.status.as_str()
        );
        match step_data.status {
            StepStatus::Failed => emit("ERROR", &msg),
            StepStatus::Running => emit("WARN", &msg),
            _ => emit("INFO", &msg),
        }
    }

    let warnings = collect_warnings(steps);
    if !warnings.is_empty() {
        section("Warnings:");
        for warning in warnings {
            emit("WARN", &format!("- {warning}"));
        }
    }
}

fn collect_warnings(steps: &[Step]) -> Vec<String> {
    let mut out = Vec::new();
    for step in steps {
        for line in &step.logs {
            let lower = line.to_lowercase();
            if lower.starts_with("warning ")
                || lower.contains(" warning")
                || lower.contains("deprecated")
                || lower.contains("vulnerable")
            {
                out.push(line.trim().to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out.into_iter().take(10).collect()
}

fn friendly_step(name: &str) -> String {
    match name {
        "source" => "Preparing source".to_string(),
        "detect-build-config" => "Detecting build configuration".to_string(),
        "compatibility-check" => "Running compatibility checks".to_string(),
        "incremental-prepare" => "Preparing incremental build data".to_string(),
        "install" => "Installing dependencies".to_string(),
        "unused-deps" => "Detecting unused dependencies".to_string(),
        "security-first" => "Running security-first checks".to_string(),
        "security-scan" => "Running security scan".to_string(),
        "deps-cache-save" => "Saving dependency cache".to_string(),
        "build" => "Building project".to_string(),
        "deploy" => "Generating artifacts".to_string(),
        "sign-artifacts" => "Signing artifacts".to_string(),
        "cache-state-save" => "Saving cache state".to_string(),
        "build-metrics" => "Writing build metrics".to_string(),
        "cnb-lifecycle" => "Writing CNB lifecycle metadata".to_string(),
        _ => format!("Running {name}"),
    }
}
