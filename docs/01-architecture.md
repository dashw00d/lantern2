# Lantern v2 — Core Architecture

Status: **authoritative**. Every subsystem doc (`1x-*.md`) builds on this file. A subsystem doc
may *propose* a change to anything here, but it must do so in its own "Proposed changes to
core architecture" section — not by silently diverging.

Precedence: **`02-decisions.md` > this file > `10`–`16`**. This file has been revised to absorb
the accepted proposals from the subsystem docs (see `02-decisions.md` §B).

v1 source for reference lives at `.reference/lantern-v1/` (git-ignored). v1 file paths below
are relative to that directory.

---

## 1. What Lantern is

A local development control plane for one Linux machine and one user:

1. **Knows every dev server**: the ones it manages *and* the ones you started by hand in a
   terminal (discovered from listening sockets).
2. **Runs them at any branch**: each git checkout of a project (primary, existing worktree, or
   a worktree Lantern creates on demand) is a separately runnable *instance* with its own port
   and domain.
3. **Gives each one a stable HTTPS name**: `myapp.glow`, `feature-x.myapp.glow`.
4. **Exposes everything to agents** via a single MCP endpoint (list, start, stop, logs,
   docs, endpoints, call APIs).
5. **Is pleasant to use**: instant native GUI, KDE tray, scriptable CLI.

### Goals
- One language (Rust), one binary (`lantern`), one config format the user can read.
- Instant startup and low idle overhead. The daemon should idle at near-zero CPU.
- Dev servers survive a daemon restart or upgrade.
- No runtime root. Root is needed only once, at install/`lantern init`.
- MCP tool names and arguments stay backward compatible with v1 so existing agent setups keep
  working.
- `lantern.yaml` v1 manifests keep working unchanged.

### Non-goals (v2.0)
- macOS / Windows. Linux + systemd only (target: Ubuntu 26.04, KDE Plasma on Wayland; must
  not *require* KDE).
- Multi-user or remote machines.
- Container orchestration. Docker/compose can be a project's `run.cmd`, nothing more.
- REST backward compatibility with v1. Only the CLI/GUI/tray consume REST, and they are
  rewritten. (MCP *is* kept compatible.)
- Replacing Caddy. Caddy stays as the reverse proxy and local CA.

---

## 2. Glossary (use these words exactly)

| Term | Meaning |
|---|---|
| **Project** | A registered codebase: a directory, usually a git repo root, with a `lantern.yaml` manifest (or detected defaults). Identified by `ProjectId` (slug, e.g. `myapp`). |
| **Checkout** | A directory containing a version of a project's code. Kinds: `primary` (the registered path), `worktree` (a git worktree the user made), `managed` (a worktree Lantern created). |
| **Instance** | A runnable unit = one Project × one Checkout. This is what gets started, stopped, given a port and a domain. Identified by `InstanceId`. |
| **Primary instance** | The instance for the primary checkout. `InstanceId == ProjectId`. |
| **Manifest** | `lantern.yaml` / `lantern.yml`. Resolved *per checkout*: a branch may change its own manifest. |
| **Service** | Shared infrastructure that is not a project: Mailpit, Redis, Postgres. Managed via systemd units. |
| **Listener** | An observed TCP LISTEN socket (v4/v6, loopback and non-loopback), with its owning process, cwd, and (if matched) project/instance. Ephemeral. Not persisted. |
| **Unmanaged instance** | A Listener whose cwd maps to a known project checkout but which Lantern did not start. Can be *adopted* (gets a domain/route) without Lantern owning the process. |
| **Tool** | A project whose manifest declares `kind: tool` or `kind: capability` plus endpoints/docs, surfaced to agents via MCP. (Same as v1.) |
| **Profile** | A named set of services + auto-start instances + env overrides (v1 concept, kept). |
| **Settings** | Global user config (`config.toml`). |
| **State** | Daemon-owned runtime/registry data (`state.json`). Not hand-edited. |

---

## 3. Process & deployment model

