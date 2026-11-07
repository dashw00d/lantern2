# 10 - Runtime (supervisor)

Status: draft for review. Builds on `01-architecture.md` (the "core doc"); uses its glossary and
type names. v1 paths are relative to `.reference/lantern-v1/`. Facts about systemd, journald and
the shell were verified on the target machine (Ubuntu 26.04.1, systemd 259, cgroup v2) and are
marked **(verified)** where an experiment settled a doubt.

## 0. Plain-language summary

When you press "start" on an instance, the runtime does this:

1. Works out *what* to run (a dev-server command, a `deploy.start` command, or nothing at all
   for PHP/static sites) and starts any `depends_on` dependencies first.
2. Picks a port for it (the same port every time for the same instance).
3. Writes a small bash **launcher script** that sets up the environment (your real `PATH` with
   nvm/fnm/volta/pyenv, the project's Node/Python version, `PORT`, the manifest's `env`) and then
   `exec`s your `run.cmd`.
4. Asks systemd to run that script as a **transient user unit** named `lantern-inst-<slug>`.
   systemd, not the daemon, owns the process: it survives daemon restarts, is killed as a whole
   group on stop, and its output goes to journald.
5. Waits until the process is *really* serving: something in that unit's own cgroup is listening
   on the assigned port. If the server ignores `${PORT}`, Lantern says so explicitly.
6. Tells the router (Caddy, via `13-system`) where to send traffic, then keeps watching: it
   polls systemd about exits, runs health checks, retries crashes with backoff, and gives up
   (status `Crashed`) after repeated failures.

Everything is a plain file or a plain shell command you can run yourself: the launcher script
(`cat $XDG_RUNTIME_DIR/lantern/run/lantern-inst-<slug>.sh`), `systemctl --user status
lantern-inst-<slug>`, `journalctl --user -u lantern-inst-<slug>`. The daemon never stops dev
servers when it exits.

---

## 1. Purpose & scope

Owns: the supervisor and everything that turns an `Instance` into a running, watched process
and back; systemd transient units; the launch environment (PATH/toolchain problem); log capture
and the log ring buffers; readiness; health checks; crash/backoff policy; `deploy.*` overrides
and `upstream_url`; port allocation and leases; `depends_on` ordering; per-instance operation
serialization; the `InstanceStatus` state machine; startup reconciliation; the "running"
semantics for process-less (PHP/static) instances.

Not in scope (owner in brackets):
- Worktree creation, setup steps, dependency install, `.env` copying (`11-instances`). The
  runtime calls a hook (`InstancePreparer`) before spawning.
- Socket scanning, process-to-checkout matching, `External` detection (`12-discovery`). The
  runtime consumes its listener data and never parses `/proc/net/tcp` itself.
- Caddy routes, TLS, DNS, php-fpm installation, shared Services, `lantern init`, lingering
  setup (`13-system`).
- Manifest and settings schema, state.json schema (`14-config`). This doc lists the keys and
  fields the runtime needs.
- REST/SSE/MCP shapes (`15-api-mcp`); CLI/GUI rendering (`16-clients`).

---

## 2. v1 behavior audit

### 2.1 What v1 does

