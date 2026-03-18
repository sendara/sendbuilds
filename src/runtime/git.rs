use anyhow::{bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub fn clone(repo: &str, dest: &Path) -> Result<()> {
    let normalized = normalize_repo(repo);
    if normalized.is_empty() {
        bail!("git clone failed: repository is empty");
    }

    ensure_clone_destination(dest)?;
    let dest_arg = dest.display().to_string();
    let normalized_for_action = normalized.clone();

    run_git(
        None,
        ["clone", "--depth", "1"],
        [normalized.as_str(), dest_arg.as_str()],
        GitAction::Clone {
            repo: repo.to_string(),
            normalized: normalized_for_action,
        },
    )
    .map(|_| ())
}

pub fn checkout(dest: &Path, target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        bail!("git checkout failed: target is empty");
    }

    ensure_git_worktree(dest)?;

    if local_ref_exists(dest, target)? {
        checkout_detached(dest, target)
            .with_context(|| format!("git checkout failed: unable to use local ref `{target}`"))?;
        return Ok(());
    }

    match fetch_remote_branch(dest, target) {
        Ok(()) => {
            checkout_tracking_branch(dest, target).with_context(|| {
                format!("git checkout failed: unable to switch to branch `{target}`")
            })?;
            Ok(())
        }
        Err(branch_err) => {
            if let Ok(()) = fetch_ref(dest, target) {
                checkout_detached(dest, target).with_context(|| {
                    format!("git checkout failed: unable to detach to ref `{target}`")
                })?;
                return Ok(());
            }

            Err(branch_err).with_context(|| {
                format!("git checkout failed: `{target}` was not found locally or on origin")
            })
        }
    }
}

pub fn fetch_and_checkout(dest: &Path, commit: &str) -> Result<()> {
    let commit = commit.trim();
    if commit.is_empty() {
        bail!("git fetch failed: commit is empty");
    }

    ensure_git_worktree(dest)?;

    if !local_ref_exists(dest, commit)? {
        fetch_ref(dest, commit).or_else(|specific_err| {
            fetch_all_history(dest).with_context(|| {
                format!(
                    "git fetch failed: unable to download commit `{commit}` from origin after targeted fetch failed: {specific_err:#}"
                )
            })
        })?;
    }

    checkout_detached(dest, commit)
        .with_context(|| format!("git checkout failed: unable to switch to commit `{commit}`"))
}

fn checkout_detached(dest: &Path, target: &str) -> Result<()> {
    run_git(
        Some(dest),
        ["checkout", "--detach"],
        [target],
        GitAction::Checkout {
            target: target.to_string(),
        },
    )
    .map(|_| ())
}

fn checkout_tracking_branch(dest: &Path, branch: &str) -> Result<()> {
    run_git(
        Some(dest),
        ["checkout", "-B", branch, "--track"],
        [format!("origin/{branch}")],
        GitAction::Checkout {
            target: branch.to_string(),
        },
    )
    .map(|_| ())
}

fn fetch_remote_branch(dest: &Path, branch: &str) -> Result<()> {
    let refspec = format!("refs/heads/{branch}:refs/remotes/origin/{branch}");
    run_git(
        Some(dest),
        ["fetch", "--depth", "1", "origin"],
        [refspec],
        GitAction::Fetch {
            target: branch.to_string(),
        },
    )
    .map(|_| ())
}

fn fetch_ref(dest: &Path, target: &str) -> Result<()> {
    run_git(
        Some(dest),
        ["fetch", "--depth", "1", "origin"],
        [target],
        GitAction::Fetch {
            target: target.to_string(),
        },
    )
    .map(|_| ())
}

fn fetch_all_history(dest: &Path) -> Result<()> {
    let unshallow = run_git(
        Some(dest),
        ["fetch", "--unshallow", "--tags", "origin"],
        std::iter::empty::<&str>(),
        GitAction::Fetch {
            target: "full history".to_string(),
        },
    );
    if unshallow.is_ok() {
        return Ok(());
    }

    run_git(
        Some(dest),
        ["fetch", "--tags", "origin"],
        std::iter::empty::<&str>(),
        GitAction::Fetch {
            target: "full history".to_string(),
        },
    )
    .map(|_| ())
}

fn ensure_clone_destination(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("git clone failed: cant create {}", parent.display()))?;
    }

    if !dest.exists() {
        return Ok(());
    }

    let metadata = fs::metadata(dest)
        .with_context(|| format!("git clone failed: cant inspect {}", dest.display()))?;
    if !metadata.is_dir() {
        bail!(
            "git clone failed: destination exists and is not a directory: {}",
            dest.display()
        );
    }

    let mut entries = fs::read_dir(dest)
        .with_context(|| format!("git clone failed: cant read {}", dest.display()))?;
    if entries.next().is_some() {
        bail!(
            "git clone failed: destination already exists and is not empty: {}",
            dest.display()
        );
    }

    Ok(())
}

