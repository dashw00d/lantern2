# 16 — Clients: CLI, GUI, Tray

Status: draft for review. Builds on `01-architecture.md` (authoritative). Terms (Project, Checkout,
Instance, Listener, Unmanaged instance, Service, Profile, Settings, State) are used exactly as
defined there. v1 paths are relative to `.reference/lantern-v1/`.

## Summary (plain language)

Lantern has three front doors: the `lantern` command, a desktop window, and a tray icon. All three
are thin clients of the daemon's HTTP API. None of them owns state, starts a dev server, or reads
a state file. If the daemon is down they say so and offer to start it.

- **CLI** is the complete interface. Everything the GUI can do, the CLI can do, with `--json` for
  scripts and agents. It is written in Rust (`clap`), replaces the v1 bash+curl+jq script, and
  has a 30 ms start-up budget.
- **GUI** is an `iced` app. It keeps a live copy of the daemon's state from one SSE stream, shows
  it grouped by project (instances nested under their project, unmanaged listeners underneath),
  and sends commands over REST. It never guesses the result: a button press shows "working..." until
  the daemon reports the new state.
- **Tray** is `ksni` (StatusNotifierItem) living inside the GUI process. Closing the window
  destroys the window but keeps the process, the tray, and the notifications. "Quit" quits the GUI
  only; the daemon and every dev server keep running.
- **Biggest risks**, all with mitigations below: on Wayland a client cannot hide or raise its own
  window (so we destroy and re-create windows), `iced` has no selectable text or screen-reader
  support (so logs get row selection plus copy buttons), and bringing a window to the front from
  the tray needs an `iced_winit` patch if KWin does not focus new windows by itself.
- **Dropped from v1**: the forms that edited project metadata in the registry (Entry, Run, Routing
  tabs). `lantern.yaml` is the source of truth now; the GUI shows the resolved manifest and opens it
  in your editor.

---

## 1. Purpose & scope

In scope: the `lantern` CLI (command set, output, exit codes, completions, `mcp install`); the
`lantern-client` crate (typed HTTP + SSE client used by CLI and GUI); the GUI (information
architecture, screens, keyboard, iced architecture, theming); the tray; the GUI process model
(single instance, tray-resident, autostart); daemon-down and reconnect behavior for all clients.

Not in scope: REST/SSE/MCP shapes (`15-api-mcp.md`), what `start`/`run branch` do inside the daemon
(`10-runtime.md`, `11-instances.md`), listener detection and adoption rules (`12-discovery.md`),
`lantern init` privileged steps, doctor check contents, services, packaging files
(`13-system.md`), settings and manifest schema (`14-config.md`). Where this doc needs something
from them it states the assumed interface in section 4.

---

## 2. v1 behavior audit

### 2.1 CLI (`cli/lantern`, 1017 lines of bash)

| v1 command | v2 | Notes |
|---|---|---|
| `init` | `init` | Front-end only; logic and privilege handling are `13-system.md`. v1 asked the daemon to do it; v2 needs root once. |
| `scan` | `scan` | Kept. Output now lists new/updated/removed projects. |
| `list` / `ls` | `ls` | Now instances (all checkouts) plus unmanaged listeners. |
| `on` / `activate`, `off` / `deactivate`, `restart` | `on`, `off`, `restart` | Take several targets; wait for completion by default; v1 aliases kept. |
| `status [name]` | `status [target]` | Global summary or one instance, with the "why" for failures. |
| `logs <name>` | `logs [target] [-f]` | v1 always streamed and parsed SSE with a `jq` fork per line. v2 prints a backlog, then follows only with `-f`. |
| `open <name>` | `open [target]` | Browser by default; `--editor`, `--terminal` are new. |
| `mailpit on/off/status/open`, `services` | `services ...` | `mailpit` kept as hidden alias. |
| `health` | `doctor` | `health` kept as hidden alias. Doctor also checks the client side (unit installed, session bus, tray). |
| `config`, `config set k v` | `config get/set/path` | Same data, over `/api/settings`. |
| `mcp url/bridge/install` | `mcp url`, `mcp install` | `bridge` dropped: the target clients speak streamable HTTP natively (`15-api-mcp.md` 3.9). Targets become `claude-code`, `codex`, `cursor`, `json`. |
| `completions bash/zsh/fish` | `completions <shell>` | v1 spawned `curl` + `jq` on every TAB. v2 uses `clap_complete` dynamic completion. |
| (none) | `run`, `setup`, `wt`, `adopt`, `add`, `project`, `manifest`, `schema`, `doctor`, `migrate-v1`, `daemon`, `gui`, `events` | New. |

v1 faults to fix: needs `curl` and `jq`; a `status` forks around ten processes; the "daemon down"
hint says `sudo systemctl start lanternd` (system service; v2 is a user unit); every failure exits
`1`; no machine-readable output; names are projects only (no branch targets).

### 2.2 Electron main process (`desktop/src/main/`)

| v1 file | Behavior | v2 |
|---|---|---|
| `main.ts` | Single-instance lock; second launch focuses window; window-all-closed does not quit. | Keep all three. Single instance via D-Bus name (3.7). |
| `windows.ts` | `close` is intercepted and the window hidden. | Wayland cannot hide a window (3.7). We destroy it instead and keep the process. |
| `tray.ts` + `ipc.ts` | Menu rebuilt from `GET /api/projects` and `/api/services` polled every 10 s; project submenus Open/Start/Stop. Tray actions go **through the renderer** over IPC (`tray:action`), so they fail when the window is gone. "Quit Lantern" POSTs `/api/system/shutdown` (stops all projects), then exits. | Menu is driven by the same SSE state as the GUI. Tray actions are handled by the app's update loop, which runs with no window. Quit stops the GUI only. |
| `daemon.ts` | Polls health every 10 s; notifications "Daemon connected / connection lost". | SSE connection state replaces polling. Keep the "daemon stopped" notification, with a Start action. |
| `ipc.ts` `dialog:pick-folder` | Electron folder dialog. | `rfd` (XDG portal) for Settings > workspace roots. |

### 2.3 React renderer (`desktop/src/renderer/`)

| v1 page / tab | Decision | v2 location |
|---|---|---|
| **Dashboard**: stat cards (total/running/errors), Project Health grid (polled 30 s), Active Routes, Issues list | Replace | One grouped list with summary chips and an inline status dot. Failures show on the row (3.4.2). |
| **Projects**: grid/list toggle, status/type filters, category chips (Tools/Sites/APIs/Projects), Add modal, Scan, hide/show, kind selector, delete | Merge into Dashboard | Filter box and "Show hidden"; kind chips (from `Project.kind`); "Add project..." (folder picker) and "Scan" in the header menu, plus a Suggestions block (14); hide/remove in the selected-row action bar. Grid view and per-row kind selector dropped (kind comes from the manifest). |
| **ProjectDetail > Overview** (domain, port, type, detection, URLs, PID, features) | Keep, expand | Overview tab, plus dependencies and mail chip. |
| > **Entry** (edit name/path/kind/domain/docs/tags), **Run** (edit command/cwd; env read-only), **Routing** (edit domain/tags) | Drop editing | Manifest is the source of truth: Manifest tab (resolved command, cwd, env, domain, tags, read-only) + "Open lantern.yaml". Env editing: see 2.4 #8. |
| > **Mail** (Mailpit enabled flag, SMTP, link) | Fold | A "Mail" chip in Overview; Services page owns Mailpit. |
| > **Logs** (1000-line array copy per line, client timestamps, only while tab mounted) | Keep, rebuild | Server timestamps, 20,000-line ring, virtualized (3.5.6). Log stream stays open whenever the project page is open so the "why" panel can show the last output. |
| > **Docs** (list + `<pre>` viewer + "edit docs list" textarea) | Keep viewer | Rendered with `iced::widget::markdown`. Docs list edit dropped (manifest). |
| > **Endpoints** (method/path/risk table) | Keep | Read-only table; click copies the URL. |
| > **Health** (status, latency, history, Check Now) | Keep | Health tab; the last 10 samples (`HealthSummary.recent`) as bars. |
| > **Dependencies** (depends on / depended by) | Fold | Overview section, clickable. |
| > **Commands (deploy)** (`deploy.*` with run buttons) | Fold, drop buttons | Shown in Manifest tab. Start/Stop already honor `deploy.*` (`10-runtime.md`). |
| **Services** (cards: ports, credentials, Start/Stop, Open UI) | Keep | Same, plus a "not installed" state. |
| **Settings**: workspace roots (+Browse), TLD, PHP-FPM socket, Caddy mode, active profile, "Shutdown All Runtime", About | Keep some | Roots, TLD (read-only), profile, PHP-FPM socket (under Advanced) kept. Caddy mode dropped (v2 always uses the admin API). "Shutdown All Runtime" is split into "Stop all instances" and "Stop daemon". |
| Header search (Ctrl+K focuses search) | Replace | Ctrl+K opens the quick switcher; `/` focuses the filter. |
| Toasts (5 s auto-dismiss), skeleton loaders, error boundary | Keep toasts; drop rest | Errors persist until dismissed. No skeletons: state is local and instant; "Connecting..." is a banner. |

v1 UX defects not to repeat: a failed Start set the row to `error` locally, hiding the true state
(`ProjectDetail.tsx` handlers); two sources of truth (polling plus channels); log timestamps taken
on the client; every log line copied the whole array into the store (`stores/appStore.ts`
`appendLog`); tray broken when the window is closed.

### 2.4 `issue_log.md` gaps

| # | Gap | v2.0 decision |
|---|---|---|
| 1 | Settings save: no error feedback | Rule: **every `Task` result that can fail produces a toast** (persisting until dismissed). Enforced by the `CommandDone` path (3.5.4). |
| 2, 3 | Silent `catch` in health check / tool list | Same rule. No `let _ =` on results in `update`. |
| 4 | Missing animation plugin | N/A (no CSS). |
| 5-7, 9, 10 | Raw buttons, links without labels | N/A, but: every icon-only button has a tooltip and a keyboard path. Screen-reader support is **not** available in `iced` 0.14 (accepted; CLI covers everything). |
| 8 | **Env vars are read-only; no edit UI** | **Not in v2.0.** Env lives in `lantern.yaml` `run.env`; editing comments-preserving YAML from a client needs a daemon-side patch API that `14-config.md` has not defined. v2.0 shows resolved env with its source (manifest / profile / template) and an "Edit lantern.yaml" button. Candidate for v2.1 (open question Q3). |

---

## 3. Design

### 3.1 Decisions at a glance

| Topic | Decision | Why |
|---|---|---|
| GUI toolkit | `iced` 0.14 (`iced::daemon`, `tokio` feature) | Pure Rust, MIT, already chosen in core; `daemon` mode gives windowless tray residency; compositor is dropped when the last window closes. Alternatives and fallback in 3.10. |
| Wayland window lifecycle | Close = destroy window; open = create window. Never hide/minimize-to-tray. | winit 0.30 on Wayland: `set_visible` and `focus_window` are no-ops, un-minimize is ignored. |
| State sync | One SSE connection replays the full state (`hello` ... `snapshot_complete`), then live events; clients are reducers over it | Core rule "snapshots not diffs"; no REST/SSE race (15 section 3.5). |
| Optimistic UI | **No.** Per-instance `Pending` state with spinner and progress step | Daemon owns truth (per-instance op lock, may reject); localhost latency is ~ms. |
| CLI waits | `on/off/restart/run/wt add` wait by default; `--no-wait` detaches; `--wait` accepted explicitly | `lantern on x && curl ...` must work. |
| Tray | `ksni` inside the GUI process; retry registration on failure; icon states idle / active / attention / offline | StatusNotifierItem works on Plasma and GNOME+AppIndicator. |
| Single instance | D-Bus name `dev.lantern.Lantern` implementing `org.freedesktop.Application` | Atomic, no lock files, standard activation + token path. |
| Log view | Own windowed list (fixed row height) inside `scrollable`; 20,000-line ring | `iced` has no virtual list. |
| Fonts | System sans/mono resolved with `fc-match` at startup; Lucide icon font embedded | `cosmic-text` hard-codes "Open Sans"/"Noto Sans Mono" as generic families (verified in source). |
| Theme | Custom Lantern palette, light/dark chosen from the system color-scheme | `iced::system::theme_changes()` + XDG portal (`linux-theme-detection`). |

