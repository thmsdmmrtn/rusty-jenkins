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

Windows lacks a built-in C linker. The easiest option is [Strawberry Perl](https://strawberryperl.com), which ships MinGW-W64. After installing it, add its `bin` directory to your PATH and build with the GNU target:

```powershell
# Add Strawberry Perl's MinGW to PATH (adjust drive letter if needed)
$env:Path = "C:\Strawberry\c\bin;$env:Path"

rustup toolchain install stable-x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
# binary at: target\x86_64-pc-windows-gnu\release\rj.exe
```

Alternatively, install [Visual Studio Build Tools](https://aka.ms/vs/17/release/vs_BuildTools.exe) with the **Desktop development with C++** workload, then `cargo build --release` with no extra flags.

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

## Architecture

```
src/
├── main.rs              # #[tokio::main] entry point and 4-arm command dispatch
├── cli.rs               # clap derive structs for all commands and subcommands
├── client.rs            # JenkinsClient — Basic Auth, CSRF crumb fetch & cache
└── commands/
    ├── inspect.rs       # Job/parameter JSON deserialisation and display
    ├── build.rs         # Plain and parameterized POST build trigger
    ├── logs.rs          # Async progressive-text polling loop
    └── config.rs        # XML config GET and POST
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

58 tests across all modules, covering:

- CLI argument parsing (unit)
- Basic Auth header attachment (wiremock)
- CSRF crumb fetch, attachment, and caching (wiremock)
- Job JSON deserialisation for String, Boolean, and Choice parameters (unit)
- Plain and parameterized build POST with form body verification (wiremock)
- Log streaming: `X-Text-Size` offset advancement and `X-More-Data` loop control (wiremock + unit)
- `config.xml` GET and POST with `Content-Type: application/xml` verification (wiremock)
