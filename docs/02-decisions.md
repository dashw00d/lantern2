# 02 — Cross-doc decisions (audit of 10–16)

Status: **authoritative**. Precedence when documents disagree: **02 > 01 > 10–16**.

The seven subsystem docs were written in parallel against `01-architecture.md`, and each one
ended with "Proposed changes to core architecture". This file records the audit:

- Section A lists every conflict found between the docs, with the decision.
- Section B lists the proposals that were accepted into `01-architecture.md`.
- Section C lists what has to be proven before building on it.

The conflicting passages in 10–16 were also patched in place and point back here (`→ 02 Dn`).

---

## A. Conflicts and decisions

### D1. Template variables: substitution vs. environment variables
**Conflict.**
- 10 and 11: in shell commands, variables become environment variables (`${PORT}` →
  `${LANTERN_PORT}`).
- 14: closed-set *textual* substitution everywhere, "even inside single quotes".
- 11 says `${PORT}` is not available in `setup`; 14 says it is.
- `${BRANCH}` for a detached HEAD: 11 says it is the short sha, 14 says it is empty.

**Decision.** Two contexts, one grammar.
- **Grammar (from 14):**
  - Only known names (plus the deprecated v1 aliases `NAME`→`PROJECT` and `PATH`→`CHECKOUT`)
    are recognized.
  - Any other `${…}` or `$…` passes through to the shell untouched, so `${HOME}` and
    `${X:-y}` keep working.
  - `$${` is the escape for a literal `${`.
  - A near-miss name raises the warning `possible_var_typo`.
  - This replaces 01's old "unknown variables are an error" rule.
- **Shell contexts:** `run.cmd`, `run.pre_start`, `run.post_stop`, `setup[]`, `deploy.*`.
  - Known names are *rewritten* to `${LANTERN_<NAME>}` and exported by the launcher. They are
    never pasted into the text.
  - Reason: git branch names may legally contain `$ ; & ( )` and backticks, so pasting is
    command injection from any branch you fetch.
  - Lint `var_in_single_quotes` (warning): v1 substituted inside `'…'`, but a parameter
    expansion inside single quotes does not expand.
  - `${PATH}` in a shell context gets a *loud* `deprecated_var` warning, because it means the
    checkout path, not the shell's `$PATH`. `lantern manifest check --fix` rewrites it to
    `${CHECKOUT}`.
- **Data contexts:** `run.env` values, `worktree.env` values, profile `env` values, `run.cwd`.
  These are substituted textually; the values are then quoted by the launcher or used as paths.
- **API (`lantern-core`):** `TemplateString::render_data(&VarContext) -> String` and
  `TemplateString::render_shell() -> String`. The second does the rewrite and needs no
  context. These replace 10's `expand_text` and `rewrite_for_shell` and 14's `render`.
- **`${PORT}` is available everywhere, including `setup`.** The port lease is taken when an
  instance record is created (leases are sticky, 10 §3.6), so it exists before preparing
  starts.
- **Detached HEAD:** `${BRANCH}` and `${BRANCH_SLUG}` are the 7-character short sha.
  Non-git projects: empty string.
- **Added:** `${BRANCH_SNAKE}` (the slug with `-`→`_`, for SQL identifiers).
- **v1 compatibility:** the launcher also exports `PORT`, `DOMAIN` and `PROJECT_NAME`
  unprefixed, as 10 §3.4.6 does.

### D2. Filesystem-safe instance name
**Conflict.** 10 and 11 use `myapp@feature-x` → `myapp_feature-x`. 14 uses `myapp--feature-x`.

**Decision:** `_`.
- `InstanceId` charset: `[a-z0-9-]` on each side of at most one `@`, so the mapping is
  injective. `InstanceId::safe_name()` replaces `@` with `_`.
- `safe_name()` is used for unit names (`lantern-inst-<safe>`), script names, and
  `${INSTANCE}`.
- The raw id is exported as `LANTERN_INSTANCE_ID`, which reconciliation needs (14 §3.9
  assumed it).
- 10's launcher example is corrected to `LANTERN_INSTANCE='myapp_feature-x'`.

### D3. Default port range
**Conflict.** 10 uses 29000–29999. 14 and v1 use 41000–42000.

