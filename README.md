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

Credentials are read from environment variables, which means you never need to pass them as flags:

```powershell
$env:JENKINS_URL   = "http://jenkins.example.com:8080"
$env:JENKINS_USER  = "your-username"
$env:JENKINS_TOKEN = "<your-api-token>"
```

To make them permanent (survives reboots):

```powershell
[System.Environment]::SetEnvironmentVariable("JENKINS_URL",   "http://jenkins.example.com:8080", "User")
[System.Environment]::SetEnvironmentVariable("JENKINS_USER",  "your-username",            "User")
[System.Environment]::SetEnvironmentVariable("JENKINS_TOKEN", "<your-api-token>",          "User")
```

You can also pass them as flags on any command:

```
rj --url http://jenkins.local:8080 --user alice --token <token> inspect my-job
```

Generate an API token in Jenkins under **User → Configure → API Token**.

### Folders and nested jobs

All commands accept a `/`-separated path as the job name. `rj` translates it
into the nested `job/` URL structure the Jenkins REST API requires:

```
"folder/subfolder/my-job"
        ↓
job/folder/job/subfolder/job/my-job/...
```

### CloudBees CI / controllers with a URL prefix

CloudBees CI controllers sit under a path prefix (e.g. `/app-shared-controller`).
Set `JENKINS_URL` to everything **up to and including that prefix** — stop at
the first `/job/` segment — then pass the folder path as the job name:

```bash
# Base URL includes the controller prefix
export JENKINS_URL="https://ci.example.com/controller-name"

# Job name is the slash-separated folder path (no leading /job/)
rj inspect "folder-name/subfolder-name/my-job"
```

`rj` constructs the full URL automatically:

```
https://ci.example.com/controller-name/job/folder-name/job/subfolder-name/job/my-job/api/json
```

The CSRF crumb, build triggers, log streaming, and config endpoints all follow
the same pattern and require no extra configuration.

---

## Usage

### `inspect`

```
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

```
rj build <job>
```

Trigger a parameterized build (repeat `-p` for each parameter):

```
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

```
rj logs <job>
```

Stream a specific build number:

```
rj logs <job> --build 42
```

Control the polling interval (default 1000 ms):

```
rj logs <job> --poll-ms 500
```

The loop polls `/logText/progressiveText`, advances the byte offset using `X-Text-Size`, and exits when `X-More-Data` is no longer `true` — meaning the build has finished.

---

### `config get`

Print the raw `config.xml` for a job:

```
rj config get <job>
```

Pipe it to a file to edit locally:

```
rj config get my-job > my-job.xml
```

---

### `config set`

Upload a local `config.xml` to replace a job's configuration:

```
rj config set <job> <file>
```

Example workflow — download, edit, re-upload:

```powershell
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

## Architecture

```
src/
├── main.rs              # #[tokio::main] entry point — parses CLI, builds client, dispatches
├── cli.rs               # clap derive structs for all commands and subcommands
├── client.rs            # JenkinsClient — Basic Auth, CSRF crumb fetch & cache
└── commands/
    ├── inspect.rs       # Job/parameter JSON deserialisation and display
    ├── build.rs         # Plain and parameterized POST build trigger
    ├── logs.rs          # Async progressive-text polling loop
    ├── config.rs        # XML config GET and POST
    └── sweep.rs         # Multi-build loop: queue polling, build-wait, log saving
```

**Key dependencies:**

| Crate | Role |
|---|---|
| `clap` v4 (derive + env) | CLI parsing, env-var fallbacks |
| `tokio` (full) | Async runtime |
| `reqwest` | HTTP client with JSON and form support |
| `serde` / `serde_json` | JSON deserialisation |
| `anyhow` | Ergonomic error propagation with context chains |
| `wiremock` *(dev)* | Mock HTTP server for integration tests |

---

## Testing

```powershell
cargo test
```

78 tests across all modules, covering:

- CLI argument parsing including shell-array-style multi-value flags (unit)
- Basic Auth header attachment (wiremock)
- CSRF crumb fetch, attachment, and caching (wiremock)
- Job JSON deserialisation for String, Boolean, and Choice parameters (unit)
- Plain and parameterized build POST with form body verification (wiremock)
- Log streaming: `X-Text-Size` offset advancement and `X-More-Data` loop control (wiremock + unit)
- `config.xml` GET and POST with `Content-Type: application/xml` verification (wiremock)
- Sweep: queue item polling, build-complete polling, log file writing, full end-to-end loop (wiremock + unit)
- Folder/nested job path encoding: plain jobs, single folder, deep nesting, spaces in segment names (unit)