### 3.2 The `lantern-client` crate

Depends only on `lantern-core` (the `lantern_core::api` types defined in `15-api-mcp.md` 3.2:
`List<T>`, `ProjectSummary`, `ProjectDetail`, `InstanceView`, `Operation`, `OperationResponse`,
`WireEvent`, `LogPage`, `ApiError`, ...). Async (`tokio`, `reqwest` 0.13 with rustls, json, stream).
No iced, no clap. SSE is parsed with `eventsource-stream` 0.2 over `bytes_stream()` (verified to
compile and run with `reqwest 0.13`; parser only, last release 2022 but feature-complete). Do
**not** use `reqwest-eventsource` (pinned to `reqwest 0.12`).

```rust
pub struct Client { /* reqwest::Client + base URL; Clone is cheap */ }

impl Client {
    pub fn new(cfg: ClientConfig) -> Result<Client, ClientError>;     // base: LANTERN_DAEMON_URL, default http://127.0.0.1:4777
    pub async fn ping(&self) -> Result<ApiRoot, ClientError>;          // GET /api, 1 s budget; refuses api_version > built-for
    pub async fn system(&self) -> Result<SystemInfo, ClientError>;     // GET /api/system
    pub async fn health(&self) -> Result<SystemHealth, ClientError>;   // GET /api/system/health (13): doctor checks

    // reads: one REST call each
    pub async fn projects(&self, f: ProjectFilter) -> Result<Vec<ProjectSummary>, ClientError>;
    pub async fn project(&self, id: &ProjectId) -> Result<ProjectDetail, ClientError>;
    pub async fn instances(&self, f: InstanceFilter) -> Result<Vec<InstanceView>, ClientError>;
    pub async fn checkouts(&self, p: &ProjectId) -> Result<Vec<InstanceView>, ClientError>;   // all, incl. unpinned (11)
    pub async fn branches(&self, p: &ProjectId) -> Result<Vec<BranchInfo>, ClientError>;      // (11)
    pub async fn listeners(&self) -> Result<Vec<Listener>, ClientError>;
    pub async fn services(&self) -> Result<Vec<ServiceStatus>, ClientError>;
    pub async fn suggestions(&self) -> Result<Vec<Suggestion>, ClientError>;                  // (14)
    pub async fn manifest(&self, id: &InstanceId) -> Result<ManifestReport, ClientError>;
    pub async fn docs(&self, id: &InstanceId) -> Result<Vec<DocEntry>, ClientError>;          // + doc(), endpoints()
    pub async fn logs(&self, id: &InstanceId, p: LogParams) -> Result<LogPage, ClientError>;  // REST backlog, tail <= 5000
    pub async fn removal_check(&self, id: &InstanceId) -> Result<RemovalCheck, ClientError>;  // (11)
    pub async fn operation(&self, id: &OperationId, wait: Option<Duration>) -> Result<Operation, ClientError>;
    pub async fn settings(&self) -> Result<Settings, ClientError>;                            // + profiles()

    // commands: `Wait { wait: bool, timeout: Option<Duration> }` maps to ?wait=&timeout=
    pub async fn start(&self, id: &InstanceId, w: Wait) -> Result<OperationResponse, ClientError>;  // + stop, restart, setup
    pub async fn stop_all(&self, w: Wait) -> Result<Vec<Operation>, ClientError>;
    pub async fn run_branch(&self, p: &ProjectId, t: RunTarget, o: RunOptions, w: Wait)
        -> Result<OperationResponse, ClientError>;   // RunTarget: Branch | NewBranch{name, base} | Detached | Pr (11)
    pub async fn remove_instance(&self, id: &InstanceId, p: RemoveParams, w: Wait) -> Result<OperationResponse, ClientError>;
    pub async fn pin(&self, id: &InstanceId, pinned: bool) -> Result<InstanceView, ClientError>;
    pub async fn adopt(&self, id: &InstanceId, port: Option<u16>) -> Result<InstanceView, ClientError>; // + unadopt
    pub async fn scan(&self, roots: Option<Vec<PathBuf>>) -> Result<ScanReport, ClientError>;
    pub async fn register_project(&self, r: RegisterProjectRequest) -> Result<ProjectSummary, ClientError>;
    pub async fn service(&self, name: &str, a: ServiceAction) -> Result<ServiceStatus, ClientError>;
    pub async fn patch_settings(&self, patch: serde_json::Value) -> Result<SettingsPatched, ClientError>;
    pub async fn import_v1(&self, r: ImportV1Request) -> Result<ImportReport, ClientError>;
    /* also: patch/unregister project, dismiss_suggestion, activate_profile, intel/doc reads: 1:1 with 15's table */

    pub fn events(&self, o: EventsOptions) -> impl Stream<Item = StreamEvent> + Send + 'static + use<>;
}

pub struct EventsOptions {
    pub state: bool,                // false: log/progress-only stream (?state=false)
    pub logs: Vec<InstanceId>,      // ?logs=a,b (daemon max 8)
    pub tail: usize,                // backlog lines per instance (daemon max 1000); spliced atomically by the daemon
    pub op: Option<OperationId>,    // ?op=: with state=false still delivers this operation's progress
    pub reconnect: bool,            // true for GUI/tray and `logs -f`; false for one-shot streams
}
pub enum StreamEvent {
    Connecting { attempt: u32 },
    Connected,                                 // HTTP 200; `Hello` follows
    Event(WireEvent),                          // Hello, state events, SnapshotComplete, live events. Resync is consumed here.
    Lost { error: String, daemon_unreachable: bool, attempt: u32, retry_in: Duration },
}
```

Rules the implementation must follow:

- `ClientError`: `DaemonUnreachable` (refused / no route), `Timeout`, `Api(ApiError + HTTP status)`
  parsed from the error envelope, `Protocol`. Mapping to CLI exit codes lives in `lantern-cli`.
- `events()` backoff: 0.5, 1, 2, 4, 8 s, then 15 s, each +-20 % jitter; the attempt counter resets
  after a connection has been up 10 s. A `resync` event (the stream ends right after it) or a clean
  close reconnects **immediately**, without backoff (15 section 3.5). Unknown `type` values are skipped
  (forward compatibility). A stream with no bytes for 40 s is treated as lost (the daemon sends
  `: ping` every 15 s).
- Timeouts: connect 2 s; total 10 s per request, except `wait=true` calls (`timeout` + 5 s) and
  streams. `Origin` is never sent; `Host` is the daemon's own address (the daemon rejects others).
- `--json` output of the CLI is the serde form of these `lantern_core::api` types, so 15's REST
  documentation also describes CLI JSON.

### 3.3 The CLI

#### 3.3.1 Conventions

- **Targets.** `myapp` (primary instance); `myapp@feature-x` (instance id) or `myapp@feature/x`
  (any branch name of that project); `@feature-x` (branch of the project containing the current
  directory); nothing (the instance containing the current directory, where allowed). Resolution is
  client-side over `instances()`: exact `InstanceId`; else exact branch name or branch slug within
  the project (the same rule as MCP, 15 section 3.7.3); else a unique prefix; else exit 4 listing
  candidates. "Current directory" = longest `checkout.path` prefix of the canonical cwd.
- **Streams**: data on stdout; progress, spinners, hints on stderr (TTY only). Colors only when
  stdout is a TTY and neither `--no-color` nor `NO_COLOR` is set. Status is never color alone.
- **Glyphs** (shared with GUI and tray): `●` running, `◐` preparing/starting/stopping,
  `▲` unhealthy, `✖` crashed, `○` stopped, `◇` external, `?` needs-config.
- **`--json`**: one JSON document on stdout for one-shot commands; NDJSON for `logs -f` and
  `events`. Errors go to stderr as the core envelope plus `"exit_code"`. Nothing else on stdout.
- **Waiting.** `on`, `off`, `restart`, `run`, `setup`, `wt add` wait for the operation by default.
  With `--json` or a non-TTY they send one `POST ...?wait=true&timeout=N` and print the final
  `OperationResponse`. On a TTY they POST without `wait` (202 + `Operation`), then show live steps
  from `GET /api/events?state=false&op=<id>` while `GET /api/operations/<id>?wait=true&timeout=N`
  provides the authoritative result (a broken SSE only loses the step text). `--no-wait` prints
  the operation id after the 202. `--timeout` defaults to 120 s. A repeated request of the same
  kind joins the in-flight operation (15 section 3.3), so re-running after Ctrl+C re-attaches;
  Ctrl+C only stops *waiting*, and says so. Several targets run in parallel; the exit code is the
  worst result.
- **Terminal output safety**: log text keeps SGR color sequences on a TTY; every other escape or
  control sequence is removed (a dev server must not retitle or rewrite your terminal). Stripped
  entirely when not a TTY.
- **Bare `lantern`** runs `lantern ls`.

#### 3.3.2 Command tree

```
lantern [--json] [--host URL] [--no-color] [--start-daemon] [-v...]
├─ ls [PROJECT] [-r|--running] [-a|--all] [--no-listeners]                alias: list
├─ status [TARGET]                                                        alias: st
├─ on <TARGET>... [--open] [WAIT]                                         aliases: start, up
├─ off <TARGET>... | --all [-y] [WAIT]                                    aliases: stop, down
├─ restart <TARGET>... [WAIT]
├─ run <PROJECT>@<BRANCH> [--new [--base REF]] [--detach] [--pr N] [--no-start] [--open] [WAIT]
├─ setup <TARGET> [--force] [WAIT]                                        re-run dependency setup (11)
├─ logs [TARGET] [-f] [-n N=200] [--stream stdout|stderr] [-t]            alias: log
├─ open [TARGET] [--editor | --terminal] [--print] [--no-start]
├─ wt                                                                     alias: branch (11)
│   ├─ ls [PROJECT]            all checkouts, visible or not
│   ├─ branches [PROJECT]      branches, including ones with no checkout
│   ├─ add <PROJECT> <BRANCH> [--new [--base REF]] [--detach] [--pr N [--update]] [WAIT]   = run --no-start
│   ├─ pin|unpin <TARGET>      show / hide an unpinned worktree
│   ├─ relabel <TARGET> <SLUG>
│   ├─ gc [PROJECT] [--apply] [--fetch]
│   └─ rm <TARGET> [--force] [--delete-branch] [-y]
├─ adopt <TARGET|PORT> [--port N]    |   unadopt <TARGET>
├─ scan [PATH...]
├─ add [PATH] [--write-manifest]                                          register a project (14)
├─ project  rm <ID> [--keep-running] [--remove-worktrees] | enable <ID> | disable <ID> | show <ID>
├─ manifest  check [PATH] | show [TARGET]                                 `check` works without the daemon (14)
├─ schema [manifest|profile|config]                                       offline JSON Schema (14)
├─ services [start|stop|restart|open <NAME>]                              alias: svc; hidden alias: mailpit
├─ doctor [--fix]                                                         hidden alias: health
├─ init [-y]                                                              (13)
├─ migrate-v1 [--from DIR] [--dry-run] [-y] [--force]                     (14)
├─ daemon [run | start | stop | restart | status | logs [-f] | enable | disable]
├─ gui [--tray] [--quit] [--foreground] [TARGET]
├─ mcp  url | install <claude-code|codex|cursor|json> [--scope user|project] [--force] [--print]
├─ config  [get <KEY> | set <KEY> <VALUE> | path | reload]
├─ events [--logs TARGET]                                                 NDJSON passthrough of /api/events
└─ completions <bash|zsh|fish|elvish|powershell>
WAIT = [--no-wait | --wait] [--timeout SECS]
```

