# sendbuilds

Build automation CLI with step events, caching, auto-detection, metrics, sandbox controls, artifact signing, and multi-target outputs.

## Supported Language/Frameworks
- Next.js, Rails, Django, Flask, Spring (Maven/Gradle), Laravel, plus generic toolchain detection by language.
- Node.js, Python, Ruby, Go, Java, PHP, Rust, Static Sites, Shell Scripts, C/C++, Gleam, Elixir, Deno, and .NET.

## Run

```bash
sendbuilds build --config sendbuild.toml
```

Unused dependency detection (best-effort, per language):

```bash
sendbuilds build --unused-deps
```

## Workspace / Monorepo

sendbuilds can detect and build workspaces (Node, Rust, Go, Gradle/Maven, .NET, Python).
Workspace mode is opt-in via config or CLI flags; single-project behavior is unchanged.

CLI examples:

```bash
sendbuilds build --workspace --affected
sendbuilds build --workspace --packages api,web
sendbuilds deploy --local --workspace --all
```

Config example:

```toml
[workspace]
enabled = true
root = "."
mode = "auto" # or "explicit"
packages = ["service-a", "service-b"]
build = "affected"

[[package]]
name = "service-a"
path = "packages/service-a"
language = "nodejs"
build_cmd = "pnpm run build"
output_dir = "dist"
targets = ["directory", "container_image"]
container_image = "service-a:latest"
```

## Install

```bash
curl -fsSL https://sendara.cloud/sendbuilds/install.sh | bash
```

## Install from Releases

Release assets are packaged for direct CLI install:
- Linux/macOS: `.tar.gz` (contains `sendbuilds` + `install.sh`)
- Windows: `.zip` (contains `sendbuilds.exe` + `install.ps1`)

Linux/macOS:

```bash
tar -xzf sendbuilds-linux-x86_64.tar.gz
./install.sh
sendbuilds --help
```

Windows PowerShell:

```powershell
Expand-Archive .\sendbuilds-windows-x86_64.zip -DestinationPath .\sendbuilds
.\sendbuilds\install.ps1
sendbuilds.exe --help
```

Windows note:
- `sendbuilds.exe` is a CLI, so double-clicking it in Explorer opens/closes quickly by design.
- Run it from Terminal/PowerShell (`sendbuilds.exe --help`).

## Local development and testing

Build and run the CLI locally:

```bash
cargo build --release
./target/release/sendbuilds --help
./target/release/sendbuilds build --config sendbuild.toml
```

On Windows PowerShell:

```powershell
cargo build --release
.\target\release\sendbuilds.exe --help
.\target\release\sendbuilds.exe build --config sendbuild.toml
```

Run without a release build:

```bash
cargo run -- build --config sendbuild.toml
```

Localhost testing flow for a web app:

1. Build with `sendbuilds` (`build` command).
2. Enter the produced artifact folder under `deploy.artifact_dir`.
3. Start your framework runtime from that artifact (for example `pnpm run start`, `python manage.py runserver`, etc.).

If `[source]` is omitted in `sendbuild.toml`, `sendbuilds` uses the current workspace as source input.

## CLI commands

```bash
sendbuilds build [--config sendbuild.toml] [--events true|false]
sendbuilds build [--config sendbuild.toml] [--in-place] [--events true|false]
sendbuilds build [--config sendbuild.toml] [--reproducible]
sendbuilds build --git <repo> --docker [--branch <name>] [--image <tag>]
sendbuilds deploy [<owner/repo|git-url>] [--local] [--build] [--branch <name>] [--docker] [--target <kubernetes|serverless|tarball|directory|container_image>] [--image <tag>] [--dry-run] [--remote]
sendbuilds debug <build-id> [--config sendbuild.toml]
sendbuilds replay [<build-id>] [--buildid <build-id>] [--time-machine <date>] [--config sendbuild.toml]
sendbuilds rollback [<build-id>] [--to <date>] [--config sendbuild.toml]
sendbuilds artifacts list [--all] [--limit <n>] [--config sendbuild.toml]
sendbuilds artifacts prune [--keep-last <n>] [--max-age <days>] [--config sendbuild.toml]
sendbuilds artifacts download <artifact> [--out <path>] [--config sendbuild.toml]
sendbuilds init [--template <framework>] [--yes]
sendbuilds cache save|restore|clear|status [--config sendbuild.toml]
sendbuilds clean [--all] [--cache-only] [--config sendbuild.toml]
sendbuilds info [--env] [--dependencies] [--config sendbuild.toml]
sendbuilds rebase [--config sendbuild.toml] [--base <image>] [--image <tag>] [--from-image <tag-or-id>]
sendbuilds rebase --git [--repo <git-url>] [--branch <name>] [--base <image>] [--image <tag>]
```

