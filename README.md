# rusty-jenkins (`rj`)

A modular, async Rust CLI for the Jenkins REST API.

## Features

| Command | What it does |
|---|---|
| `inspect` | Show a job's parameters, types, defaults, and last build status |
| `build` | Trigger a plain or parameterized build |
| `logs` | Stream live console output by polling `progressiveText` |
| `config get` | Download and print a job's `config.xml` |
| `config set` | Upload a local `config.xml` to replace a job's configuration |
| `sweep` | Run a job repeatedly, varying one parameter each time, and save each build's log |
| `config sweep` | Patch an XML tag in a job's config for each value, trigger a build, save the log, then restore |
| `tag list` | Read the value of an XML tag from every job in a folder or explicit list |
| `tag patch` | Set an XML tag in every job in a folder or explicit list — no build, no restore |
| `list` | List the jobs and sub-folders inside a folder (or the root) |

All commands handle Basic Auth and Jenkins CSRF crumbs automatically.

---

## Installation

### Prerequisites

- Rust toolchain via [rustup](https://rustup.rs) — the repo pins `stable` channel via `rust-toolchain.toml`; cargo picks the correct host target automatically

### Build — macOS

No extra setup required. Works on both Apple Silicon and Intel:

```bash
cargo build --release
# binary at: target/release/rj
```

### Build — Windows

Windows lacks a built-in C linker. The steps below use [Strawberry Perl](https://strawberryperl.com), which ships MinGW-W64.

**One-time setup:**

```powershell
# 1. Install the GNU Rust toolchain and make it the default
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu

# 2. Add Strawberry Perl's MinGW to your permanent PATH, then restart the terminal
[System.Environment]::SetEnvironmentVariable(
    "Path",
    "C:\Strawberry\c\bin;" + [System.Environment]::GetEnvironmentVariable("Path", "User"),
    "User"
)
```

**Build:**

```powershell
cargo build --release
# binary at: target\release\rj.exe
```

> **Why `rustup default`?** Passing `--target x86_64-pc-windows-gnu` only changes the output target, not the active toolchain. Build scripts (proc-macro2, serde, etc.) still compile with the default toolchain — which on a fresh Rust install is MSVC — and MSVC needs `link.exe`. Making GNU the *default* ensures everything uses the GNU linker.

Alternatively, install [Visual Studio Build Tools](https://aka.ms/vs/17/release/vs_BuildTools.exe) with the **Desktop development with C++** workload, then `cargo build --release` with no extra setup.

---

## Configuration

### Basic Auth (username + API token)

Credentials are read from environment variables:

```bash
export JENKINS_URL="http://jenkins.example.com:8080"
export JENKINS_USER="your-username"
export JENKINS_TOKEN="your-api-token"
```

To make them permanent on macOS/Linux, add those lines to your `~/.zshrc` or `~/.bashrc`. On Windows:

```powershell
[System.Environment]::SetEnvironmentVariable("JENKINS_URL",   "http://jenkins.example.com:8080", "User")
[System.Environment]::SetEnvironmentVariable("JENKINS_USER",  "your-username",                   "User")
[System.Environment]::SetEnvironmentVariable("JENKINS_TOKEN", "your-api-token",                   "User")
```

You can also pass them as flags on any command:

```bash
rj --url http://jenkins.local:8080 --user alice --token <token> inspect my-job
```

Generate an API token in Jenkins under **User → Configure → API Token**.

---

### SSO authentication (Okta, SAML, etc.)

If your Jenkins is behind SSO, API tokens and passwords won't work — the login flow lives in the identity provider. `rj` can read session cookies directly from your browser after you log in normally.

#### `--from-chrome` / `--from-firefox`

Log into Jenkins in your browser via SSO, then pass the flag on any command:

```bash
rj --from-chrome list
rj --from-chrome inspect folder/my-job
rj --from-chrome sweep my-job --param-name ENV --value staging prod
```

`rj` reads your browser's cookie database, extracts all `JSESSIONID.*` cookies for the Jenkins hostname, and sends them as the `Cookie` header. All other cookies (preferences, analytics) are ignored.

**Platform notes:**

| Platform | Chrome | Firefox |
|---|---|---|
| macOS | Keychain → PBKDF2 → AES-128-CBC | Plaintext SQLite |
| Windows | DPAPI → AES-256-GCM | Plaintext SQLite |

On macOS, the first run of `--from-chrome` may show a Keychain permission prompt — click **Allow** (or **Always Allow** to skip it on future runs).

#### Non-default Chrome profile

If you use a work profile rather than the default Chrome profile, pass its folder name. Open `chrome://version` and look at **Profile Path** — the last folder name is what you need:

```bash
rj --from-chrome --chrome-profile "Profile 1" list
```

Common names: `Default`, `Profile 1`, `Profile 2`.

#### Diagnosing cookie issues — `--list-cookies`

Run without a subcommand to see which cookies are found for the Jenkins hostname:

```bash
rj --from-chrome --list-cookies
rj --from-chrome --chrome-profile "Profile 1" --list-cookies
rj --from-firefox --list-cookies
```

Example output:

```
Looking for cookies matching host: ci.example.com
Found 9 cookie(s):
  JSESSIONID.06393bc    ← auth
  JSESSIONID.656c2ac9   ← auth
  JSESSIONID.b12b9956   ← auth
  javamelody.period     (preference, ignored)
  jenkins-timestamper   (preference, ignored)
  screenResolution      (preference, ignored)

rj will use: JSESSIONID.06393bc, JSESSIONID.656c2ac9, JSESSIONID.b12b9956
Run with --from-chrome to authenticate.
```

If no cookies are found: make sure you're logged in, check the profile name, and verify the session hasn't expired.

#### Manual cookie (`--cookie` / `JENKINS_COOKIE`)

Paste a cookie string directly — useful when `--from-chrome` can't decrypt or you want to reuse a known-good value from browser DevTools (**F12 → Application → Cookies**):

```bash
# Must be name=value format
export JENKINS_COOKIE="JSESSIONID.06393bc=node0abc123def456.node0"
rj list
```

**Authentication precedence** (highest wins):

```
JENKINS_COOKIE / --cookie
    > --from-chrome
        > --from-firefox
            > JENKINS_TOKEN / Basic Auth
```

---

### Folders and nested jobs

All commands accept a `/`-separated path as the job name. `rj` translates it into the nested `job/` URL structure the Jenkins REST API requires:

```
"folder/subfolder/my-job"
        ↓
job/folder/job/subfolder/job/my-job/...
```

### CloudBees CI / controllers with a URL prefix

CloudBees CI controllers sit under a path prefix (e.g. `/app-shared-controller`). Set `JENKINS_URL` to everything **up to and including that prefix** — stop at the first `/job/` segment — then pass the folder path as the job name:

```bash
export JENKINS_URL="https://ci.example.com/controller-name"

rj inspect "folder-name/subfolder-name/my-job"
```

`rj` constructs the full URL automatically:

```
https://ci.example.com/controller-name/job/folder-name/job/subfolder-name/job/my-job/api/json
```

---

## Usage

### `list`

List the jobs and sub-folders inside a folder. Use this to explore the job tree and validate that a folder path is correct before running other commands.

```bash
rj list                      # root
rj list folder/subfolder     # specific folder
```

**Example output:**

```
folder/subfolder/
  [FOLDER]  another-folder
  [JOB]     deploy-prod                          SUCCESS
  [JOB]     nightly-tests                        FAILED
  [JOB]     integration-suite                    NOT BUILT
  [JOB]     hotfix-pipeline                      SUCCESS   *building*

  1 folder(s), 4 job(s)
```

| Color | Status |
|---|---|
| `blue` | SUCCESS |
| `red` | FAILED |
| `yellow` | UNSTABLE |
| `aborted` | ABORTED |
| `disabled` | DISABLED |
| *(absent / other)* | NOT BUILT |

A `*building*` indicator appears next to any job currently running.

---

### `inspect`

```bash
rj inspect <job>
```

Prints the job's buildability, last build result, and all defined parameters with their types and default values.

```
Job:        deploy-prod
Desc:       Deploys to the production environment
Buildable:  true
Last build: #42 — SUCCESS

Parameters:
  ENV                    [String  ] Default: "staging"              — Target environment
  VERBOSE                [Boolean ] Default: false                  — Enable verbose output
  REGION                 [Choice  ] Choices: us-east-1, eu-west-1, ap-southeast-1  — AWS region
```

---

### `build`

Trigger a build with no parameters:

```bash
rj build <job>
```

Trigger a parameterized build (repeat `-p` for each parameter):

```bash
rj build <job> -p KEY=VALUE -p OTHER=VALUE
```

On success, prints the Jenkins queue item URL:

```
Queued: http://jenkins.example.com:8080/queue/item/123/
```

Values containing `=` are handled correctly — the split always occurs on the **first** `=` only.

---

### `logs`

Stream the console log for the most recent build:

```bash
rj logs <job>
```

Stream a specific build number:

```bash
rj logs <job> --build 42
```

Control the polling interval (default 1000 ms):

```bash
rj logs <job> --poll-ms 500
```

The loop polls `/logText/progressiveText`, advances the byte offset using `X-Text-Size`, and exits when `X-More-Data` is no longer `true` — meaning the build has finished.

---

### `config get`

Print the raw `config.xml` for a job:

```bash
rj config get <job>
```

Pipe it to a file to edit locally:

```bash
rj config get my-job > my-job.xml
```

---

### `config set`

Upload a local `config.xml` to replace a job's configuration:

```bash
rj config set <job> <file>
```

Example workflow — download, edit, re-upload:

```bash
rj config get my-job > my-job.xml
# edit my-job.xml
rj config set my-job my-job.xml
```

The request is sent with `Content-Type: application/xml` and the CSRF crumb attached automatically.

---

### `sweep`

Run a job repeatedly, varying one parameter across a list of values. Each build runs to completion before the next is triggered, and the full console log is saved to disk.

```bash
rj sweep <job> \
    --param-name <KEY> \
    --value <val1> <val2> <val3> \
    [--output-dir <dir>] \
    [--poll-ms <ms>] \
    [-p KEY=VALUE]...
```

**With a shell array (bash/zsh):**

```bash
envs=("staging" "prod" "dev")

rj sweep my-job \
    --param-name ENV \
    --value "${envs[@]}" \
    -p VERSION=1.0 \
    --output-dir ./results \
    --poll-ms 3000
```

> Use `"${array[@]}"` — not `$array` (first element only) or `"${array[*]}"` (single string).

**Console output:**

```
[1/3] ENV=staging
  Queued as build #42
  Result: SUCCESS
  Log:    results/my-job__ENV__staging__#42.log

[2/3] ENV=prod
  Queued as build #43
  Result: SUCCESS
  Log:    results/my-job__ENV__prod__#43.log

[3/3] ENV=dev
  Queued as build #44
  Result: FAILURE
  Log:    results/my-job__ENV__dev__#44.log

Sweep complete. Logs in 'results'.
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--param-name` | *(required)* | The parameter to vary |
| `--value` / `-v` | *(required)* | One or more values — space-separated list or repeated flags |
| `-p KEY=VALUE` | | Fixed parameters passed to every build |
| `--output-dir` | `sweep-logs` | Directory for log files (created if absent) |
| `--poll-ms` | `2000` | How often to poll the queue and build status |

Log files are named `{job}__{param}__{value}__#{build}.log`. A build failure or cancellation is logged and the sweep continues with the next value.

---

### `config sweep`

Iterate over a list of values by patching an XML tag in the job's `config.xml` before each build. Useful when the variation lives in the job configuration rather than a build parameter — for example, changing **Branch Sources → Repository Name** in a Multibranch Pipeline.

```bash
rj config sweep <job> \
    --xml-tag <tag> \
    --value <val1> <val2> <val3> \
    [--branch <branch>] \
    [--post-config-delay-ms <ms>] \
    [--output-dir <dir>] \
    [--poll-ms <ms>] \
    [--no-restore]
```

**Finding the right tag name:**

```bash
# Inspect the parent pipeline's XML to find the field you want to change
rj config get PIPELINE-NAME | grep -A2 -B2 repository
```

For a GitHub Branch Source the tag is typically `<repository>`:

```xml
<source class="...GitHubSCMSource">
  <repoOwner>my-org</repoOwner>
  <repository>my-repo</repository>   ← --xml-tag repository
```

**Multibranch Pipeline usage**

When sweeping a Multibranch Pipeline, use `--branch` to build a specific branch directly instead of triggering a full scan that would kick off every branch:

```bash
rj config sweep PIPELINE-NAME \
    --xml-tag repository \
    --value repo-a repo-b repo-c \
    --branch main \
    --output-dir ./results \
    --poll-ms 1000
```

> **Important:** target the pipeline name itself, not a branch path. `PIPELINE-NAME` — not `PIPELINE-NAME/main`.

Jenkins needs a moment to apply a config change before it accepts a build request. If you get HTTP 400 errors, increase `--post-config-delay-ms` (default 3000 ms). `rj` also automatically retries up to 5 times with exponential backoff if the first attempt gets a 400.

**Console output:**

```
Config: 'PIPELINE-NAME' — Build target: 'PIPELINE-NAME/main' (no branch scan triggered)
Fetching config.xml for 'PIPELINE-NAME'…

[1/3] <repository> = repo-a
  Config updated.
  Waiting 3000ms for Jenkins to apply config…
  Queued as build #47
  Result: SUCCESS
  Log:    results/PIPELINE-NAME__main__repository__repo-a__#47.log

[2/3] <repository> = repo-b
  Config updated.
  Waiting 3000ms for Jenkins to apply config…
  Queued as build #48
  Result: SUCCESS
  Log:    results/PIPELINE-NAME__main__repository__repo-b__#48.log

Restoring original config.xml… done.
Config sweep complete. Logs in 'results'.
```

**Options:**

| Flag | Default | Description |
|---|---|---|
| `--xml-tag` | *(required)* | XML tag name to patch (first occurrence in the document) |
| `--value` / `-v` | *(required)* | Values to iterate — space-separated or repeated flags |
| `--branch` | | Build this specific branch instead of triggering a full pipeline scan. Use with Multibranch Pipelines to avoid triggering every branch. |
| `--post-config-delay-ms` | `3000` | Wait this many ms after uploading config before triggering the build. Increase if Jenkins returns HTTP 400. |
| `--output-dir` | `config-sweep-logs` | Directory for log files (created if absent) |
| `--poll-ms` | `2000` | Polling interval for queue and build-complete checks |
| `--no-restore` | | Skip restoring the original config after the sweep |

The original `config.xml` is downloaded once, patched in memory for each iteration (only the target tag is changed), and restored when the sweep completes. Use `--no-restore` if you want the last value to remain.

Without `--branch`, the pipeline job itself is built (triggering a branch scan). With `--branch`, only that specific branch job is built — no cascade across other branches.

Log files are named `{pipeline}__{branch}__{tag}__{value}__#{build}.log` when `--branch` is used, or `{job}__{tag}__{value}__#{build}.log` otherwise.

---

### `tag list`

Read the value of an XML tag from every job in a folder or an explicit list. Useful for auditing configuration across many jobs at once — for example, checking which branch each pipeline is set to.

```bash
# Every job in a folder
rj tag list --path folder/subfolder --xml-tag repository

# Specific jobs only
rj tag list --job-name folder/job1 --job-name folder/job3 --xml-tag repository

# Both — combine a folder scan with extra individual jobs
rj tag list --path folder --job-name other-folder/special-job --xml-tag repository
```

**Example output:**

```
folder/job1:s3
folder/job2:deep-clear
folder/job3:main
```

If a job's config does not contain the tag, the line reads `(tag <name> not found)`. Errors on individual jobs are printed and the loop continues.

| Flag | Description |
|---|---|
| `--path` | Folder to scan — all direct (non-folder) job children are included |
| `--job-name` | Specific job path (repeatable) |
| `--xml-tag` | XML tag whose text content to read |

---

### `tag patch`

Set the value of an XML tag in every job in a folder or explicit list. Unlike `config sweep`, this does not trigger a build and does not restore the original config — the change is permanent.

```bash
# Set the branch for every job in a folder
rj tag patch --path folder/subfolder --xml-tag branches/name --value "*/develop"

# Only specific jobs
rj tag patch --job-name folder/job1 --job-name folder/job3 --xml-tag branches/name --value "*/s3"

# Show the existing value before the new one for easy auditing
rj tag patch --path folder --xml-tag branches/name --value "*/develop" --show-old
```

**Example output (with `--show-old`):**

```
[1/3] folder/job1 … <branches/name>: */main → */develop
[2/3] folder/job2 … <branches/name>: */staging → */develop
[3/3] folder/job3 … <branches/name>: */develop → */develop
```

**Example output (without `--show-old`):**

```
[1/3] folder/job1 … <branches/name> → */develop
[2/3] folder/job2 … <branches/name> → */develop
[3/3] folder/job3 … <branches/name> → */develop
```

Failures on individual jobs are printed and the loop continues to the remaining jobs.

| Flag | Description |
|---|---|
| `--path` | Folder to scan — all direct (non-folder) job children are included |
| `--job-name` | Specific job path (repeatable) |
| `--xml-tag` | XML tag to update (supports `/`-separated paths — see below) |
| `--value` | New value to set |
| `--show-old` | Print the existing value before the new one |

> Sub-folders inside `--path` are skipped — only direct job children are targeted, preventing accidental mass updates across deeply nested structures.

---

### XML tag paths

Both `tag list` and `tag patch` accept a `/`-separated path for `--xml-tag` to disambiguate when multiple elements share the same tag name. Each segment is found by depth-first search within the match of the previous segment.

**Example — a pipeline config with two `<name>` elements:**

```xml
<properties>
  <hudson.model.StringParameterDefinition>
    <name>FOOBAR</name>          ← parameter name
  </hudson.model.StringParameterDefinition>
</properties>
<definition>
  <scm>
    <branches>
      <hudson.plugins.git.BranchSpec>
        <name>*/main</name>      ← branch name
      </hudson.plugins.git.BranchSpec>
    </branches>
  </scm>
</definition>
```

| `--xml-tag` value | Resolves to |
|---|---|
| `name` | `FOOBAR` (first `<name>` in depth-first order) |
| `branches/name` | `*/main` (first `<name>` inside `<branches>`) |
| `hudson.plugins.git.BranchSpec/name` | `*/main` (more specific ancestor) |

Element names containing dots work as-is — the `/` is the only separator.

---

## Architecture

```
src/
├── main.rs              # #[tokio::main] entry point — parses CLI, builds client, dispatches
├── cli.rs               # clap derive structs for all commands and subcommands
├── client.rs            # JenkinsClient — Basic Auth, CSRF crumb fetch & cache
├── browser.rs           # Firefox/Chrome cookie extraction for SSO auth
└── commands/
    ├── inspect.rs       # Job/parameter JSON deserialisation and display
    ├── build.rs         # Plain and parameterized POST build trigger
    ├── logs.rs          # Async progressive-text polling loop
    ├── config.rs        # XML config GET and POST
    ├── config_sweep.rs  # XML-patch loop: patch config, build, wait, save log, restore
    ├── list_tag.rs      # Read an XML tag value across a folder or job list
    ├── patch_tag.rs     # Set an XML tag value across a folder or job list
    ├── sweep.rs         # Build-param loop: queue polling, build-wait, log saving
    └── list.rs          # Folder contents listing with status and building indicator
```

**Key dependencies:**

| Crate | Role |
|---|---|
| `clap` v4 (derive + env) | CLI parsing, env-var fallbacks |
| `tokio` (full) | Async runtime |
| `reqwest` | HTTP client with JSON and form support |
| `serde` / `serde_json` | JSON deserialisation |
| `anyhow` | Ergonomic error propagation with context chains |
| `rusqlite` (bundled) | Read Firefox/Chrome cookie databases |
| `xmltree` | In-memory XML patching for `config sweep` |
| `aes-gcm` | AES-256-GCM decryption for Chrome cookies (Windows) |
| `cbc` / `pbkdf2` / `sha1` *(macOS)* | AES-128-CBC + key derivation for Chrome cookies (macOS) |
| `wiremock` *(dev)* | Mock HTTP server for integration tests |

---

## Testing

```bash
cargo test
```

122 tests across all modules, covering:

- CLI argument parsing including shell-array-style multi-value flags (unit)
- Basic Auth header attachment (wiremock)
- CSRF crumb fetch, attachment, and caching (wiremock)
- Job JSON deserialisation for String, Boolean, and Choice parameters (unit)
- Plain and parameterized build POST with form body verification (wiremock)
- Log streaming: `X-Text-Size` offset advancement and `X-More-Data` loop control (wiremock + unit)
- `config.xml` GET and POST with `Content-Type: application/xml` verification (wiremock)
- Sweep: queue item polling, build-complete polling, log file writing, full end-to-end loop (wiremock + unit)
- Folder/nested job path encoding: plain jobs, single folder, deep nesting, spaces in segment names (unit)
- Folder listing: color-to-status mapping, `_anime` building detection, folder vs job class detection, root vs nested path routing (wiremock + unit)
- Browser cookie extraction: hostname parsing, Firefox profile discovery, Chrome AES-GCM/CBC roundtrip decryption (unit)
- Config sweep: XML tag patching, `--branch` targeting, HTTP 400 retry with backoff, full build loop with config restore (wiremock + unit)
- `tag list` / `tag patch`: folder-to-job resolution, XML tag read/write, per-job error isolation (wiremock + unit)