Compile-checked as a clap derive tree in the scratch crate: `env = "LANTERN_DAEMON_URL"` needs clap's `env`
feature; `Client::events` needs `+ use<>` in edition 2024 so the stream does not borrow `&self`.

#### 3.3.3 Command semantics

| Command | Behavior |
|---|---|
| `ls` | Instances grouped by project, then unmanaged listeners. Default hides disabled projects and instances the daemon marks not visible (unpinned, stopped, non-primary; 11); `-a` shows all, `-r` only active ones (running, starting, unhealthy, crashed, external). A failing row gets a second line with the reason. |
| `status` | No target: daemon + system health + counts + services. With target: one instance, plus a **Why** block when not running (`message`, `issue`, last exit, retry countdown). |
| `on` | Start existing instance(s); never creates a checkout (exit 4, suggests `lantern run`). `--open` opens the browser when ready. |
| `off` | Stop. `--all` = `POST /api/system/stop-all`; asks on a TTY, exits 2 on a non-TTY without `-y`. Never touches unmanaged listeners. |
| `run` | Run a branch (11). `Branch` must exist locally or on a remote; a typo fails with exit 4 and a hint, **creating a branch needs `--new`** (`--base` = start point). `--pr N` checks out a PR via `gh` if available; `--detach` for a commit-ish. Creates the managed worktree, runs setup, starts it. |
| `setup` | Re-run dependency setup for a checkout (needed when `setup_stale` is shown). |
| `logs` | `-n` lines (default 200, max 5000) from `GET /api/instances/{id}/logs`, then exit. `-f` uses `GET /api/events?state=false&logs=<id>&tail=N` so backlog and live lines join without a gap. After a daemon restart it reconnects with `tail=0` and prints `-- reconnected; lines in between may be missing --` on stderr. Line format `HH:MM:SS out text` / `HH:MM:SS err text`; `-t` full timestamps. No target = cwd. |
| `open` | Browser (`open` crate / `xdg-open`) at the instance URL; starts a stopped instance first unless `--no-start`. `--editor` / `--terminal` open the checkout directory using `[clients]` settings (3.4.7). `--print` prints the URL/path only. |
| `wt rm` | Calls `removal_check` first and prints blockers (dirty files, unpushed commits, running, locked); `--force` overrides dirty/unpushed/orphaned only. Only `managed` checkouts can be removed (11: `409 not_managed`); for others the CLI suggests `wt unpin`. |
| `wt gc` | Lists removal suggestions (merged, upstream gone, stale); `--apply` removes them after the same checks. |
| `adopt` | Adopt an `External` instance (route to the process Lantern did not start). A bare port is mapped to its matched instance; an unmatched port prints "no project here; run `lantern add <cwd>`". |
| `add`, `project`, `manifest`, `schema` | `add` registers a directory (default cwd), `--write-manifest` also writes a starter `lantern.yaml` (14). `project rm` unregisters (stops it unless `--keep-running`; never deletes files). `manifest check` validates offline with `lantern-core` and prints diagnostics with line:col; exit 8 on errors. `manifest show` prints the resolved manifest with provenance. |
| `doctor` | Client-side checks (unit installed and active, daemon reachable, API version skew, session bus, tray watcher) plus `GET /api/system/health`. `--fix` runs user-level fixes and prints the single root command for the rest. |
| `daemon` | Bare `lantern daemon` = `daemon run` = what the systemd unit executes: `lantern-cli` returns `CliAction::RunDaemon`, the binary calls `lantern-daemon` (cli never depends on daemon). The others wrap `systemctl --user` / `journalctl --user -u lantern`. |
| `gui` | See 3.7. Detaches from the terminal unless `--foreground`. |
| `init`, `migrate-v1` | `init` elevates with `pkexec`/`sudo` for the privileged steps (13). `migrate-v1` calls `POST /api/import/v1`; default flow = dry-run, print the plan, ask, apply (14). |
| `mcp install` | Pure string logic is `lantern_core::mcp_install` (15 3.9); the CLI does the IO: back up to `<file>.lantern-bak`, atomic write, keep file mode, idempotent. `claude-code` runs `claude mcp add --transport http --scope user lantern http://127.0.0.1:4777/mcp` and never edits `~/.claude.json`. Afterwards it POSTs an MCP `initialize` and says if the daemon is down (the install still succeeds). `--print` writes nothing. |
| `config` | `get`/`set` over `/api/settings` (JSON merge-patch; prints `restart_required` keys); `reload` = `POST /api/settings/reload`; `path` prints `config.toml`. |

#### 3.3.4 Example output

```
$ lantern ls
PROJECT      INSTANCE          BRANCH       STATUS        PORT   URL
myapp        myapp             main         ● running     43127  https://myapp.glow
             myapp@feature-x   feature/x *  ● running     43130  https://feature-x.myapp.glow
             myapp@fix-login   fix/login    ○ stopped     -      https://fix-login.myapp.glow
billing-api  billing-api       main         ✖ crashed     -      https://billing-api.glow
               exit 1, retry in 4s (2/5): port 3000 is held by `node` (pid 4821) in ~/dev/other-app
docs-site    docs-site         main         ○ stopped     -      https://docs-site.glow

UNMANAGED LISTENERS
PORT   LABEL        PID    CWD            MATCH
5173   Vite         5120   ~/dev/myapp    myapp@main   ◇ external
9000   php server   7311   ~/tmp/test     -
```

```
$ lantern run myapp@feature/login --open
◐ creating worktree ~/.local/share/lantern/worktrees/myapp/feature-login      (stderr, live)
◐ setup: pnpm install
◐ starting
● myapp@feature-login  running in 41.8s
  https://feature-login.myapp.glow   127.0.0.1:43133
```

```
$ lantern on billing-api
✖ billing-api  crashed while starting (exit 1)
  port 3000 is held by `node` (pid 4821) in ~/dev/other-app
  next: lantern logs billing-api -n 50      or      lantern adopt 3000
$ echo $?
5
```

```
$ lantern doctor
  ✔ daemon           running (pid 2210, v0.1.0, api 1)
  ✔ systemd unit     lantern.service enabled, active
  ✔ caddy            admin API reachable
  ✖ dns              *.glow does not resolve to 127.0.0.1
        fix (root): sudo lantern init
  ▲ firefox          profile 'default-release' has its own certificate store
        fix: certutil -A -n Lantern -t C,, -d sql:~/.mozilla/firefox/xxxx.default-release -i <ca.pem>
  ✔ session bus / tray watcher present
2 problems.                                                              (exit code 8)
```

#### 3.3.5 Exit codes

| Code | Meaning | Examples |
|---|---|---|
| 0 | Success | |
| 1 | Unspecified error | I/O error, unexpected response |
| 2 | Usage error | clap parse failure; `off --all` on a non-TTY without `-y` |
| 3 | Daemon not reachable | connection refused, unit missing or failed |
| 4 | Not found / ambiguous | no such project, instance, branch, listener |
| 5 | Operation failed | crashed while starting; setup step failed; readiness failed |
| 6 | Timed out | `--wait` exceeded `--timeout` (the operation may still be running) |
| 7 | Conflict / refused | `operation_in_progress`, `dirty_worktree`, `port_unavailable`, `not_managed` |
| 8 | Problems found | any `doctor` check `fail`, `manifest check` errors (warnings alone exit 0) |

#### 3.3.6 Daemon down

Every command except `daemon *`, `completions`, `schema`, `manifest check`, `mcp url`, `config path`
and `--help/--version` needs the daemon. On `DaemonUnreachable`:

```
error: can't reach the Lantern daemon at http://127.0.0.1:4777
  start it:    systemctl --user start lantern      (or: lantern daemon start)
  not set up?  sudo lantern init
```

- On a TTY (and not `--json`): prompt "Start it now? [Y/n]", run `systemctl --user start lantern`,
  poll `ping()` for up to 10 s, then continue the original command.
- `--start-daemon` / `LANTERN_START_DAEMON=1`: the same without a prompt (scripts, agents).
- Otherwise exit 3; with `--json`, `error.details.hint` carries the start command.
- If the unit does not exist (`systemctl --user cat lantern.service` fails) the hint is
  `sudo lantern init`. Dev servers survive the daemon; `lantern daemon stop` says so.

#### 3.3.7 Completions

`lantern completions <shell>` prints the registration snippet (same as `COMPLETE=<shell> lantern`);
packaging installs the generated files into the standard completion directories. `<TARGET>`
completers do one blocking `GET /api/instances` with a 300 ms budget and print nothing on any error.
Verified at runtime in the scratch crate: `COMPLETE=bash <bin>` emits a working registration script
and completing `on my<TAB>` returns `myapp` and `myapp@feature-x`. The feature is `unstable-dynamic`
in `clap_complete` 4.6.11: pin the exact version and keep a static `aot::generate` fallback.

#### 3.3.8 Implementation notes