**Decision: 29000–29999.** 41000–42000 lies inside Linux's ephemeral port range
(32768–60999, verified by 10), so outgoing connections can steal leased ports.
- The `migrate-v1` import keeps an explicitly *customized* v1 range, and drops the v1
  default.
- The built-in `demo` profile uses `29900–29999`.

### D4. `state.json` schema
**Conflict.**
- 14 nests `checkouts[]` inside projects, with a slim `CheckoutRecord`.
- 11 wants top-level `repos` plus a much richer `CheckoutRecord`.
- 12 stores adoption as a persisted intent `Instance.adopt: bool`; 14 stores
  per-process `adopted[]` records.
- 10 needs `activated` for site-only instances; 14's schema lacks it.

**Decision.** 14 owns the file. This is the merged shape (version 1):

```jsonc
{
  "version": 1, "saved_at": "…", "active_profile": "light",
  "repos": {                                   // 11: keyed by canonical git common dir
    "/home/u/sites/mono/.git": { "slug": "mono" }
  },
  "projects": {
    "myapp": { "id": "myapp", "aliases": ["MyApp"], "root": "/home/u/sites/myapp",
               "repo": { "common_dir": "/home/u/sites/myapp/.git", "subpath": "" },   // D5
               "source": "manifest", "registered_at": "…", "last_seen_at": "…",
               "missing": false, "overrides": {} }
  },
  "instances": {                               // one record per Project × Checkout
    "myapp@feature-x": {
      "project": "myapp",
      "checkout": { "path": "…", "kind": "managed", "phase": "ready", "created_at": "…",
                    "created_branch": true, "materialized": [".env"],
                    "setup": { "lock_hash": "…", "commands_hash": "…", "finished_at": "…" },
                    "missing_since": null },
      "pinned": true, "adopt": false, "activated": false,
      "first_started": "…", "last_started": "…",
      "last_runtime": { "status": "Running", "port": 29003, "unit": "lantern-inst-myapp_feature-x",
                        "main_pid": 4242, "started_at": "…", "last_exit": null, "restarts": 0,
                        "message": null }
    }
  },
  "port_leases": { "myapp@feature-x": { "port": 29003, "leased_at": "…" } },
  "dismissed_suggestions": ["/home/u/dev/scratch"],
  "imports": { "v1": { "at": "…", "from": "…", "projects": 7 } }
}
```

- 11's `CheckoutRecord` fields move under `instances[id].checkout`.
- `pinned`, `first_started` and `last_started` live on the instance record. The rule
  `claimed = pinned || first_started.is_some()` is unchanged.
- 14's `adopted[]` array is **dropped**. Adoption is the intent flag `instances[id].adopt`,
  re-applied whenever discovery matches a listener to that instance (12 §3.11). Pids are never
  persisted as identity.
- `activated` (10 §3.11) means a site-only instance should be republished at boot.

### D5. Monorepo field on `Project`
**Conflict.** 11 uses `Project.repo: Option<ProjectRepo { common_dir, git_root, subpath }>`.
14 uses `Project.subdir: Option<PathBuf>`.

**Decision: 11's `repo`.** `git_root` is re-derived at runtime and is not persisted. The
persisted shape is `{ common_dir, subpath }` (D4). Non-git projects have `repo: None`, which
replaces 01's `is_git`.

### D6. Setup commands and their output
**Conflict.**
- 11 assumes a public `OneShotRunner` in 10, which 10 doesn't expose.
- 11 writes `setup.log` under `$XDG_STATE_HOME/lantern/logs/`, a directory 10 removed in
  favour of journald.
- 10's LogHub ignores `lantern-op-*` units.
- The `Stream` enum has variants proposed separately: `System` (10) and `Setup` (11).

**Decision.**
- 10 exposes `Runtime::run_oneshot(OneShotSpec) -> OperationHandle`, which is 10's existing
  `deploy::run_oneshot` made public. It runs a transient unit
  `lantern-op-<safe>-<verb>-<seq>.service` with the same launcher, environment and toolchain
  steps as `run.cmd`, and enforces a timeout and cancellation (stopping the unit kills its
  cgroup).
- 11's `OneShotRunner::run(OneShotSpec{…, on_line})` is this call. Lines arrive through the
  LogHub.
- LogHub maps `lantern-op-<safe>-setup-*` to the owning instance as `Stream::Setup`, and
  `lantern-op-<safe>-deploy-*` as `Stream::System`.