Use `--in-place` to build directly in the current workspace instead of a temp copy (useful for Next.js `pnpm start` expecting `.next` in project root).
If `sendbuild.toml` is missing, `sendbuilds build` automatically falls back to a smart local mode with inferred defaults and in-place build.
For zero-config enterprise mode, use `sendbuilds build --git <repo> --docker`: it auto-generates runtime config, enables security-first checks, auto-generates a local signing key if missing, signs artifacts, emits SBOM/supply-chain metadata, and builds container images even when no Dockerfile exists.
Default storage paths are OS-aware under a `sendbuilds` data root:
- Windows: `%LOCALAPPDATA%/sendbuilds`
- macOS: `~/Library/Application Support/sendbuilds`
- Linux: `$XDG_DATA_HOME/sendbuilds` (or `~/.local/share/sendbuilds`)
Artifacts default to `<data-root>/artifacts` and cache defaults to `<data-root>/cache`.
Within those defaults, sendbuilds auto-namespaces by a stable project storage key (`project-name + repo/path fingerprint`) so duplicate repo names do not collide.
Accepted repo formats include:
- `owner/repo` (for example `notsliver/sendara-landing`)
- `https://github.com/owner/repo`
- `https://github.com/owner/repo.git`

## Build Any Repo In One Command

```bash
sendbuilds deploy owner/repo
```

This is a one-command wrapper around the full build+deploy pipeline (clone, detect, install, build, SBOM/security, image/sign, publish).

Examples:

```bash
sendbuilds deploy owner/repo --docker --target kubernetes
sendbuilds deploy owner/repo --branch main --target tarball
sendbuilds deploy owner/repo --dry-run
sendbuilds deploy --local --target tarball
```

Flags:
- `--docker`: ensure container image output is produced
- `--target`: select publish targets (`kubernetes`, `serverless`, `tarball`, `directory`, `container_image`)
- `--dry-run`: print planned steps without executing
- `--branch`: deploy a specific branch
- `--local`: deploy current workspace (no git clone required)
- `--build`: force rebuild before deploy (skip artifact reuse/start)
- `--remote`: reserved for cloud workers; currently falls back to local execution

Local deploy does not require Docker unless you request `--docker` or `--target container_image`.
When no explicit Docker/target flags are set, `deploy` auto-detects container need (for example from `sendbuild.toml` targets or local Dockerfile presence) and otherwise runs local non-container flow.
In local non-container mode, `deploy` now reuses the latest existing `directory` artifact and starts it automatically when possible; if start detection fails, it exits with guidance instead of rebuilding silently.
When container mode is active (`--docker` or container target), deploy also starts the built image as a local Docker container and prints published port mappings.

## Artifact Management

Inspect and manage build history:

```bash
sendbuilds artifacts list
sendbuilds artifacts list --all
sendbuilds artifacts prune --keep-last 20 --max-age 30
sendbuilds artifacts download 20260306_210619/artifact.tar.gz --out ./downloads
```

Debug a specific build-id:

```bash
sendbuilds debug 20260306_210619
```

Replay deploy from a build-id (starts container artifact if present, otherwise starts directory artifact):

```bash
sendbuilds replay 20260306_210619
```

Time-travel replay by date/time:

```bash
sendbuilds replay --time-machine 2026-03-06
sendbuilds replay --time-machine "2026-03-06 21:25:31"
sendbuilds replay --time-machine "2026-03-06T21:25:31+01:00"
```

Rollback shortcuts:

```bash
sendbuilds rollback 20260306_210619
sendbuilds rollback --to 2026-03-06
```

## Rebase Base Images

Update only the runtime/base layer for a sendbuilds layered Dockerfile without rebuilding everything.

```bash
sendbuilds rebase
```

Use an explicit runtime base and target image:

```bash
sendbuilds rebase --base gcr.io/distroless/nodejs20-debian12 --image my-app:rebased
```

Use a local tag or Docker image ID as cache source:

```bash
sendbuilds rebase --from-image my-app:latest --image my-app:rebased
sendbuilds rebase --from-image sha256:0123456789abcdef --image my-app:rebased
```

If your layered Dockerfile is not in the current directory, pass `--context` and/or `--dockerfile`.