`lantern-cli` exposes `pub fn main(argv) -> ExitCode` and `enum CliAction { Done(ExitCode),
RunDaemon(DaemonArgs), RunGui(GuiArgs) }`; the binary crate dispatches the last two so the
dependency rule holds. Call `CompleteEnv::with_factory(...).complete()` first in `main`. Tables are
hand-rolled (`unicode-width`); colors via `anstream`/`anstyle` (already in clap's tree). `-v` /
`RUST_LOG` enable `tracing` on stderr. Startup budget: `lantern ls` < 30 ms with a warm daemon;
winit/wgpu `dlopen` the Wayland, xkbcommon and Vulkan libraries, so the CLI must not link them at
load time (check with `ldd`). If load time or size ever hurts the CLI, split the GUI into a
`lantern-gui` executable that `lantern gui` execs (Q6).

### 3.4 GUI information architecture

#### 3.4.1 The six flows and where they live

| Flow | Path | Clicks / keys |
|---|---|---|
| (a) Glance at everything, incl. unmanaged | Dashboard; tray tooltip and icon | 0 (it is the home screen) |
| (b) Start/stop | Row `[▶]/[■]`; tray submenu; Quick switch | 1 click; or `Ctrl+K`, type, `Enter` |
| (c) Run this branch | Header "Run branch..." (`Ctrl+B`); project page > Branches tab; Quick switch `myapp@`; tray "Run branch..." | pick project, type branch, `Enter` |
| (d) Live logs | Row `[≡]`; select row + `L`; project > Logs; tray "Logs" | 1 click |
| (e) Open in browser / editor / terminal | Row `[⇗]` / `O`; project page buttons / `E` / `T` | 1 click |
| (f) Why did it fail | Row second line; project > Overview "Why" panel; crash notification | 0-1 click |

#### 3.4.2 Dashboard (home)

```text
┌─ Lantern ──────────────────────────────────────────────────────────────────────────────────────────┐
│ [ Filter...  / ]                      [ Run branch...  Ctrl+B ]   [ Quick switch  Ctrl+K ]         │
│ 3 running  ·  1 needs attention  ·  2 unmanaged            Caddy ●  DNS ●  TLS ●   daemon ●        │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ ▾ myapp                                                              ~/dev/myapp                   │
│   ● myapp           main           :43127  https://myapp.glow                 [■] [⇗] [≡]          │
│   ● ↳ feature-x     feature/x *    :43130  https://feature-x.myapp.glow       [■] [⇗] [≡]          │
│   ○ ↳ fix-login     fix/login      -       https://fix-login.myapp.glow       [▶] [⇗] [≡]          │
│ ▾ billing-api                                                        ~/dev/billing-api             │
│   ✖ billing-api     main           -       https://billing-api.glow           [▶] [⇗] [≡]          │
│       exit 1, retry in 4 s (2/5). Port 3000 is held by `node` (pid 4821) in ~/dev/other-app [Show] │
│ ▸ docs-site                                 1 stopped                                              │
│ ▾ Other listeners (unmanaged)  2                                                                   │
│   ◇ :5173  Vite        ~/dev/myapp  -> myapp@main (external)          [Adopt]   [⇗]                │
│   ◇ :9000  php server  ~/tmp/scratch  (no project)               [Add project]   [⇗]               │
│ ▾ Detected projects  1                                       (not registered yet)                  │
│   ~/dev/notes-api   nextjs, high confidence                       [Add]  [Dismiss]                 │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Services   ● Mailpit [■]    ○ Redis [▶]    ○ Postgres [▶]                                          │
│ [▶] start  [■] stop  [⇗] open  [≡] logs        selected row:  R restart   E editor   T terminal    │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

- Group order: projects alphabetical (stable, for muscle memory). Inside a project: primary first,
  then other instances running-first, then by name. A project with no running instance may be
  collapsed (`▸`); collapse state persists in `gui.json`.
- Summary chips ("1 needs attention") are filters. Header shows system health dots (Caddy/DNS/TLS
  from `SystemHealth`) and daemon state; a red dot opens the Doctor screen.
- Row = status glyph, id (`↳` marks non-primary), branch (`*` = dirty; a "branch changed" badge when
  the checkout moved to another branch, 11), port, URL, three buttons. Only instances the daemon
  marks visible are listed (primary, running/external, pinned). The failure line appears for
  `Crashed`, `NeedsConfig`, `Unhealthy`: `RuntimeState.message` plus `issue` and `next_retry_at`
  ("retry in 4 s (2/5)") from 10. A `setup_stale` instance shows "dependencies changed" with a
  **Re-run setup** button.
- Selecting a row (click or arrows) shows an **action bar** for the rest: Restart, Copy URL,
  Editor, Terminal, Hide/Show project, Remove..., Adopt (for external). No context menus: `iced`
  has no native popup menu, and a bar is keyboard-friendly.
- "Other listeners" lists every `Listener` with `managed == false` that discovery does not hide
  (12): `label` (Vite, uvicorn, ...), port, cwd, an "all interfaces" tag when `exposure` is not
  loopback, Docker badge. A matched one shows `project@branch (external)` with **Adopt**
  (**Unadopt** when `adopted`); an unmatched one with a suggestion (14) shows **Add project**.
  (Killing a foreign process is not offered in v2.0; Q4.)
- "Detected projects" (from `GET /api/suggestions`, 14): path, framework, confidence, **Add** and
  **Dismiss**. Hidden when empty. Refetched after each scan and whenever listeners change.
- Services strip at the bottom: name, state, one-click toggle.
- Empty states: no projects ("Add a project or run `lantern scan`"), no match for filter, not
  connected (3.8).

#### 3.4.3 Project page

```text
┌─ Lantern  >  myapp ────────────────────────────────────────────────────────────────────────────────┐
│ ‹ Dashboard        myapp   [project]   ~/dev/myapp                                                 │
│ Instance  [ myapp@feature-x  (feature/x)        ▾ ]  ● running 5m   :43130   [Copy URL]            │
│ [■ Stop]  [↻ Restart]  [⇗ Open https://feature-x.myapp.glow]  [Editor]  [Terminal]                 │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Overview   Branches   Logs   Health   Docs (3)   Endpoints (12)   Manifest                         │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Status      ● Running since 14:02 (5m)    restarts 0    health ● 12 ms                             │
│ Checkout    ~/.local/share/lantern/worktrees/myapp/feature-x   (managed, clean)                    │
│ Branch      feature/x  abc1234                                                                     │
│ Depends on  postgres ●   redis ○ [Start]            Depended on by: -                              │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ Why it isn't running   (only shown when stopped/crashed/needs-config)                              │
│   Exited with code 1 after 2 s. Restarting in 4 s (attempt 2 of 5).                                │
│   Port 3000 is held by `node` (pid 4821) in ~/dev/other-app.            [Show listener]            │
│   Last output:  Error: listen EADDRINUSE: address already in use :::3000                           │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Tabs: **Overview** (above), **Branches** (the "Branches and checkouts" panel of 11: every branch
with its checkout kind, status and [Run]/[Start]/[Stop]/[Pin]/[Remove...]; the removal dialog is
built from `RemovalCheck` and lists blockers before enabling the confirm button; "Run branch..."
opens the dialog in 3.4.5), **Logs**, **Health** (status, latency, the last 10 samples as bars,
"Check now"), **Docs** (list + markdown viewer; links open only if `http(s)`), **Endpoints**
(method, path, description, risk; click copies the full URL), **Manifest** (resolved `run.cmd`,
cwd, env with provenance, `deploy.*`, diagnostics with line:col from `ManifestReport`, "Open
lantern.yaml" and "Open checkout in editor"). Tabs with no content (Docs, Endpoints) are hidden,
as in v1. `Ctrl+Tab`/`Ctrl+Shift+Tab` or `Alt+1`..`Alt+7` switch tabs.

The "Why it isn't running" panel answers flow (f): `message`, `issue` (`code`, `detail`, `hint`
from 10; for `PortInUse` the GUI also finds the listener by port in the mirror and offers **Show
listener**; for an invalid manifest, **Open lantern.yaml**), last exit (`ExitInfo`: code or signal,
when, uptime), the retry countdown, and the last 8 output lines from the log buffer.

#### 3.4.4 Logs tab

```text
┌─ myapp@feature-x  >  Logs ─────────────────────────────────────────────────────────────────────────┐
│ Overview  Branches  [Logs]  Health  Docs  Endpoints  Manifest                                      │
│ [ all streams ▾ ]  [ Filter text... ]  [x] Follow   [Copy]  [Save...]  [Clear view]  [Selectable]  │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 14:02:11  ready in 612 ms                                                                          │
│ 14:02:11  ➜  Local:   http://127.0.0.1:43130/                                                      │
│ 14:03:40  warn: deprecated config key `foo`                            (stderr rows are tinted)    │
│ 14:03:41  GET /api/users 200 14ms                                                                  │
│                                                                        [ ↓ Jump to latest ]        │
├────────────────────────────────────────────────────────────────────────────────────────────────────┤
│ 12,408 lines buffered (cap 20,000)       click = select row, Shift+click = range, Ctrl+C = copy    │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Follow mode sticks to the bottom; scrolling up pauses it and shows "Jump to latest". Filter is a
case-insensitive substring over the buffered lines. "Selectable" swaps the list for a read-only
`text_editor` holding a snapshot of the last 5,000 filtered lines (gives real mouse selection,
`Ctrl+A`, `Ctrl+C`); "Copy" copies the filtered buffer; "Save..." writes it to a file chosen with
the portal dialog. ANSI SGR escapes are stripped on ingestion in v2.0 (colored rendering is
v2.1).

#### 3.4.5 Run branch dialog and Quick switcher

```text
┌─ Run branch ─────────────────────────────────────────────────────────────┐
│ Project   [ myapp                        ▾ ]                             │
│ Branch    [ feat|                        ▾ ]                             │
│           ┌────────────────────────────────────────────┐                 │
│           │ ● feature/x            running             │                 │
│           │ ● feature/login        has instance        │                 │
│           │   feature/billing      local               │                 │
│           │ ↓ origin/feature/search                    │                 │
│           │ + Create new branch "feat" from main       │                 │
│           └────────────────────────────────────────────┘                 │
│ [x] Start after creating      [x] Open in browser when ready             │
│                                          [ Cancel ]   [ Run  Enter ]     │
└──────────────────────────────────────────────────────────────────────────┘
```

The dropdown is `iced::widget::combo_box` (searchable). Entries come from `branches(project)`:
local branches, remote-only branches (`↓`), branches that already have an instance (`●`), and a
final "Create new branch ..." row when the typed text matches nothing. Choosing a branch that
already has an instance just starts it. Options persist in `gui.json`.

```text
┌─ Quick switch  (Ctrl+K) ─────────────────────────────────────────────┐
│ > myapp fea|                                                         │
├──────────────────────────────────────────────────────────────────────┤
│   ● myapp@feature-x       running     Enter: open   Ctrl+Enter: logs │
│   ○ myapp@fix-login       stopped     Enter: start                   │
│   ↓ myapp@feature/search  no checkout Enter: create + start          │
│   Actions:  Stop all running  ·  Settings  ·  Doctor                 │
├──────────────────────────────────────────────────────────────────────┤
│ Type `myapp@` to list a project's branches. Esc closes.              │
└──────────────────────────────────────────────────────────────────────┘
```

Quick switcher (`Ctrl+K`): one text input over a result list; fuzzy match (`nucleo-matcher`) on
`project@branch`, branch names and action names. Query forms: free text (instances + actions),
`myapp@` (all branches of that project, including ones without a checkout), `>` (actions only).
`Enter` = primary action (start if stopped, open in browser if running, create + start if no
checkout), `Ctrl+Enter` = open the project page on Logs, `Alt+Enter` = start and open the browser
when ready. Arrow keys move, `Esc` closes.

#### 3.4.6 Services, Settings, Doctor, Daemon-down

```text
┌─ Services ─────────────────────────────────────────────────────────────────────────────────────────┐
│ ┌───────────────────────────┐ ┌───────────────────────────┐ ┌─────────────────────────┐            │
│ │ Mailpit          ● running│ │ Redis            ○ stopped│ │ Postgres   ! not found  │            │
│ │ smtp 1025  web 8025       │ │ port 6379                 │ │ postgresql not installed│            │
│ │ [■ Stop]  [⇗ Open UI]     │ │ [▶ Start]                 │ │ [Copy install command]  │            │
│ └───────────────────────────┘ └───────────────────────────┘ └─────────────────────────┘            │
│ Credentials (shown when the service defines them):  user: postgres   password: [Copy]              │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

```text
┌─ Settings ─────────────────────────────────────────────────────────────────────────────────────────┐
│ General                                                                                            │
│   Workspace roots     ~/dev  [x]     ~/work  [x]     [ + Add folder... ]                           │
│   Managed worktrees   [ ~/.local/share/lantern/worktrees      ]                                    │
│   Domain suffix       .glow   (read-only here; changing it re-runs setup)                          │
│   Active profile      [ default ▾ ]                                                                │
│ Interface                                                                                          │
│   Theme               (•) Follow system   ( ) Light   ( ) Dark                                     │
│   Start Lantern at login (tray only)   [ on ]                                                      │
│   Notifications       crash [on]    branch ready [ only slow starts ▾ ]                            │
│   Editor command      [ auto: code {path}       ]   Terminal  [ auto: konsole --workdir {path} ]   │
│ Daemon                                                                                             │
│   Version 0.1.0 · API 1 · up 3h 12m · pid 2210      [Restart daemon]  [Stop daemon]                │
│   [Stop all instances]    Paths: config, state, logs  [Copy diagnostics]                           │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Settings applies changes immediately (no Save button); each control shows an inline error on
failure. Keys are owned by `14-config.md`; the GUI-only ones live in `gui.json` (theme,
notification preferences, start-at-login, window size, collapsed groups, recent branches).

```text
┌─ Welcome to Lantern - setup needed ────────────────────────────────────────────────────────────────┐
│ A few things must be set up before https://*.glow works. Dev servers are unaffected.               │
│                                                                                                    │
│   ✔ Daemon               running (pid 2210)                                                        │
│   ✔ Caddy                running on :443 and :80                                                   │
│   ✖ DNS                  *.glow does not resolve to 127.0.0.1                                      │
│         The systemd-resolved drop-in is missing.                       [ Fix... ]                  │
│   ✖ HTTPS certificate    Lantern's local CA is not trusted by this system                          │
│         Needs a one-time administrator step.                           [ Fix... ]                  │
│   ▲ Firefox              profile 'default-release' keeps its own trust store                       │
│         certutil -A -n Lantern -t C,, -d sql:... -i <ca.pem>           [ Copy command ]            │
│                                                                                                    │
│ [ Run setup  (asks for your password) ]   [ Re-check ]   [ Skip for now ]                          │
│ Setup output  ▾                                                                                    │
└────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Shown instead of the Dashboard on first run and whenever `SystemHealth` has a failing check; a
"Skip for now" returns to the Dashboard with a red health dot. Rows come straight from the
daemon's `HealthCheck { id, title, status, detail, fix }` (13-system). **Fix...** expands the
explanation; **Run setup** executes `pkexec <abs path to lantern> init --yes` (Polkit prompt),
streams its output into the pane, and re-checks on exit 0. "Copy command" is always offered
(`sudo lantern init`).

```text
┌─ Lantern ────────────────────────────────────────────────────────────┐
│                                                                      │
│             The Lantern daemon isn't running                         │
│             Dev servers you started keep running while it is down.   │
│                                                                      │
│             [ Start daemon ]    [ Show why ]    [ Copy command ]     │
│             systemctl --user start lantern                           │
│                                                                      │
│             Retrying automatically... next attempt in 4 s            │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

"Start daemon" runs `systemctl --user start lantern`, then retries the SSE connection at once.
"Show why" runs `systemctl --user status lantern` and `journalctl --user -u lantern -n 30` and
shows the text. If the unit does not exist, the screen switches to the Doctor view.

#### 3.4.7 Keyboard

Single-key shortcuts only fire when no text input has focus (iced reports the event as `Ignored`).

| Key | Action | Key | Action |
|---|---|---|---|
| `Ctrl+K` | Quick switcher | `Ctrl+B` | Run branch dialog |
| `/` | Focus filter | `Esc` | Close overlay / clear filter / back |
| `Up` `Down` `j` `k` | Move selection | `Left` `Right` | Collapse / expand project |
| `Enter` | Open project page | `Space` | Start/stop selected |
| `r` | Restart | `o` | Open in browser |
| `e` | Editor | `t` | Terminal |
| `l` | Logs | `c` | Copy URL |
| `1`-`4` | Dashboard / Services / Settings / Doctor | `Ctrl+W` | Close window (tray stays) |
| `F5` | Force resync | `Ctrl+Q` | Quit GUI (not the daemon) |

Editor/terminal commands are run **by the GUI/CLI process** (it has the graphical session
environment; the daemon, a systemd user service, may not). Templates come from `[clients] editor`
and `terminal` in `config.toml` (default `auto`): editor tries `code`, `codium`, `zed`, `kate`,
then `xdg-open`; terminal tries `$TERMINAL`, then by desktop: `konsole --workdir {path}`,
`gnome-terminal --working-directory={path}`, `kitty --directory {path}`, `foot --working-directory={path}`.
The template is split with `shlex` first, then `{path}` is substituted into one argv element.
Never `sh -c`. Children are spawned in their own process group so they outlive the GUI.

### 3.5 iced architecture

Elm architecture, with four rules for implementers: (1) `view` is pure and does no I/O;
(2) `update` never blocks (no file, process or network work inline; return a `Task`);
(3) every REST call goes through one `Command::execute` path so errors always reach a toast;
(4) the daemon mirror (`Mirror`) is only ever changed by `reduce(WireEvent)`, never by a button handler.
The API calls used below were type-checked in a scratch crate (Rust 1.99): `iced::daemon` with
`Subscription::run/run_with`, `window::*`, `scrollable`, `combo_box`, `markdown`, `text_editor`; `ksni`
0.3.7; `zbus` 5.19 single-instance (also run against the session bus); `notify-rust` 4.18; the clap tree.
Type names such as `Mirror`, `Effect`, `Command` are design, not verified code. Cargo:
`iced = { version = "=0.14.0", features = ["tokio", "markdown", "advanced"] }` (defaults keep wgpu,
tiny-skia, wayland, x11, linux-theme-detection), `lucide-icons` with `iced`.

Layout of `lantern-gui/src`: `lib.rs` (bootstrap), `app.rs` (State, Message, update, subscription),
`model.rs` (`Mirror`, `reduce`, derived rows), `views/*` (one file per screen), `widgets/*`
(log list, status glyphs, icons, theme), `platform/*` (single_instance, tray, notify, open, fonts,
autostart, prefs).

#### 3.5.1 Bootstrap order

```rust
pub fn run(args: GuiArgs) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<UiEvent>();
    let (tray_tx, tray_rx) = tokio::sync::watch::channel(TraySnapshot::default());

    // 1. Claim the D-Bus name before any GPU work. A second instance activates the first and exits here.
    let _bus = match rt.block_on(single::acquire(ui_tx.clone(), !args.tray))? {
        Acquired::Secondary => return Ok(()),
        Acquired::Primary(conn) => conn,                 // must live as long as the process
    };
    // 2. Tray on the background runtime. It retries until a StatusNotifierHost exists.
    rt.spawn(tray::run(ui_tx.clone(), tray_rx));
    // 3. iced blocks the main thread. Its own tokio executor runs Tasks (reqwest works inside them).
    gui::install_ui_receiver(ui_rx);                    // static slot read by Subscription::run
    let _guard = rt.enter();
    app::run_iced(Client::new(ClientConfig::from_env())?, args, ui_tx, tray_tx)
}

pub fn run_iced(client: Client, args: GuiArgs, ui_tx: UnboundedSender<UiEvent>, tray_tx: watch::Sender<TraySnapshot>) -> iced::Result {
    iced::daemon(move || App::boot(client.clone(), &args, ui_tx.clone(), tray_tx.clone()), App::update, App::view)
        .title(App::title).theme(App::theme).subscription(App::subscription)
        .font(lucide_icons::LUCIDE_FONT_BYTES)           // + default_font(system sans) from 3.5.7
        .run()
}
```

`iced::daemon` starts with **no window** and keeps running when the last one closes; `boot` opens a
window unless `--tray`. `Subscription::run` takes a plain `fn()`, so the tray/D-Bus receiver is
handed over through a `static Mutex<Option<UnboundedReceiver<UiEvent>>>` taken once.

#### 3.5.2 State

```rust
pub struct App {
    // daemon mirror: rebuilt on every connect, patched by events, never edited by handlers
    model: Mirror,                      // live + staging Model: projects, instances, listeners, services, health, suggestions
    conn: Conn,                         // Connecting | Live | Down { since, attempt, retry_at, error }
    // ui
    page: Page,                         // Dashboard | Project{project, instance, tab} | Services | Settings | Doctor
    filter: String, selection: Option<RowKey>, collapsed: HashSet<ProjectId>,
    pending: HashMap<InstanceId, Pending>,           // Pending { kind: Start|Stop|Restart, since, op, step }
    overlay: Option<Overlay>,                        // Palette | RunBranch | Confirm
    toasts: VecDeque<Toast>, logs: LogView, detail: DetailCache,
    // platform
    main_window: Option<window::Id>, focused: bool, tray_ok: bool,
    ui_tx: UnboundedSender<UiEvent>,            // lets notification click handlers call back into the app
    tray_tx: watch::Sender<TraySnapshot>, system_mode: theme::Mode,
    prefs: GuiPrefs, fonts: Fonts, client: Client,
}
```

The stream (15 section 3.5) opens with `hello`, then one event per object, then `snapshot_complete`,
then live events. The model is therefore rebuilt on every (re)connect without a flash of empty UI:
events after `hello` go into a **staging** model, which replaces the live one at `snapshot_complete`
(the old data stays on screen, dimmed, until then; an object that vanished while we were away
disappears with the swap). A changed `boot_id` in `hello` means the daemon restarted, so all
`Pending` entries are dropped.

```rust
pub fn reduce(m: &mut Mirror, ev: WireEvent) -> Vec<Effect> {
    match ev {
        WireEvent::Hello { boot_id, .. }      => m.begin_staging(boot_id),     // [Effect::DaemonRestarted] if boot_id changed
        WireEvent::SnapshotComplete           => { m.swap_staging(); vec![Effect::Resynced] }
        WireEvent::InstanceUpdated { instance } => {
            let from = m.target().instances.insert(instance.id.clone(), instance.clone()).map(|i| i.runtime.status);
            vec![Effect::Transition { id: instance.id.clone(), from, to: instance.runtime.status }]
        }
        WireEvent::OperationProgress { op, instance, step, done, error } => vec![Effect::Progress { op, instance, step, done, error }],
        /* ProjectUpdated/Removed, InstanceRemoved, ListenersChanged, ServiceUpdated, SystemHealth: replace the entry */
        WireEvent::LogLine { .. }             => vec![],     // belongs to the log stream (3.5.5), not the mirror
        WireEvent::Resync { .. }              => vec![],     // consumed by lantern-client
    }
}
```
`Effect`s are data (`Transition`, `Progress`, `Resynced`, `DaemonRestarted`); `update` turns them into pending-state
changes, toasts, notifications (3.9) and a new `TraySnapshot`. Because `reduce` is pure it is unit
tested without iced (section 7).

#### 3.5.3 Messages

```rust
pub enum Message {
    // platform
    Ui(UiEvent),                       // from tray / D-Bus: ShowWindow, ShowTarget, Tray(action), TrayStatus, Quit
    WindowOpened(window::Id), WindowClosed(window::Id), WindowFocus(window::Id, bool),
    SystemTheme(theme::Mode), Key(keyboard::Key, keyboard::Modifiers), Tick(Instant),
    // connection
    Stream(StreamEvent), RetryNow, StartDaemon, DaemonStarted(Result<(), String>),
    // navigation
    Navigate(Page), FilterChanged(String), Select(Option<RowKey>), ToggleGroup(ProjectId),
    // commands (the only way to change the daemon)
    Cmd(Command), CommandDone(Option<InstanceId>, Result<OperationResponse, UiError>),   // Command: Start|Stop|Restart|Setup|RunBranch|Remove|Pin|Adopt|Service|...
    // local actions (run in this process, not the daemon)
    OpenUrl(String), OpenEditor(PathBuf), OpenTerminal(PathBuf), CopyText(String),
    // sub-screens
    Logs(LogsMsg), Detail(DetailMsg), Overlay(OverlayMsg), Settings(SettingsMsg), Toast(ToastMsg),
    Noop,
}
```

#### 3.5.4 Commands, pending state, no optimistic UI

```rust
Message::Cmd(cmd) => {
    let key = cmd.instance();                               // Some(id) for per-instance operations
    if let Some(id) = &key { self.pending.insert(id.clone(), Pending::new(cmd.kind())); }
    let client = self.client.clone();
    Task::perform(async move { cmd.execute(&client).await.map_err(UiError::from) },   // wait = false
                  move |r| Message::CommandDone(key, r))
}
Message::CommandDone(key, Err(e)) => { self.pending.remove_opt(&key); self.toast_error(e); Task::none() }
Message::CommandDone(_, Ok(resp)) => { /* 202: keep pending, SSE settles it; 200 + noop: clear it */ Task::none() }
```

- The GUI sends commands with `wait = false` and watches `OperationProgress` on the stream, so a
  slow `pnpm install` is just a row that says "setup: pnpm install" with a spinner.
- A `Pending` is cleared by (in order): `OperationProgress { done: true }` for that instance; an
  `InstanceUpdated` reaching the *target* state (Start: Running or Unhealthy; Stop: Stopped;
  Restart: Running) or a failure state (Crashed, NeedsConfig); the REST call failing; a 180 s
  safety timeout (then a toast "Still no answer from the daemon").
- While pending, the row's button is replaced by a spinner plus the latest step text and cannot be
  double-clicked; other rows stay interactive.
- `Tick` (500 ms) is subscribed **only** while something is pending or a toast needs expiry, so the
  idle GUI has no timers. `iced` 0.14 also renders reactively (redraws on events only).
- Every failure becomes a toast. Errors persist until dismissed; successes expire after 4 s.

#### 3.5.5 Subscriptions

```rust
fn subscription(&self) -> Subscription<Message> {
    let mut s = vec![
        Subscription::run(|| ui_events().map(Message::Ui)),                     // tray + D-Bus
        Subscription::run_with(ClientKey(self.client.base()), |k|               // state stream, always on
            Client::new(&k.0).events(EventsOptions { state: true, reconnect: true, ..Default::default() }).map(Message::Stream)),
        window::close_events().map(Message::WindowClosed),
        window::events().filter_map(|(id, e)| match e {                         // focus tracking
            window::Event::Focused => Some(Message::WindowFocus(id, true)),
            window::Event::Unfocused => Some(Message::WindowFocus(id, false)), _ => None }),
        iced::system::theme_changes().map(Message::SystemTheme),
        iced::event::listen_with(|ev, status, _| match (ev, status) {           // Ignored keys only
            (iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }), event::Status::Ignored)
                => Some(Message::Key(key, modifiers)), _ => None }),
    ];
    if self.needs_tick() { s.push(iced::time::every(Duration::from_millis(500)).map(Message::Tick)); }
    if let Some(id) = self.logs.wanted() {                                      // log stream while a project page is open
        s.push(Subscription::run_with(LogKey(self.client.base(), id), |k|
            logs::batched(Client::new(&k.0).events(EventsOptions { state: false, logs: vec![k.1.clone()], tail: 1000, op: None, reconnect: true })
        )));
    }
    Subscription::batch(s)
}
```
Subscriptions are identified by their hash: changing the project page changes `LogKey`, which
closes the old stream and opens the new one. `logs::batched` coalesces lines into
`Message::Logs(LogsMsg::Batch(Vec<LogRow>))` every 30 ms, so a flood of 5,000 lines/s costs at most
33 updates/s. On every (re)connect of the log stream the buffer is cleared and rebuilt from the
daemon's `tail` backlog, so no de-duplication logic exists.

#### 3.5.6 Log view with 10k+ lines

Rows have a fixed height (`ROW_H = 18.0`, monospace 13 px, no wrapping; long lines are clipped
and fully visible with "Selectable" or on row select). Only rows intersecting the viewport (plus
a margin) are built; spacers stand in for the rest, so the widget tree is ~60 elements regardless
of buffer size.

```rust
pub struct LogView {
    instance: Option<InstanceId>, lines: VecDeque<LogRow>,   // cap 20_000; pop_front when full
    follow: bool, scroll_y: f32, viewport_h: f32, selected: Option<(usize, usize)>, filter: Option<Filter>,
}
fn log_list(&self) -> Element<'_, Message> {
    let total = self.logs.visible_len();                       // filtered length
    let first = ((self.logs.scroll_y / ROW_H) as usize).saturating_sub(5).min(total);
    let last = (first + (self.logs.viewport_h / ROW_H) as usize + 12).min(total);
    let mut col = column![space::vertical().height(first as f32 * ROW_H)];
    for row in self.logs.visible_range(first..last) { col = col.push(log_row(row)); }
    col = col.push(space::vertical().height((total - last) as f32 * ROW_H));
    scrollable(col).id(LOG_SCROLL_ID).on_scroll(|vp| Message::Logs(LogsMsg::Scrolled(vp))).height(Length::Fill).into()
}
```
- `Scrolled` stores `absolute_offset().y` and viewport height; `follow = distance_to_end < 2 rows`.
  When `follow` and a batch arrives, `update` returns `iced::widget::operation::snap_to_end(LOG_SCROLL_ID)`.
- A filter keeps a `Vec<u64>` of absolute line numbers (counter increments on every push; index =
  `n - base`), extended per batch, so popping the front never invalidates it.
- Row selection (click, Shift+click range, `Ctrl+C`, "Copy") is index arithmetic; `iced::clipboard::write`
  does the copy. **`iced` 0.14 has no selectable `text` widget** (verified: no selection code in
  `text.rs`); the read-only `text_editor` snapshot is the escape hatch (3.4.4).
- Memory: 20,000 lines of ~100 bytes is ~2-3 MB. Budget: batch of 1,000 lines applied in < 2 ms; a
  frame with the log tab open < 8 ms.

#### 3.5.7 Theme and fonts

```rust
fn theme(&self, _w: window::Id) -> Option<Theme> {
    Some(match self.prefs.theme { ThemePref::Light => lantern_theme(Mode::Light),
                                  ThemePref::Dark  => lantern_theme(Mode::Dark),
                                  ThemePref::System => lantern_theme(self.system_mode) })
}
fn lantern_theme(mode: theme::Mode) -> Theme {            // Theme::custom(name, Palette { background, text, primary, success, warning, danger })
    /* dark: bg #14161A, text #E6E6E6, primary (lantern amber) #F5A623, success #3FB950, warning #D29922, danger #F85149
       light: bg #FAFAF8, text #1F2328, primary #C77D0A, success #1A7F37, warning #9A6700, danger #CF222E */
}
```
- `system_mode` is initialized with `iced::system::theme()` at boot and updated from
  `iced::system::theme_changes()`. This is fed by the XDG settings portal (`mundy`, enabled by iced's
  default `linux-theme-detection` feature), which Plasma 6 and GNOME both implement. Returning
  `None` from `theme()` would also follow the system but with iced's built-in palettes, not ours.
- **Fonts.** `cosmic-text` 0.15 hard-codes its generic families to "Open Sans" / "Noto Sans Mono"
  / "DejaVu Serif" (verified in `font/system.rs`), which may not exist. At startup (before the first
  window) run `fc-match -f '%{family[0]}' sans-serif` and `monospace` (200 ms cap; fallback "Noto
  Sans" / "DejaVu Sans Mono"), leak the names into `&'static str` once, and use
  `Font::with_name(..)` as `default_font` and for logs. On this machine they resolve to Noto Sans and
  DejaVu Sans Mono. Icons: Lucide font (`lucide-icons` crate, `iced` feature, ~1 MB TTF embedded,
  subset later if size matters) rendered as text.