- No log files on disk. `$XDG_STATE_HOME/lantern/logs/` does not exist.
- `Stream = { Stdout, Stderr, System, Setup }`. `Stderr` is reserved, because journald merges
  a unit's stdout and stderr (10, verified).

### D7. Events: what they carry
**Conflict.**
- 14 proposes a `ManifestSummary` inside events.
- 15 proposes `InstanceView`/`ProjectSummary` and removes `LogLine` from the in-process bus.
- 10 and 15 name the log subscription differently (`subscribe_logs` / `tail_and_subscribe`).

**Decision.** Accept 15's shape and put 14's summary inside it.
- `Event::InstanceUpdated { instance: InstanceView }`, where `InstanceView` contains
  `manifest: ManifestSummary`. The full manifest is at `GET /api/instances/{id}/manifest`.
  Env values never go on the bus.
- `LogLine` is not an in-process `Event` variant. 10 serves logs per instance through
  `Runtime::subscribe_logs(id, backlog) -> (Vec<LogLine>, broadcast::Receiver<LogLine>)`,
  which is atomic with no gap. That is 10's name; 15's `tail_and_subscribe` refers to the
  same function. The SSE wire format still carries `log` events.
- Added: `SuggestionsChanged` (14), and `ListenersChanged { seq, listeners }` with the
  non-hidden snapshot only (12).
- `InstanceUpdated` fires on changes to status, message, issue, upstream, restarts or
  `health.state`, not on every health sample (10).

### D8. API gaps and shape mismatches (found by 16)
**Decision.** 15 is extended with the following:

| Route | Owner of semantics |
|---|---|
| `GET /api/projects/{id}/checkouts`, `GET /api/projects/{id}/branches` | 11 |
| `POST /api/instances/{id}/pin`, `/unpin`, `/rename` | 11 |
| `GET /api/instances/{id}/removal-check` | 11 |
| `POST /api/instances/{id}/prepare` (`{force}` re-runs setup) | 11 (verb is **prepare**, matching status `Preparing`; the CLI verb `lantern setup <target>` maps to it) |
| `GET /api/suggestions`, `POST /api/suggestions/dismiss` | 14 |
| `POST /api/import/v1`, `GET /api/schema/{name}` | 14 |
| `PATCH /api/projects/{id}` with `{id}` (rename project, which keeps the old id in `aliases`) | 14 |

**Run-branch request.** `POST /api/projects/{id}/instances` takes 11's richer form:

```jsonc
{ "target": { "branch": "feature/x" }            // or {"new_branch":"x","base":"main"} | {"detached":"<rev>"} | {"pr":123}
, "fetch": true, "start": true, "pin": true }
```

15's flat `{branch, base}` is accepted as shorthand (`branch` maps to `target.branch`, and
`branch` + `base` maps to `new_branch`). The MCP tool `start_branch` keeps the flat arguments.
`pr` is in v2.0 scope (11 §3.9.3, needs `gh` only to pick the remote).

**Removal.** 11's rule wins over 15's text:
- Lantern never deletes a user-made (`worktree`) checkout's directory, even with `force`.
- `DELETE /api/instances/{id}` on a `worktree` checkout unpins and hides it (`200`, `forgotten:
  true`).
- On a `managed` checkout it removes the worktree. `force` only overrides the dirty and
  unpushed-commit checks.
- On the primary it returns `409 cannot_remove_primary`.

### D9. External (hand-started) servers and routing
**Conflict.** 13 assumes discovery only creates `External` instances after you adopt them. In
12, any stable matched listener sets `External`, and routing requires adoption.

**Decision: 12's model.** You can see every hand-started server immediately, but it only gets
a domain once adopted.
- Route rule: a route exists iff status is `Running` or `Unhealthy`, **or**
  (`External` **and** (`adopt` **or** `discovery.auto_adopt`)).
- Upstream: `runtime.external.upstream`.
- 13 §3.3's route table is patched to match.

### D10. MCP listener tools
**Conflict.** 12 proposes the new tool name `get_listeners` and an unchanged `get_ports` shape.
15 names it `list_listeners` and widens `get_ports`.

**Decision.**
- `get_ports` keeps v1's shape `{ "<id>": { "port", "health_status", … } }`, with additive
  keys only (`source`, `pid`, `branch`, `url`). Unmanaged listeners appear under their
  matched `InstanceId`. Unmatched listeners do **not** appear in `get_ports`.