Git mode does a full rebuild (same as `build --git --docker`) and infers the image from the git repo name when not provided:

```bash
sendbuilds rebase --git
sendbuilds rebase --git --repo https://github.com/owner/repo --branch main
```

## Deterministic Reproducible Builds

Use reproducible mode for stricter deterministic behavior:

```bash
sendbuilds build --reproducible
```

In reproducible mode, sendbuilds applies strict defaults:
- Isolated env baseline (`TZ=UTC`, `LANG/LC_ALL=C`, `PYTHONHASHSEED=0`, `SOURCE_DATE_EPOCH`)
- No host env passthrough (`env_from_host` ignored)
- Strict sandbox mode
- Build cache disabled
- No install fallback retries (single deterministic install command)
- Required lock/toolchain files enforced (for example `package-lock.json`/`yarn.lock`/`pnpm-lock.yaml`, `Cargo.lock`, `go.sum`, `Gemfile.lock`, `composer.lock`, `.NET global.json`)

## Minimal config

```toml
[project]
name = "my-app"

[deploy]
artifact_dir = "./artifacts"
```

`source`, `language`, `install_cmd`, `build_cmd`, and `output_dir` are optional. If `[source]` is omitted, `sendbuilds` uses the current folder contents as build input.

## Full config (all features)

```toml
[project]
name = "my-app"
language = "nodejs" # optional override

[source] # optional
repo = "https://github.com/you/my-app.git" # optional
branch = "main" # optional

[build]
install_cmd = "pnpm install --frozen-lockfile --prefer-offline" # optional override
build_cmd = "pnpm run build"                                     # optional override
parallel_build_cmds = ["pnpm run build:client", "pnpm run build:server"] # optional
output_dir = ".next"                                             # optional override

[deploy]
artifact_dir = "./artifacts"
targets = ["directory", "tarball", "serverless_zip", "container_image", "kubernetes"] # optional
container_image = "my-app:latest"                                       # optional
container_platforms = ["linux/amd64", "linux/arm64"]                    # optional (buildx)
push_container = true                                                    # optional (required for multi-arch)
rebase_base = "gcr.io/distroless/nodejs20-debian12"                     # optional runtime rebase base

[deploy.kubernetes] # optional (used by target="kubernetes")
enabled = true
namespace = "default"
replicas = 2
container_port = 3000
service_port = 80
image_pull_policy = "IfNotPresent"

[deploy.gc] # optional automatic artifact garbage collection
enabled = true
keep_last = 5
max_age_days = 14

[output]
events = false # default hidden; set true to show EVENT {...} lines

[cache]
enabled = true
dir = "./artifacts/.sendbuild-cache"
registry_ref = "ghcr.io/your-org/my-app-buildcache" # optional buildx registry cache

[scan]
enabled = true
command = "npm audit --json --omit=dev --audit-level=high"

[security]
enabled = true
fail_on_critical = true
critical_threshold = 0
fail_on_scanner_unavailable = true
generate_sbom = true
auto_distroless = true
# distroless_base = "gcr.io/distroless/nodejs20-debian12"
# rewrite_dockerfile_in_place = false

[sandbox]
enabled = true

[signing]
enabled = true
key_env = "SENDBUILD_SIGNING_KEY"
auto_generate_key = true
key_file = ".sendbuild/signing.key"
generate_provenance = true
cosign = false
# cosign_key = "env://COSIGN_PRIVATE_KEY"

[compatibility]
target_os = "linux"
target_arch = "x86_64"
target_node_major = 20

env_from_host = ["GITHUB_TOKEN", "NPM_TOKEN"]

[env]
NODE_ENV = "production"
API_BASE_URL = "https://api.example.com"
```

## Step events

When `[output].events = true` (or `--events true`), machine-readable step events are emitted to stdout:

```text
EVENT {"type":"STEP_STARTED","channel":"build-step","step":"install","status":"running","timestamp":"..."}
EVENT {"type":"STEP_COMPLETED","channel":"build-step","step":"install","status":"completed","timestamp":"...","duration_ms":1234,"cpu_percent":5.2,"memory_mb":24,"disk_mb":300}
EVENT {"type":"STEP_FAILED","channel":"build-step","step":"build","status":"failed","timestamp":"...","duration_ms":4321,"error":"..."}
```

## Added capabilities