```
                       ┌────────────────────────────────────────────┐
  lantern (CLI) ──┐    │  lantern daemon   (systemd --user service) │
  lantern gui  ───┼──► │  127.0.0.1:4777                            │
  tray (in gui) ──┘    │   ├─ REST  /api/...                        │
  agents ─────────────►│   ├─ SSE   /api/events                     │
                       │   ├─ MCP   /mcp  (stateless streamable HTTP)│
                       │   ├─ docs  /<project>/docs                 │
                       │   └─ gateway /__lantern/* (stopped pages)  │
                       └──────┬───────────────┬──────────────┬──────┘
                              │ systemd-run   │ admin API    │ DNS :5354
                              ▼ --user        ▼ unix socket  ▼
                  transient units       lantern-caddy        embedded responder
                  lantern-inst-<safe>   .service :443/:80    → *.glow → 127.0.0.1
                  (app-lantern.slice)   Lantern-made CA      (systemd-resolved
                  logs → user journal                         DNS delegate ~glow)
```

- **One binary**, `lantern`, with subcommands:
  - `lantern daemon` — the long-running service. Installed as a **systemd user unit**
    (`~/.config/systemd/user/lantern.service` or packaged in `/usr/lib/systemd/user/`), not a
    system service. It runs as the user, so dev servers run as the user with the user's env.
  - `lantern gui` — the iced desktop app, which also hosts the tray icon. Autostarts via XDG
    autostart (`--tray` flag = start minimized to tray).
  - `lantern <command>` — CLI (`ls`, `on`, `off`, `logs`, `scan`, `wt`, `init`, `mcp install`, …).
- The **CLI, GUI and tray are pure HTTP clients** of the daemon. They never touch state
  files, spawn dev servers, or talk to Caddy directly. If the daemon is down, they say so and
  offer to start it (`systemctl --user start lantern`).
- **Dev servers run as systemd transient user units** (`systemd-run --user --unit=lantern-inst-<slug>`)
  so they (a) survive daemon restarts/upgrades, (b) are killed as a whole cgroup (no orphaned
  grandchildren), (c) have logs captured independently of the daemon. The runtime doc
  (`10-runtime.md`) owns the details and may refine this, but the three properties are
  requirements.