- The full listener list is `list_listeners` (15's name).

### D11. Reserved names
15 reserves `api`, `mcp` and `lantern`; 13 reserves `lantern` and `mailpit`. **Union:**
`ProjectId` must not be `api`, `mcp`, `lantern` or `mailpit`. The registry returns
`422 reserved_id`. Branch slugs are unrestricted, because they are always the
second-level label.

### D12. Caddy admin endpoint
13 moves the admin API from `localhost:2019` to the Unix socket
`$XDG_RUNTIME_DIR/lantern/caddy-admin.sock`, because a system Caddy may own 2019. **Accepted.**
12's default hide list keeps port 2019 for the case where a *system* Caddy is running.

### D13. DNS mechanism
13 replaces the `resolved.conf` drop-in with a **systemd-resolved DNS delegate**
(`/etc/systemd/dns-delegate.d/lantern.dns-delegate`, systemd ≥ 258; Ubuntu 26.04 ships 259).
A plain drop-in would make Lantern's responder a default route for *every* lookup.
**Accepted.**
- The responder listens on `127.0.0.1:5354`.
- It answers A records only by default; AAAA is opt-in.

### D14. Binding :80/:443 without root ⚠ owner decision
13 chooses the sysctl `net.ipv4.ip_unprivileged_port_start=80`. This lets **any** local
program bind ports 80–1023. The alternative is `setcap` on the Caddy binary, which is lost
silently whenever the binary is replaced.

**Default: sysctl.** The bundled Caddy lives in a Lantern-owned path and the machine is
single-user. `lantern init --ports=setcap` offers the alternative, and `doctor` accepts
either. **The owner should confirm this choice.**

### D15. Intel cache path
14 mentions `$XDG_STATE_HOME/lantern/cache/`; 15 defines
`$XDG_STATE_HOME/lantern/intel-cache.json`. **Decision: 15's single file.**

### D16. Crate layout additions
All of the following are accepted:
- `lantern-system` (13): synchronous, no tokio. It holds `init`, `doctor`, CA and trust
  stores, and paths. Dependency rule: `core ← system ← {daemon, cli, lantern bin}`. `cli` uses
  it only for `init`, `doctor` and `uninstall`, which must work with the daemon down.
- `lantern-daemon/src/instances/` (11): worktree derivation, prepare, files, deps, remove.
  `registry/` stays pure state.
- `lantern-daemon/src/operations.rs` (15): `OperationTracker`.
- `lantern-cli` returns `CliAction::{RunDaemon, RunGui}` to the binary, so it never depends on
  `daemon` or `gui` (16).

### D17. Client-written files
01 said clients never touch state files. 16 needs GUI preferences. **Accepted:** clients may
write exactly `$XDG_STATE_HOME/lantern/gui.json` and the XDG autostart entry. The
`[clients]` settings (`editor`, `terminal`) are in `config.toml` (14 adds them).

### D18. Operation preemption
10 refines 01 §8: `Stop` preempts an in-flight `Start`, `Restart` or `Prepare`, so a hung start
or install can be cancelled. Identical requests join the in-flight operation; other
combinations return `409 operation_in_progress`. **Accepted.**

### D19. Auto-start
- The daemon never starts anything at boot except reconciliation: adopting units that are
  already running and republishing `activated` site-only instances.
- Auto-start comes only from the active profile's `auto_start`. It applies to primary
  instances unless a non-primary `InstanceId` is named (10, 11).

### D20. `SystemHealth.checks`
The shape 16 needs is defined in `lantern-core::api` and filled by `lantern-system::doctor`:

```rust
pub struct Check { id: String, title: String, status: CheckStatus /* Ok|Warn|Fail|Skipped */,
                   detail: String, fix: Option<Fix { description: String, command: String, needs_root: bool }> }
```


### D21. Environment variable namespace (found during the audit)
**Conflict.** Two names were used for two different things:
- `LANTERN_PORT`: 14 uses it as the daemon's API-port override; 10's launcher exports it to
  every dev server as the instance's port.
- `LANTERN_HOST`: 16 uses it as the client's daemon address; 10 exports it as `127.0.0.1`.

So any `lantern` command run from inside a managed server (an npm script, or an agent's shell)
would talk to the wrong place.

**Decision.**
- `LANTERN_<TEMPLATE VAR>` belongs to the launcher, for the instance's own values.
- Daemon and client settings use `LANTERN_DAEMON_*`: the daemon reads `LANTERN_DAEMON_PORT`,
  and clients read `LANTERN_DAEMON_URL` (default `http://127.0.0.1:4777`).
- The other settings overrides keep their names (`LANTERN_TLD`, `LANTERN_WORKSPACES`,
  `LANTERN_*_DIR`, …); none collide with a template variable name. New template variables
  must be checked against this list.
- The launcher **unsets every inherited `LANTERN_*`** before exporting its own, so a server
  never sees the daemon's settings.


### D22. Corrections found while building wave 1
The lanes reported places where 10–16 still contradict this file. **This file wins in each
case**, and the code follows it:

| Doc passage | Correct rule |
|---|---|
| 10 §3.13 Start/Restart joining matrix | D18: only identical requests join; only `Stop` preempts; everything else is `409`. |
| 10 §3.10 one-shots with `--pipe` / `KillMode=process` | D6: one-shot output goes to the journal; cancelling means stopping the unit, which kills its whole cgroup. |
| 11 §3.3, any remaining "`${PORT}` not available in `setup`" | D1: `${PORT}` is available everywhere. |
| 14 §3.5 S3 label | S3 **passed** with `serde-saphyr` 2.1.1 (positions confirmed for nested keys, list values and typed errors). |
| 15/16 SSE event name `log_line` | The SSE event is **`log`**, carrying a `model::LogLine`. |
| 16/13 doctor status names | D20: `CheckStatus = ok \| warn \| fail \| skipped`. |
| Service names like `php-fpm` in examples | Wire names are snake_case: `php_fpm`. |
| 01 glossary "listening socket on localhost" | A Listener is any TCP LISTEN socket, loopback **and** non-loopback (12). |
| 12 §3.13 Docker pid / container IP fields | v2.0's `DockerInfo` is `{ label, container_port }`. The Docker API comes in v2.1, so foreign Docker sockets keep `pid: None`. |
| `php.fpm_socket` default | `"auto"`: probe `/run/php/php*-fpm.sock`, newest version first (13 §3.10 decides a user pool later, spike S7). |

**Found in wave 2** (the code follows these):
- `GET` never mutates. Checkout refresh is `POST /api/projects/{id}/sync`, and
  `GET …/checkouts?refresh=true` returns `400`. Intel `GET` is cache-only; refresh is a `POST`.
- D19 wins over 14 §3.11: manifest `features.auto_start` does **not** start anything at boot.
  Only the active profile does.
- Dirty checks use `git status --porcelain --untracked-files=all`, minus the files materialized
  by `worktree.copy`/`worktree.link`.
- Being located under the managed worktrees dir does not prove a checkout is Lantern-made. Only
  a `managed` record created by Lantern does.
- The CA name constraint is the permitted subtree `.glow`, which excludes the apex `glow`. That
  is intended: no instance uses the apex.
- `ResolvedRun` gains `pre_start`, `post_stop` and `startup_timeout` (10 §3.2). Until then the
  runtime treats them as absent.

**Found in wave 5** (the code follows these):
- **D14 is confirmed by the owner (2026-11-06): sysctl** `net.ipv4.ip_unprivileged_port_start=80`,
  written to `/etc/sysctl.d/` by `sudo lantern init --system-steps` and applied live. `doctor`'s
  `ports.bind` passes when the live threshold is ≤ min(http, https) or the selected Caddy has
  `cap_net_bind_service=ep`. `--ports=setcap` applies only to a root-owned
  `/usr/lib/lantern/caddy` that is also the configured `[caddy].binary`; it never grants the
  capability to a user-writable file. Packaging (M6) decides whether to set that default.
- Caddy resolution order: `[caddy].binary` → `$LANTERN_DATA_DIR/bin/caddy` → `PATH`. `--dev`
  defaults `LANTERN_DATA_DIR` to `~/.local/share/lantern-dev`, with no extra `data/` component.
- Spike S5 partly passed with upstream Caddy 2.11.9: HTTPS for `<project>.<tld>` and
  `feature-x.<project>.<tld>` verifies against the name-constrained Lantern CA alone, a
  WebSocket survives `POST /load`, and Caddy recovers its routes after SIGKILL. The browser
  (Firefox Snap, Chrome) and Vite HMR parts of S5 are still unverified.
- `session.systemd` is the user manager's health (`running` = ok, `degraded` = warn).
  Whether `lantern.service` is enabled is a separate check, `session.unit`.
- Spike S1's API checks are done: iced 0.14 `daemon`, ksni 0.3.7 and zbus 5.19 single
  instance build and run headless. The Plasma hands-on part and the budgets are still open.

**v1's self-registered `lantern` project:** parsing accepts `id: lantern`, so v1 manifests still
load (C5). Resolution reports `reserved_id` and the project is not usable. `migrate-v1` **skips
it with a note**, because self-registration was dropped (15 §3.10).

**`manifest_rev`:** the registry computes it as a hash of the serialized `ResolvedManifest`,
refreshed whenever the manifest is re-resolved. `ResolvedManifest::summary(rev)` takes it as an
argument.

---

## B. Proposals accepted into `01-architecture.md`

Taken from the docs' §9 sections:

- **10:** `RuntimeIssue`, `ExitInfo`, `HealthSummary`, `RuntimeState.{issue, next_retry_at,
  upstream}`; journald for logs; no zbus; slice `app-lantern.slice`.
- **11:** `Project.repo`; `Checkout.{git_root, locked, missing}`; `Instance.setup_stale`; managed
  worktrees keyed by repo slug; the git watcher is a stat-gated poll.
- **12:** extended `Listener`; `RuntimeState.external`; `Instance.adopt`; `CheckoutIndex` in
  core; `/proc` read with `std::fs` and netlink `sock_diag` via `rustix`, with a
  `/proc/net/tcp` fallback.
- **13:** the `lantern-system` crate; Caddy as `lantern-caddy.service` on an admin socket; a
  name-constrained Lantern CA; the DNS delegate; sysctl (D14); the `/__lantern/*` gateway
  with its own guard; the bundled Caddy 2.11.x (Ubuntu's 2.6 is too old).
- **14:** `serde-saphyr` for YAML; `aliases`; `ServeMode`; `RestartPreventExitStatus=78`;
  `state.lock` and `.bak`.
- **15:** `InstanceView`; stateless MCP over `rmcp` 3.x; the Host/Origin/Sec-Fetch-Site
  guard; `OperationTracker`; intel per instance; `?wait` semantics.
- **16:** iced 0.14 using `iced::daemon`; the ksni tray; the D-Bus single-instance name
  `dev.lantern.Lantern` with `DoNotQueue`.

---

## C. Must-prove-first spikes

Each of these is a decision that rests on something not yet run end-to-end on the target
machine. Do them before building the subsystem that depends on them. Each one is a few hours.

| # | Spike | Pass criterion | Fallback |
|---|---|---|---|
| S1 | **iced + ksni on Plasma 6 Wayland** (16) | Tray click opens or raises the window; closing the window keeps the tray; 20k-line log view scrolls smoothly | Patch `iced_winit` for the activation token; otherwise Qt/QML via cxx-qt |
| S2 | **rmcp stateless streamable HTTP** with Claude Code and Codex (15) | `list_projects` and `start_project` work, and keep working across a daemon restart without re-adding the server | `legacy_session_mode(true)` (one-line change) |
| S3 | **serde-saphyr `Spanned` positions** in nested maps (14) | Error output shows `lantern.yaml:12:10` for a bad nested value | `yaml-rust2` and a manual walk |
| S4 | **systemd-run transient units**: `RemainAfterExit`, `reset-failed`, `--user` journal readability (10) | Start, crash, read exit code, restart under the same name, logs readable as the user | Already verified on this machine; re-run on a fresh VM |
| S5 | **Caddy 2.11 user unit + sysctl + nested-subdomain certs + HMR across a reload** (13) | `https://feature-x.myapp.glow` trusted in Firefox (Snap) and Chrome; Vite HMR survives `POST /load` | setcap (D14) |
| S6 | **DNS delegate** routes only `~glow` (13) | `resolvectl query x.y.glow` → 127.0.0.1; other lookups untouched | NetworkManager dnsmasq (v1 style) |
| S7 | **php-fpm user pool** for Laravel (13) | Laravel page served via `php_fastcgi` to a user-owned socket | Use the system php-fpm socket read-only |
