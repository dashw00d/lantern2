# Lantern v2 — design docs

These docs describe the Rust rewrite of Lantern: a single `lantern` binary that runs and names
every local dev server, at any git branch, and exposes everything to agents over MCP. No code
exists yet; these docs are the spec.

## Reading order
1. **`01-architecture.md`**: the core. Process model, crates, domain types, persistence,
   events, API conventions.
2. **`02-decisions.md`**: the audit of the subsystem docs. Conflicts and their resolutions,
   plus the must-prove-first spikes. **It wins any disagreement.**
3. The subsystem doc for whatever you're building:

| Doc | Subsystem |
|---|---|
| `10-runtime.md` | Supervisor: systemd units, launcher and env (nvm/fnm/venv), logs, ports, readiness, health, crashes |
| `11-instances.md` | Branches and worktrees: checkouts → instances, run branch X, setup, monorepos, removal |
| `12-discovery.md` | Finding every listening dev server, including hand-started ones; adoption |
| `13-system.md` | Caddy, TLS/CA, DNS, `init`/`doctor`, systemd units, shared services, packaging, v1 cleanup |
| `14-config.md` | `lantern.yaml`, `config.toml`, `state.json`, detection, templates, profiles, v1 import |
| `15-api-mcp.md` | REST and SSE, MCP server (v1-compatible), intel (docs/OpenAPI), API security |
| `16-clients.md` | CLI, iced GUI, ksni tray |

4. **`20-roadmap.md`**: milestones M0–M6, running v2 next to v1, and the testing strategy.

## Precedence
`02-decisions.md` > `01-architecture.md` > `10`–`16`. Passages in 10–16 that were overridden
are marked `→ 02 Dn`.

## Reference
v1 source: `../.reference/lantern-v1/` (git-ignored copy of github.com/dashw00d/lantern).