1. Build metrics: per-step duration, status, cache hit/miss accounting, plus `build-metrics.json` in the artifact root.
2. Resource usage tracking: per-step CPU, memory delta, and disk delta in events and step summaries.
3. Sandboxing controls: optional sandbox mode (`[sandbox].enabled`) with basic command blocking and restricted env baseline.
4. Signed artifacts: optional HMAC-SHA256 manifest signing with `artifact-manifest.json` and `artifact-manifest.sig`.
5. Environment variable injection: explicit `[env]` values and `env_from_host` passthrough.
6. Multiple output targets: `directory`, `static_site`, `tarball`, `serverless_zip` / `serverless_function`, `container_image`, and `kubernetes` (Kubernetes manifests).
7. Compatibility checks: optional warnings for target OS/arch/node-major mismatches, including `engines.node` checks when available.
8. Multi-language support: Node.js, Python, Ruby, Go, Java, PHP, Rust, Static Sites, Shell Scripts, C/C++, Gleam, Elixir, Deno, and .NET.
9. Multi-framework support: Next.js, Rails, Django, Flask, Spring (Maven/Gradle), Laravel, plus generic toolchain detection by language.
10. Automatic artifact garbage collection: optional `[deploy.gc]` retention by count and age after each successful deploy.
11. Security-First Buildpack (enterprise): auto-generates SBOM (`sbom.json`), runs vulnerability scans during build, enforces critical-CVE build failure policy, auto-switches Dockerfile final base to distroless, and emits `security-report.json` plus `supply-chain-metadata.json`.
12. CNB lifecycle parity metadata: exports `cnb/lifecycle-contract.json` and `cnb/lifecycle-metadata.json` with standardized detect/analyze/restore/build/export phase mapping.
13. Layered and rebase-ready container output: generated layered Dockerfiles and `.sendbuild-rebase-plan.json` for runtime-base upgrades.
14. Registry-backed container cache/export: optional buildx `--cache-from/--cache-to` via `[cache].registry_ref`.
15. First-class multi-arch container builds: optional `container_platforms` with buildx push flow.
16. Provenance attestations and cosign integration: emits `provenance.intoto.jsonl`; optional cosign sign/attest.
17. Deterministic reproducible mode: `build --reproducible` enforces locked inputs, isolated env, strict sandboxing, and deterministic install behavior.

## Security scan failure details

When legacy `security-scan` fails, the error includes vulnerable package names and actionable suggestions.

Example:

```text
EVENT {"type":"STEP_FAILED","channel":"build-step","step":"security-scan","status":"failed","timestamp":"...","duration_ms":2065,"error":"security scan failed. command=`npm audit --json --omit=dev --audit-level=high` exit=Some(1). vulnerable packages: minimist(high,fix:available), braces(high,fix:upgrade). suggestions: 1) npm audit fix 2) update vulnerable packages/lockfile 3) if blocked, pin safe versions and rebuild cache"}
```

## Notes

- Builds run in temporary work directories under system temp.
- Deploy artifacts are emitted under timestamped directories in `deploy.artifact_dir`.
- With target `kubernetes`, `sendbuilds` writes `kubernetes/deployment.yaml` and `kubernetes/service.yaml` into the artifact root.
- If `[deploy.gc].enabled = true`, old timestamped artifact directories are pruned automatically after deploy.
- Security-first output is written to artifact root as `sbom.json`, `security-report.json`, and `supply-chain-metadata.json`, and embedded in `build-metrics.json`.
- CNB lifecycle parity output is written to artifact root under `cnb/lifecycle-contract.json` and `cnb/lifecycle-metadata.json`.
- Provenance output is written as `provenance.intoto.jsonl` when signing provenance is enabled.
- If `signing.auto_generate_key = true`, `sendbuilds` creates a random key in `signing.key_file` (default `.sendbuild/signing.key`) and exports it to `signing.key_env` when missing.
- If both `[security].enabled` and `[scan].enabled` are true, `security-first` runs and legacy `security-scan` is skipped to avoid duplicate scanning.
- For Next.js production runtime, prefer `output: "standalone"` and set `output_dir` accordingly.

## Contributing

1. Fork and create a branch from `master`.
2. Make focused changes with clear commit messages.
3. Run local checks before opening a PR:

```bash
cargo fmt --all -- --check
cargo check
cargo test
```

4. If you changes, update `README.md` and `sendbuild.toml` examples.
5. Open a PR with:
- what changed
- why it changed
- how you tested it

## CI

GitHub Actions CI runs on push and pull requests. It validates formatting, compilation, tests, and release build output for Linux and Windows.