- Renderer: default is wgpu with automatic fallback to tiny-skia; `ICED_BACKEND=tiny-skia`
  forces software rendering (document in `doctor` output for broken GPUs). When the last window
  closes iced drops the compositor (checked in `iced_winit`), so tray-only mode holds no GPU
  resources. Window uses `PlatformSpecific::application_id = "dev.lantern.Lantern"` (Wayland
  app_id; matches the `.desktop` file so the task manager shows the right icon).

### 3.6 The tray (ksni)

`ksni` 0.3.7 (released 2026-10-20, tokio feature default, `zbus` 5) is maintained and exposes the
Plasma `ProvideXdgActivationToken` extension. `lantern-gui/platform/tray.rs` owns the `ksni::Handle`;
the app only sends `TraySnapshot` values through a `watch` channel:

```
 ksni task (background runtime)                         iced (main thread)
 menu click / Activate ── UiEvent ──► unbounded mpsc ──► Subscription::run ──► Message::Ui ──► update()
 handle.update(|t| t.snap = s) ◄── tray task ◄── watch::Receiver ◄── watch::Sender::send ◄── update()
```

```rust
pub struct TraySnapshot { daemon_up: bool, running: Vec<TrayRow>, attention: Vec<TrayRow>, recent: Vec<TrayRow> }
impl ksni::Tray for GuiTray {
    fn id(&self) -> String { "lantern".into() }
    fn status(&self) -> Status { if self.snap.attention.is_empty() { Status::Active } else { Status::NeedsAttention } }
    fn icon_name(&self) -> String { /* offline | active | idle -symbolic names, table below */ }
    fn attention_icon_name(&self) -> String { "lantern-attention-symbolic".into() }
    fn activate(&mut self, _x: i32, _y: i32) { let _ = self.tx.send(UiEvent::ShowWindow { token: ksni::current_xdg_activation_token() }); }
    fn menu(&self) -> Vec<MenuItem<Self>> { /* built from the snapshot; closures send UiEvent::Tray(..) */ }
}
```