- **Caddy runs as the transient user unit `lantern-caddy.service`** (bundled Caddy 2.11.x;
  Ubuntu's 2.6 is too old), configured exclusively by a full `POST /load` to its **admin API on
  a Unix socket** (`$XDG_RUNTIME_DIR/lantern/caddy-admin.sock`), never by writing Caddyfiles into
  `/etc`. Binding :80/:443 as non-root: sysctl `net.ipv4.ip_unprivileged_port_start=80`, set once
  at init (`02` D14; setcap is the opt-in alternative). TLS comes from a **Lantern-generated,
  name-constrained root CA** (only `.glow` names) that Caddy issues per-host certs from.
- **DNS**: a tiny hand-rolled responder in the daemon (`127.0.0.1:5354`) answers `*.<tld>` → A
  `127.0.0.1` (AAAA opt-in). A one-time **systemd-resolved DNS delegate**
  (`/etc/systemd/dns-delegate.d/lantern.dns-delegate`, systemd ≥ 258) routes only `~glow` to it.
  No dnsmasq, no NetworkManager restarts.
- **Gateway**: the daemon serves `/__lantern/*` on every Lantern hostname (via Caddy): the
  "myapp is stopped — Start" page, status polling, CA download. It has its own same-origin guard
  (`13-system.md` §3.4.5).
- **Root**: only `pkexec lantern init --system-steps` (or the .deb postinst) needs root, for:
  the sysctl, the DNS delegate, trusting Lantern's CA in the system store. Every step is
  idempotent and checked by `lantern doctor`. v1's sudoers file is removed entirely — notably its
  `cp /tmp/lantern_priv_* /etc/systemd/system/*` rule, which lets any process running as the
  user gain root.

---

## 4. Code layout (Cargo workspace)

```
lantern/
├─ Cargo.toml                 # workspace
├─ crates/
│  ├─ lantern-core/           # domain types, ids, manifest + settings parsing, slugs,
│  │                          # template vars, api types. Pure: no tokio, no network, no spawning.
│  ├─ lantern-system/         # sync (no tokio): paths, `init` steps, `doctor`, CA + trust stores.
│  │                          # Must work with the daemon down and as root.
│  ├─ lantern-daemon/         # everything that runs inside `lantern daemon`
│  │  └─ src/
│  │     ├─ registry/         # projects, checkouts, instances, state.json persistence (pure state)
│  │     ├─ instances/        # worktree derivation, prepare pipeline, file copy/link, deps, removal
│  │     ├─ operations.rs     # OperationTracker (ids, progress, wait/join, preemption)
│  │     ├─ runtime/          # supervisor: start/stop/restart, one-shots, readiness, logs, health
│  │     ├─ git/              # worktree list/add/remove, branch info (shells out to `git`)
│  │     ├─ discovery/        # listening-socket scanner, cwd → checkout matching
│  │     ├─ ports/            # port allocator/leases
│  │     ├─ net/              # route plan, caddy unit + admin client, dns responder, tls, gateway
│  │     ├─ services/         # mailpit/redis/postgres
│  │     ├─ intel/            # docs + OpenAPI/endpoint discovery (v1 "discovery" of docs/apis)
│  │     ├─ api/              # axum REST + SSE
│  │     ├─ mcp/              # MCP server (tools/resources/prompts)
│  │     └─ events.rs         # the event bus
│  ├─ lantern-client/         # typed async HTTP + SSE client for the daemon API (used by cli+gui)
│  ├─ lantern-cli/            # clap command definitions + output formatting
│  ├─ lantern-gui/            # iced app + ksni tray
│  └─ lantern/                # the binary: main.rs dispatches daemon | gui | cli
├─ schema/                    # generated JSON Schemas (lantern.yaml, profiles, config.toml)
├─ packaging/                 # deb, systemd user unit, desktop file, icons, polkit policy
└─ docs/
```

Dependency rule: `core` ← everything. `system` depends only on `core`. `client` depends only
on `core`. `cli` depends on `client` + `core` (+ `system` for `init`/`doctor`/`uninstall`);
`gui` on `client` + `core`; **neither ever depends on `daemon`**. `lantern-cli` returns
`CliAction::{RunDaemon, RunGui}` to the binary instead of calling those crates. `daemon` never
depends on `client`, `cli`, or `gui`. API request/response types live in `lantern-core::api` so
client and daemon share one definition.

> Note on naming: v1 used "discovery" for *docs/OpenAPI ingestion*. In v2, **discovery means
> finding listening servers**; docs/OpenAPI ingestion is **intel**. Don't mix them.

---

## 5. Baseline technology choices

Subsystem docs may add crates; they must not swap these without a "Proposed changes" entry.
Verify current versions/maintenance status at implementation time; do not pin here.

| Concern | Choice |
|---|---|
| Async runtime | `tokio` |
| HTTP server | `axum` (+ `tower-http`) |
| HTTP client | `reqwest` (rustls) |
| MCP | `rmcp` 3.x (official Rust SDK), **stateless** streamable-HTTP transport mounted at `/mcp` |
| Serialization | `serde`, `serde_json`; YAML via **`serde-saphyr`** (pinned exactly, isolated in `lantern_core::yaml`); `toml` / `toml_edit`; JSON Schema via `schemars` |
| CLI | `clap` (derive) + `clap_complete` |
| GUI | `iced` 0.14 (`iced::daemon`, pinned `=0.14.x`) |
| Tray | `ksni` (StatusNotifierItem — works on KDE/Wayland; GNOME needs the AppIndicator extension); direct `zbus = "5"` so ksni/notify-rust/iced share one copy |
| Errors | `thiserror` in libraries, `anyhow` only at binary edges |
| Logging | `tracing` + `tracing-subscriber` (journald when run under systemd) |
| Git | shell out to the `git` CLI with `--porcelain`/`-z` formats. No libgit2. |
| Process info | `/proc` read directly with `std::fs` behind an injectable root (fixture-testable); listening sockets via netlink `sock_diag` (`rustix`), `/proc/net/tcp{,6}` fallback |
| Systemd | shell out to `systemctl --user` / `systemd-run --user`; one batched `systemctl show` poll for unit state. No D-Bus in v2.0. |

---

## 6. Domain model (lives in `lantern-core`)

These are sketches — field names are binding, exact types can be refined.

```rust
/// "myapp". Lowercase [a-z0-9-], 1..=48 chars, unique. From manifest `id`, else dir name slugified.
pub struct ProjectId(String);

/// Primary: "myapp". Other checkouts: "myapp@feature-x" (branch slugified; detached HEAD →
/// "myapp@<short-sha>"; collisions → numeric suffix). Charset [a-z0-9-] on each side of at
/// most one '@'. Frozen once the instance is claimed (pinned or started once; 11 §3.8).
/// `safe_name()` = '@' → '_' ("myapp_feature-x"): used for unit names, scripts, ${INSTANCE}.
pub struct InstanceId(String);

/// Reserved: api, mcp, lantern, mailpit (422 reserved_id).
pub struct Project {
    pub id: ProjectId,
    pub aliases: Vec<String>,         // legacy (v1) names accepted on lookup, e.g. by MCP tools
    pub name: String,                 // display name
    pub root: PathBuf,                // project dir in the primary checkout (absolute, canonical)
    pub repo: Option<ProjectRepo>,    // None = not a git project
    pub kind: ProjectKind,            // Project | Service | Tool | Capability | Website
    pub enabled: bool,
    pub tags: Vec<String>,
    pub registered_at: Timestamp,
    pub source: ProjectSource,        // Manifest | Detected | Manual
}

pub enum CheckoutKind { Primary, Worktree, Managed }

/// Monorepos: a project may be a subdirectory of its repo; several projects may share a repo.
pub struct ProjectRepo {
    pub common_dir: PathBuf,          // canonical git common dir: the repo's identity
    pub git_root: PathBuf,            // primary worktree top level (derived, not persisted)
    pub subpath: PathBuf,             // project dir relative to git_root ("" for repo root)
}

pub struct Checkout {
    pub path: PathBuf,                // project dir inside this checkout = <git_root>/<subpath>
    pub git_root: Option<PathBuf>,
    pub kind: CheckoutKind,
    pub branch: Option<String>,       // None = detached
    pub head: Option<String>,         // commit sha
    pub dirty: Option<bool>,          // best-effort, refreshed lazily
    pub locked: Option<String>,       // git worktree lock reason
    pub missing: bool,                // worktree no longer listed / path gone
}

pub struct Instance {
    pub id: InstanceId,
    pub project: ProjectId,
    pub checkout: Checkout,
    pub domain: String,               // "myapp.glow" | "feature-x.myapp.glow"
    pub manifest: Arc<ResolvedManifest>, // read from *this checkout* (fallback: primary's);
                                         // includes `serve: ServeMode { Proxy, Php, Static, Unknown }`
    pub runtime: RuntimeState,
    pub pinned: bool,                 // show even when stopped (non-primary instances)
    pub adopt: bool,                  // route this instance's hand-started server when seen (12)
    pub setup_stale: bool,            // lockfiles/setup commands changed since last prepare (11)
}

pub enum InstanceStatus {
    Stopped,
    Preparing,      // creating worktree / running setup steps
    Starting,       // process spawned, waiting for readiness
    Running,
    Unhealthy,      // running but health checks failing
    Stopping,
    Crashed,        // exited unexpectedly; may be retrying
    NeedsConfig,    // can't run: no run cmd / invalid manifest
    External,       // a matched Listener we didn't start (unmanaged / adopted)
}

pub struct RuntimeState {
    pub status: InstanceStatus,
    pub port: Option<u16>,
    pub unit: Option<String>,         // systemd unit name when managed
    pub main_pid: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub last_exit: Option<ExitInfo>,
    pub restarts: u32,
    pub next_retry_at: Option<Timestamp>,
    pub health: HealthSummary,
    pub message: Option<String>,      // human-readable reason for current status
    pub issue: Option<RuntimeIssue>,  // machine-readable cause { code, detail, hint } (10)
    pub upstream: Option<SocketAddr>, // where readiness found the listener; Caddy routes here
    pub external: Option<ExternalState>, // ephemeral: the matched hand-started server (12)
}
// ExitInfo, HealthSummary, RuntimeIssue/IssueCode: 10-runtime.md §3, §9.

pub struct Listener {               // TCP LISTEN socket, v4/v6, loopback and non-loopback
    pub addr: SocketAddr,
    pub pid: Option<u32>,             // None if owned by another user
    pub exe: Option<PathBuf>,
    pub cmdline: Vec<String>,         // ALWAYS redacted before leaving discovery/
    pub cwd: Option<PathBuf>,
    pub matched: Option<InstanceId>,  // cwd inside a known checkout
    pub managed: bool,                // belongs to a Lantern-started unit
    // + port, other_addrs, uid, exposure, started_at, project, branch, matched_by, unit,
    //   class, label, probe, docker, adopted, hidden: 12-discovery.md §3.2
}
```

Route rule (`02` D9): an instance has a Caddy route iff status is `Running`/`Unhealthy`, or
`External` and (`adopt` or settings `discovery.auto_adopt`).

### Domains
- Primary: `<project>.<tld>`. Non-primary: `<branch-slug>.<project>.<tld>`.
- A manifest `domain:` override applies to the primary instance only; non-primary instances
  prepend `<branch-slug>.` to it.
- Default TLD `glow` (stored without the leading dot in v2; accept `.glow` from v1 settings).

### Template variables (see `02` D1 for the full rule)
`${PORT}`, `${HOST}` (127.0.0.1), `${DOMAIN}`, `${URL}` (https://domain), `${PROJECT}`,
`${INSTANCE}` (`safe_name()`), `${BRANCH}` (raw; short sha if detached), `${BRANCH_SLUG}`,
`${BRANCH_SNAKE}`, `${CHECKOUT}` (abs path), `${PRIMARY}` (primary checkout abs path).
Deprecated v1 aliases: `${NAME}` → PROJECT, `${PATH}` → CHECKOUT (warned).

- **Closed set**: only these names are recognized; any other `${…}` (e.g. `${HOME}`) passes
  through to the shell untouched. `$${` is a literal `${`.
- **Shell contexts** (`run.cmd`, `run.pre_start`, `run.post_stop`, `setup`, `deploy.*`): names
  are rewritten to `${LANTERN_<NAME>}` and exported, **never pasted into the command text**
  (branch names can contain shell metacharacters).
- **Data contexts** (`run.env`, `worktree.env`, profile `env` values, `run.cwd`): substituted
  textually.

---

## 7. Persistence

| What | Where | Format | Who writes |
|---|---|---|---|
| Settings | `$XDG_CONFIG_HOME/lantern/config.toml` | TOML | user (hand-edit) and daemon (via API) |
| Profiles, user templates | `$XDG_CONFIG_HOME/lantern/{profiles,templates}/*.yaml` | YAML | user / daemon |
| Registry + runtime state | `$XDG_STATE_HOME/lantern/state.json` | JSON, versioned (`"version": 1`) | daemon only, atomic write (tmp + rename), debounced |
| Daemon lock, backups | `$XDG_STATE_HOME/lantern/state.{lock,json.bak,json.corrupt-*}` | — | daemon |
| Instance + setup + deploy logs | **user journal** (units in `app-lantern.slice`); no log files | journald | systemd |
| Intel cache | `$XDG_STATE_HOME/lantern/intel-cache.json` (disposable) | JSON | daemon |
| Last applied Caddy config | `$XDG_STATE_HOME/lantern/caddy.json` | JSON | daemon |
| Runtime files | `$XDG_RUNTIME_DIR/lantern/` (launcher scripts `run/`, caddy admin socket) | tmpfs | daemon |
| Captured login-shell env | `$XDG_CACHE_HOME/lantern/shell-env.sh` (0600) | sh | daemon |
| Managed worktrees | `$XDG_DATA_HOME/lantern/worktrees/<repo-slug>/<instance-slug>/` (overridable) | git worktree | daemon |
| Caddy data (CA, certs), bundled binaries, service data | `$XDG_DATA_HOME/lantern/{caddy,bin,services}/` | caddy | daemon / caddy |
| GUI preferences | `$XDG_STATE_HOME/lantern/gui.json` | JSON | **GUI** (the only client-written file, with the XDG autostart entry) |

No database. Data is small (tens of projects) and a single process writes it. Plain JSON is
debuggable with `cat` and `jq`. On startup the daemon **reconciles** state.json against
reality (which `lantern-inst-*` units are actually running, which worktrees still exist) and
the reality wins.

---

## 8. Concurrency model inside the daemon

- **Registry actor** owns projects/instances (a `tokio::sync` task with an mpsc command
  channel, or an `RwLock<Registry>` with all mutations in one module, `registry/` decides).
  All state mutations go through it, which is what makes persistence and events consistent.
- **One supervisor task per running instance** (`runtime/`), owning that instance's lifecycle
  and readiness/health loop. It reports status transitions back to the registry.
- **Per-instance operation lock**: start/stop/restart/prepare on the same instance are
  serialized. An identical request joins the in-flight operation; **`Stop` preempts** an
  in-flight start/restart/prepare (a hung start or install must be cancellable); any other
  combination is `409 operation_in_progress`. Never a race.
- **Nothing auto-starts at boot** except reconciliation (adopt running units, republish
  `activated` site-only instances). Auto-start comes only from the active profile.
- **Background workers**: listener scanner (periodic + on demand), git watcher (periodic
  `git worktree list` per project, cheap), intel refresher (docs/OpenAPI), health checker.
  All are cancellable tokio tasks under one `JoinSet`/`CancellationToken` tree for clean
  shutdown.
- **Daemon shutdown never stops dev servers.** `lantern off --all` or the GUI's "Stop
  everything" does that explicitly.

---

## 9. Events

A single in-process `tokio::sync::broadcast` bus carries `Event` values. Externally the bus is
exposed as **Server-Sent Events** at `GET /api/events` (JSON per event, `event:` = kind).
The GUI and tray subscribe; the CLI uses it for `lantern logs -f` and `lantern on --wait`.

```rust
pub enum Event {
    InstanceUpdated { instance: InstanceView },      // view incl. ManifestSummary; never env values
    InstanceRemoved { id: InstanceId },
    ProjectUpdated  { project: ProjectSummary },
    ProjectRemoved  { id: ProjectId },
    ListenersChanged{ seq: u64, listeners: Vec<Listener> }, // non-hidden snapshot, only on change
    SuggestionsChanged { suggestions: Vec<Suggestion> },
    ServiceUpdated  { service: ServiceStatus },
    OperationProgress { op: OperationId, instance: InstanceId, step: String, done: bool, error: Option<String> },
    SystemHealth    { health: SystemHealth },        // SystemHealth { checks: Vec<Check>, … } (02 D20)
}
// Log lines are NOT on this bus. Runtime::subscribe_logs(id, backlog) returns an atomic
// (backlog, live receiver) pair per instance. LogLine { instance, stream: Stream, line, ts },
// Stream = Stdout | Stderr (reserved: journald merges them) | System | Setup.
```

Rules: snapshots, not diffs (clients stay simple and a lagged client just takes the next
snapshot). `InstanceUpdated` fires on status/message/issue/upstream/restarts/health-state
changes, not per health sample. SSE starts with a full snapshot (`hello` … `snapshot_complete`);
log lines are only sent for instances named in `/api/events?logs=<id>,…`. A lagging SSE client
gets `resync` and the stream closes; the reconnect starts with a fresh snapshot.

---

## 10. API conventions

- Bind `127.0.0.1:4777` only. No auth in v2.0, but one guard on every route (except the
  `/__lantern/*` gateway, which has its own): `Host` must be `127.0.0.1:4777`, `localhost:4777`
  or `lantern.<tld>`; `Origin` absent or the daemon's own; `Sec-Fetch-Site` only
  `same-origin`/`none`; no CORS headers; JSON bodies require `application/json`; `GET` never
  changes state (DNS-rebinding/CSRF protection — any website you visit could otherwise drive
  `POST /api/instances/x/start`). `15-api-mcp.md` §6 owns the details.
- JSON everywhere. Errors: `{"error": {"code": "snake_case", "message": "...", "details": {}}}`
  with a proper HTTP status.
- Resources: `/api/projects`, `/api/instances`, `/api/listeners`, `/api/services`,
  `/api/profiles`, `/api/templates`, `/api/settings`, `/api/system`, `/api/events`.
- Long operations (start with worktree creation + `pnpm install`) return `202` with an
  `OperationId` and progress over SSE. `?wait=true` blocks: `200` when finished OK, the error
  envelope when it failed, `202` with the current operation if the timeout elapsed first.
- `InstanceId`s containing `@` are used as-is in paths (`/api/instances/myapp@feature-x`),
  percent-encoded by clients.

---

## 11. Subsystem docs and ownership

| Doc | Owns |
|---|---|
| `10-runtime.md` | Supervisor, systemd transient units, start/stop/restart semantics, readiness (port bind check), retries/backoff, logs, health checks, `deploy.*` overrides, port allocation, dependency ordering (`depends_on`) |
| `11-instances.md` | Checkouts & instances: worktree listing/creation/removal, branch → instance lifecycle, setup steps, `.env`/untracked file copying, dependency install strategy, per-branch resources (`${BRANCH}` DBs), cleanup/GC, git edge cases |
| `12-discovery.md` | Listening-socket scanner, process → cwd → checkout matching, unmanaged/adopted instances, port-conflict detection, performance budget |
| `13-system.md` | Caddy (admin API, routes, TLS/local CA, trust), DNS resolver + systemd-resolved, privileges & `lantern init`, systemd user units, shared services (Mailpit/Redis/Postgres), packaging (.deb), install/uninstall, v1 → v2 migration of the system bits |
| `14-config.md` | `lantern.yaml` schema (v1-compatible + new keys), settings (`config.toml`), state.json schema & migrations, workspace scanning, framework detection & templates, profiles |
| `15-api-mcp.md` | Full REST + SSE spec, MCP server (v1-compatible tool surface + instance-aware extensions), intel (docs/OpenAPI ingestion, `call_tool_api` proxying), security of the local API |
| `16-clients.md` | CLI command set & output, GUI (iced) screens/state/architecture, tray (ksni) menu & behavior, client-side reconnect/daemon-down handling |
| `02-decisions.md` | Cross-doc conflict resolutions (authoritative), must-prove-first spikes |
| `20-roadmap.md` | Milestones, migration plan from v1, testing strategy |

Each subsystem doc uses this outline:

1. Purpose & scope (and explicit *not in scope*, pointing at the owning doc)
2. v1 behavior audit: what v1 does (with file references), what to keep, change, drop, and why
3. Design (the bulk): types, algorithms, state machines, sequence of operations
4. Interfaces: what this module exposes to other modules, and what it consumes
5. Failure modes & edge cases, each with the intended behavior
6. Security considerations (if any)
7. Testing strategy (unit / integration / manual)
8. Open questions (with a recommended answer for each)
9. Proposed changes to core architecture (if any)