fn ensure_git_worktree(dest: &Path) -> Result<()> {
    if !dest.exists() {
        bail!(
            "git operation failed: repository directory does not exist: {}",
            dest.display()
        );
    }
    if !dest.join(".git").exists() {
        bail!(
            "git operation failed: directory is not a git checkout: {}",
            dest.display()
        );
    }
    Ok(())
}

fn local_ref_exists(dest: &Path, target: &str) -> Result<bool> {
    let output = git_command()
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{target}^{{commit}}"))
        .current_dir(dest)
        .output()
        .with_context(|| format!("failed to run git rev-parse for `{target}`"))?;
    Ok(output.status.success())
}

fn run_git<I, S, J, T>(
    cwd: Option<&Path>,
    fixed_args: I,
    trailing_args: J,
    action: GitAction,
) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    J: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    let mut command = git_command();
    command.args(fixed_args);
    command.args(trailing_args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let output = command
        .output()
        .with_context(|| format!("failed to spawn git for {}", action.describe()))?;
    if output.status.success() {
        return Ok(output);
    }

    bail!("{}", format_git_failure(&action, &output))
}

fn git_command() -> Command {
    let mut command = Command::new("git");
    // Builds should reuse existing non-interactive credentials only.
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GCM_INTERACTIVE", "Never");
    command.stdin(Stdio::null());
    command
}

fn format_git_failure(action: &GitAction, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let details =
        first_non_empty(&stderr, &stdout).unwrap_or("git returned a non-zero exit status");
    let hint = classify_git_hint(action, &stderr, &stdout);
    let mut message = format!(
        "{} (exit={})",
        action.describe(),
        output.status.code().unwrap_or(-1)
    );
    message.push_str(&format!(": {details}"));
    if let Some(hint) = hint {
        message.push_str(&format!(". {hint}"));
    }
    message
}

fn classify_git_hint(action: &GitAction, stderr: &str, stdout: &str) -> Option<String> {
    let lower = format!("{stderr}\n{stdout}").to_lowercase();

    if lower.contains("terminal prompts disabled")
        || lower.contains("could not read username")
        || lower.contains("authentication failed")
        || lower.contains("could not authenticate")
        || lower.contains("repository not found")
        || lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("fatal: could not read from remote repository")
    {
        return Some(auth_hint(action));
    }

    if lower.contains("could not resolve host")
        || lower.contains("name or service not known")
        || lower.contains("temporary failure in name resolution")
        || lower.contains("operation timed out")
        || lower.contains("failed to connect")
        || lower.contains("network is unreachable")
    {
        return Some(
            "check network access from the build environment and confirm the remote host is reachable"
                .to_string(),
        );
    }

    if lower.contains("not a git repository")
        || lower.contains("this operation must be run in a work tree")
    {
        return Some("the working directory is not a valid git checkout".to_string());
    }

    if lower.contains("pathspec") && lower.contains("did not match any file") {
        return Some("the requested branch, tag, or commit was not found".to_string());
    }

    if lower.contains("couldn't find remote ref")
        || lower.contains("server does not allow request for unadvertised object")
        || lower.contains("no such remote ref")
    {
        return Some("the requested ref is not available from origin".to_string());
    }

    if lower.contains("destination path") && lower.contains("already exists") {
        return Some("choose an empty destination directory for the clone".to_string());
    }

    if lower.contains("detected dubious ownership") {
        return Some(
            "git refused to use this checkout because of safe.directory protections; trust the directory in global git config before building".to_string(),
        );
    }

    if lower.contains("ssh") && lower.contains("host key verification failed") {
        return Some(
            "the SSH host key is not trusted in this environment; pre-seed known_hosts or use an HTTPS remote"
                .to_string(),
        );
    }

    None
}

fn auth_hint(action: &GitAction) -> String {
    match action {
        GitAction::Clone { repo, normalized } => {
            let trimmed = repo.trim().trim_matches('/');
            if is_owner_repo_shorthand(trimmed) {
                format!(
                    "builds do not prompt for Git login. `{repo}` was expanded to `{normalized}`; if this repo relies on SSH auth, use the full SSH URL instead"
                )
            } else {
                "builds do not prompt for Git login; configure a non-interactive credential helper, SSH remote, or tokenized HTTPS URL".to_string()
            }
        }
        GitAction::Fetch { .. } | GitAction::Checkout { .. } => {
            "builds do not prompt for Git login; make sure the cloned remote can be accessed non-interactively".to_string()
        }
    }
}

fn first_non_empty<'a>(a: &'a str, b: &'a str) -> Option<&'a str> {
    if !a.trim().is_empty() {
        return Some(a.trim());
    }
    if !b.trim().is_empty() {
        return Some(b.trim());
    }
    None
}