| State | Condition | Icon (`icon_name`) | SNI status |
|---|---|---|---|
| offline | daemon unreachable | `lantern-offline-symbolic` | Active |
| attention | any Crashed / Unhealthy / NeedsConfig instance of an enabled project | `lantern-attention-symbolic` | NeedsAttention |
| active | at least one Running or Starting | `lantern-active-symbolic` | Active |
| idle | otherwise | `lantern-idle-symbolic` | Active |

Idle uses `Active`, not `Passive`: Plasma hides Passive items in the overflow popup. The four
symbolic SVGs are installed under `hicolor/scalable/status/` (`13-system.md` packaging); in a dev
tree set `LANTERN_ICON_DIR` and return it from `icon_theme_path()`. Tooltip: "Lantern: 3 running, 1
crashed".

```text
┌─ tray menu ──────────────────────────────────────────────────────────────┐
│ Lantern - 3 running, 1 crashed                                           │
├──────────────────────────────────────────────────────────────────────────┤
│ myapp                      ▸  Open in browser / Logs / Restart / Stop    │
│ myapp@feature-x            ▸  ...                                        │
│ billing-api  (crashed)     ▸  Start / Logs                               │
├──────────────────────────────────────────────────────────────────────────┤
│ Recent                     ▸  Start myapp@fix-login / Start docs-site ...│
│ Run branch...                                                            │
├──────────────────────────────────────────────────────────────────────────┤
│ Open Lantern                                                             │
│ More                       ▸  Stop all instances                         │
│ Quit Lantern (daemon keeps running)                                      │
└──────────────────────────────────────────────────────────────────────────┘
```

- Menu is rebuilt from the snapshot on every change (cheap, tens of items). At most 12 running
  rows, then "N more..." which opens the window. Crashed instances are listed in the first block
  with `(crashed)` and offer Start / Logs. "Recent" = the 5 most recently run **stopped**
  instances (from `started_at`/`last_exit`; no client-side history).