| v1 behavior | Where | Verdict |
|---|---|---|
| One `ProcessRunner` GenServer per project owns a BEAM `Port` (child of the daemon). Logs, retries and the process all die with the daemon. `Application.prep_stop` runs `deactivate_all`. | `projects/process_runner.ex`, `application.ex` | **Replace.** Transient systemd units; daemon exit never stops servers (core goal). |
| Spawn is `/usr/bin/setsid /bin/sh -lc <cmd>`. On Ubuntu `/bin/sh` is dash: `-l` reads `/etc/profile` and `~/.profile` but **not** `~/.bashrc`/`~/.zshrc`, where nvm, fnm, pyenv, asdf and mise are initialised. Env is the daemon's own env plus a hard-coded `DISPLAY=:0` and `XDG_RUNTIME_DIR` guess (wrong on Wayland). No per-project tool version handling. | `process_runner.ex` `spawn_program_and_args`, `inject_display_env` | **Replace.** See 3.4. This is the PATH bug. |
| Kill = `kill -TERM -<pid>` on the process group, then `-KILL` after 5 s. Blocks the runner's `handle_call`. | `process_runner.ex` `do_stop` | **Replace** with cgroup kill (`KillMode=control-group`): catches double-forked grandchildren that leave the process group. |
| Stdout+stderr merged, delivered as raw port chunks (not lines); 1000-chunk in-memory ring; lost on daemon restart. | `process_runner.ex` `append_log` | **Change.** Lines, journald-backed, ring rebuilt from journald after restart. |
| Retry: up to 5, backoff `1000*2^n` ms, only when status was `:running`. `retry_count` never resets (the 6th crash in a daemon's *lifetime* is final). After a retry the status is set to `:running` with no readiness check. | `process_runner.ex` `handle_info exit_status` | **Change.** Counter resets after a stable run; every restart goes through `Starting` and readiness. |
| Readiness = a TCP `connect` to `127.0.0.1:<port>` every 150 ms for 15 s. Any listener satisfies it (a stale process, another project). IPv6-only servers (Vite on `localhost` with Node >= 17 binds `::1`) fail it. Failure message is generic. | `manager.ex` `wait_for_runtime_ready` | **Replace.** Ownership-aware (cgroup) and family-agnostic; explicit port-mismatch diagnosis. |
| `do_deactivate` runs `lsof -tiTCP:<port>` and SIGTERM/SIGKILLs **whatever** listens there. | `manager.ex` `stop_listener_on_port` | **Drop.** Lantern never kills processes it did not start. |
| `PortAllocator`: 41000-42000, persisted assignments, `gen_tcp.listen` probe. But `do_deactivate` releases the port for `:proxy` projects, so ports change on every stop/start. | `port_allocator.ex`, `manager.ex` `maybe_release_port` | **Change.** Sticky per `InstanceId`; released only on instance removal. Default range moved out of the ephemeral range (3.6). |
| `Manager` is one GenServer: every activate/deactivate/restart for every project runs inside it, including the up-to-15 s readiness wait and shell commands. `restart` ignores the result of `do_deactivate`. | `manager.ex` `handle_call` | **Replace** with one supervisor actor per instance (core doc section 8). |
| `deploy.start/stop/restart/logs/status` run via `System.cmd("sh -c")` with no timeout, inside the Manager process, output not logged. `deploy.start` replaces `run.cmd` for Start; `deploy.stop` runs after the listener kill; `deploy.restart` is only reachable from a separate REST endpoint (Manager's restart is stop+start). `deploy.logs` is tried before local logs. A start command must reference `${PORT}` or `upstream_url` must be set. | `deploy_runner.ex`, `manager.ex`, `deploy_controller.ex`, `mcp/tools/get_project_logs.ex` | **Keep semantics, fix mechanics** (one-shot transient units with timeouts, logged). See 3.10. |
| `upstream_url`: activation sets `:running` immediately, no readiness, no port allocation, Caddy routes to the URL. | `manager.ex` `do_activate` | **Keep**, add a reachability probe. |
| `depends_on` is stored and shown (`get_dependencies` MCP tool, Dependencies tab) but **never used for ordering**. | `mcp/tools/get_dependencies.ex`, `manager.ex` | **Implement** (3.12). |
| `run.pre_start` / `run.post_stop` are parsed and never executed. | `config/lantern_yml.ex` | **Implement** (3.4.6). |
| Health checker: separate GenServer, 60 s interval, 5 s timeout, `base_url`/`upstream_url` + `health_endpoint`, statuses healthy/unhealthy/unreachable/error, 10-sample history, persisted. Runs for every enabled project even when stopped; never affects project status. | `health/checker.ex` | **Change.** Only while up; drives the `Unhealthy` status; 10 s interval with backoff (3.8). |
| PHP/static activation: status `:running` as soon as the Caddy file is written (a Caddy failure is only logged: "continuing without reverse proxy"). A `run_cmd` on a php/static project is also started as a side process (Vite HMR next to Laravel). | `manager.ex` `do_activate`, `ensure_caddy_config` | **Keep** the companion-process idea (3.11); route failures are no longer swallowed. |
| Daemon boot auto-activates every project that has a `run_cmd` or `upstream_url`. | `manager.ex` `handle_info :initial_scan_complete` | **Drop.** Auto-start belongs to profiles (`14-config`); the runtime only offers `start`. |

### 2.2 `RUNTIME_TESTING_AND_REGRESSIONS.md`: each recorded issue and its v2 answer

| v1 issue / invariant | v2 handling |
|---|---|
| **Regression A**: a manual registration whose path is under a workspace root is dropped by `scan` (`preserve_registered_projects`), emptying `/api/tools`. | Registry-level (`11-instances`/`14-config`): scan may add and refresh `source: Detected/Manifest` records but never deletes `source: Manual`. Runtime constraint: a `Passive` plan (3.2) must not require the checkout path to exist, so `upstream_url`-only manual entries are first-class. Runtime test: "manual upstream-only instance survives N registry rescans and a daemon restart". |
| Invariant C: after a daemon restart every field and the running state must match. | Servers survive restarts (units). Startup reconciliation (3.15) adopts them; persisted `port_leases`, `last_exit` and `activated` come back from state.json. |
| Invariant D: Caddy failure must yield handled errors (422/504), not 500; retries recover after the service resets. | `Router` errors map to `RuntimeIssue{RouteFailed}`; process-backed instances stay `Running` with the issue set; `Runtime::republish_all()` is called by `13-system` when Caddy comes back. v1's "swallow and continue" is gone. |
| Enabled=false must reject activation. | `start` on a disabled instance returns `instance_disabled` (409). The registry calls `stop` when `enabled` flips to false. |
| Enum/kind/type normalisation, docs endpoints, scan matrices | Not runtime (`14-config`, `15-api-mcp`, `11-instances`). |

---

## 3. Design

### 3.1 Shape of the module

```
 API / MCP / CLI / profiles          12-discovery        13-system          11-instances
        |  start/stop/restart            | listeners         | Router,Services   | InstancePreparer
        v                                v                   v                   v
 +-----------------------------------------------------------------------------------------+
 |  Runtime (facade)       HashMap<InstanceId, InstanceHandle>                              |
 |    |                                                                                     |
 |    +-- Supervisor actor (1 per instance, owns its state machine and operation lock)      |
 |    |      pure fn transition(State, Ev) -> (State, Vec<Effect>)  <- table in 3.14         |
 |    |      effect runner does the IO: units, probes, router, timers                       |
 |    |                                                                                     |
 |    +-- UnitPoller   (1 task) one batched `systemctl --user show` -> snapshots -> actors  |
 |    +-- LogHub       (1 task) one `journalctl --user -f` -> rings + per-instance channels |
 |    +-- ShellEnv     (cached login-shell environment)                                     |
 |    +-- PortLeases   (pure allocator over the registry's lease table)                     |
 +-----------------------------------------------------------------------------------------+
        |  systemd-run --user / systemctl --user                      |
        v                                                             v
   lantern-inst-<slug>.service  (in app-lantern.slice)         journald (user journal)
```

Source layout (`crates/lantern-daemon/src/`): `runtime/{mod,plan,env,launcher,systemd,poller,
supervisor,readiness,health,logs,deps,deploy,passive,reconcile}.rs` and `ports/mod.rs` (the
core doc keeps `ports/` separate). `plan`, `launcher`, `deps`, the allocator and `transition`
are pure functions: no tokio, no IO, unit-testable with plain `#[test]`. Types that appear in
API payloads (`ExitInfo`, `HealthSummary`, `RuntimeIssue`, ...) live in `lantern-core`.

Design rule used throughout: **the daemon decides, systemd executes, the journal remembers.**
Every effect the runtime performs on the system is logged at `debug` as the equivalent shell
command, so any step can be reproduced by hand.

### 3.2 Launch plan: what "start" means for an instance

`plan::resolve(&Instance, &Settings) -> Result<LaunchPlan, PlanError>` is a pure function of the
resolved manifest. A plan has up to two components; the instance is `Running` when all present
components are ready.

```rust
pub struct LaunchPlan {
    pub process: Option<ProcessPlan>,   // something Lantern starts and watches
    pub site:    Option<SitePlan>,      // something the router serves
    pub depends_on: Vec<DepRef>,
}
pub enum ProcessPlan {
    Unit(UnitSpec),                     // run.cmd under a transient unit (the common case)
    Deploy(DeploySpec),                 // deploy.start/stop/... (3.10); process lives elsewhere
}
pub struct UnitSpec {
    pub cmd: String, pub cwd: PathBuf, pub env: BTreeMap<String,String>,
    pub pre_start: Option<String>, pub post_stop: Option<String>,
    pub port: PortPolicy,               // Leased | Fixed(u16 from upstream_url) | None (portless worker)
    pub startup_timeout: Duration,
}
pub enum SitePlan {
    Upstream(UpstreamKind),             // route to the process port, or to a fixed upstream_url
    Php { root: PathBuf },              // Caddy php_fastcgi -> php-fpm socket
    Static { root: PathBuf },           // Caddy file_server
}
```

Resolution table (first matching row wins; `type` is the manifest's `proxy|php|static|unknown`):

| `deploy.start` | `run.cmd` | `upstream_url` | `type` | Result |
|---|---|---|---|---|
| set | any | any | any | `process = Deploy`, `site = Upstream`. (v1: `deploy.start` replaces `run.cmd` for Start.) Needs `${PORT}` in a deploy command **or** `upstream_url`, else `NeedsConfig` (v1 rule kept). |
| - | set | - | proxy/unknown | `process = Unit(port: Leased)`, `site = Upstream(process)`. The default case. |
| - | set | set | any | `process = Unit(port: Fixed)`, `site = Upstream(Fixed)`. `${PORT}` expands to the upstream's port if its host is loopback. |
| - | set | - | php/static | `process = Unit(port: Leased only if cmd uses ${PORT}, else None)`, `site = Php/Static`. Companion process (e.g. `pnpm dev` for Vite HMR beside Laravel). |
| - | - | set | any | `process = None`, `site = Upstream(Fixed)`. Externally run service given a domain. |
| - | - | - | php | `process = None`, `site = Php`. |
| - | - | - | static | `process = None`, `site = Static`. |
| - | - | - | unknown/proxy | `PlanError::NoRunCommand` -> status `NeedsConfig`. |

`${...}` usage is detected by scanning the command for the core doc's template variable names.
`pre_start`/`post_stop` come from `run.*` (v1 parsed them; see 3.4.6).

### 3.3 systemd transient units

#### 3.3.1 Naming

```rust
const UNIT_PREFIX: &str = "lantern-inst-";
fn unit_name(id: &InstanceId) -> String {            // "myapp@feature-x" -> "lantern-inst-myapp_feature-x.service"
    format!("{UNIT_PREFIX}{}.service", id.as_str().replace('@', "_"))
}
fn instance_id_from_unit(unit: &str) -> Option<InstanceId>   // strip prefix+suffix, first '_' -> '@'
```

Rules: `ProjectId` and branch slugs are `[a-z0-9-]` (core doc), so `_` never occurs in an id and the
mapping is injective. Do not use `@` in unit names: systemd treats `name@instance.service` as a
template instance **(verified: it works, but systemd silently creates an extra `app-<prefix>.slice` per
prefix)**. The `InstanceId` is also stored in the unit as
`Environment=LANTERN_INSTANCE_ID=<id>` (raw id; `02` D2) so reconciliation can recover it from the unit alone.
A debug assertion enforces the charset; see Proposed change 2.

One-shot helper units (deploy commands, 3.10) are `lantern-op-<slug>-<verb>-<seq>.service`.

#### 3.3.2 Exact invocation

Built as an argv vector (no shell), by `systemd::start_unit(&UnitStart)`:

```
systemd-run --user
  --unit=lantern-inst-myapp_feature-x
  --slice=app-lantern.slice
  --description=Lantern: myapp@feature-x
  --service-type=exec
  --expand-environment=no
  --quiet
  --property=RemainAfterExit=yes
  --property=WorkingDirectory=/home/ryan/dev/myapp-wt/feature-x
  --property=KillMode=control-group
  --property=TimeoutStopSec=10s
  --property=StandardInput=null
  --property=StandardOutput=journal
  --property=StandardError=journal
  --property=SyslogIdentifier=lantern-inst-myapp_feature-x
  --property=LogRateLimitIntervalSec=10s
  --property=LogRateLimitBurst=20000
  --property=Environment=LANTERN_INSTANCE_ID=myapp@feature-x
  [--property=ExecStopPost=/bin/bash /run/user/1000/lantern/run/lantern-inst-myapp_feature-x.post.sh]
  -- /bin/bash /run/user/1000/lantern/run/lantern-inst-myapp_feature-x.sh
```

Why each choice:

| Flag / property | Reason |
|---|---|
| `--slice=app-lantern.slice` | Groups all dev servers: `systemctl --user status app-lantern.slice` shows the tree, a future memory/CPU cap is one property, and the journal field `_SYSTEMD_USER_SLICE=app-lantern.slice` lets one `journalctl -f` follow every instance, including ones started later **(verified)**. (`app.slice` is the XDG slice for user applications.) |
| `--service-type=exec` | `systemd-run` returns only after the program has really been `exec`ed, so a missing `bash`, bad `WorkingDirectory` or unreadable script is a non-zero exit of `systemd-run` **(verified)**, not a silent later failure. |
| `--expand-environment=no` | systemd otherwise rewrites `$`/`${VAR}` in the command line (`$$` becomes `$`) **(verified)**. We pass only a script path, but the flag guarantees nothing is ever mangled. (`%` specifiers are not expanded for `systemd-run` commands **(verified)**.) |
| `RemainAfterExit=yes`, no `--collect` | See 3.3.3: keeps exit status readable after the process ends. |
| `KillMode=control-group` (explicit) | Stop signals every process in the cgroup; leftovers after the main process exits are also killed. This is core requirement (b). |
| `TimeoutStopSec=10s` | SIGTERM, then SIGKILL after 10 s (setting `runtime.stop_timeout_secs`). systemd's default is 90 s, far too long for dev servers. |
| `StandardInput=null` + launcher keeps a pipe open | See 3.4.5 (Create-React-App style tools exit on stdin EOF; v1's Port had an open pipe). |
| `StandardOutput/Error=journal` | Logs without the daemon. See 3.5. |
| `LogRateLimit*` | Default journald limit is 10000 messages / 30 s per unit. We raise it to 20000 / 10 s so chatty bundlers are not truncated, but a runaway loop cannot flood the shared journal. |
| `Restart=` not set (default `no`) | Restarting is the supervisor's job (status, backoff, messages, dependency gating). |
| No sandboxing properties | Dev servers need the user's full filesystem and network. |

Before every start the runtime runs `systemctl --user stop <unit>` then `systemctl --user
reset-failed <unit>` (errors ignored): a loaded unit that is `failed` or `active (exited)` makes
`systemd-run` fail with "Unit ... was already loaded" **(verified)**.

Stop: `systemctl --user stop <unit>` (blocks until inactive, hard-capped at
`stop_timeout + 15 s` by the runtime). Never `kill` PIDs directly.

#### 3.3.3 Learning about exits: polling, not D-Bus

**Decision: poll `systemctl --user show` in one batched call; do not use zbus.**

Rationale: (1) with `RemainAfterExit=yes` and no `--collect`, unit state is *durable*: a crashed
unit stays `failed` and a clean exit stays `active (exited)` with the exit code readable, until
the runtime itself clears it. Polling therefore cannot lose an exit, even if the daemon is down
for an hour; a signal subscription would need the same poll as a backstop anyway. (2) One
`Command` call is trivial to fake in tests and to run by hand; D-Bus adds an async connection
lifecycle, `Subscribe()` bookkeeping and object-path escaping for little gain at tens of units.
(3) Cost is bounded: a single `systemctl` spawn per tick for all units, and no ticks at all
when nothing is expected to be running. The `UnitBackend` trait (3.16) lets a zbus
implementation replace the poller later without touching the supervisor.

```
systemctl --user --timestamp=unix show \
  -p Id,LoadState,ActiveState,SubState,Result,MainPID,ExecMainStatus,ExecMainCode,\
ExecMainStartTimestamp,ActiveEnterTimestamp,InvocationID,ControlGroup \
  'lantern-inst-*.service'
```

Output is blocks of `Key=Value` separated by a blank line, one block per *loaded* matching unit
**(verified; glob works, unloaded units are simply absent)**. Parsed to:

```rust
pub struct UnitSnapshot {
    pub unit: String, pub load: Load, pub active: Active /*Active|Inactive|Failed|Activating|Deactivating*/,
    pub sub: String, pub result: String,            // systemd "Result": success|exit-code|signal|oom-kill|timeout|...
    pub main_pid: Option<u32>, pub exit_code: Option<i32>, pub exit_kind: ExitCodeKind, // ExecMainCode: 1 exited, 2 killed, 3 dumped
    pub started_at: Option<Timestamp>, pub invocation_id: Option<String>, pub cgroup: Option<String>,
}
```

Poll cadence (adaptive, one task): **1 s** while any instance is `Preparing/Starting/Stopping`;
**3 s** while any managed unit is `Running/Unhealthy/Crashed-pending`; **dormant** (waits on a
`Notify`) when no unit is expected. Readiness probes can `poke()` the poller for an immediate
extra poll (for example when the unit's cgroup becomes empty).

Interpretation of a snapshot (`UnitState::classify`):

| Snapshot | Meaning | Runtime action |
|---|---|---|
| `active/running` | alive | nothing |
| `activating` | starting | nothing |
| `active/exited` (RemainAfterExit) | main process exited with code 0 | read `ExecMainStatus`, then `stop` unit to clear; treat as clean exit |
| `failed` (`Result` is `exit-code`, `signal`, `oom-kill` or `timeout`) | crashed | read `ExecMainStatus`/signal/`Result`, then `reset-failed`; feed `UnitExited` |
| `inactive`, `LoadState=not-found` | gone (we stopped it, or reboot) | if desired state is Up: `UnitGone` |
| `deactivating` | systemd is stopping it | wait |

How the three flags interact with crash detection:
- **`RemainAfterExit=yes`** keeps a zero-exit unit visible (`active (exited)`). Without it the unit
  is unloaded immediately and the exit status is gone **(verified)**.
- **`--collect`** (`CollectMode=inactive-or-failed`) would unload even *failed* units, destroying
  the evidence. Not used for long-running instances. Used only for one-shot deploy units (3.10),
  whose result we read from `systemd-run --wait`'s exit status.
- **`--wait`** makes `systemd-run` itself block and imply `--collect`. Used only for one-shots
  (`--pipe --wait --collect` returns the command's exit status and stdout **(verified)**).

#### 3.3.4 Environment of the user manager: lingering and graphical session

| Situation | Behaviour |
|---|---|
| Daemon is a user service; user manager running (normal) | All of the above. |
| No lingering, and the user logs out of every session | systemd stops `user@UID.service`: the daemon *and* every dev server die. This is systemd semantics, not a Lantern bug. Startup preflight reads `loginctl show-user $UID -p Linger --value`; if `no`, `SystemHealth` carries a warning "dev servers stop when you log out; run `loginctl enable-linger`". `lantern init` (13-system) offers to enable it. |
| Daemon started before the graphical session exists (early boot with linger) | Units inherit the user manager's *current* environment at spawn time. `DISPLAY`/`WAYLAND_DISPLAY` appear in the manager only after Plasma imports them. The runtime never caches or hard-codes session variables (v1 hard-coded `DISPLAY=:0`); they come from the manager at each spawn. Processes needing a display fail only if started before login; the failure message includes a hint when stderr mentions `DISPLAY`/`WAYLAND`. |
| `systemctl --user is-system-running` prints `degraded` | **Normal** (some unrelated unit failed; observed on the target machine). Preflight accepts `running` and `degraded`; `starting` waits up to 30 s; anything else, or a missing `XDG_RUNTIME_DIR`/bus, makes the runtime `unavailable`. |
| User manager unavailable (daemon run via `sudo`, cron, bare `ssh` without PAM session) | Every `start` fails fast with `systemd_user_unavailable` and the explanation. **There is deliberately no fallback that spawns children directly**: it would bring back orphaned processes and servers dying with the daemon. |

### 3.4 The environment problem (PATH, version managers)

#### 3.4.1 Why units are missing your tools

A user unit gets the *user manager's* environment, not a login shell's. nvm, fnm, pyenv, asdf and
mise are initialised from `~/.bashrc`/`~/.zshrc` (interactive files), so `pnpm`, `node`, `python`
resolve only if the manager happens to carry those PATH entries. On the target machine the
manager's PATH has one hard-coded nvm default (`.../versions/node/v25.9.0/bin`) because Plasma
imported it at login. That is luck, not design, and it is wrong for any project with a different
`.nvmrc`. v1's `sh -lc` (dash) never reads `.bashrc`, so v1 only worked where tools were on the
plain login PATH.

#### 3.4.2 Options considered

| Option | Verdict |
|---|---|
| **A. Run every command through `$SHELL -l -i -c` at spawn time.** Freshest env. | Rejected. 0.3 s+ per start with a light `.bashrc`, seconds with oh-my-zsh/nvm; interactive shells **ignore SIGTERM** (bash) and print job-control noise without a tty, which breaks graceful stop; dotfile side effects (tmux, prompts that `read`) can hang a unit. |
| **B. `systemctl --user import-environment` / `set-environment`.** | Rejected. Mutates the global user-manager environment for all units, and the daemon is itself such a unit (circular). |
| **C. Capture the login-shell environment once, cache it, refresh on triggers; per-project tool selection in the launcher script.** | **Chosen.** Cost is paid once, not per start; the same approach VS Code and Zed use for "shell environment resolution"; the result is a plain file you can read. |
| D. Hard-code a list of tool directories. | Used only as a safety net (3.4.4). |

#### 3.4.3 Layer 1: capturing the login-shell environment (`ShellEnv`)

Command (spawned by the daemon via tokio, in a new session so there is no controlling tty and
no job-control stall; `stdin=/dev/null`; whole process group killed after
`runtime.env_capture_timeout_secs` = 10):

```
$SHELL -l -i -c 'printf "<<LANTERN_ENV_BEGIN>>"; env -0; printf "<<LANTERN_ENV_END>>"'
env: LANTERN_RESOLVING_ENV=1  TERM=dumb
```

- `$SHELL` = the login shell from the passwd entry (`getent passwd`), overridable by
  `runtime.shell` in config.toml. Works for bash, zsh and fish (all accept `-l -i -c`). Measured
  on the target machine: 0.29 s for bash with nvm sourced **(verified)**.
- Output between the markers is parsed as NUL-separated `KEY=VALUE`. Anything else the dotfiles
  print (motd, `bash: no job control in this shell`) is ignored; stderr is kept for debugging.
- `LANTERN_RESOLVING_ENV=1` lets users guard slow or interactive-only dotfile blocks
  (`[ -n "$LANTERN_RESOLVING_ENV" ] && return`), as VS Code's equivalent variable does.
- **Session variables are never taken from the shell.** The deny set (`DISPLAY`,
  `WAYLAND_DISPLAY`, `XAUTHORITY`, `XDG_RUNTIME_DIR`, `XDG_SESSION_*`, `XDG_SEAT`, `XDG_VTNR`,
  `DBUS_SESSION_BUS_ADDRESS`, `SESSION_MANAGER`, `SSH_CONNECTION/CLIENT/TTY`, plus per-shell noise
  `_ SHLVL PWD OLDPWD TERM TERM_PROGRAM* COLORTERM LS_COLORS LANTERN_RESOLVING_ENV`) is dropped;
  those keep the user manager's live value. `PATH` is merged: shell `PATH` entries first, then
  any manager entries not already present, de-duplicated preserving order.
- Result written to `$XDG_CACHE_HOME/lantern/shell-env.sh` (mode 0600) as `export KEY='value'`
  lines (single-quote-escaped), atomically (tmp + rename). Launcher scripts `source` it, so
  there is exactly one copy. `lantern env show` prints it; `lantern env refresh` re-captures.
- The parsed map is also kept in memory (`ShellEnv { vars, shell, captured_at, source, warnings }`)
  for toolchain detection (`which fnm`), diagnostics and `SystemHealth`.

**When it runs:** (1) in the background at daemon start (a start request that arrives earlier
awaits it, up to 15 s); (2) on `Runtime::refresh_shell_env()` (CLI/GUI/API "refresh
environment"); (3) lazily before a start if the mtime of any of `~/.profile ~/.bash_profile
~/.bashrc ~/.zshenv ~/.zprofile ~/.zshrc ~/.config/fish/config.fish ~/.config/environment.d/*`
changed since capture (a handful of `stat` calls); (4) automatically **once** (rate-limited to
one per 5 minutes) when an instance exits with status 127 within 15 s of starting, then retries
once. A refresh never touches running processes.

**Failure (timeout, shell missing, garbage):** fall back to `source: Fallback` = manager env +
the safety-net directories below, record a warning shown in `SystemHealth` and in the instance's
`message` if the instance then fails with 127. Starts still proceed.

#### 3.4.4 Safety net: well-known tool directories

After merging, append (only if the directory exists and is not already on PATH):
`~/.local/bin`, `~/.cargo/bin`, `~/.volta/bin`, `~/.asdf/shims`, `~/.pyenv/shims`,
`~/.local/share/mise/shims`, `~/.local/share/pnpm`, `~/.bun/bin`, `~/.deno/bin`,
`/home/linuxbrew/.linuxbrew/bin`. This covers a failed capture and tools installed after login.

#### 3.4.5 Layer 2: per-project toolchain, resolved at start

Shim-based managers (**volta, asdf, pyenv, mise shims**) pick the version from the current
directory at exec time, so they need nothing beyond PATH from Layer 1 and `cwd` = the checkout
(the launcher `cd`s first). Managers that mutate the shell (**nvm, fnm**) and virtualenvs need an
explicit step in the launcher. `plan` detects, looking from `cwd` up to the checkout root, in this
order (first match wins):

| Detected (against captured PATH / filesystem) | Launcher step |
|---|---|
| `mise` on PATH **and** (`mise.toml`, `.mise.toml` or `.tool-versions`) | the final line becomes `exec mise exec -- bash -c '<cmd>'` instead of `exec bash -c '<cmd>'`. Handles Node/Python/etc. from `.tool-versions`, `.node-version`, `.python-version` per mise config. |
| `fnm` on PATH **and** (`.node-version` or `.nvmrc`) | `eval "$(fnm env --shell bash)"` then `fnm use --version-file-strategy=recursive --silent-if-unchanged \|\| lantern_fail "..."`. (`fnm use` with no version resolves from the version files; `fnm env --shell` and `fnm use` flags verified in fnm's docs.) |
| nvm available (`$NVM_DIR/nvm.sh` or `~/.nvm/nvm.sh`) **and** `.nvmrc` | `. "$NVM_DIR/nvm.sh" --no-use; nvm use --silent \|\| lantern_fail "..."`. Adds about 0.1 s (measured 90 ms **verified**), only for projects with `.nvmrc`. |
| `.venv/pyvenv.cfg` (or `venv/pyvenv.cfg`) in cwd | `export VIRTUAL_ENV=<dir>; export PATH=<dir>/bin:$PATH`. (`uv run ...` and `poetry run ...` need nothing; `uv` is on PATH from Layer 1.) |
| anything else (volta, asdf, pyenv, system tools) | no step |

`lantern_fail` prints `[lantern] <message>` to stderr (it lands in the instance log) and exits
**78**. Exit codes **78** (Lantern toolchain error), **126** and **127** (not executable /
command not found) are *deterministic*: never retried (3.9), and mapped to issue codes
`ToolchainMissing` / `CommandNotFound` with the hint "run `lantern env refresh`, or install the
version (`nvm install`, `fnm install`)".

Package managers (pnpm, npm, yarn, bun) come with Node, corepack or `~/.local/share/pnpm`/`~/.bun/bin`
(Layer 1 + safety net).

#### 3.4.6 The launcher script

Written to `$XDG_RUNTIME_DIR/lantern/run/<unit-name-without-.service>.sh`, mode 0600, directory
0700, regenerated on every start, left in place while the unit exists (so `cat` shows exactly
what is running; `/run/user/UID` is tmpfs, cleaned at logout). Environment values never appear
in `systemctl show` or `ps`; only the script path does.

```bash
#!/usr/bin/env bash
# Generated by Lantern for instance 'myapp@feature-x' (2026-11-06T17:50:00Z). Run by hand: bash <this file>
lantern_fail() { echo "[lantern] $*" >&2; exit 78; }
cd '/home/ryan/dev/myapp-wt/feature-x' || lantern_fail "working directory missing"
. '/home/ryan/.cache/lantern/shell-env.sh'                     # Layer 1: captured login-shell env
export PATH="$PATH:/home/ryan/.volta/bin"                      # Layer 1b: safety-net dirs that exist
export NVM_DIR='/home/ryan/.nvm'                               # Layer 2: toolchain (only if detected)
. "$NVM_DIR/nvm.sh" --no-use && nvm use --silent || lantern_fail "nvm: .nvmrc version not installed (run: nvm install)"
export LANTERN_PORT='29003' LANTERN_HOST='127.0.0.1' LANTERN_DOMAIN='feature-x.myapp.glow'
export LANTERN_URL='https://feature-x.myapp.glow' LANTERN_PROJECT='myapp' LANTERN_INSTANCE='myapp_feature-x' LANTERN_INSTANCE_ID='myapp@feature-x'
export LANTERN_BRANCH='feature/x' LANTERN_BRANCH_SLUG='feature-x'
export LANTERN_CHECKOUT='/home/ryan/dev/myapp-wt/feature-x' LANTERN_PRIMARY='/home/ryan/dev/myapp'
export PORT="$LANTERN_PORT" DOMAIN="$LANTERN_DOMAIN" PROJECT_NAME="$LANTERN_PROJECT"   # v1 names
export 'APP_ENV'='development'                                  # manifest run.env (values expanded + quoted)
{ docker compose up -d db; } || exit $?                         # run.pre_start, if any
exec bash -c 'pnpm dev --port ${LANTERN_PORT}' < <(exec sleep infinity)
```

Rules for generating it (`launcher::render(&LaunchSpec) -> String`, golden-file tested):
- Every literal goes through one function `sh_quote(&str)` (single quotes, `'` becomes `'\''`).
  NUL bytes in values are an error. No other interpolation exists.
- **Template variables in command strings become shell parameter expansions**: `${PORT}` in
  `run.cmd`/`pre_start`/`post_stop`/`deploy.*` is rewritten to `${LANTERN_PORT}` (and so on), *not*
  textually substituted. Reason: `${BRANCH}` and `${CHECKOUT}` can contain shell metacharacters
  (git ref names may contain `$`, `;`, `&`, `(`, backticks); a parameter expansion is never
  re-parsed as code, textual substitution would be command injection. In `run.env` *values* (data,
  not code) variables are substituted textually and then quoted. Core provides both functions
  (Proposed change 6).
- `exec bash -c '<cmd>'`: the command runs under **bash**, not the user's login shell, so syntax
  is predictable. When `<cmd>` is a single simple command bash `exec`s it directly, so the
  unit's main PID is the real server and exit codes/signals are exact.
- `< <(exec sleep infinity)` gives the command an open, never-ending stdin. Without it
  `StandardInput=null` hands tools an immediate EOF; Create-React-App's `react-scripts start`
  exits on stdin EOF unless `CI=true`. v1 (BEAM Port) had an open pipe, so this preserves
  behaviour. The `sleep` lives in the unit's cgroup and dies with it.
- **Precedence, lowest to highest:** user-manager env < captured shell env < safety-net PATH <
  toolchain/venv < settings `[runtime.env]` < Lantern variables < `env_file` (if the manifest has
  `deploy.env_file`, parsed as dotenv) < manifest `run.env`. (`run.env` wins over `PORT` as in v1;
  the readiness check still uses the leased port and the diagnosis in 3.7 will fire if they
  differ.)
- `post_stop` (if any) is written to a second script and attached as `ExecStopPost=`; systemd runs
  it after the main process ends for any reason (stop or crash), same cwd. It never blocks
  restart beyond `TimeoutStopSec`.

### 3.5 Logs

**Decision: journald is the store; the daemon keeps a bounded in-memory ring and one follower.**

Why journald over `StandardOutput=append:<file>`: timestamps for free; survives daemon restarts
and `systemctl --user stop`/unit garbage-collection; retention and disk bounding are journald's
(a runaway server cannot fill the disk or need Lantern rotation code); the data is readable with
standard tools. Costs, all verified and handled:

| journald behaviour **(verified)** | Handling |
|---|---|
| stdout and stderr of a unit share one `_STREAM_ID` and `PRIORITY=6`: **cannot be told apart** | `LogLine.stream` is `Stdout` for all process output and `System` for Lantern-generated lines. `Stderr` is never emitted in v2.0 (v1 also merged them). |
| A `MESSAGE` containing an ESC byte (ANSI colour) or invalid UTF-8 is emitted by `journalctl -o json` as a JSON **array of byte values**, not a string | Parser accepts `String` or `Vec<u8>` (`#[serde(untagged)]`), converts lossily, then strips ANSI/OSC sequences. A `\r` keeps only the text after the last `\r` (progress bars). |
| Fields over 4096 bytes come out as `null` unless `--all` | Always pass `--all`. Lines over 48 KiB are split by journald (`_LINE_BREAK=line-max`). The ring truncates each line to 8 KiB. |
| `LogExtraFields=` is **not** applied to the unit's stdout stream entries | Do not rely on it; match on `_SYSTEMD_USER_SLICE` / `_SYSTEMD_USER_UNIT` instead. |
| `journalctl -u <glob>` expands the glob once at start (units started later are missed) | Follow by slice match `_SYSTEMD_USER_SLICE=app-lantern.slice`, which includes later units. |

**Live feed (`LogHub`).** One child process for all instances, restarted with backoff
(1 s to 30 s) if it dies:

```
journalctl --user --follow --output=json --all --no-pager \
           --after-cursor=<anchor-or-last-seen> _SYSTEMD_USER_SLICE=app-lantern.slice
```

Each JSON line: map `_SYSTEMD_USER_UNIT` (`lantern-inst-*.service`) to an `InstanceId`
(unknown units, and `lantern-op-*` one-shots, are ignored), decode `MESSAGE`, build `LogLine
{ instance, stream: Stdout, line, ts: __REALTIME_TIMESTAMP }`, push into the instance's ring and
its broadcast channel, remember `__CURSOR` as `last_seen`.

> **Reconciled → 02 D6:** `lantern-op-<safe>-setup-*` units are **not** ignored: they map to the owning instance as `Stream::Setup`; `lantern-op-<safe>-deploy-*` map as `Stream::System`. `deploy::run_oneshot` is public as `Runtime::run_oneshot(OneShotSpec) -> OperationHandle` and is what 11-instances calls for `setup` commands.


**Ring buffer.** Per instance `VecDeque<LogEntry>` bounded by `logs.ring_lines` (default 2000)
**and** `logs.ring_bytes` (default 2 MiB), oldest dropped first. A `broadcast::Sender<LogLine>`
(capacity 1024) per instance feeds subscribers; a slow subscriber gets `Lagged` and the API
layer sends the client a `resync` (core doc section 9). Lantern's own lifecycle lines
(`[lantern] started pid 4242`, `[lantern] exited with code 1; restarting in 4 s (2/5)`,
`[lantern] stopping`) are pushed into the ring and channel as `stream: System`; they are *not*
written to the journal (history queries show systemd's own lines instead).

```rust
pub struct LogHub { ... }
impl LogHub {
    /// Atomic: backlog snapshot + live receiver, no gap, no duplicate (single lock).
    pub fn subscribe(&self, id: &InstanceId, backlog: usize) -> (Vec<LogLine>, broadcast::Receiver<LogLine>);
    pub fn tail(&self, id: &InstanceId, n: usize) -> Vec<LogLine>;                 // ring only; instant
    pub async fn history(&self, id: &InstanceId, q: HistoryQuery) -> Result<Vec<LogLine>, LogError>; // journalctl
}
pub struct HistoryQuery { pub limit: usize, pub since: Option<Timestamp>, pub until: Option<Timestamp>,
                          pub grep: Option<String>, pub current_run_only: bool }
```

**History / before a daemon restart.** `history()` runs `journalctl --user -o json --all
--no-pager -n <limit> [--since --until] [-g <regex>] _SYSTEMD_USER_UNIT=<unit>` (adding
`_SYSTEMD_INVOCATION_ID=<id>` for `current_run_only`). About 25 ms **(verified)**. It works after
the unit is gone, after the daemon restarted, and for stopped instances. Used by `lantern logs`,
MCP `get_project_logs`, and the GUI's "load earlier".

**Rebuilding rings on daemon start (gap-free).** (1) run `journalctl --user -n 1 -o json --all`
and keep that entry's `__CURSOR` as the anchor and its `__REALTIME_TIMESTAMP` as `anchor_ts`;
(2) start the follower with `--after-cursor=<anchor>` **(verified)**; (3) for each adopted
instance run `history(limit = ring_lines)` and drop entries newer than `anchor_ts` (the follower
delivers those). Duplicates and gaps are both avoided without persisting a cursor file.

**Retention.** Entirely journald's (`SystemMaxUse`, rotation). Lantern does not delete journal
entries when an instance is removed; they age out. Ring memory is freed on `InstanceRemoved`.

**Deploy-managed instances.** `deploy.logs` (if configured) is run on demand as a one-shot and
its output returned as the log text (v1 behaviour); there is no live feed for it. Output of
`deploy.start/stop/restart` is captured and appended to the ring as `System` lines.

### 3.6 Port allocation (`ports/`)

State: `port_leases: BTreeMap<InstanceId, u16>` inside state.json (owned by the registry actor;
the runtime mutates it through `RegistryHandle`). The allocator is a pure function:

```rust
pub struct PortRange { pub start: u16, pub end: u16 }   // inclusive
pub fn allocate(leases: &BTreeMap<InstanceId,u16>, id: &InstanceId, range: PortRange,
                reserved: &[u16], is_free: impl Fn(u16) -> bool) -> Result<u16, PortError>;
```

- **Sticky:** if `id` already has a lease, that port is returned *if it is still free*.
- **Else** the lowest port in range that is (a) not leased to any instance, (b) not in `reserved`
  (daemon port 4777, Caddy admin 2019, 80, 443), and (c) `is_free`.
- `is_free(p)` is the runtime's conflict check against **actual listeners**, two layers: (1) the
  scanner snapshot (`ListenerSource::listeners_on_port(p)` empty), (2) an authoritative bind
  probe: `std::net::TcpListener::bind` on `127.0.0.1:p` and (if IPv6 is up) `[::1]:p`, then
  drop. (std sets `SO_REUSEADDR` on Unix, matching what servers do, so TIME_WAIT does not count.)
  A bind probe also catches listeners owned by other users that discovery cannot see.
- **Re-checked at spawn time.** Preparing re-runs `is_free(lease)` just before writing the
  launcher. If a foreign process grabbed the leased port meanwhile, the instance is re-leased to
  a new port, state.json updated, and `message` says "port 29003 was busy; using 29017".
  Stickiness is a preference, not a promise.
- **Released** only when the registry reports `InstanceRemoved` (and at reconcile, for leases
  whose instance no longer exists). Never on stop or crash (v1 released on stop, so ports
  moved every time).
- `upstream_url` instances (`PortPolicy::Fixed`) and portless workers hold no lease.
- Exhausted range -> `RuntimeIssue{PortsExhausted}`, instance stays `Stopped`, error returned.
- **Default range 29000-29999** (`[ports] range_start/range_end`), not v1's 41000-42000: Linux's
  ephemeral range is `32768-60999` (`/proc/sys/net/ipv4/ip_local_port_range` **(verified)**), and
  outgoing connections can steal a leased port there, causing sporadic "address in use". At daemon
  start the runtime reads that sysctl and puts a warning in `SystemHealth` if the configured range
  overlaps it.

### 3.7 Readiness

**Definition (process with a port):** the instance is *ready* when a TCP socket in state LISTEN
exists on the expected port (any of `127.0.0.1`, `::1`, `0.0.0.0`, `::`, or the loopback address
it chose) **and its owning process is in the unit's cgroup.** Listening alone is not enough: it
could be a stale process or another project (v1 bug).

Data comes from `12-discovery` (assumed interface, section 4.2):
`listeners_for_pids(&[u32])` (sockets owned by the unit's PIDs) and `listeners_on_port(u16)`
(any owner). The unit's PIDs are read from `/sys/fs/cgroup/<ControlGroup>/cgroup.procs`
**(verified)**; a listener's cgroup is checked via `/proc/<pid>/cgroup` (`0::/user.slice/.../
lantern-inst-X.service`) when only a PID is known. The same helper, `unit_of_pid(pid) ->
Option<UnitName>`, is exported to discovery so it can set `Listener.managed` and match listeners
to instances without cwd heuristics.

Probe loop while `Starting` (every 250 ms for 10 s, then 500 ms):

```
1. snapshot = latest UnitSnapshot; if not active -> UnitExited event (do not keep probing)
2. pids = cgroup.procs; if empty -> poke poller, continue
3. own = listeners_for_pids(pids)
   a. own has the expected port            -> READY (record bind addr -> RuntimeState.upstream)
   b. own non-empty, none on expected port -> remember "listening on :X instead" (hint in message)
4. foreign = listeners_on_port(expected) with pid not in pids
   -> remember holder (pid, exe, cmdline) for diagnosis
5. now >= deadline (startup_timeout, default 60 s; manifest run.startup_timeout) -> failure below
```

`upstream` = `127.0.0.1:port` if bound to loopback/wildcard IPv4, `[::1]:port` if bound only to
`::1` (Node >= 17 `localhost`), the literal address otherwise. `13-system` must route to
`RuntimeState.upstream`, not assume `127.0.0.1`. A wildcard bind (`0.0.0.0`/`::`) adds a
`message` note "exposed on all interfaces".

**Failure diagnosis at the deadline** (`readiness::diagnose`), most specific first:

| Observation | Status | Issue code and message |
|---|---|---|
| Unit exited before ready | `Crashed` (retry policy 3.9) | `ExitedNonZero` / `CommandNotFound` / ... with the exit info and last 10 log lines |
| Server listens on another port, none on expected (ignoring inspector ports 9229, 9230, 9222) | **`NeedsConfig`**, unit stopped, no retry | `PortMismatch`: "myapp is listening on 127.0.0.1:5173 but Lantern assigned port 29003. Use `--port ${PORT}` in `run.cmd`, or set `upstream_url: http://127.0.0.1:5173` to use a fixed port." Detected **early**: once an own listener on a non-expected port has been present for 15 s while the expected port is still unbound. |
| Foreign process holds the expected port | `Crashed`, retry allowed | `PortInUse`: "port 29003 is held by pid 4242 (node server.js)". |
| Nothing listening, unit alive | `Crashed` (final, no retry), unit stopped | `StartupTimeout`: "did not listen on :29003 within 60 s (see logs)" |

`NeedsConfig` is used for port mismatch because the manifest is wrong; retrying cannot help. A
startup timeout is not retried either (a hung server will hang again); the user can start again.

**Other kinds of "ready":**

| Plan | Ready when |
|---|---|
| `Unit`, `port: None` (portless worker/companion) | Unit active and still alive after `runtime.settle_secs` (2 s). |
| `Unit`, `port: Fixed` (loopback `upstream_url`) | Same as leased port, with the upstream's port. |
| `Unit`/`Deploy` with non-loopback `upstream_url` | TCP `connect` to `host:port` succeeds (no cgroup check possible). |
| `Deploy` | `deploy.start` exited 0, then the expected port is listening (*any owner*; the process is not in our cgroup) or, with no port, `deploy.status` exits 0. |
| Upstream-only (no process) | TCP connect to the upstream succeeds. |
| `Php` / `Static` | See 3.11. |

### 3.8 Health checks

`HealthSummary` (core type, defined here, Proposed change 1):

```rust
pub enum HealthState { Unknown, Healthy, Failing }
pub struct HealthSummary {
    pub state: HealthState, pub checked_at: Option<Timestamp>, pub latency_ms: Option<u32>,
    pub consecutive_failures: u32, pub last_error: Option<String>,
    pub recent: Vec<HealthSample>,           // newest first, max 10 (v1 parity)
}
```

- **Runs only** for instances in `Running` or `Unhealthy`. Stopped instances are not probed (v1
  probed everything enabled).
- **Probe:** with `health_endpoint`: `GET http://<RuntimeState.upstream><health_endpoint>`
  (or `upstream_url + endpoint`), timeout 3 s, no redirect following, success = status < 400
  (v1: only 2xx). Directly to the process, never through Caddy: no DNS or TLS-trust dependency.
  Without `health_endpoint`: a liveness probe, "the expected port is still LISTEN in the unit's
  cgroup" (a `/proc` read, no connect). Both produce the same `HealthSample`.
- **Schedule:** every `health.interval_secs` (10). While failing, the interval doubles per
  consecutive failure up to 60 s; a success resets it.
- **Transitions:** `Running -> Unhealthy` after `health.fail_threshold` (3) consecutive
  failures; `Unhealthy -> Running` after 1 success. Failures in the first `health.grace_secs`
  (15) after reaching `Running` are recorded but do not change the status (warm-up routes).
- **No automatic restart on unhealthy.** It would mask bugs and loop; it is informational
  (open question 8.4).
- `InstanceUpdated` is emitted only when `status`, `message`, `issue`, `upstream`, `restarts`
  or `health.state` change, not for every sample (avoids one SSE event per 10 s per instance);
  full history is available from the instance endpoint.

### 3.9 Crash handling

An exit is **unexpected** when a unit leaves the active state while the desired state is Up (a
stop requested by Lantern is never a crash).

Classification of an unexpected exit (`ExitInfo` carries `kind`, `code`, `signal`, `uptime`,
raw systemd `Result`):

| Exit | Treatment |
|---|---|
| code 0 | **Clean exit** -> `Stopped`, message "exited with code 0". For a port-expecting process that exits 0 *before* becoming ready: `Crashed` ("exited before listening"). |
| killed by SIGTERM, SIGINT or SIGHUP (or code 143/130/129 from a wrapper) | Someone stopped it deliberately -> `Stopped`, message "terminated by SIGTERM". |
| code 126, 127, 78 | Deterministic -> `Crashed`, **final** (no retry), issue `CommandNotFound`/`ToolchainMissing`. Triggers the one-time env refresh + single retry for 127 (3.4.3). |
| other non-zero, SIGKILL, SIGSEGV/abort, `oom-kill` | Crash: **retry** per policy below. `oom-kill` sets message "killed by the kernel (out of memory)". |
| `StartupTimeout`, `PortMismatch` | Not exits; handled in 3.7, no retry. |

**Retry policy** (settings `[runtime]`: `max_retries = 5`, `backoff_base_secs = 1`,
`backoff_cap_secs = 30`, `stable_after_secs = 60`):

- `streak` = consecutive crashes without a stable run. A run that reached `Running` and lasted
  `>= stable_after_secs` resets `streak` to 0 *before* counting its own exit (v1 never reset).
- On a crash: `streak += 1`. If `streak <= max_retries`: status `Crashed`, `next_retry_at = now +
  min(base * 2^(streak-1), cap)` (1, 2, 4, 8, 16 s), message "exited with code 1; restarting in
  4 s (2/5)". When the timer fires: `Preparing -> Starting` (full path: env, port re-check,
  readiness; dependencies already up are skipped). `RuntimeState.restarts` counts auto-restarts
  since the last manual start.
- If `streak > max_retries`: `Crashed` **final** (`issue: CrashLoop`): "crashed 6 times in a
  row; giving up. Last exit: code 1" with the last log lines attached to the issue detail. It
  stays until the user starts it (counters reset) or stops it.
- A manual `start`/`restart` always resets `streak` and `restarts`.
- A crash of a dependency does **not** cascade to dependents; they keep their status.

`diagnose_exit` also runs on every crash: if the last 20 log lines contain `EADDRINUSE` or
"address already in use", or a foreign listener holds the port, the issue is upgraded to
`PortInUse`.

### 3.10 `deploy.*` overrides and `upstream_url`

`deploy.start/stop/restart/logs/status` are shell snippets from the manifest. They are *not*
accepted over the API. They run through `deploy::run_oneshot`:

```
systemd-run --user --pipe --wait --collect --quiet --expand-environment=no
  --unit=lantern-op-<slug>-<verb>-<seq> --slice=app-lantern.slice
  --property=WorkingDirectory=<cwd> --property=KillMode=process --property=RuntimeMaxSec=<timeout+10>s
  -- /bin/bash <launcher-script>        # same launcher (env, toolchain) as a unit start
```

`--pipe` hands stdout/stderr to the daemon and `--wait` returns the command's exit status
**(verified)**. `KillMode=process` lets `docker compose up -d`, `pm2 start`, or a self-daemonising
script leave background children alive after the one-shot ends (the default control-group mode
would kill them when the unit finishes **(verified both ways)**). The daemon enforces `runtime.deploy_timeout_secs`
(120; 15 for `status`/`logs`) itself and on timeout runs `systemctl --user stop` on the unit.
Output is capped at 1 MiB. A command may not be interactive. This replaces v1's untimed
`System.cmd` inside the Manager process.

Mapping to the status machine:

| Command | Behaviour |
|---|---|
| `start` | `Preparing -> Starting`: run it; non-zero -> `Crashed` (final, `DeployCommandFailed`, output tail in detail; no auto-retry); zero -> readiness wait on the expected port (any owner). Replaces `run.cmd` for Start, as in v1. |
| `stop` | `Stopping`: run it; then wait up to `stop_timeout` for the port to stop listening -> `Stopped`. If it still listens: back to `Running`, error `stop_incomplete`. Lantern **does not kill** the listener (v1 did). If `deploy.stop` is not configured: route removed, `Stopped`, message "no deploy.stop configured; the process may still be running". |
| `restart` | If configured, used by `restart` instead of stop+start: `Running -> Starting` (message "restarting via deploy.restart"), run it, then readiness. Otherwise stop + start. |
| `status` | Liveness probe for deploy-managed instances: run every `health.interval_secs` while `Running/Unhealthy` (exit 0 = alive). 3 consecutive non-zero -> treated as an unexpected exit of the process (3.9 retry policy, retry = run `deploy.start`). Also callable on demand (`/deploy/status`). Without `deploy.status`, liveness = port listening. |
| `logs` | On demand: run, return output (v1 behaviour, 15 s timeout). No live feed. |

`upstream_url` semantics: Lantern allocates no port; the router sends traffic to that URL. With a
`run.cmd`, `${PORT}` is the upstream's port (loopback only) so `--port ${PORT}` stays correct.
Without a process the instance is *passive*: `Starting` until the upstream accepts a TCP
connection, then `Running`; if it stops accepting for 3 consecutive probes -> `Unhealthy`
("upstream unreachable"). No cgroup ownership check applies (nothing of ours runs).

### 3.11 PHP and static sites (Caddy + php-fpm), and other process-less instances

Nothing for Lantern to supervise, so "running" is defined by the router and php-fpm
(`13-system` interface in 4.2):

| Kind | `Running` means | Becomes `Unhealthy` when |
|---|---|---|
| `Php` | `router.publish()` succeeded **and** php-fpm's socket (`router.php_fpm()`, e.g. `/run/php/php8.3-fpm.sock`) accepts a connection | the socket stops accepting (probed every 10 s); message "php-fpm not reachable at <path>" |
| `Static` | `router.publish()` succeeded and the root directory exists | the root directory disappears |
| Upstream-only | see 3.10 | unreachable |

Lifecycle: `start`: `Preparing` (check php-fpm / root; if php-fpm is missing -> `NeedsConfig`
with `PhpFpmUnavailable`: "php-fpm socket not found; install php-fpm or run `lantern doctor`")
-> `Starting` (publish; instantaneous) -> `Running`. `stop`: `Stopping` (unpublish) -> `Stopped`;
php-fpm itself is a shared system service and is never touched. No port lease. A worktree
instance serves its own checkout (`<checkout>/<root>`), so `feature-x.myapp.glow` works.

A php/static project with a `run.cmd` is `Php + companion process`: `Running` requires both the
site and the companion unit (3.2). The companion follows the crash policy in 3.9 while the site
stays published.

`activated: bool` is persisted per instance in state.json for **site-only** plans, because they
have no unit to rediscover: on daemon start, `activated` instances are republished and marked
`Running` (after the php-fpm check), `Caddy` state being rebuilt from state, not the reverse.
Process-backed instances are *not* restarted after a reboot (the units are gone); auto-start
belongs to profiles.

### 3.12 `depends_on`

Syntax (manifest, unchanged from v1): a list of names. A bare name resolves **project first,
then Service**; `service:redis` forces a Service (`ProjectId` cannot contain `:`, so no
ambiguity). A project dependency means that project's **primary** instance; matching a
same-named branch worktree of the dependency is not done in v2.0 (open question 8.3).

Graph checks (`deps::validate`, pure): unknown name -> `NeedsConfig`/`DependencyMissing`;
dependency disabled -> `DependencyFailed`; **cycle** (including self-reference) -> `NeedsConfig`/
`DependencyCycle` with the path ("a -> b -> a"), reported on the instance being started. Also run
when a manifest is registered, for early warning.

**Start order.** `start(X)`: set `Preparing` with step "dependencies"; for every dependency
concurrently (`try_join_all`): `ensure_up(dep)` which is "if `Running`/`Unhealthy`, done;
if an operation is in flight, join it; else start it (recursively starting *its* dependencies
first)". Services use `ServiceControl::ensure_running`. Because the per-instance operation lock
deduplicates, diamonds start each node once, and the cycle check guarantees termination. Each
dependency has its own startup timeout. If any dependency fails: X goes back to `Stopped`
(nothing was spawned) with `DependencyFailed: "not started: dependency 'db' failed: <issue>"`
and the operation returns that error. If a dependency enters `Stopped/NeedsConfig/final
Crashed` *while* X waits, X aborts the same way.

**Stop semantics.**
- `stop(X)` stops **only X**. If running dependents exist, the response carries `warnings:
  ["web depends on api and is still running"]`. Rationale: least surprise, and consistent with
  how Services behave; a dev stopping `api` often wants to restart it by hand. (systemd's
  `Requires=` would cascade; we deliberately do not.)
- `stop(X, cascade: true)` (`lantern off api --cascade`) first stops running dependents,
  dependents-before-dependencies, then X.
- `stop_all` (`lantern off --all`) stops every instance in reverse topological order (dependents
  first). Services are never stopped by project operations.
- `restart(X)` restarts X only; dependents are untouched.
- Dependencies are not auto-stopped when no dependent remains (they may be used by others; a
  future "stop unused deps" option is out of scope).

### 3.13 Operation serialization

One supervisor actor per instance owns an `Option<InFlight { op, id, cancel: CancellationToken,
done: watch<Option<OpResult>> }>`. Requests go through the actor's mpsc channel, so they are
serialized by construction; policy for a request that arrives while an operation is in flight:

| In flight -> request | `Start` | `Stop` | `Restart` |
|---|---|---|---|
| none | run | run | run |
| `Start` | **join** (same result) | **preempt**: cancel the start, then stop | **preempt**: cancel, then restart |
| `Stop` | 409 `operation_in_progress` | **join** | 409 `operation_in_progress` |
| `Restart` | **join** | **preempt** | **join** |

Stop preempts because a hung start (60 s of readiness) must be cancellable; this refines core
doc section 8 (Proposed change 3). `Stop` also cancels a pending crash-retry timer. Auto-restarts
are internal operations of the same actor; a user `Start` joins one. Different instances
proceed concurrently. Every request returns `OperationHandle { id: OperationId, wait() ->
OpResult }`; steps are published as `OperationProgress { step: "dependencies" | "port" |
"prepare" | "env" | "spawn" | "wait-ready" | "publish" , ... }`.

The actor is a **pure transition function plus an effect runner**:

```rust
fn transition(s: &SupState, ev: Ev, now: Instant, cfg: &RuntimeCfg) -> (SupState, Vec<Effect>);
enum Effect { SetStatus(..), WriteLauncher, StartUnit, StopUnit{..}, ResetFailed, PublishRoute, Unpublish,
              ProbeReadiness, ScheduleTimer(TimerKind, Duration), EmitLog(LogLine), RunDeploy(Verb), ... }
```
The runner executes effects and feeds results back as events. All of 3.14 is table-tested
against `transition` with no systemd.

### 3.14 Status state machine

`InstanceStatus` is a projection of the supervisor's richer state. "Up" below means the desired
state is Up. Every row's actions also update `RuntimeState` (`message`, `issue`, `next_retry_at`,
counters) and emit `InstanceUpdated`.

| From | Trigger (guard) | To | Actions |
|---|---|---|---|
| **Stopped** | `start` (enabled, plan ok) | Preparing | reset streak/restarts/issue; set desired Up |
| | `start` (disabled) | Stopped | error `instance_disabled` |
| | `start` (plan error: no cmd, cycle, missing dep, bad manifest) | NeedsConfig | issue set; error `needs_config` |
| | `stop` | Stopped | no-op (idempotent) |
| | external matched listener appears (from discovery) | External | |
| | reconcile finds live unit | Starting or Running | adopt (3.15) |
| | reconcile finds `activated` site-only instance | Starting | republish |
| **Preparing** | deps ready, port leased, `InstancePreparer` ok, launcher written (process plan) | Starting | `StartUnit` / run `deploy.start` |
| | all ready, site-only plan | Starting | check php-fpm/root, `publish` |
| | dependency/port/prepare/env/spawn failure (nothing spawned) | Stopped | issue (`DependencyFailed`, `PortsExhausted`, `SystemdUnavailable`, ...) |
| | cycle / missing dep discovered | NeedsConfig | |
| | `stop` / preempt | Stopped | cancel op; nothing to kill |
| | `start` | Preparing | join |
| **Starting** | ready (3.7) | Running | `started_at`, `upstream`, publish route (failure sets `RouteFailed` but stays Running), start health loop |
| | unit exits before ready, retries available or code 0-before-ready | Crashed | schedule retry (3.9) |
| | unit exits before ready, deterministic code (126/127/78) | Crashed (final) | issue; maybe one env refresh retry |
| | deadline, own listener on another port | NeedsConfig | stop unit; `PortMismatch` |
| | deadline, nothing listening | Crashed (final) | stop unit; `StartupTimeout` |
| | `deploy.start` non-zero | Crashed (final) | `DeployCommandFailed` |
| | `stop` / preempt | Stopping | `StopUnit` |
| **Running** | unit exits 0 / terminated by SIGTERM,INT,HUP | Stopped | unpublish; clear unit; `last_exit` |
| | unit exits otherwise, retries available | Crashed | schedule retry |
| | unit exits otherwise, budget exhausted | Crashed (final) | `CrashLoop` |
| | health: `fail_threshold` consecutive failures after grace | Unhealthy | |
| | `stop` | Stopping | unpublish first (so users do not see 502s), then `StopUnit` |
| | `restart` | Stopping, then Preparing | |
| | manifest changed | Running | message "manifest changed; restart to apply" |
| **Unhealthy** | health success | Running | |
| | unit exits | Crashed / Stopped | as for Running |
| | `stop` / `restart` | Stopping | as for Running |
| **Stopping** | unit inactive (or never existed) | Stopped | `reset-failed`; `last_exit` only if it died before the stop |
| | not inactive after `stop_timeout + 15 s` | Stopping | operation error `stop_timeout`; message lists PIDs; next `stop` re-issues; (never silently marked Stopped) |
| | `restart` pending | Preparing | after Stopped |
| | `start` | Stopping | 409 |
| **Crashed** (retry pending) | retry timer fires (Up) | Preparing | auto-restart, `restarts += 1` |
| | `start` | Preparing | reset counters |
| | `stop` | Stopped | cancel timer |
| | external listener appears | External | cancel timer |
| **Crashed** (final) | `start` | Preparing | reset counters |
| | `stop` | Stopped | clear issue |
| **NeedsConfig** | `start` | Preparing, or NeedsConfig | re-resolve plan; issue refreshed |
| | manifest changed and plan now valid | Stopped | |
| | `stop` | NeedsConfig | no-op |
| **External** | external listener disappears | Stopped | |
| | `start` | External | 409 `externally_running` ("pid 1234 listens on :5173; stop it or adopt it") |
| | `stop` | External | 409 `not_managed` |

Notes: `Stopping` and `Preparing` are the only states where operations are in flight. `External`
is driven by discovery (it owns the transition in/out via `Runtime::note_external`); the runtime
only guards `start`/`stop`. Killing a foreign process is not offered in v2.0 (open question 8.6).

### 3.15 Reconciliation on daemon startup

Runs once, before accepting requests (target < 300 ms; the `show` call is a single spawn):

1. Preflight systemd (3.3.4); capture `ShellEnv` in the background.
2. `systemctl --user show 'lantern-inst-*.service'` -> snapshots. Compute the anchor and rebuild
   rings (3.5).
3. For each registry instance that persisted a `unit` or that has a snapshot:

| Found | Result |
|---|---|
| unit `active/running`, readiness satisfied (leased port LISTEN in cgroup, or portless) | adopt: `Running`; `started_at` from `ActiveEnterTimestamp`; spawn supervisor actor; republish route |
| unit `active/running`, not (yet) ready, within `startup_timeout` of `ActiveEnterTimestamp` | adopt as `Starting` with the remaining deadline |
| unit `active/running`, not ready, past the deadline | adopt as `Starting` with an expired deadline -> normal timeout handling (`StartupTimeout`, stop) |
| unit `failed` | `Crashed` (final, **no auto-retry** since we do not know when it died), `ExitInfo.observed_late = true`, `reset-failed` |
| unit `active (exited)` | `Stopped` (clean exit), unit stopped |
| no unit, state.json says `Starting/Running/Unhealthy/Stopping/Crashed` | `Stopped`, message "not running (unit gone, e.g. after reboot)"; `last_exit` kept |
| no unit, site-only plan and `activated` | republish -> `Running` |
| unit exists with no registry instance (orphan `lantern-inst-*`) | left running, logged and listed in `SystemHealth.orphans`; never killed automatically |

4. Garbage-collect `port_leases` entries whose instance no longer exists.
5. Hand the `Listener` snapshot interest to discovery (so `Listener.managed` uses `unit_of_pid`).
6. Daemon shutdown (SIGTERM) stops the pollers/follower and exits. **It does not stop units.**

### 3.16 Settings and constants

Keys the runtime reads (to be added to `config.toml` by `14-config`; defaults shown):

```toml
[ports]
range_start = 29000
range_end   = 29999

[runtime]
shell                    = ""     # empty = login shell from passwd
startup_timeout_secs     = 60     # run.startup_timeout in a manifest overrides
stop_timeout_secs        = 10
settle_secs              = 2      # portless workers
env_capture_timeout_secs = 10
deploy_timeout_secs      = 120
max_retries              = 5
backoff_base_secs        = 1
backoff_cap_secs         = 30
stable_after_secs        = 60
[runtime.env]                      # extra env for every instance (lowest manifest-adjacent layer)
# FOO = "bar"

[health]
interval_secs  = 10
timeout_secs   = 3
fail_threshold = 3
grace_secs     = 15

[logs]
ring_lines = 2000
ring_bytes = 2097152
```

Backend trait (so supervisor tests need no systemd, and a D-Bus backend could replace polling):

```rust
#[async_trait] pub trait UnitBackend: Send + Sync {
    async fn start(&self, spec: &UnitStart) -> Result<(), UnitError>;        // systemd-run
    async fn stop(&self, unit: &str) -> Result<(), UnitError>;               // systemctl stop
    async fn reset_failed(&self, unit: &str) -> Result<(), UnitError>;
    async fn show_all(&self) -> Result<Vec<UnitSnapshot>, UnitError>;        // batched show
    async fn run_oneshot(&self, spec: &OneShot) -> Result<OneShotResult, UnitError>;
}
// impls: SystemctlBackend (production), FakeBackend (tests: scripted snapshots and failures)
```

---

## 4. Interfaces

### 4.1 Exposed by the runtime

```rust
pub struct Runtime { /* facade */ }
impl Runtime {
    pub async fn start(&self, id: &InstanceId, o: StartOpts)   -> Result<OperationHandle, RuntimeError>;
    pub async fn stop(&self, id: &InstanceId, o: StopOpts)     -> Result<OperationHandle, RuntimeError>; // StopOpts{cascade}
    pub async fn restart(&self, id: &InstanceId)               -> Result<OperationHandle, RuntimeError>;
    pub async fn stop_all(&self)                               -> Result<OperationHandle, RuntimeError>;
    pub fn   state(&self, id: &InstanceId)                     -> Option<RuntimeState>;
    pub fn   subscribe_logs(&self, id: &InstanceId, backlog: usize) -> (Vec<LogLine>, broadcast::Receiver<LogLine>);
    pub async fn log_history(&self, id: &InstanceId, q: HistoryQuery) -> Result<Vec<LogLine>, LogError>;
    pub async fn deploy_logs(&self, id: &InstanceId)           -> Result<String, RuntimeError>;
    pub async fn deploy_status(&self, id: &InstanceId)         -> Result<DeployStatus, RuntimeError>;
    pub async fn refresh_shell_env(&self)                      -> Result<EnvReport, RuntimeError>;
    pub fn   env_report(&self)                                 -> EnvReport;           // for `lantern env show`
    pub async fn republish_all(&self);                         // 13-system: Caddy came back
    pub fn   note_external(&self, id: &InstanceId, l: Option<&Listener>); // from discovery/registry
    pub fn   on_instance_added(&self, i: &Instance); pub async fn on_instance_removed(&self, id: &InstanceId);
    pub fn   on_manifest_changed(&self, id: &InstanceId);
    pub async fn reconcile(&self)                              -> ReconcileReport;     // once at boot
}
pub fn unit_of_pid(pid: u32) -> Option<String>;                // /proc/<pid>/cgroup -> "lantern-inst-x.service"
pub enum RuntimeError { NotFound, Disabled, Busy{op,status}, NeedsConfig(RuntimeIssue), External{pid,cmdline},
                        SystemdUnavailable(String), StopIncomplete, DependencyFailed(..), Io(..) }
```

API mapping for `15-api-mcp`: `NotFound` 404 `not_found`; `Disabled` 409 `instance_disabled`;
`Busy` 409 `operation_in_progress`; `NeedsConfig` 422 `needs_config`; `External` 409
`externally_running`/`not_managed`; `SystemdUnavailable` 503 `systemd_user_unavailable`. A
start that ends in `Crashed`/`NeedsConfig` is an *operation result* (`OpResult { status, issue,
message }`), returned with `?wait=true` as HTTP 200 with `ok: false` or as 422, at `15`'s
discretion. MCP `start_project`/`stop_project`/`restart_project`/`get_project_logs`/`check_health`
map one-to-one onto these calls.

> **Reconciled → 02 D1, D6, D7:** add `pub async fn run_oneshot(&self, spec: OneShotSpec) -> Result<OperationHandle, RuntimeError>` (same launcher/env/toolchain as `run.cmd`; timeout; cancel = stop the unit). `subscribe_logs` is the atomic backlog+live call 15-api-mcp refers to as `tail_and_subscribe`. Template functions are `TemplateString::render_shell()` / `render_data(ctx)` (replacing `rewrite_for_shell` / `expand_text`). Port leases are taken when the instance record is created, so `${PORT}` exists during `setup`.


### 4.2 Consumed from other modules (assumed contracts)

**From 11-instances** (`InstancePreparer`):
```rust
#[async_trait] pub trait InstancePreparer {
    /// Ensure the checkout exists and setup steps are done. Idempotent; fast when already prepared.
    async fn ensure_ready(&self, id: &InstanceId, progress: &dyn ProgressSink) -> Result<(), PrepareError>;
}
```
**From 12-discovery** (`ListenerSource`; no socket parsing in the runtime):
```rust
pub trait ListenerSource: Send + Sync {
    /// Fresh /proc read (not the periodic cache): LISTEN sockets owned by any of `pids`.
    fn listeners_for_pids(&self, pids: &[u32]) -> io::Result<Vec<Listener>>;
    /// Fresh read: every LISTEN socket on `port`, any owner (pid may be None for other users).
    fn listeners_on_port(&self, port: u16) -> io::Result<Vec<Listener>>;
}
```
Discovery should call `runtime::unit_of_pid` to fill `Listener.managed` and the match to
`InstanceId`, and call `Runtime::note_external` when a non-managed listener matches an instance.
**From 13-system:**
```rust
#[async_trait] pub trait Router {
    async fn publish(&self, inst: &Instance, up: Upstream) -> Result<(), RouterError>;   // idempotent
    async fn unpublish(&self, id: &InstanceId) -> Result<(), RouterError>;               // idempotent
    async fn php_fpm(&self) -> Result<PathBuf, PhpFpmError>;                             // socket that accepts connections
}
pub enum Upstream { Tcp(SocketAddr), Url(Url), Php, Static }        // Php/Static: 13 derives root/socket from inst.manifest
#[async_trait] pub trait ServiceControl {
    fn has(&self, name: &str) -> bool;
    async fn ensure_running(&self, name: &str, timeout: Duration) -> Result<(), ServiceError>; // idempotent; waits for its own readiness
}
```
13-system also: calls `Runtime::republish_all()` after Caddy recovers; renders a friendly page for
known-but-unpublished domains (runtime calls `unpublish` on `Stopped`/final `Crashed`, keeps the
route during `Crashed` retry so browsers see 502, not a TLS error); owns lingering
(`loginctl enable-linger`) and the `systemd-resolved`/Caddy setup.
**From the registry (`RegistryHandle`):** `instance(id)`, `instances()`, `project(id)`,
`update_runtime(id, FnOnce(&mut RuntimeState))` (persist + emit `InstanceUpdated`),
`port_lease/set_port_lease/remove_port_lease`, `settings()`, and `InstanceAdded/Removed/
ManifestChanged` notifications.
**From 14-config:** `ResolvedManifest` accessors `run.{cmd,cwd,env,pre_start,post_stop,
startup_timeout}`, `deploy.{start,stop,restart,logs,status,env_file}`, `upstream_url`,
`health_endpoint`, `depends_on`, `type`, `root`, and the settings keys in 3.16; state.json fields
`port_leases`, `instances[id].runtime.{activated,last_exit,restarts,unit}`.
**Core (`lantern-core`):** `rewrite_for_shell()` and `expand_text()` template functions, and the
types in Proposed changes.

---

## 5. Failure modes & edge cases

| Situation | Intended behavior |
|---|---|
| `systemd-run` fails to start the unit (bad cwd, script unreadable) | `--service-type=exec` makes it a non-zero exit; instance returns to `Stopped` (nothing ran) with issue `SpawnFailed` and stderr text. |
| Daemon crashes or is upgraded mid-run | Units keep running. New daemon reconciles (3.15), rebuilds rings from the journal. Open SSE clients reconnect (16-clients). |
| Reboot / logout without lingering | Units vanish; reconcile marks instances `Stopped` ("unit gone"). Lingering warning in `SystemHealth` (3.3.4). |
| `journalctl` follower dies | Restart with backoff from `last_seen` cursor; no lines lost (journal retains them); `SystemHealth` notes "log follower restarting" if >3 restarts/min. |
| Journal unreadable or `Storage=none` | Instances run; ring is empty; `history()` returns `LogError::Unavailable` with the fix hint. A file-based fallback is future work (open question 8.8). |
| Dev server prints 100k lines/s | journald rate limit (20000 per 10 s) suppresses the excess ("Suppressed N messages", surfaced as a `System` line). Ring and channel stay bounded. Slow SSE client gets `resync`. |
| Foreign process takes the leased port between lease and spawn | Re-lease at spawn (3.6). If it happens *after* spawn, the server exits with EADDRINUSE -> `PortInUse` diagnosis and normal retry. |
| Server ignores `${PORT}` | `PortMismatch` -> `NeedsConfig`, unit stopped (3.7). |
| Server binds only `::1` | Ready; `upstream = [::1]:port`; router must use it. |
| Server binds a LAN address only | Ready; `upstream` is that address; warning in `message`. |
| Two instances' `run.cmd` start the same project at the same port because `run.env` hard-codes `PORT` | Second instance gets `PortMismatch` or `PortInUse`; message names the other holder. |
| `.nvmrc` names an uninstalled Node | Launcher exits 78 with a clear line in the log; `Crashed (final)`, issue `ToolchainMissing`; no retry loop. |
| Command not found (127) | One env refresh + one retry; then final `CommandNotFound` with the PATH Lantern used (`lantern env show`). |
| Capture of the login shell hangs (dotfile prompts) | Killed at 10 s; fallback env; warning. Users can guard dotfiles with `LANTERN_RESOLVING_ENV`. |
| `stop` while `Starting` | Preempts; unit stopped; `Stopped`. |
| `stop` of a unit stuck in uninterruptible I/O | systemd's SIGKILL after 10 s does not complete; instance stays `Stopping`, operation errors `stop_timeout` listing PIDs; never reported `Stopped` while processes remain. |
| User kills the process by hand (`kill -9`) | Detected by poll within 3 s as an unexpected exit -> `Crashed` + retry. `kill -TERM`/Ctrl-C style -> `Stopped`. |
| Second `start` while one is in flight | Joins (3.13). Concurrent `start` and `stop` -> stop wins. |
| Manifest edited while running | Status unchanged; `message` "manifest changed; restart to apply". |
| `enabled` flipped to false while running | Registry calls `stop`; further `start` refused. |
| Dependency cycle | `NeedsConfig` with the cycle path. |
| Dependency Service not installed (e.g. Redis) | `DependencyFailed` with `ServiceError` text from 13-system. |
| php-fpm down while a Php site is `Running` | `Unhealthy` (3.11). |
| Caddy down at start | Process-backed: `Running` + issue `RouteFailed`; recovered by `republish_all()`. Site-only: `Stopped`/`NeedsConfig` with `RouteFailed` since there is nothing to serve. |
| Orphan `lantern-inst-*` unit with no registry entry | Left alone, reported (3.15). |
| Instance id with a character outside `[a-z0-9@-]` | Rejected at construction (core); `unit_name` debug-asserts. |
| Unit name length (limit 255) | ProjectId <= 48 and branch slug bounded by 11-instances keeps it far below. |

---

## 6. Security considerations

- **Commands come from manifests, never from the API.** `run.cmd`, `pre_start`, `post_stop`,
  `deploy.*` are read only from the resolved manifest of a registered checkout. API and MCP can
  choose *which* instance to start, not what to execute. (v1 had the same rule.)
- **Untrusted checkouts.** A checked-out PR branch can ship a `lantern.yaml` whose `run.cmd` runs
  arbitrary code the next time anyone presses start. This is the same trust as running `pnpm dev`
  in that checkout, but Lantern makes it one click or one MCP call. Recommendation (open
  question 8.9): hash the command-bearing fields per checkout, and require an explicit
  confirmation the first time they change on a non-primary checkout.
- **No shell injection through template variables** (3.4.6): variables in code become parameter
  expansions; values in data are single-quoted; one `sh_quote`, fuzz-tested.
- **Secrets.** Launcher scripts and `shell-env.sh` are 0600 in 0700 directories; env values are
  not placed on any command line or in unit properties, so they do not show in `ps` or
  `systemctl show`. The captured shell env inherits whatever the user exports in their dotfiles
  (e.g. `GITHUB_TOKEN`), as their terminal would; `shell-env.sh` therefore must never be logged.
- **No privilege.** Everything is a user unit; no sudo; nothing is installed in `/etc`.
- **Never kill what we did not start.** Stops go through the unit's cgroup only (v1's
  `lsof | kill` is gone).
- **Wildcard binds** (`0.0.0.0`) expose a dev server on the LAN; flagged in `message`.
- Health probes only target loopback/`upstream_url` hosts the manifest names; they do not follow
  redirects, so a redirect cannot make the daemon probe arbitrary hosts.

---

## 7. Testing strategy

**Unit tests (pure, no systemd, run in CI):**
- `transition()` table: one test per row of 3.14 plus the operation matrix of 3.13, using a
  scripted clock.
- `plan::resolve` over the table in 3.2; toolchain detection against fixture directories
  (`.nvmrc`, `.tool-versions`, `.venv`, none).
- `launcher::render` golden files (nvm, fnm, mise, venv, plain, hostile values: `$(touch x)`,
  quotes, newlines, branch `x\`id\``); `sh_quote` property test (round-trips through a real `bash`).
- `systemctl show` parser (all states, missing units, multiple blocks); `ShellEnv` marker
  parser with noisy stdout/stderr; PATH merge/dedup; deny-list.
- Journal line decoder: string `MESSAGE`, byte-array `MESSAGE`, `null`, invalid UTF-8, ANSI,
  `\r` progress, over-long line, duplicate suppression with the anchor timestamp.
- Port allocator: sticky, first-fit, reserved, exhausted, re-lease on conflict; range vs
  ephemeral warning.
- `deps::validate`: cycles, self-loop, diamond, missing, disabled; start-order and cascade-stop
  order.
- Retry policy: streak reset after a stable run, 127 never retried, exit 0 vs SIGTERM vs SIGKILL.

**Integration tests (need a user systemd; marked `#[ignore]`, run by `cargo test -- --ignored`
locally and in a CI job with `systemd --user`):** using real transient units and tiny servers
(`python3 -m http.server`, a Node one-liner):
start/ready/stop; crash then retry then give up; `kill -9` detection; bind `::1` only; server
ignores `${PORT}` (expect `PortMismatch`); foreign listener on the leased port; `deploy.start`
that daemonises; daemon "restart" (drop the Runtime, build a new one, reconcile adopts and the
ring refills); ANSI output reaches `LogLine` stripped; unit-name reuse after failure.

**Manual checklist (needs a real desktop):** project with `.nvmrc` under nvm; fnm project; pnpm
project via corepack; uv + uvicorn; Laravel with php-fpm and Vite companion; log out and in
(with and without lingering); `systemctl --user restart lantern` with servers up; edit
`~/.bashrc` then start (auto-refresh); `lantern env show` output sanity; runaway log producer;
Caddy stopped and restarted.

---

## 8. Open questions (with recommendations)

1. **stdout vs stderr distinction.** Journald cannot give it. *Recommend:* accept merged output
   (as v1); `Stream::Stderr` stays unused.
2. **zbus later?** *Recommend:* stay with batched polling until profiling shows cost or latency
   problems; the `UnitBackend` seam makes it a drop-in.
3. **`depends_on` resolving to a same-branch worktree** of the dependency (`web@feature-x` ->
   `api@feature-x` if it exists). *Recommend:* not in v2.0; primary instance only; revisit once
   11-instances has per-branch resource conventions.
4. **Restart on persistent `Unhealthy`.** *Recommend:* no; add an opt-in manifest key
   `health.restart_after` later.
5. **Health checks for PHP/static sites** (Laravel's `/up`). Needs a loopback probe through Caddy
   with the right `Host` and the local CA. *Recommend:* not in v2.0; 13-system may later expose
   `Router::probe_url(inst)`.
6. **Stopping an `External` process from Lantern.** *Recommend:* not in v2.0; show pid/cmdline
   so the user can act; a future explicit "terminate pid" requires confirmation and a same-uid,
   same-checkout check.
7. **Adopt-on-mismatch mode** (follow the port the server actually chose). *Recommend:* no; it
   breaks per-instance port uniqueness. Fixed ports are expressed with `upstream_url`.
8. **File-based log fallback** when journald is `Storage=none` or unreadable. *Recommend:* defer;
   surface the problem in `SystemHealth` and revisit if real users hit it.
9. **Trust-on-change for manifest commands** in non-primary checkouts. *Recommend:* implement in
   14-config/11-instances (hash and confirm); the runtime only consumes a `trusted: bool`.
10. **Default port range 29000-29999** instead of v1's 41000-42000. *Recommend:* yes (3.6); v1's
    range was never written to user settings, so no migration is needed.

---

## 9. Proposed changes to core architecture

1. **`lantern-core` runtime types** (core references them without defining them): `ExitInfo
   { at, kind: ExitKind, code, signal, uptime_secs, unit_result, observed_late }`,
   `HealthSummary`/`HealthState`/`HealthSample` (3.8), `RuntimeIssue { code: IssueCode, detail,
   hint }`, `Stream { Stdout, Stderr, System }`. Add to `RuntimeState`: `issue:
   Option<RuntimeIssue>` (machine-readable cause behind `message`; GUI/MCP can offer fixes),
   `next_retry_at: Option<Timestamp>` (retry countdown), and `upstream: Option<SocketAddr>`
   (where readiness found the listener: `[::1]:5173` vs `127.0.0.1:port`; `13-system` routes
   to it).
2. **`InstanceId` charset invariant:** `[a-z0-9@-]`, no `_`; this makes the `@` to `_` unit-name
   mapping injective. Section 3's example `systemd-run --user --unit=lantern-inst-<slug>` is
   refined by 3.3.2: `--slice=app-lantern.slice`, `--service-type=exec`, `RemainAfterExit=yes`,
   `--expand-environment=no`.
3. **Section 8, per-instance operation lock:** refine "second request returns the in-flight
   result or 409" with the matrix in 3.13: `Stop` preempts an in-flight `Start/Restart` (a hung
   start must be cancellable); same-operation requests join; other combinations 409.
4. **Section 7 persistence:** logs live in **journald** (user journal), not
   `$XDG_STATE_HOME/lantern/logs/`; drop that row, add `$XDG_RUNTIME_DIR/lantern/run/` (launcher
   scripts, tmpfs) and `$XDG_CACHE_HOME/lantern/shell-env.sh`. state.json gains `port_leases`
   and per-instance `runtime.{activated, last_exit, restarts, unit}`.
5. **Section 5 (systemd):** confirm shell-out; no zbus in v2.0 (3.3.3).
6. **Section 6 template variables:** `lantern-core` provides two functions: `expand_text`
   (substitute values; for `run.env` data) and `rewrite_for_shell` (turn `${X}` into
   `${LANTERN_X}` for code strings, 3.4.6, to prevent injection via branch names), and the runtime
   exports the variables as `LANTERN_*` plus v1-compatible `PORT`, `DOMAIN`, `PROJECT_NAME`.
   `${HOST}` is not exported unprefixed.
7. **Section 9 events:** `InstanceUpdated` is emitted on changes of status, message, issue,
   upstream, restarts and `health.state`, not on every health sample.
8. **Default port range** `29000-29999` in settings (3.6) because 41000-42000 lies inside Linux's
   ephemeral port range.