fn normalize_repo(repo: &str) -> String {
    let trimmed = repo.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
        || trimmed.starts_with("git@")
        || trimmed.starts_with("ssh://")
        || looks_like_local_path(trimmed)
    {
        return trimmed.to_string();
    }

    let shorthand = trimmed.trim_matches('/');
    if is_owner_repo_shorthand(shorthand) {
        return format!("https://github.com/{shorthand}.git");
    }

    trimmed.to_string()
}

fn looks_like_local_path(input: &str) -> bool {
    input.starts_with("./")
        || input.starts_with("../")
        || input.starts_with(".\\")
        || input.starts_with("..\\")
        || input.starts_with('/')
        || input.starts_with('\\')
        || input.chars().nth(1) == Some(':')
}

fn is_owner_repo_shorthand(input: &str) -> bool {
    let mut parts = input.split('/');
    let Some(owner) = parts.next() else {
        return false;
    };
    let Some(repo) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }

    is_slug(owner) && is_slug(repo)
}

fn is_slug(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

enum GitAction {
    Clone { repo: String, normalized: String },
    Fetch { target: String },
    Checkout { target: String },
}

impl GitAction {
    fn describe(&self) -> String {
        match self {
            GitAction::Clone { repo, normalized } => {
                format!("git clone failed: {repo} (normalized={normalized})")
            }
            GitAction::Fetch { target } => format!("git fetch failed for `{target}`"),
            GitAction::Checkout { target } => format!("git checkout failed for `{target}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        auth_hint, classify_git_hint, ensure_clone_destination, first_non_empty,
        is_owner_repo_shorthand, looks_like_local_path, normalize_repo, GitAction,
    };
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn normalize_repo_keeps_full_url() {
        let repo = "https://github.com/owner/repo.git";
        assert_eq!(normalize_repo(repo), repo);
    }

    #[test]
    fn normalize_repo_expands_owner_repo() {
        assert_eq!(
            normalize_repo("owner/repo"),
            "https://github.com/owner/repo.git"
        );
    }

    #[test]
    fn normalize_repo_keeps_local_paths() {
        assert_eq!(normalize_repo("../repo"), "../repo");
        assert_eq!(normalize_repo("C:\\repo"), "C:\\repo");
        assert!(looks_like_local_path(".\\repo"));
    }

    #[test]
    fn normalize_repo_rejects_invalid_shorthand() {
        assert_eq!(normalize_repo("owner/repo/extra"), "owner/repo/extra");
        assert!(!is_owner_repo_shorthand("owner/repo/extra"));
    }

    #[test]
    fn auth_hint_explains_https_expansion_for_shorthand() {
        let hint = auth_hint(&GitAction::Clone {
            repo: "owner/repo".to_string(),
            normalized: "https://github.com/owner/repo.git".to_string(),
        });
        assert!(hint.contains("do not prompt for Git login"));
        assert!(hint.contains("full SSH URL"));
    }

    #[test]
    fn classify_git_hint_detects_auth_failures() {
        let hint = classify_git_hint(
            &GitAction::Fetch {
                target: "main".to_string(),
            },
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "",
        )
        .expect("auth hint");
        assert!(hint.contains("do not prompt for Git login"));
    }

    #[test]
    fn classify_git_hint_detects_network_failures() {
        let hint = classify_git_hint(
            &GitAction::Fetch {
                target: "main".to_string(),
            },
            "fatal: unable to access 'https://github.com/x/y.git/': Could not resolve host: github.com",
            "",
        )
        .expect("network hint");
        assert!(hint.contains("network access"));
    }

    #[test]
    fn classify_git_hint_detects_missing_refs() {
        let hint = classify_git_hint(
            &GitAction::Checkout {
                target: "missing".to_string(),
            },
            "error: pathspec 'missing' did not match any file(s) known to git",
            "",
        )
        .expect("missing ref hint");
        assert!(hint.contains("not found"));
    }

    #[test]
    fn first_non_empty_prefers_stderr() {
        assert_eq!(first_non_empty(" err ", " out "), Some("err"));
        assert_eq!(first_non_empty("", " out "), Some("out"));
        assert_eq!(first_non_empty(" ", " "), None);
    }

    #[test]
    fn ensure_clone_destination_accepts_missing_directory() {
        let root = temp_dir("git-dest-missing");
        let dest = root.join("repo");
        ensure_clone_destination(&dest).expect("destination should be creatable");
        assert!(root.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ensure_clone_destination_rejects_non_empty_directory() {
        let root = temp_dir("git-dest-non-empty");
        let dest = root.join("repo");
        fs::create_dir_all(&dest).expect("dest dir");
        fs::write(dest.join("file.txt"), b"x").expect("file");
        let err = ensure_clone_destination(&dest).expect_err("should reject non-empty dir");
        assert!(err.to_string().contains("not empty"));
        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("sendbuilds-{prefix}-{unique}"))
    }
}