- Menu actions are `UiEvent::Tray(TrayAction::{OpenBrowser, Start, Stop, ShowLogs, RunBranchDialog,
  StopAll})` and are handled in `update` like any other message, so they work with no window open
  (v1's tray went through the renderer and died with the window).
- Left click = `activate` = open/raise the window; middle click does nothing in v2.0;
  `MENU_ON_ACTIVATE` stays `false`.
- **Failure and startup.** `spawn()` returns `Error::WontShow` / `Watcher(_)` when no
  StatusNotifierHost/Watcher exists (GNOME without the AppIndicator extension, or autostart racing
  plasmashell). The tray task retries every 2 s for 60 s, then reports
  `UiEvent::TrayStatus { available: false }`; the GUI then behaves as a normal windowed app
  (closing the window quits) and Settings shows "No system tray found (GNOME: enable the
  AppIndicator extension)". `watcher_offline` returns `true` so the service survives a plasmashell
  restart and re-registers on `watcher_online`.
- **Activation tokens.** Plasma sends `ProvideXdgActivationToken` before `Activate`; `ksni` exposes
  it via `current_xdg_activation_token()` (retry once after 1 ms as in its example). We forward
  it in `UiEvent::ShowWindow { token }`. `iced_winit` 0.14 never passes a token to winit
  (verified: no use of `activation_token`/`startup_notify`), so today the token is only logged;
  it is the hook for the patch in 3.7.

### 3.7 Process model

| Invocation | Behavior |
|---|---|
| `lantern gui` | Primary: tray + window. Secondary: call `Activate` on the primary over D-Bus, exit 0. Detaches from the terminal unless `--foreground` (re-exec in its own process group), so closing the terminal does not kill it. |
| `lantern gui --tray` (autostart) | Primary: tray only, no window, no GPU. Secondary: exit 0 without activating. If no tray is available it opens the window instead. |
| `lantern gui myapp@feature-x` | As `gui`, then the primary navigates to that instance (`Open(["lantern://instance/<id>"])`). |
| `lantern gui --quit` | `ActivateAction("quit")` on the primary; no-op if none. For package upgrades. |

**Single instance.** The GUI claims the session-bus name `dev.lantern.Lantern` and serves the
standard `org.freedesktop.Application` interface (`Activate`, `Open`, `ActivateAction`) at
`/dev/lantern/Lantern`. Pitfall found by running it: in `zbus` 5.19, `connection::Builder::name()`
**queues** instead of failing when the name is taken, so a second instance silently "succeeds".
Build the connection with `serve_at` only, then call
`request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())` and treat `Error::NameTaken`
as "someone else is primary". Tested on the session bus here: first claim primary; second claim
secondary and its `Activate` arrived at the primary as `ShowWindow`. The name is released
automatically if the primary dies. With `DBusActivatable=true` in the `.desktop` file plus a D-Bus
service file, launcher clicks and KRunner use the same path and pass `platform-data` containing
`activation-token`.

**Window lifecycle.** States: `Headless` (no window; tray only) and `Windowed`.

| Event | Result |
|---|---|
| `ShowWindow` while Headless | `window::open(..)` (≈100-300 ms with wgpu; state is in `App`, so the UI reappears as it was) |
| `ShowWindow` while Windowed | `window::gain_focus` (works on X11) + `window::request_user_attention(Informational)` (Wayland: xdg-activation attention request) |
| Close button, `Ctrl+W` | `window::close` -> Headless if `tray_ok`, else exit the process |
| `Ctrl+Q`, tray Quit, SIGTERM/SIGINT, D-Bus `quit` | `iced::exit()` after `ksni::Handle::shutdown()`; daemon and dev servers untouched |

*Why destroy instead of hide.* winit 0.30 on Wayland: `set_visible` is a no-op, `focus_window` is
a no-op, un-minimizing is ignored (read in `wayland/window/mod.rs`). So there is no way to hide
a window and bring it back; the only portable "hide" is to destroy it. `iced::daemon` is built
for this (no window at start, survives the last window closing). Re-opening re-creates the
surface; no user state is lost because it lives in `App` (scroll offsets of non-log views reset;
acceptable).

*Raising from the tray on Wayland.* A client may only take focus with an XDG activation token.
Plan, in order: (1) tray click with no window creates a new window, which KWin normally focuses
(verify in the spike, Q1); (2) with a window already open: `gain_focus` + attention request;
(3) if the spike shows (1) or (2) fail on Plasma 6: carry a small patch to `iced_core`/`iced_winit`
(`window::Settings::platform_specific.activation_token: Option<String>`, passed to winit's
`WindowAttributesExtStartupNotify::with_activation_token`, ~30 lines via `[patch.crates-io]`) and
make "raise" = close + re-open with the token from `ksni`/`Activate`. The plumbing (`token` in
`UiEvent`) exists from day one.

**Autostart.** Settings > "Start Lantern at login" writes/removes
`~/.config/autostart/dev.lantern.Lantern.desktop` (`Exec=lantern gui --tray`,
`X-KDE-autostart-after=panel`, `X-GNOME-Autostart-Delay=3`, `NoDisplay=true`). The tray retry loop
is the real protection against startup races; the keys only reduce them.

### 3.8 Daemon-down and reconnect (GUI)

| Situation | `Conn` | What the user sees |
|---|---|---|
| First connect in flight | Connecting | Window chrome only; after 300 ms "Connecting...". No empty-dashboard flash. |
| Never connected, `daemon_unreachable` | Down | Daemon-down screen (3.4.6): Start daemon / Show why / Copy command, countdown to next attempt. |
| Was live, connection lost | Down | Dashboard stays with last data dimmed, all action buttons disabled, top banner "Connection lost: <error>. Retrying in 4 s [Retry now] [Start daemon]". Dev servers are still running, so the data is only stale, not wrong. |
| Reconnected | Live | `snapshot_complete` swaps in the staged mirror; pending cleared if `boot_id` changed; banner gone; toast "Reconnected" if down > 5 s. |
| Version skew (`api_version`/GUI version mismatch from `ping`) | Live | Banner "Daemon is newer/older than this GUI. [Restart GUI]" (`lantern gui --quit` then relaunch). |

- Reconnect policy lives in `lantern-client` (3.2). The SSE attempt *is* the liveness probe. A
  silent stall is caught by a 40 s no-bytes watchdog (assumes the daemon sends an SSE comment
  keep-alive every 15 s; section 4).
- "Start daemon" runs `systemctl --user start lantern` in a `Task`, then `RetryNow`; the first line
  of stderr is shown on failure. Unit missing -> Doctor screen.
- Tray while down: offline icon; the menu is reduced to "Daemon not running", "Start daemon",
  "Open Lantern", "Quit". One notification after 10 s down (at most every 10 min).

### 3.9 Notifications

`notify-rust` 4.18 (zbus backend, no extra features). Sent from `update` via `std::thread::spawn`
because `show()` and `wait_for_action()` block; a click delivers `UiEvent::ShowTarget`.

| Trigger | Condition | Text | Click |
|---|---|---|---|
| Crash | `Transition { from: Running/Starting/Unhealthy, to: Crashed }`, no Stop pending; again when `issue.code == CrashLoop` (retries exhausted) | "`myapp@feature-x` crashed" / `message` + `issue.detail` ("retry in 4 s (2/5)" or "gave up after 6 crashes") | Project page, Overview |
| Branch ready | `OperationProgress { done: true, error: None }` for an op longer than 5 s (`notify_ready = slow`), window not focused | "`myapp@feature-x` is ready" / URL | Open in browser |
| Operation failed | `done: true, error: Some(_)`, window not focused | "Couldn't start `x`" / error | Project page |
| Daemon stopped | Down > 10 s, tray-only | "Lantern daemon stopped" | Runs `systemctl --user start lantern` |
| Setup needed | First `SystemHealth` with a failing check and no window | one time | Doctor |

Rules: nothing while the GUI window is focused; per-instance debounce 60 s; one crash
notification per Running -> Crashed edge (a restart loop does not spam); the body is escaped
(`& < >`) because the notification spec allows markup and the text can contain dev-server output;
preferences in `gui.json` (`notify_crash` on, `notify_ready` off|slow|always).

### 3.10 Framework check, verified facts, fallback

No blocker found for `iced`. Real limitations, all with mitigations above:

| Limitation | Evidence | Mitigation |
|---|---|---|
| Wayland: cannot hide, un-minimize or focus own window | winit 0.30 source | Destroy/re-create; attention request; optional token patch (3.7) |
| No selectable text | no selection code in `iced_widget` `text.rs` | Row selection + copy; read-only `text_editor` snapshot |
| No virtual list | no such widget in `iced_widget` 0.14.2 | Windowed list (3.5.6) |
| No accessibility / screen reader | none in 0.14 | Accepted; the CLI is the accessible interface |
| Generic font families hard-coded | `cosmic-text` 0.15 `font/system.rs` | Resolve with `fc-match` (3.5.7) |
| Not native widgets | by design | Custom palette that follows the system light/dark; Plasma server-side decorations come from KWin |
| One release per year | 0.14.0 on 2025-12-07 is latest | Pin `=0.14.x`; upgrade deliberately |

0.14 features we rely on: `iced::daemon`, reactive rendering (no redraw when idle), compositor
dropped on last close, `system::theme_changes`, `combo_box`, `markdown`, `scrollable::snap_to_end`;
for development, headless `iced_test` and hot reload.

Fallback: if the **M0 spike** (section 7) misses its budgets or Plasma will not focus windows,
the order of escape is (1) token patch, (2) software renderer, (3) switch toolkit. Alternatives:
*Slint* (MIT/GPL/commercial; built-in virtual `ListView`, selectable `TextEdit`, AccessKit a11y; but
same winit Wayland window limits and no tray, so ksni integration is identical) and *Qt via
`cxx-qt`* (best Plasma integration: native Breeze look, real activation tokens, `QSystemTrayIcon`,
a11y, selectable text; costs a C++/Qt6 toolchain, QML, and heavier packaging; unattractive for AI
implementers and a non-Rust-expert owner). Recommendation: stay on `iced`; reach for Qt only if the
spike shows the native-feel goal cannot be met.

---

## 4. Interfaces

### 4.1 Provided

| Interface | Consumer |
|---|---|
| `lantern-client` (section 3.2) | `lantern-cli`, `lantern-gui` |
| `lantern_cli::main(argv) -> ExitCode` and `CliAction::{RunDaemon, RunGui}` | `lantern` binary |
| `lantern_gui::run(GuiArgs)` | `lantern` binary |
| D-Bus `dev.lantern.Lantern` (`org.freedesktop.Application`) | launchers, `lantern gui` |
| Files for `13-system.md` packaging: `dev.lantern.Lantern.desktop` (`Exec=lantern gui`, `DBusActivatable=true`, `StartupNotify=true`), `dev.lantern.Lantern.service` (D-Bus), app icon, four tray symbolic SVGs, shell completion files | .deb |
| Client-written files: `$XDG_STATE_HOME/lantern/gui.json` (GUI prefs only), `~/.config/autostart/dev.lantern.Lantern.desktop` | GUI only |

### 4.2 Assumed from other modules

The operations below are what the clients call; paths are the ones in `15-api-mcp.md` 3.3 (it owns
them). "Gap" marks something a sibling doc has not yet defined.

