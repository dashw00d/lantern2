# 20 — Roadmap, migration, testing

## Principles
- **Every milestone ends usable.** v1 keeps working until the cutover milestone, and nothing
  forces a big-bang switch.
- **Spikes first** (`02-decisions.md` §C). A design that rests on an unproven assumption is
  proven before code depends on it.
- **The order follows what you need first:** run my servers → name them → see everything →
  branches → GUI.

## Running v2 next to v1 during development
v1 owns `:4777`, `:443` and `.glow`. Until cutover, the v2 daemon runs with a dev profile so
both can run at once:
- API port `LANTERN_DAEMON_PORT=4778` (clients: `LANTERN_DAEMON_URL=http://127.0.0.1:4778`).
- TLD `LANTERN_TLD=glow2`.
- Caddy on `:8443`/`:8080`, which needs no sysctl.
- Separate dirs: `LANTERN_CONFIG_DIR`, `LANTERN_STATE_DIR` and `LANTERN_DATA_DIR` point at
  `~/.local/share/lantern-dev/…`.

`cargo run -- daemon --dev` sets all of these.

---

## M0 — Spikes (S1–S7 in `02` §C)
Throwaway code in `spikes/`, never imported by the workspace. Order:
1. **S4** (systemd units).
2. **S3** (YAML positions).
3. **S2** (rmcp with real agent clients).
4. **S5 + S6** together (Caddy, CA, DNS delegate, HMR).
5. **S7** (php-fpm).
6. **S1** (iced + ksni). It can run any time before M5.

Each spike ends with a short note appended to `02` §C: pass, or fallback taken.

## M1 — Skeleton you can query
- Cargo workspace with all crates. CI runs `cargo fmt --check`, `clippy -D warnings` and
  `test`.
- `lantern-core`: ids and slugs, domain types, `api` types, the manifest
  (parse/validate/resolve, JSON Schema), `config.toml`, `state.json` (load/migrate/atomic
  write), and the template grammar (`02` D1).
- `lantern-daemon`: the registry, workspace scan, `migrate-v1` in **read-only/dry-run** mode,
  the axum API with the Host/Origin guard, SSE with snapshot/resync, and the
  `OperationTracker`.
- `lantern-cli`: `ls`, `status`, `scan`, `manifest check`, `--json`, completions.
- **Done when:** `lantern ls` shows the same projects as v1 (imported from v1 state), and
  `lantern manifest check` gives a line:column error for a broken `lantern.yaml`.

## M2 — Run servers (replaces v1's process management)
- `runtime/`:
  - transient units and the launcher script, including captured shell env and the
    nvm/fnm/mise/venv steps;
  - sticky port leases (29000–29999);
  - readiness by cgroup-owned listener (needs a minimal `discovery::port_binding`);
  - journald LogHub, health checks, crash/backoff, `deploy.*` one-shots, `depends_on`;
  - boot reconciliation and the operation preemption matrix.
- CLI: `on`, `off`, `restart`, `logs -f`, `env show|refresh`.
- **Done when:**
  - every v1 project starts with `lantern on`, including nvm projects with `.nvmrc`;
  - it survives `systemctl --user restart lantern`;
  - a server that ignores `${PORT}` gets a clear `PortMismatch` message;
  - every regression in `RUNTIME_TESTING_AND_REGRESSIONS.md` has a passing test.

## M3 — Names, HTTPS and agents (the cutover milestone)
- `lantern-system`: `init` (pkexec steps: sysctl, DNS delegate, CA trust), `doctor`, NSS trust
  for Chrome and Firefox (including Snap).
- `net/`: Caddy user unit + `POST /load`, the route plan, DNS responder, the `/__lantern/*`
  gateway, PHP and static serving. `services/`: Mailpit, Redis, Postgres as decided in 13.
- `mcp/` + `intel/`: every v1 tool, resource and prompt with identical names, plus the new
  ones; `lantern mcp install`.
- `migrate-v1` **for real**: import state, then stop/remove v1's `lanternd`, sudoers, the
  `/etc/caddy` files and the dnsmasq conf (13 §3.12).
- **Done when:**
  - `lantern doctor` is all green;
  - `https://myapp.glow` loads trusted in Firefox and Chrome, and Vite HMR works through it;
  - Claude Code's existing Lantern MCP setup works unchanged;
  - **v1 is uninstalled.**

## M4 — See everything, run any branch (the new features)
- `discovery/`: the netlink scanner, process→checkout matching, labels, probe, hide rules,
  port-conflict reports, External instances, adopt.
- `git/` + `instances/`: worktree listing and the stat-gated poll, monorepo mapping, run
  branch (local, remote, new, PR), the prepare pipeline (`copy`/`link`/`setup`,
  lockfile-stamped re-runs), removal and GC.
- CLI: `run myapp@feature-x`, `wt add|rm|ls`, `adopt`, `ls` including unmanaged servers.
- **Done when:**
  - `lantern run myapp@feature-x` goes from nothing to `https://feature-x.myapp.glow` with
    deps installed and `.env` copied;
  - a `pnpm dev` started by hand in a terminal shows up in `lantern ls` labelled with its
    project and branch.

## M5 — GUI and tray
- `lantern-gui`: dashboard, project detail with a virtualized log view, branch picker, Ctrl+K
  switcher, services, settings, doctor/onboarding; ksni tray with state icons; notifications;
  single-instance over D-Bus; XDG autostart (`--tray`).
- **Done when:** the daily flows in 16 §3.4 all work without a terminal, and the GUI opens
  instantly.

## M6 — Package and polish
- A `.deb` via cargo-deb: bundled Caddy, user unit, desktop file, polkit policy, postinst and
  postrm (13 §3.11). `lantern uninstall`. A fresh-VM install test (13 §7).
- Docs: user-facing README, the `lantern.yaml` reference generated from the schema.

---

## Testing strategy (cross-cutting)
| Layer | What | How |
|---|---|---|
| Pure logic | slugs/ids, template grammar + both renderers, manifest resolution precedence, state migrations, route plan → Caddy JSON, the status state machine, port allocator, adoption rules | Unit tests and golden files (`insta`). These carry most of the coverage. |
| OS boundaries | `/proc`, netlink, `systemctl`/`systemd-run`, `git`, Caddy admin | Each behind a trait (`UserSystemd`, `ProcRoot`, `Git`, `CaddyAdmin`) with a fake for unit tests. Real-system tests are marked `#[ignore]` and run locally or on the VM with `cargo test -- --ignored`. |
| Git | worktree scenarios (monorepo, detached, locked, bare, submodules, branch with `$(…)` in its name) | Temp repos built by a test helper using real `git` |
| Daemon API | REST and SSE contracts, the Host/Origin guard, `?wait` semantics, MCP tool schemas | `axum` in-process tests; a snapshot of the MCP `tools/list` output guards v1 compatibility |
| End to end | the manual checklist in 13 §7 on a fresh Ubuntu 26.04 KDE VM | Before M3 cutover and M6 release |

Security regressions get permanent tests:
- Host-header rebinding.
- Cross-origin POST.
- Branch-name injection.
- `call_tool_api` SSRF.
- Path traversal in docs serving.