| Operation | REST (15 unless noted) | Notes |
|---|---|---|
| liveness / version | `GET /api` (cheap), `GET /api/system` | `api_version`, `version`, `pid`, `boot_id` |
| doctor checks | `GET /api/system/health` -> `SystemHealth` (13) | Resolved (02 D20): `SystemHealth.checks: Vec<Check>` in `lantern-core::api` |
| list state | `GET /api/projects`, `/instances`, `/listeners`, `/services`, `/suggestions` (14) | the GUI uses the SSE stream for these and REST only for one-offs |
| instance detail | `GET /api/instances/{id}`, `/manifest`, `/docs`, `/docs/{path}`, `/endpoints`, `/logs`, `POST /health/check` | `ManifestReport` from 14 |
| start / stop / restart / setup / stop-all / operation | `POST /api/instances/{id}/{start,stop,restart,prepare}`, `POST /api/system/stop-all`, `GET /api/operations/{id}?wait=true&timeout=N` | Resolved (02 D8): REST verb is `prepare`; CLI verb stays `lantern setup` |
| run branch | `POST /api/projects/{id}/instances` | Resolved (02 D8): 11's `target` form (`branch` / `new_branch`+`base` / `detached` / `pr`), `fetch`, `start`, `pin` |
| checkouts, branches | `GET /api/projects/{id}/checkouts`, `/branches` (11) | Resolved (02 D8): added to 15 |
| pin, relabel, remove | `POST /api/instances/{id}/pin`, `/unpin`, `/rename`; `GET .../removal-check`; `DELETE .../{id}?force=&delete_branch=&stop=` | 11 says only `managed` checkouts can be removed (`409 not_managed`); 15's table says user-made worktrees are only forgotten. The CLI follows 11 and suggests `wt unpin` |
| adopt, register, scan | `POST /api/instances/{id}/adopt {port?}`, `/unadopt`; `POST /api/projects`, `/projects/scan`, `PATCH`/`DELETE /api/projects/{id}` | |
| suggestions | `GET /api/suggestions`, dismiss (14) | Resolved (02 D8, D7): `GET /api/suggestions`, `POST /api/suggestions/dismiss`; `SuggestionsChanged` event |
| services, v1 import | `POST /api/services/{name}/{start,stop,restart}` (synchronous, 30 s cap); `POST /api/import/v1 {from, dry_run, force}` (14) | |
| settings, profiles | `GET/PATCH /api/settings` (-> `restart_required`, `overridden_by_env`), `POST /api/settings/reload`, `GET /api/profiles`, `POST /api/profiles/{name}/activate` | keys: 14 |
| events | `GET /api/events[?state=false&logs=a,b&tail=N&op=ID]` | `hello` ... `snapshot_complete`; `: ping` every 15 s; `resync` closes the stream |

Also assumed on the types: `InstanceView.visible`, `setup_stale`, `checkout.{branch, dirty,
locked, missing}` (11); `RuntimeState.{issue: Option<RuntimeIssue{code, detail, hint}>,
next_retry_at, last_exit: ExitInfo{at, kind, code, signal, uptime_secs}}` (10, proposed there);
`HealthSummary.recent` (10); `Listener.{label, class, exposure, project, branch, adopted, docker}` (12).

---

## 5. Failure modes & edge cases

| Case | Behavior |
|---|---|
| Daemon down at CLI start | 3.3.6: prompt/auto-start/exit 3. |
| Daemon dies during `--wait` | Stream `Lost` -> poll `instance()` every 1 s for up to 10 s; if still unreachable exit 3 ("operation state unknown; dev server may still be starting"). |
| `--wait` timeout | Exit 6; the operation continues in the daemon; hint `lantern status <target>`. |
| Target ambiguous / missing | Exit 4 with up to 5 candidates (Levenshtein/prefix). |
| Two operations on one instance | Daemon returns 409 (core: per-instance op lock); CLI exit 7; GUI shows the existing pending state because the same `OperationProgress` arrives. |
| SSE lag or `resync` | Reconnect at once (no backoff); the new stream's staged mirror replaces the old at `snapshot_complete`. |
| Unknown event kind or enum value (newer daemon) | Skipped; unknown `InstanceStatus` renders as "unknown" with a neutral glyph. |
| Huge log line (> 64 KiB) | Truncated at ingestion with "... (+N bytes)"; full text only via `lantern logs`. |
| Log flood | 30 ms batching; ring cap; follow mode keeps up; filter recomputed incrementally. |
| No tray host | Windowed-only behavior (3.6). |
| No session D-Bus (SSH) | `lantern gui` prints "no session bus; the GUI needs a desktop session" and exits 1; CLI unaffected. |
| No GPU / wgpu init fails | iced falls back to tiny-skia automatically; if both fail, exit 1 with the wgpu error and a hint for `ICED_BACKEND`. |
| Instance removed while its page is open | `InstanceRemoved` -> navigate to the project, then Dashboard, with a toast. |
| Editor/terminal not found | Toast with the tried commands and where to set `[clients]`. |
| `pkexec` missing or cancelled | Setup output pane shows the error; "Copy command" fallback. |

---

## 6. Security considerations

- Clients talk only to a loopback daemon and send no credentials. `--host`/`LANTERN_DAEMON_URL` may name
  another address; a non-loopback host prints a warning (plain HTTP, no auth).
- The browser-DNS-rebinding defense is the daemon's job; clients comply by never sending `Origin`
  and using the daemon's own `Host`.
- Untrusted text reaches us in log lines, instance names, `message`, docs and manifests. CLI:
  non-SGR escape sequences stripped before writing to a TTY. GUI: ANSI stripped on ingestion;
  notification bodies escaped; docs rendered as markdown with links opened only for `http(s)`
  and only on click; `lantern://` URIs received over D-Bus are parsed as instance ids, never as
  paths or commands.
- Process spawning (editor, terminal, `systemctl`, `pkexec`, `xdg-open`): always `Command` with
  an argv vector; templates are `shlex`-split before substitution; no `sh -c`; `pkexec` runs the
  absolute path of the current executable with fixed arguments.
- The D-Bus application object accepts `Activate`/`Open`/`quit` from any process of the same user;
  the worst case is a raised window or a closed GUI. Clients write only `gui.json` and the autostart
  file, in the user's own directories.

---

## 7. Testing strategy

**Unit (no daemon, no display)**
- Target parsing and resolution (exact, branch, prefix, ambiguous, cwd).
- Table and glyph rendering (`NO_COLOR`, widths with Unicode), exit-code mapping, ANSI sanitizer.
- SSE parsing across chunk boundaries; backoff schedule and jitter bounds; watchdog.
- `reduce` with scripted event sequences: transitions, crash detection, pending clearing, resync.
- Tray snapshot derivation and icon state; palette ranking; log ring/window arithmetic and filter
  index after `pop_front`; editor/terminal template substitution; `mcp install` JSON merge.

**Integration**
- A fake daemon (axum, scripted REST + SSE, including `resync`, stalls, restarts) drives
  `assert_cmd` tests for `ls/on/off/run/logs -f/doctor`, `--json` shapes, every exit code, the
  daemon-down prompt (pty), and `--wait` across a daemon restart.
- GUI logic through `iced_test` headless simulator (API to be confirmed at implementation time):
  dashboard grouping, pending spinner, banner on `Lost`, daemon-down screen, palette flow.
- Session-bus test of single instance (already exercised in the scratch crate): two acquisitions,
  second is `Secondary`, `Activate` arrives; a test D-Bus daemon (`dbus-run-session`) in CI.

**Manual on the target machine (Ubuntu 26.04, Plasma 6 Wayland), and GNOME+AppIndicator**
- **M0 spike, before building screens** (one afternoon): `iced::daemon` + ksni + the D-Bus name, a
  window with a 20,000-line log list fed at 1,000 lines/s. Pass criteria: cold `lantern gui` to
  first frame < 400 ms; `--tray` idle RSS < 40 MB and ~0 % CPU; window open from tray < 300 ms;
  50 open/close cycles grow RSS < 5 MB; log frames < 16 ms; tray click with no window yields a
  *focused* window on Plasma (Q1); tray click with a window behind another raises or flashes it;
  fonts resolve to Noto Sans / DejaVu Sans Mono; dark/light follows the Plasma color scheme live.
- Kill and restart the daemon during use (banner, retry, mirror swap, tray icon); start/stop/crash a
  project and check notifications and their click actions on both desktops; time `lantern ls`
  with `hyperfine` and check `ldd lantern` shows no GUI libraries at load.

---

## 8. Open questions

| # | Question | Recommended answer |
|---|---|---|
| Q1 | Does KWin focus a newly created `iced` window with no activation token, and does `request_user_attention` raise an existing one? | Test in M0. If not, ship the small `iced_core`/`iced_winit` activation-token patch (3.7). Do not block the rest of the GUI on it. |
| Q2 | Should `Enter` in the quick switcher on a stopped instance also open the browser when ready? | No: `Enter` starts, `Alt+Enter` starts and opens. Revisit after use. |
| Q3 | Env var editing (issue #8) in v2.1? | Yes, if `14-config.md` adds a comment-preserving patch endpoint (e.g. `PATCH /api/projects/{id}/manifest` on a path like `run.env.FOO`). Until then: viewer + "Edit lantern.yaml". |
| Q4 | "Stop this process" for unmanaged listeners owned by the user? | Defer to v2.1; needs a daemon operation and a confirmation UX. v2.0 offers Adopt and Open only. |
| Q5 | Colored ANSI rendering in the GUI log view? | v2.1 with `rich_text` spans per visible row (fits the windowed list); strip in v2.0. |
| Q6 | Split the GUI into its own executable if the single binary hurts CLI start-up? | Measure in M0. Keep one binary unless `lantern ls` exceeds 30 ms warm or binary size becomes a packaging problem. |
| Q7 | Show profiles on the Dashboard (apply profile = start its services and auto-start instances)? | v2.1. v2.0 shows and switches the active profile in Settings only. |
| Q8 | Keep `lantern mailpit` and `lantern health` as hidden aliases forever? | Keep through v2.x, mention in release notes, remove in v3. |

---

## 9. Proposed changes to core architecture

Not needed any more because sibling docs already carry them: `Event::Snapshot` (15 sends `hello`,
per-object events, `snapshot_complete`), the SSE `tail` parameter and keep-alive (15 3.5),
`RuntimeState.issue` and `next_retry_at` (10 section 9). This doc relies on all three.

1. **Allow clients to write exactly two files**: `$XDG_STATE_HOME/lantern/gui.json` and the XDG
   autostart entry. Core section 3 says clients "never touch state files"; this carves out GUI
   preferences, which the daemon must not own.
2. **Settings section `[clients]`** for `14-config.md`: `editor` and `terminal` (`"auto"` or a
   template containing `{path}`). They are used by the CLI and GUI processes, not the daemon.
   GUI-only preferences (theme, notifications, start-at-login) stay in `gui.json`.
3. **`lantern-cli` returns `CliAction::{RunDaemon, RunGui}`** to the binary crate instead of calling
   those crates, so "cli never depends on daemon or gui" holds. Core section 4 can state it.
4. **Pin policy** (core section 5): `iced = "=0.14.x"` (about one release per year),
   `clap_complete` with `unstable-dynamic` pinned exactly, `eventsource-stream` 0.2, and a direct
   `zbus = "5"` so that `ksni`, `notify-rust` and iced's theme detection share one `zbus` copy
   (verified with `cargo tree`). Additional crates for this subsystem: `ksni`, `notify-rust`,
   `rfd`, `open`, `nucleo-matcher`, `lucide-icons`, `shlex`, `unicode-width`, `anstream`.
5. **Requests to siblings** (listed in 4.2 as gaps): 13 defines `SystemHealth.checks`; 15 adds
   `checkouts`, `branches`, `pin`/`unpin`/`rename`, `removal-check`, `suggestions` routes and
   reconciles `setup` vs `prepare`; 11 and 15 reconcile the run-branch request shape and the
   rule for removing user-made worktrees; 14 mentions `lantern project rename` but no endpoint
   exists, so the CLI has no `rename` yet.
