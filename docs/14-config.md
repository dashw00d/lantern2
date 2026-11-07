# 14 — Config: manifest, settings, state, detection, templates, profiles

Status: draft for review. Builds on `01-architecture.md` (authoritative). v1 paths are relative to
`.reference/lantern-v1/`. Crate facts were checked on crates.io / docs.rs on 2026-11-06.

## Summary (plain language)

Lantern keeps three kinds of data, in three different places, and each has one owner:

| What | File | Who edits it |
|---|---|---|
| How to run **one project** | `lantern.yaml` inside the project | you (and agents) |
| Your **global preferences** (workspace folders, TLD, port range) | `~/.config/lantern/config.toml` | you, or the GUI |
| Lantern's **own bookkeeping** (registered projects, ports, last-known status) | `~/.local/state/lantern/state.json` | the daemon only |

Decisions in one breath:

- **Every v1 `lantern.yaml` keeps loading.** Unknown keys only warn. Mistakes produce messages
  like `lantern.yaml:7:8 error[wrong_type] run.cmd: expected a string, found a list`, and the
  same checks are published as a JSON Schema so editors autocomplete the file.
- **`${PORT}`, `${DOMAIN}`... are substituted; everything else is left for the shell.** A shell
  `${HOME}` in `run.cmd` must keep working (it did in v1), so the core doc's "unknown variable is
  an error" rule is replaced by "closed set + typo warning" (see §9).
- **Each checkout reads its own manifest**, falls back to the primary's, and gaps are filled from
  a template or from framework detection. Precedence is a fixed 6-layer list (§3.6).
- **Scanning auto-adds only folders that have a manifest** (as v1). Folders that merely look like
  a dev project become *suggestions* with an "add" button that can write a starter `lantern.yaml`.
- **YAML crate: `serde-saphyr`** (pure Rust, line/column spans, parser budgets). Not
  `serde_yaml` (archived), not `serde_yml` (RUSTSEC-2025-0068).
- **`lantern migrate-v1`** reads the old `~/.config/lantern` files without modifying them and is
  safe to run repeatedly.

---

## 1. Purpose & scope

**In scope:** `lantern.yaml` schema/validation/resolution; template variables; `config.toml`;
`state.json`; workspace scanning and suggestions; framework detection; templates; profiles;
`migrate-v1`; the `lantern-core` config API and the `registry/` persistence interface.

**Not in scope:** what `run.cmd` *does* once started, readiness, retries, env injection, port
allocation (`10-runtime`); worktree creation, what `setup`/`worktree.*` *mean* (`11-instances`);
listener→checkout matching (`12-discovery`); Caddy/DNS/systemd/v1 system-level cleanup
(`13-system`); REST/MCP route shapes and docs/OpenAPI ingestion (`15-api-mcp`); CLI/GUI layouts
(`16-clients`). This doc defines the *shape and validity* of those keys, not their runtime meaning.

---

## 2. v1 behavior audit

| v1 file | What it does | Verdict |
|---|---|---|
| `daemon/lib/lantern/config/lantern_yml.ex` | Whitelist of 23 top-level keys; sub-keys whitelisted for `features`/`deploy`/`endpoints`; unknown top-level keys silently dropped; invalid `type`→`unknown`, invalid `kind`→`project`; `docs`/`docs_auto`/`api_auto` normalised leniently (strings→lists, `"yes"`→true) | **Keep** every key and the leniency. **Change**: warn on unknown/invalid instead of silence; add line/col. |
| same, `interpolate/2`; `projects/project.ex` `interpolate_cmd/2` | Plain `String.replace` of `${PORT}`, `${DOMAIN}`, `${NAME}`, `${PATH}`; **anything else is left untouched** | **Keep** the closed-set behaviour (it is why `${HOME}` works). Rename: `NAME`→`PROJECT`, `PATH`→`CHECKOUT` (old names stay as deprecated aliases). |
| `projects/detector.ex` | Manifest first; on *any* manifest parse error silently falls back to heuristics; heuristic chain laravel→symfony→php→next→nuxt→remix→vite→fastapi→django→flask→static; lockfile package-manager detection; `npx `→`pnpm exec`/`yarn`/`bunx` rewrite | **Keep** chain and lockfile logic. **Change**: a broken manifest is an error shown to the user, never a silent fallback; add `packageManager` field, Python runners, `${HOST}` binding. |
| `projects/scanner.ex` | Depth 1 under each root; skips dot-dirs and `node_modules vendor _build deps ...`; imports **only** dirs with `lantern.yaml`/`.yml`; default roots `~/sites`, `~/tools` | **Keep** manifest-first. **Change**: configurable depth (default 2), stop-descending rule, linked-worktree skip, suggestions. |
| `projects/manager.ex` (`do_scan`, `load_projects`, `persist_projects`) | State holds a full copy of every project; **on rescan the state copy wins over the manifest** (only `path` is refreshed); `reset_from_manifest` re-reads the file. Stale paths dropped at load | **Change**: manifest is the source of truth; GUI edits become explicit sparse *overrides* (§3.6, §3.9). Missing paths are flagged, not dropped. |
| `config/store.ex` | `state.json` in `$LANTERN_STATE_DIR` (default `~/.config/lantern`); keys `projects`, `port_assignments`, `service_states`, `health_results`; tmp+rename write, no fsync, **corrupt file silently replaced by defaults** | **Replace** (§3.9): versioned, fsync, `.bak`, loud recovery. |
| `config/settings.ex` | `settings.json` in the same dir; defaults `workspace_roots, tld ".glow", php_fpm_socket, caddy_mode, state_dir, port_range 41000-42000`; API-updatable: `workspace_roots tld php_fpm_socket caddy_mode default_template active_profile`; env overrides only `LANTERN_WORKSPACES`, `LANTERN_TLD` (plus `config/runtime.exs` for port/state dir/PHP socket/port range) | **Replace** with `config.toml` (§3.8), map every key. |
| `templates/registry.ex`, `priv/templates/*.yml` | 7 built-ins (django fastapi laravel nextjs nuxt static vite) + user `~/.config/lantern/templates/*.yml`; hand-rolled YAML writer breaks on quotes; the manifest `template:` key is parsed and stored but **never applied** | **Keep** shape (flat manifest-like file) and CRUD. **Change**: real YAML writer; `template:` finally means something. |
| `profiles/manager.ex`, `profile.ex` | Built-ins `light`, `full_stack`, `demo`; user `profiles/*.json`; `active_profile` plain file. **Activation only records the name** — services, `auto_start_projects`, `env` are never acted on | **Keep** the concept and names; **define** real semantics (§3.11). YAML files. |
| `projects/port_allocator.ex` | `port_assignments` name→port, first free in range | Owned by `ports/`; persistence shape in §3.9. |
| repo-root `lantern.yaml`, `lantern.yaml.example` | Real-world manifests (note `type: self`, `kind: service`, `deploy.*` with `systemctl`, `routing.triggers`) | Become golden test fixtures (§7). |

Surprises worth remembering: yml `name:` is ignored by v1 (display name is the directory name);
`run.pre_start`/`post_stop`/`deploy.install` are parsed and never executed; `routing.aliases/paths/
websocket` are stored but Caddy generation never reads them; `features.auto_start` is stored but
v1 auto-activated *every* scanned project with a command; v1's own daemon runs as the installing
user, so its data is in `~/.config/lantern` (`packaging/postinst` adds `User=`).

---

## 3. Design

### 3.1 Files and directories

```
$XDG_CONFIG_HOME/lantern/            (~/.config/lantern; override LANTERN_CONFIG_DIR)
  config.toml                        settings (§3.8), hand-editable
  profiles/<name>.yaml               profiles (§3.11)
  templates/<name>.yaml              user templates (§3.7); *.yml also read
$XDG_STATE_HOME/lantern/             (~/.local/state/lantern; override LANTERN_STATE_DIR)
  state.json  state.json.bak  state.lock  state.json.corrupt-<unixts>  state.json.pre-migrate-v<N>
$XDG_DATA_HOME/lantern/              (~/.local/share/lantern; override LANTERN_DATA_DIR)
  worktrees/ caddy/                  (owned by 11 / 13)
<repo>/schema/lantern.schema.json    committed, generated (§3.5)
<repo>/crates/lantern-core/builtin/{templates,profiles}/*.yaml   embedded with include_str!
```

`lantern_core::dirs::Dirs { config, state, data }` resolves these once (`Dirs::from_env(&dyn
EnvSource)`); tests pass a temp dir. **Note:** v1's `LANTERN_STATE_DIR` meant "config *and* state";
if a v2 daemon finds a `state.json` there without `"version"` but with `port_assignments`, it
refuses with: `/home/u/.config/lantern/state.json is a Lantern v1 file; run 'lantern migrate-v1'`.

### 3.2 YAML crate decision

| Crate (latest, crates.io) | Status | Line/col | Verdict |
|---|---|---|---|
| `serde_yaml` 0.9.34 | archived, deprecated | yes | no |
| `serde_yml` 0.0.13 | RUSTSEC-2025-0068: unsound + unmaintained; now a shim to `noyalib` | — | **never** |
| `serde_yaml_ng` 0.10.0 | last release 2024-05-26 | `Error::location()` (fork of serde_yaml) | stale |
| `serde_norway` 0.9.42 | last release 2024-12-21 | same | stale |
| `yaml_serde` 0.10.7 | YAML-org fork of serde_yaml, 2026-08-18; `Error::location()` has `line()`/`column()` | errors only | maintained, but gives **no spans for valid nodes**, which §3.5 needs |
| **`serde-saphyr` 2.1.1** | 2026-11-05; 1.0 on 2026-07-31; `unsafe` denied in the crate; `Budget` limits; `Options::duplicate_keys`; `Spanned<T>` (`referenced`/`defined` `Location` with `line()`/`column()`, 1-based); error snippets; `to_string` for writing | **yes** | **chosen** |
| `saphyr` 0.3.3, `yaml-rust2` 0.13 | event/tree parsers, no serde | markers | fallback only; `saphyr` went 0.1→0.3 in 15 days |

Rationale: only `serde-saphyr` gives both typed errors *and* per-node source positions, and its
`Budget` protects against YAML bombs in manifests that arrive with cloned repos. Risk: young and
fast-moving (2.1.0 was yanked two days before 2.1.1). Mitigations: pin the exact version in
`Cargo.toml` (`=2.1.1`), commit `Cargo.lock`, and keep **all** YAML access inside
`lantern_core::yaml` (`parse_to_tree`, `to_string`) so a swap touches one module.

Parse options for manifests: `duplicate_keys` = error; `strict_booleans` = false (v1 accepted
yes/no/on/off); `Budget { max_depth: 32, max_nodes: 20_000, max_aliases: 1_000, max_events:
100_000, .. }`; input capped at **1 MiB** *before* parsing; single document only.

### 3.3 `lantern.yaml` schema

#### Annotated example (every key; `# v2` marks new keys)

```yaml
# yaml-language-server: $schema=https://raw.githubusercontent.com/<REPO>/main/schema/lantern.schema.json
version: 1                       # optional manifest schema version (only 1 exists)
id: myapp                        # ProjectId; slugified if not already a slug
name: My App                     # display name
description: Customer portal
kind: project                    # service | project | capability | website | tool
tags: [web, portal]
enabled: true
repo_url: https://github.com/acme/myapp
template: nextjs                 # defaults source (v1: stored but unused)

type: proxy                      # proxy | php | static | unknown
domain: myapp.glow               # primary instance only; others get <branch-slug>. prefix
root: public                     # docroot for php/static, relative to the checkout
upstream_url: http://127.0.0.1:8000   # fixed upstream; Lantern runs nothing
base_url: http://127.0.0.1:8000       # for health/intel when not Lantern-run
health_endpoint: /health

run:
  cmd: pnpm dev --host ${HOST} --port ${PORT}
  cwd: .                         # relative to checkout; may not escape it
  env:
    APP_ENV: development
    PUBLIC_URL: ${URL}
setup:                           # v2 (11-instances): shell commands, run in order
  - pnpm install --frozen-lockfile
  - cp -n .env.example .env
worktree:                        # v2 (11-instances)
  copy: [".env", ".env.local"]   # globs relative to the primary checkout
  link: ["node_modules/.cache"]
  env:
    DATABASE_NAME: myapp_${BRANCH_SLUG}
deploy:                          # runtime overrides (10-runtime)
  start: systemctl --user start myapp
  stop: systemctl --user stop myapp
  restart: systemctl --user restart myapp
  logs: journalctl --user -u myapp -n 100 --no-pager
  status: systemctl --user is-active myapp
  install: pnpm install          # deprecated, see below
  env_file: .env
depends_on: [redis, postgres]
features: { mailpit: true, auto_start: false, auto_open_browser: false }
routing:
  aliases: [app.glow]
  paths: { "/api": "127.0.0.1:8000" }
  websocket: true
  triggers: [portal, customers]  # MCP intent hints
  risk: low                      # low | medium | high
  requires_confirmation: false
  max_concurrent: 1
  agents: [claude]
docs: [README.md, { path: docs/API.md, kind: api }]
docs_auto: { enabled: true, patterns: ["docs/**/*.md"], ignore: ["node_modules/**"], max_files: 80, max_bytes: 1048576 }
endpoints:
  - { method: POST, path: /api/jobs, description: Create job, category: Jobs, risk: medium, body_hint: {url: string}, params: {} }
api_auto: { enabled: true, sources: [http://127.0.0.1:8000/openapi.json], candidates: [/openapi.json], timeout_ms: 2500, max_endpoints: 400 }
x-owner: platform-team           # keys starting "x-" are ignored silently
```

#### Key reference

Status: `v1` = unchanged; `v1→X` = deprecated, maps to X; `NEW` = v2.

| Key | Type | Default | Status / notes |
|---|---|---|---|
| `version` | int | `1` | NEW. Only `1` valid; other values are an error. |
| `id` | string | slug(dir name) | v1. Invalid slug → `id_normalized` warning + slugified (v1 ids were free-form). Registered id is stable; later edits only warn (`id_changed`). |
| `name` | string | `id` | v1 (ignored by v1; now the display name). |
| `description`, `repo_url` | string | — | v1. |
| `kind` | enum | `project` | v1. Unknown value → `unknown_value` warning, `project`. |
| `tags`, `depends_on` | string list | `[]` | v1. A single string is a one-item list; comma/newline separated string is split. |
| `enabled` | lenient bool | `true` | v1. |
| `template` | string | — | v1, **now applied** (§3.6). Unknown name → `unknown_template` warning. |
| `type` | enum | inferred | v1. `proxy`/`php`/`static`/`unknown`. Unknown (e.g. `self`) → `unknown_value` warning + `unknown`. `unknown` + (`run.cmd` or `upstream_url`) is treated as `proxy` (v1 behaviour). |
| `domain` | hostname | `<id>.<tld>` | v1. Bare label `myapp` → `myapp.<tld>`. Outside the TLD → `domain_outside_tld` warning. |
| `root` | rel path | `public` (php), `.` (static) | v1. |
| `upstream_url`, `base_url` | http(s) URL | — | v1. Blank → absent. |
| `health_endpoint` | path starting `/` | — | v1. |
| `run.cmd` | string | from template/detection | v1. Blank → absent. |
| `run.cwd` | rel path | `.` | v1. Absolute or escaping the checkout → `bad_path` error. |
| `run.env` | map string→scalar | `{}` | v1. Numbers/bools coerced to strings. `PORT`/`HOST` set without `${PORT}`/`${HOST}` → `reserved_env` warning. |
| `run.pre_start`, `run.post_stop` | string | — | v1→*ignored*: parsed but never executed in v1 or v2.0 (`ignored_key` warning). Use `setup` or `deploy.*`. |
| `setup` | list of strings | `[]` | NEW. Single string = one command (a multi-line block scalar is one script). Blank entries rejected. |
| `worktree.copy`, `.link` | list of globs | `[]` | NEW. Relative, no `..`, valid `globset` syntax. |
| `worktree.env` | map string→scalar | `{}` | NEW. Keys must match `[A-Za-z_][A-Za-z0-9_]*`. |
| `deploy.start/stop/restart/logs/status` | string | — | v1. Semantics in 10-runtime. |
| `deploy.install` | string | — | v1→`setup` (parsed, never run in v1). `deprecated_key` warning; not executed. |
| `deploy.env_file` | rel path | — | v1. |
| `features.mailpit/auto_start/auto_open_browser` | lenient bool | `false` | v1. `auto_start` now honoured (§3.11); `auto_open_browser` is a client hint (16). Unknown feature keys warn. |
| `routing.aliases/paths/websocket` | list / map / bool | — | v1; parsed and validated, routed only if 13-system implements (v1 never did). |
| `routing.triggers/agents` | string list | `[]` | v1. |
| `routing.risk`, `endpoints[].risk` | `low\|medium\|high` | — | v1. Other values → `unknown_value` warning, dropped. |
| `routing.requires_confirmation` | bool | `false` | v1. |
| `routing.max_concurrent` | int ≥ 1 | `1` | v1. |
| `docs[]` | path string, or `{path, kind?}` | `[]` | v1. `kind` inferred from name (readme/changelog/contributing/markdown/text/unknown) as v1. Path must be relative, no `..` (`bad_path`). |
| `docs_auto.*` | `enabled` (true when section present), `patterns`, `ignore` (string or list), `max_files` (100), `max_bytes` (1 MiB) | section absent = disabled | v1; defaults mirror `projects/discovery.ex`. Semantics: 15 (intel). |
| `endpoints[]` | `method` (GET), `path` (required, `/…`), `description`, `category`, `risk`, `body_hint` (any), `params` (any) | `[]` | v1. Method upper-cased; one of GET POST PUT PATCH DELETE HEAD OPTIONS. |
| `api_auto.*` | `enabled`, `sources`, `candidates` (`/openapi.json`, `/swagger.json`, `/docs/openapi.json`), `timeout_ms` (2500), `max_endpoints` (400) | absent = disabled | v1; semantics: 15 (intel). |
| `x-*` | any | — | NEW. Ignored without warning. |

Any other key at any level: `unknown_key` **warning** with a did-you-mean (Levenshtein ≤ 2).

#### Rust shape (`lantern_core::manifest::raw`)

Every field is `Option` so layers can merge. Enum-like keys stay `String` here and are converted
(with warnings) during validation; this is what makes v1's "fall back to default" leniency possible.
**No `#[serde(flatten)]` and no `deny_unknown_fields`** anywhere in manifest types (flatten breaks
`serde_ignored`; unknown keys are warnings).

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RawManifest {
    pub version: Option<u32>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub kind: Option<String>,
    #[serde(deserialize_with = "string_or_list")] #[schemars(schema_with = "string_or_list_schema")]
    pub tags: Option<Vec<String>>,
    #[serde(deserialize_with = "lenient_bool")] pub enabled: Option<bool>,
    pub repo_url: Option<String>,
    pub template: Option<String>,
    #[serde(rename = "type")] pub serve: Option<String>,
    pub domain: Option<String>,
    pub root: Option<String>,
    pub upstream_url: Option<String>,
    pub base_url: Option<String>,
    pub health_endpoint: Option<String>,
    pub run: Option<RunSection>,
    #[serde(deserialize_with = "one_or_many")] pub setup: Option<Vec<String>>,   // NEW
    pub worktree: Option<WorktreeSection>,                                        // NEW
    pub deploy: Option<DeploySection>,
    #[serde(deserialize_with = "string_or_list")] pub depends_on: Option<Vec<String>>,
    pub features: Option<FeaturesSection>,
    pub routing: Option<RoutingSection>,
    pub docs: Option<Vec<DocEntry>>,
    pub docs_auto: Option<DocsAuto>,
    pub endpoints: Option<Vec<EndpointEntry>>,
    pub api_auto: Option<ApiAuto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct RunSection {
    pub cmd: Option<String>,
    pub cwd: Option<String>,
    pub env: Option<BTreeMap<String, ScalarString>>,   // ScalarString: string | number | bool -> String
    pub pre_start: Option<String>,                     // ignored_key
    pub post_stop: Option<String>,                     // ignored_key
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WorktreeSection {
    #[serde(deserialize_with = "one_or_many")] pub copy: Option<Vec<String>>,
    #[serde(deserialize_with = "one_or_many")] pub link: Option<Vec<String>>,
    pub env: Option<BTreeMap<String, ScalarString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, expecting = "a file path string or a mapping with `path` (and optional `kind`)")]
pub enum DocEntry { Path(String), Full { path: String, kind: Option<String> } }
```

`DeploySection`, `FeaturesSection`, `RoutingSection`, `EndpointEntry`, `DocsAuto`, `ApiAuto` follow
the key table field-for-field (all `Option`; `EndpointEntry.path` is the one required `String`).
Helper deserializers: `string_or_list` (v1 `normalize_string_list`: string split on newline/comma),
`one_or_many` (**no splitting**: commands contain commas), `lenient_bool` (v1 `normalize_bool`:
`true/yes/on/1`, `false/no/off/0`), `ScalarString`. Every untagged enum sets `expecting = "…"` so
errors say what was wanted instead of "did not match any variant".

#### Identity helpers (`lantern_core::ids`)

`slugify(input, max)`: lowercase; every run of characters outside `[a-z0-9]` becomes one `-`; trim
`-`; truncate to `max` and re-trim; empty → `"project"`. Non-ASCII is dropped (no transliteration).
`ProjectId::parse` requires `^[a-z0-9]([a-z0-9-]*[a-z0-9])?$`, ≤ 48 (stricter than the core doc:
ids become DNS labels, which cannot start or end with `-`). `BRANCH_SLUG_MAX = 40` (leaves room
for a collision suffix inside the 63-char DNS label limit). Collisions between *registered*
projects get `-2`, `-3`… and the result is stored in `state.json`, so it never changes later.

### 3.4 Template variables

Names (core §6): `PORT HOST DOMAIN URL PROJECT INSTANCE BRANCH BRANCH_SLUG CHECKOUT PRIMARY`.
Deprecated v1 aliases: `NAME`→`PROJECT`, `PATH`→`CHECKOUT` (`deprecated_var` warning; `${PATH}`
meant the project directory in v1, so v1 manifests keep working; write `$PATH` for the shell's).
Proposed addition: `BRANCH_SNAKE` (= `BRANCH_SLUG` with `-`→`_`, for DB names; §9).

Values: `PORT` decimal (leased before `setup` runs, so it is valid there); `HOST` `127.0.0.1`;
`DOMAIN` `feature-x.myapp.glow`; `URL` `https://feature-x.myapp.glow`; `PROJECT` the ProjectId;
`INSTANCE` the InstanceId with `@`→`_` (`myapp_feature-x`, = `InstanceId::safe_name()`, → 02 D2); `BRANCH` raw branch
(empty if detached/non-git); `CHECKOUT`/`PRIMARY` absolute paths (equal for the primary instance).

**Grammar (decided: closed-set substitution).** The scanner walks the string once:

1. `$${` → emit literal `${` (the only escape; lets a script contain the text `${PORT}`).
2. `${NAME}` where `NAME` is exactly one of the known names or aliases → substituted.
3. Everything else is copied verbatim: `${HOME}`, `${X:-y}`, `${#arr}`, `$PORT`, `$$` (shell PID),
   `$(cmd)`, unknown `${FOO}`.

> **Reconciled → 02 D1:** the grammar above is kept, but *rendering* depends on context. In **shell contexts** (`run.cmd`, `run.pre_start`, `run.post_stop`, `setup[]`, `deploy.*`) a known `${NAME}` is rewritten to `${LANTERN_NAME}` and exported by the launcher, never pasted into the text (branch names may contain shell metacharacters). In **data contexts** (`run.env`, `worktree.env`, profile `env` values, `run.cwd`) it is substituted textually. API: `render_shell()` / `render_data(ctx)` replace `render`. New lint `var_in_single_quotes` (warning). `${BRANCH}` for a detached HEAD is the short sha.

~~Substitution happens *before* the shell sees the string, so it also applies inside single quotes.~~
Lint (warning `possible_var_typo`): a verbatim `${UPPER_NAME}` within Levenshtein distance 2 of a
known name and not declared in the same manifest's `run.env`/`worktree.env` → "did you mean
`${PORT}`?". Check `port_not_referenced` (warning): serve is `proxy`, `run.cmd` is set, there is no
`upstream_url`/`deploy.start`, and none of `run.cmd`, `run.env` values, `deploy.start` mention
`PORT` in any form (`${PORT}`, `$PORT`, `${PORT:-…}`). v1 made this a hard error at activation;
v2 downgrades it because the runtime also exports `PORT` in the environment and readiness will
fail with a clear message anyway.

Applies to: `run.cmd`, `run.cwd`, `run.env` values, `setup[]`, `worktree.env` values, `deploy.*`
(except `env_file` path), profile `env` values. Not to `domain`, `upstream_url`, `docs`.

```rust
pub enum TemplateVar { Port, Host, Domain, Url, Project, Instance, Branch, BranchSlug, BranchSnake, Checkout, Primary }
pub struct TemplateString { source: String, segments: Vec<Segment> }   // Segment::Lit(String) | Segment::Var(TemplateVar)
impl TemplateString {
    pub fn parse(src: &str) -> (Self, Vec<TemplateLint>);   // never fails
    pub fn render(&self, ctx: &VarContext) -> String;       // infallible: every var is in ctx
    pub fn mentions(&self, name: &str) -> bool;             // also true for verbatim $NAME / ${NAME:-…}
}
pub struct VarContext { pub port: u16, pub host: IpAddr, pub domain: String, pub url: String, pub project: ProjectId,
                        pub instance: InstanceId, pub branch: Option<String>, pub branch_slug: Option<String>,
                        pub checkout: PathBuf, pub primary: PathBuf }
```

Because parsing is done once during resolution, **start-time expansion cannot fail**.

### 3.5 Parsing, validation, diagnostics, JSON Schema

Pipeline (`lantern_core::manifest::parse_manifest(src, file) -> ParsedManifest`; never panics,
never returns `Err`):

```
text ──(size ≤1 MiB, UTF-8, strip BOM)──► serde-saphyr into Spanned<Node>   // Node = null|bool|number|str|seq|map, each child Spanned
     ──► PosIndex (path → key span + value span)  and  serde_json::Value
     ──► for each top-level key K:  deserialize {K: value} into RawManifest
              with serde_path_to_error (typed errors, with path) + serde_ignored (unknown keys, with path)
     ──► merge the per-key successes into one RawManifest           // so several errors are reported at once
     ──► semantic validation (enums, urls, hostnames, globs, paths, templates, cross-key checks)
     ──► ParsedManifest { raw, diagnostics, usable: bool }          // usable = no Error diagnostics
```

`Node` has a hand-written `Deserialize` using `deserialize_any` whose `visit_map` reads
`next_key::<Spanned<String>>()` / `next_value::<Spanned<Node>>()`. That avoids serde's buffering,
the documented way `Spanned` loses locations. **Spike S1 (first task):** confirm nested positions
survive; fallback is a ~100-line tree builder over `yaml-rust2`'s event parser (`Marker`) with the
same `Node`/`PosIndex` output. Nothing else changes.

```rust
pub struct Diagnostic {
    pub severity: Severity,            // Error | Warning | Info
    pub code: &'static str,            // snake_case, stable, listed below
    pub path: String,                  // "run.env.DB_URL", "endpoints[2].method"; "" = whole file
    pub file: PathBuf,
    pub line: Option<u32>, pub column: Option<u32>,   // 1-based, characters; nearest ancestor if path unknown
    pub message: String,               // what is wrong
    pub expected: Option<String>,      // what was wanted
    pub help: Option<String>,
}
```

Human rendering (CLI, logs, GUI tooltip):

```
lantern.yaml:7:8: error[wrong_type] run.cmd: expected a string, found a list
      7 |   cmd: [pnpm, dev]
        |        ^
  help: run.cmd is one shell command line, e.g. `pnpm dev --port ${PORT}`
lantern.yaml:3:1: warning[unknown_key] rn: unknown key (ignored); did you mean `run`?
lantern.yaml:12:10: error[bad_value] worktree.copy[1]: invalid glob "[.env": unclosed character class
lantern.yaml:5:1: error[yaml_syntax]: tabs cannot be used for indentation
  help: use spaces
lantern.yaml:9:15: warning[possible_var_typo] run.cmd: `${PROT}` is not a Lantern variable and is passed to the shell unchanged; did you mean `${PORT}`?
```

| Code | Sev | Raised when |
|---|---|---|
| `yaml_syntax`, `yaml_limits`, `file_too_large`, `not_utf8`, `multiple_documents`, `duplicate_key` | E | file-level problems. An **empty file or comments only is a valid empty manifest** (v1 rejected it). |
| `not_a_mapping` | E | top level is not a mapping (`lantern.yaml must contain a YAML mapping`). |
| `wrong_type`, `missing_field`, `bad_value` | E | typed deserialisation / range / URL / hostname / `version` ≠ 1 |
| `bad_path` | E | `run.cwd`, `docs[]`, `worktree.*`, `root` absolute or containing `..` that escapes |
| `bad_glob` | E | `worktree.copy/link` entry fails `globset` |
| `unknown_key` | W | unknown key (except `x-*`) |
| `unknown_value` | W | invalid `type`/`kind`/`risk`; default used (v1 behaviour) |
| `deprecated_key`, `ignored_key`, `deprecated_var` | W | see key table |
| `possible_var_typo`, `port_not_referenced`, `reserved_env` | W | §3.4 |
| `domain_outside_tld`, `key_not_applicable` (e.g. `run.cmd` with `type: static`) | W | |
| `id_normalized`, `id_changed`, `unknown_template`, `template_key_ignored`, `both_manifest_files` | W | |
| `identity_key_ignored` | I | project-level key in a non-primary checkout's manifest (§3.6) |

A manifest with any `E` is *not usable*: the instance shows `NeedsConfig` with the first error as
`RuntimeState.message`. `raw` still carries the keys that parsed, so the GUI can show them.

**JSON Schema.** `lantern_core::manifest::schema::manifest_json_schema()` builds the schema from
`RawManifest` with `schemars` 1.2 (`SchemaSettings::draft07()` for the widest editor support, then
`SchemaGenerator::into_root_schema_for::<RawManifest>()`; confirm exact names against 1.2.3 docs).
Enum-like `String` keys get `schema_with` enums; `x-*` is added as `patternProperties`;
`additionalProperties: true` (unknown keys are warnings, so the schema must not reject them).
Published as:

1. `schema/lantern.schema.json` committed in the repo, with a test that regenerates and diffs
   (CI fails when stale). Public URL = raw GitHub of that file; one constant `SCHEMA_URL` in
   `lantern-core` (repo path is a placeholder until the repo is public, see §8).
2. Every starter manifest Lantern writes begins with
   `# yaml-language-server: $schema=<SCHEMA_URL>` (modeline supported by Red Hat's
   yaml-language-server in VS Code, Zed, Neovim, etc.; relative paths in a modeline resolve from the
   file, which allows `../../schema/...` inside this repo).
3. `lantern schema [manifest|profile|config]` prints the schema offline;
   `GET /api/schema/<name>` serves it from the daemon.
4. Later: submit to SchemaStore with `fileMatch: ["lantern.yaml","lantern.yml"]` so no header is needed.

Same treatment for profiles (`lantern-profile.schema.json`) and `config.toml`
(`lantern-config.schema.json`, referenced by `#:schema <url>` for Taplo). Templates reuse the
manifest schema.

### 3.6 Manifest resolution per checkout

`ResolvedManifest` is what `Instance.manifest` holds. It is fully typed, defaults applied, commands
pre-parsed into `TemplateString`, and carries provenance and diagnostics.

```rust
pub struct ResolvedManifest {
    pub id: ProjectId, pub name: String, pub description: Option<String>, pub kind: ProjectKind,
    pub tags: Vec<String>, pub enabled: bool, pub repo_url: Option<String>,
    pub serve: ServeMode,                         // Proxy | Php | Static | Unknown (after inference)
    pub domain: Option<String>,                   // explicit override (primary only)
    pub root: Option<RelPath>, pub upstream_url: Option<Url>, pub base_url: Option<Url>,
    pub health_endpoint: Option<String>,
    pub run: ResolvedRun,                         // cmd: Option<TemplateString>, cwd: RelPath, env: BTreeMap<String, TemplateString>
    pub setup: Vec<TemplateString>,
    pub worktree: WorktreeSpec,                   // copy/link: Vec<String> (validated globs), env: BTreeMap<String, TemplateString>
    pub deploy: DeploySpec, pub depends_on: Vec<String>, pub features: Features, pub routing: Routing,
    pub docs: Vec<DocRef>, pub endpoints: Vec<Endpoint>, pub docs_auto: Option<DocsAuto>, pub api_auto: Option<ApiAuto>,
    pub provenance: Provenance,                   // files read + Layer per top-level key
    pub diagnostics: Vec<Diagnostic>, pub usable: bool,
}
pub enum Layer { Override, CheckoutManifest, PrimaryManifest, Template(String), Detected(Framework), Default }
```

**Precedence, highest first** (merge is per key; `run.env`/`worktree.env` maps union with the
higher layer winning; lists and scalars are replaced, so `tags: []` really clears):

| # | Layer | Source |
|---|---|---|
| 1 | **Override** | sparse `RawManifest` in `ProjectRecord.overrides` (GUI/API edits; manual projects live entirely here) |
| 2 | **Checkout manifest** | `lantern.yaml`/`.yml` in *this* checkout's project dir |
| 3 | **Primary manifest** | the primary checkout's file, used only when layer 2's file is absent |
| 4 | **Template** | `template:` from the merged layers 1–3; else, if layers 1–3 give no way to run (no `run.cmd`, `upstream_url`, `deploy.start`, nor explicit `type: php/static`), the template for the **detected** framework; else `settings.templates.default` |
| 5 | **Detected** | `detect_dir(checkout)`: package manager / Python runner rewriting of builtin template commands. Runs per checkout, so a branch with a different lockfile gets the right runner |
| 6 | **Built-in defaults** | `cwd="."`, `kind=project`, `enabled=true`, `domain=<id>.<tld>` … |

Rules on top of the table:

1. **No inheritance between manifest files.** If the checkout has a valid manifest it *replaces*
   the primary's for instance-level keys (a branch controls how it runs). The exception is the
   **project-level keys** — `id name description kind tags enabled repo_url domain` — which are
   always taken from the primary manifest (then overrides). Setting them in a non-primary
   manifest yields `identity_key_ignored` (info).
2. A checkout manifest that exists but is **unusable does not fall back** to the primary's (that
   would run the wrong thing silently, v1's mistake): the instance is `NeedsConfig`. Exception, the
   **last-known-good rule**: an instance that is already *running* keeps its previous resolved
   manifest until restarted, and the new errors are attached as diagnostics.
3. A named `template:` that does not exist → `unknown_template` warning, continue with detection.
4. Detection never overrides the manifest and stays out entirely when the manifest already says how
   to serve (see layer 4). Rewriting (`npx`→`pnpm exec`) touches only commands that came from a
   **builtin** template; user templates and manifests are literal.
5. `serve` inference: explicit `type`; else `upstream_url` or `run.cmd` ⇒ `Proxy`; else template's
   type; else `Unknown` (⇒ `NeedsConfig`: "no run.cmd, upstream_url or detected framework").
6. `Php`/`Static` are *route-only* (no process). Assumption on 10/13: `start` activates the route
   and reports `Running`.

Example: `myapp` primary has `run.cmd: pnpm dev …`. Worktree `feature-x` has no manifest ⇒ same
cmd, domain `feature-x.myapp.glow`. Worktree `hotfix` has its own manifest with
`run.cmd: pnpm start` and `id: other` ⇒ `pnpm start` runs, `id: other` is ignored (info).

`parse`/`resolve` are pure; file reading is `manifest::load(dir) -> Option<ParsedManifest>`
(blocking `std::fs`; the daemon wraps it in `spawn_blocking`). `find_manifest(dir)` prefers
`lantern.yaml` over `lantern.yml` (as v1) and warns `both_manifest_files` if both exist.

```rust
pub struct ResolveInput<'a> {
    pub dir_name: &'a str,                    // basename of the project root, for id/name defaults
    pub is_primary: bool,
    pub primary: Option<&'a ParsedManifest>,
    pub checkout: Option<&'a ParsedManifest>, // None when absent, or when this is the primary
    pub overrides: &'a RawManifest,
    pub templates: &'a dyn TemplateLookup,    // user shadows builtin
    pub detection: &'a Detection,
    pub settings: &'a Settings,
}
pub fn resolve(input: ResolveInput) -> ResolvedManifest;        // pure, total
```

**Cache and freshness (`registry/manifest_cache.rs`).** `get(instance) -> Arc<ResolvedManifest>`,
keyed by checkout path, stamped with `(path, mtime, size)` of every file read. Re-stat on: instance
start, `GET` with `?refresh=true`, each workspace rescan tick, and any `PATCH` of overrides. No
inotify. When the new value differs (`PartialEq`), the registry emits `InstanceUpdated`.

### 3.7 Detection and templates

`detect_dir(dir) -> Detection` is sync, cheap (existence checks first; reads `package.json` ≤ 1 MiB
once, other files ≤ 64 KiB) and records its **evidence** so the GUI and `lantern config explain`
can show *why*.

```rust
pub struct Detection {
    pub framework: Framework, pub confidence: Confidence,        // High | Medium | Low
    pub template: Option<&'static str>,
    pub package_manager: Option<PackageManager>,                 // Bun | Pnpm | Yarn | Npm
    pub python_runner: Option<PythonRunner>,                     // Uv | Poetry | Pipenv | Venv | None
    pub evidence: Vec<String>,                                   // ["package.json dependencies.next=^15", "pnpm-lock.yaml"]
}
```

Rules, first match wins (v1 order kept; Remix/React Router and Symfony details improved):

| Framework | Condition | Template | Conf. |
|---|---|---|---|
| Laravel | `artisan` and `public/index.php` | `laravel` | High |
| Symfony | `bin/console` and `public/index.php` | `symfony` | High |
| PHP | `public/index.php` (root `public`) or `index.php` (root `.`) | `php` | Medium |
| Next.js | dep `next` | `nextjs` | High |
| Nuxt | dep `nuxt` or `nuxt3` | `nuxt` | High |
| React Router | dep `@react-router/dev` | `react-router` | High |
| Remix | dep `@remix-run/dev` (+ `vite.config.*` ⇒ High, else Medium) | `remix` | High/Med |
| Vite | dep `vite` | `vite` | Medium |
| FastAPI | `fastapi` in `pyproject.toml`, `requirements*.txt` or `Pipfile` | `fastapi` | Medium |
| Django | `manage.py` containing `django` (otherwise Low) | `django` | Medium |
| Flask | `flask` (case-insens.) in `app.py`/`wsgi.py`/`requirements*.txt`/`pyproject.toml` | `flask` | Medium |
| Static | `index.html` at root | `static` | Low |

Deps are read from `dependencies` and `devDependencies`.

*Package manager* (first hit): `package.json` `"packageManager": "pnpm@…"` (corepack standard);
else lockfile `bun.lockb`/`bun.lock` → bun, `pnpm-lock.yaml` → pnpm, `yarn.lock` → yarn,
`package-lock.json` → npm; else `package.json` alone → npm. Search the directory, then parents up
to and including the first directory with `.git` (max 4 levels; monorepos keep the lockfile at
the root). Runner prefix for builtin `npx ` commands: npm `npx `, pnpm `pnpm exec `, yarn `yarn `,
bun `bunx `.

*Python runner*: `uv.lock` → `uv run `, `poetry.lock` → `poetry run `, `Pipfile.lock` → `pipenv run `,
`.venv/bin/` exists → `.venv/bin/`, else bare. FastAPI entrypoint: first of `app/main.py`,
`main.py`, `src/main.py`, `app.py`, `api/main.py` containing `FastAPI(`; `module:var` from the file
and the `var = FastAPI(` assignment; fallback `app.main:app` at Low confidence.

**Built-in templates** (`crates/lantern-core/builtin/templates/<name>.yaml`; all bind `${HOST}` =
127.0.0.1 because Vite/Nuxt default to `localhost`, which can resolve to `::1` while Caddy dials
`127.0.0.1`; flags must be re-verified against current framework CLIs at implementation time,
templates are data):

| Template | `type` | `run.cmd` / `root` | v1 |
|---|---|---|---|
| `nextjs` | proxy | `npx next dev -H ${HOST} -p ${PORT}` | yes |
| `nuxt` | proxy | `npx nuxi dev --host ${HOST} --port ${PORT}` | yes |
| `remix` | proxy | `npx remix vite:dev --host ${HOST} --port ${PORT}`; classic (no vite config): `npx remix dev --port ${PORT}` | yes (classic) |
| `react-router` | proxy | `npx react-router dev --host ${HOST} --port ${PORT}` | new |
| `vite` | proxy | `npx vite --host ${HOST} --port ${PORT}`, `routing.websocket: true` | yes |
| `fastapi` | proxy | `uvicorn app.main:app --host ${HOST} --port ${PORT}` | yes |
| `django` | proxy | `python manage.py runserver ${HOST}:${PORT}` | yes |
| `flask` | proxy | `flask run --host ${HOST} --port ${PORT}` | new |
| `laravel` | php | root `public`, `features.mailpit: true` | yes |
| `laravel-serve` | proxy | `php artisan serve --host ${HOST} --port ${PORT}` (opt-in; no php-fpm needed) | new |
| `symfony` | php | root `public` | new |
| `php` | php | root `public` | new |
| `static` | static | root `.` | yes |

v1's `features.auto_start: false` lines are dropped (it is the default).

**Template file format** = a flat manifest (v1-compatible) plus `name` and `description`, which here
name the *template*. Example `~/.config/lantern/templates/my-vite.yaml`:

```yaml
name: my-vite
description: Vite with my usual env
type: proxy
run:
  cmd: pnpm vite --host ${HOST} --port ${PORT}
  env: { VITE_API: https://api.glow }
setup: [pnpm install]
```

`id`, `domain`, `repo_url`, `enabled` in a template are ignored (`template_key_ignored`). Name =
file stem, `^[a-z0-9][a-z0-9._-]{0,47}$`; a `name:` that disagrees with the stem is an error.
A user file with a builtin's name **shadows** it (as v1); deleting the file restores the builtin.

**CRUD** (`registry/templates.rs`; routes in 15): `list`, `get`, `create`, `put` (replace),
`delete`, `fork(source, new_name)`. Bodies are either structured (`{name, description, manifest}`,
daemon serialises with `lantern_core::yaml::to_string`) or `{yaml: "…"}` stored verbatim after
validation (keeps comments for text editors). Builtins are read-only: `409 template_builtin_readonly`.
Writes are atomic (tmp + rename). Response includes `builtin`, `shadows_builtin`, `diagnostics`.

### 3.8 Settings: `config.toml`

Parsed with `toml` (+ `serde_ignored` for unknown-key warnings); written with `toml_edit` so
comments and ordering survive API updates. Precedence: **built-in default < config.toml < env var
< daemon CLI flag**. The settings API returns the effective values plus `overridden_by_env:
["domains.tld"]` (GUI shows those read-only).

| Key | Default | Env override | Applies | v1 `settings.json` source |
|---|---|---|---|---|
| `daemon.port` | `4777` | `LANTERN_DAEMON_PORT` (→ 02 D21; `LANTERN_PORT` is the instance port exported to dev servers) | restart | env `LANTERN_PORT` |
| `domains.tld` | `"glow"` (leading `.` accepted and stripped) | `LANTERN_TLD` | hot, rebuilds routes | `tld` (`".glow"`) |
| `workspaces.roots` | `["~/sites","~/tools"]` | `LANTERN_WORKSPACES` (colon-separated) | hot | `workspace_roots` |
| `workspaces.depth` | `2` | — | hot | new (v1 fixed at 1) |
| `workspaces.ignore` | `[]` (added to built-ins) | — | hot | new |
| `workspaces.include_hidden` | `[]` | — | hot | new |
| `workspaces.scan_interval_secs` | `60` (`0` = manual only) | — | hot | new |
| `workspaces.suggest` | `true` | — | hot | new |
| `ports.range_start` / `range_end` | `29000` / `29999` (→ 02 D3; v1's range is inside the ephemeral range) | `LANTERN_PORT_RANGE_START` / `_END` | hot, new leases | `port_range_start/end` |
| `php.fpm_socket` | `"auto"` (newest `/run/php/php*-fpm.sock`) | `LANTERN_PHP_FPM_SOCKET` | hot, rebuilds routes | `php_fpm_socket` (v1 default hard-coded 8.3) |
| `worktrees.dir` | `""` (= `$XDG_DATA_HOME/lantern/worktrees`) | `LANTERN_WORKTREES_DIR` | hot, new worktrees | new (core §7) |
| `templates.default` | `""` | — | hot | `default_template` |
| `profiles.default` | `"light"` | — | hot | `active_profile` (initial value only; current choice lives in `state.json`) |
| `runtime.start_timeout_secs` | `15` | — | hot | v1 constant 15 000 ms |
| `runtime.stop_timeout_secs` | `5` | — | hot | v1 constant 5 000 ms |
| `runtime.max_restarts` | `5` | — | hot | v1 constant |
| `runtime.restart_backoff_ms` | `1000` (doubles) | — | hot | v1 constant |
| `runtime.log_buffer_lines` | `1000` | — | hot | v1 constant |
| `logs.level` | `"info"` | `LANTERN_LOG` (EnvFilter syntax; `RUST_LOG` if unset) | restart | — |
| *dropped* | — | — | — | `caddy_mode` (v2 always uses the admin API), `state_dir` (now `LANTERN_STATE_DIR`) |

`[runtime]` carries v1's constants as defaults to reserve the table; `10-runtime` owns the
semantics and may add keys. **Protocol for other docs:** `13-system` (`[caddy]`, `[dns]`,
`[services]`) and `10-runtime` add typed sections to `Settings` by PR; every section struct is
`#[serde(default)]`, so adding keys is non-breaking. Renamed keys stay accepted as aliases for two
minor releases with a warning. The file has no version number.

```toml
# ~/.config/lantern/config.toml   (#:schema <SCHEMA_URL for config>)
[domains]
tld = "glow"
[workspaces]
roots = ["~/sites", "~/tools", "~/dev"]
depth = 2
ignore = ["archive"]
```

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct WorkspaceSettings { pub roots: Vec<String>, pub depth: u8, pub ignore: Vec<String>,
                               pub include_hidden: Vec<String>, pub scan_interval_secs: u32, pub suggest: bool }
```

Validation (errors carry file:line:col from `toml`'s span, converted to line/column):
`tld` matches `^[a-z][a-z0-9-]{0,62}$` after stripping `.`, is not one of `com net org io dev app ai co`,
warn on `local` (mDNS); `ports`: `1024 ≤ start < end ≤ 65535`, range ≥ 16 wide, must not contain
`daemon.port`; `workspaces.roots`: `~` expanded, must be absolute, `/` rejected, `$HOME` itself warns;
`depth` 1..=5.

**Failure policy.** A syntax/type error in `config.toml` at **startup is fatal**: exit status 78
(`EX_CONFIG`), message `~/.config/lantern/config.toml:4:9: roots: expected an array of strings`.
(13-system should set `RestartPreventExitStatus=78` on the unit.) Silent defaults hide typos (v1's
`merge_from_file` ignored bad JSON). On a **reload** (`SIGHUP` or `POST /api/settings/reload`)
errors keep the old settings and surface as a `SystemHealth` warning. Missing file = defaults;
the daemon never creates `config.toml` until a setting is changed through the API. Distribution:
`tokio::sync::watch<Arc<Settings>>`.

### 3.9 `state.json`

> **Reconciled → 02 D4:** the authoritative schema is the merged one in `02-decisions.md` D4: top-level `repos`; `projects[].repo {common_dir, subpath}` instead of `subdir` (D5); checkout records (11's full `CheckoutRecord`) live under `instances[id].checkout`; `instances[id]` also carries `pinned`, `adopt`, `activated`, `first_started`, `last_started`; the `adopted[]` array is **dropped**. Everything else in this section (versioning, write path, recovery) stands.

Daemon-only, versioned, mode `0600`. Maps keyed by id for `jq`-friendly diffs. Ownership per
section: `projects`/`checkouts` → registry, `instances.last_runtime` → runtime via registry,
`port_leases` → `ports/`, `adopted` → `discovery/`.

```json
{
  "version": 1,
  "saved_at": "2026-11-06T12:00:00Z",
  "active_profile": "light",
  "projects": {
    "myapp": {
      "id": "myapp", "aliases": ["MyApp"], "root": "/home/u/sites/myapp", "subdir": null,
      "source": "manifest", "registered_at": "2026-10-01T09:00:00Z", "last_seen_at": "2026-11-06T11:59:00Z",
      "missing": false,
      "overrides": { "run": { "cmd": "pnpm dev --port ${PORT}" } },
      "checkouts": [
        { "path": "/home/u/sites/myapp", "kind": "primary", "instance": "myapp" },
        { "path": "/home/u/.local/share/lantern/worktrees/myapp/feature-x", "kind": "managed",
          "branch": "feature/x", "instance": "myapp@feature-x", "created_at": "2026-11-05T10:00:00Z" }
      ]
    }
  },
  "instances": {
    "myapp": { "pinned": false, "last_runtime": { "status": "Running", "port": 41003, "unit": "lantern-inst-myapp",
        "main_pid": 4242, "started_at": "2026-11-06T08:00:00Z", "last_exit": null, "restarts": 0, "message": null } },
    "myapp@feature-x": { "pinned": true, "last_runtime": null }
  },
  "port_leases": { "myapp": { "port": 41003, "leased_at": "2026-10-01T09:05:00Z" } },
  "adopted": [ { "instance": "myapp", "port": 5173, "pid": 9911, "pid_start": 123456789,
                 "exe": "/usr/bin/node", "cwd": "/home/u/sites/myapp", "adopted_at": "2026-11-06T10:00:00Z" } ],
  "dismissed_suggestions": ["/home/u/dev/scratch"],
  "imports": { "v1": { "at": "2026-11-06T12:00:00Z", "from": "/home/u/.config/lantern", "projects": 7 } }
}
```

```rust
pub struct StateFile {
    pub version: u32, pub saved_at: Timestamp, pub active_profile: Option<String>,
    pub projects: BTreeMap<ProjectId, ProjectRecord>,
    pub instances: BTreeMap<InstanceId, InstanceRecord>,
    pub port_leases: BTreeMap<InstanceId, PortLease>,
    pub adopted: Vec<AdoptedRecord>,
    pub dismissed_suggestions: BTreeSet<PathBuf>,
    pub imports: BTreeMap<String, ImportRecord>,
}
pub struct ProjectRecord {
    pub id: ProjectId, pub aliases: Vec<String>,        // legacy names (v1 dir names/ids) accepted on lookup
    pub root: PathBuf,                                   // canonical
    pub subdir: Option<PathBuf>,                         // root relative to git toplevel when the project is a monorepo subdir
    pub source: ProjectSource,                           // manifest | detected | manual
    pub registered_at: Timestamp, pub last_seen_at: Timestamp, pub missing: bool,
    #[serde(skip_serializing_if = "RawManifest::is_empty")] pub overrides: RawManifest,
    pub checkouts: Vec<CheckoutRecord>,                  // persisted so InstanceIds stay stable
}
pub struct CheckoutRecord { pub path: PathBuf, pub kind: CheckoutKind, pub branch: Option<String>,
                            pub instance: InstanceId, pub created_at: Option<Timestamp> }
pub struct InstanceRecord { pub pinned: bool, pub last_runtime: Option<LastRuntime> }
pub struct LastRuntime { pub status: InstanceStatus, pub port: Option<u16>, pub unit: Option<String>,
                         pub main_pid: Option<u32>, pub started_at: Option<Timestamp>,
                         pub last_exit: Option<ExitInfo>, pub restarts: u32, pub message: Option<String> }
```

Not persisted: listeners, logs, health (recomputed), service state (systemd is truth), resolved
manifests (re-derived), intel caches (15 owns `$XDG_STATE_HOME/lantern/intel-cache.json`, → 02 D15). `last_runtime` is a
hint for reconcile; **reality wins**. Timestamps are RFC 3339 UTC.

**Versioning and migration.** `version` is a `u32`; `STATE_VERSION = 1`. Loading goes through
`serde_json::Value`: `fn migrate(v: Value) -> Result<Value>` applies `m1_to_2`, `m2_to_3`… in
sequence, each a pure function with a golden fixture (`tests/fixtures/state/v1.json`). Before the
first write after a migration the old file is copied to `state.json.pre-migrate-v<N>`. A file with
`version` **greater** than supported is never touched: the daemon refuses to start with
`state.json is version 3; this lantern supports up to 2. Upgrade lantern, or restore state.json.bak`.

**Write path** (`registry/state.rs`, one writer task): mutations mark the state dirty. Trailing
debounce **250 ms** with a **2 s max-wait**; `flush().await` bypasses the debounce and is used
after register/unregister, port-lease changes, and at shutdown. Each write: serialise → create
`state.json.tmp` (mode 0600) in the same directory → `write_all` → `sync_all` → if current
`state.json` parses and `.bak` is older than 10 minutes, copy it to `state.json.bak` → `rename` over
`state.json` → fsync the directory. A `state.lock` file holds `File::try_lock` (std, stable since
1.89) for the daemon's lifetime, so a second daemon fails fast. Write errors (disk full, read-only)
never crash: keep state in memory, retry with backoff, raise a `SystemHealth` warning.

**Corruption recovery** at load:

1. Parse `state.json`. If it fails (syntax, wrong shape): rename it to `state.json.corrupt-<ts>`.
2. Try `state.json.bak`. If it parses, use it and warn: `state.json was corrupt; restored from
   state.json.bak (changes from the last ≥10 minutes may be lost)`.
3. Otherwise start empty, run a workspace scan (restores every manifest project), and let reconcile
   re-adopt running `lantern-inst-*` units. Assumption on 10-runtime: units carry
   `LANTERN_INSTANCE_ID` and `LANTERN_CHECKOUT` in their environment/description so this works.
   Manual/detected-only projects and overrides are lost; the warning says so and names the
   `.corrupt-<ts>` file for manual salvage.

### 3.10 Workspace scanning and suggestions

```rust
pub struct ScanReport { pub manifests: Vec<ManifestHit>, pub suggestions: Vec<Suggestion>,
                        pub skipped: Vec<Skipped>, pub truncated: bool }
```

`scan_roots(&WorkspaceSettings) -> ScanReport` (sync, bounded: ≤ 5 000 directories per root and a
2 s budget, then `truncated: true`). Breadth-first per root, using `read_dir` + `file_type()` (no
`stat` per entry). For a directory `D` at level ≤ `depth` (level 1 = child of a root):

1. Skip when: name in the ignore set (built-in: `node_modules vendor target dist build .next .nuxt
   __pycache__ venv .venv _build deps .cache` + `workspaces.ignore`); hidden unless listed in
   `include_hidden`; it is `$XDG_DATA_HOME/lantern/worktrees`; or `.git` is a **file** (a linked git
   worktree: a checkout, not a project; reason `linked_worktree`). Symlinks to directories are
   followed with a visited set of canonical paths (loops are skipped).
2. If `D` has a manifest → `ManifestHit`, **do not descend** (a project's sub-folders are not
   projects).
3. Else run cheap detection; if `D` is a git root or has a framework marker and detection is ≥
   `Medium` (or `Low` for a git root) → `Suggestion`; do not descend into git roots.
4. Else descend while level < `depth`.

Registry actions after a scan: new `ManifestHit` → auto-register (`source: Manifest`, v1 behaviour);
two projects deriving the same id → second gets a suffix + warning `duplicate_id` naming both
paths; a registered project whose root vanished but whose `id` shows up at a new path → relocate
(`last_seen_at`, info), otherwise flag `missing: true` (hidden from default listings, **never
auto-deleted**: overrides are valuable); a `Detected` project that gains a `lantern.yaml` becomes
`source: Manifest`. Triggers: startup, every `scan_interval_secs`, `POST /api/projects/scan`,
roots changed.

**Suggestions** (detected, not manifested):

```rust
pub struct Suggestion {
    pub path: PathBuf, pub proposed_id: ProjectId, pub framework: Framework, pub confidence: Confidence,
    pub evidence: Vec<String>, pub origin: SuggestionOrigin,   // Scan{root} | Listener{port, pid, cmdline}
    pub starter_manifest: String, pub seen_at: Timestamp,
}
```

They live in memory in `registry/suggestions.rs` (deduped by canonical path, skipped when
registered or in `dismissed_suggestions`) and are exposed as `GET /api/suggestions`, with actions:
**add** (`POST /api/projects {path}` registers with `source: Detected`, nothing written to the repo;
`write_manifest: true` also writes the file), **dismiss** (persisted). The starter manifest is
generated from a string template (keeps comments), e.g.

```yaml
# yaml-language-server: $schema=<SCHEMA_URL>
# Generated by Lantern from detected framework: nextjs (pnpm). Edit freely.
id: myapp
run:
  cmd: pnpm exec next dev -H ${HOST} -p ${PORT}
```

Written atomically, never overwriting an existing file, never `git add`-ed. Values that need quoting
are emitted as JSON strings (valid YAML double-quoted scalars). Suggested CLI for 16-clients:
`lantern add [path] [--write-manifest]`, `lantern manifest check [path]` (pure `lantern-core`
validation, works without the daemon), `lantern schema`.

**Hook for 12-discovery.** A listener whose cwd is in no known checkout calls
`registry.offer_suggestion(cwd, Listener{port, pid, cmdline})`, which uses
`lantern_core::scan::project_root_for(cwd) -> Option<PathBuf>`: walk up from `cwd` to the nearest
directory with a manifest, else the nearest with a `.git` entry, else the nearest with
`package.json`/`pyproject.toml`/`composer.json`; path components in the ignore set are skipped (a cwd of
`.../node_modules/.bin` resolves above it); never returns `$HOME`, `/`, or anything outside `$HOME`.
Then `suggest_for(root, origin)` runs detection. Re-evaluated at most once per root per 10 minutes.

### 3.11 Profiles

v1 semantics, made real. A profile is `{services, auto_start, env}` (+ optional port range). File
`~/.config/lantern/profiles/<name>.yaml`; name = file stem, `^[a-z0-9][a-z0-9_-]{0,47}$` (v1 name
`full_stack` stays valid).

```yaml
# yaml-language-server: $schema=<PROFILE_SCHEMA_URL>
description: Redis + Postgres + Mailpit, all my projects
services: [redis, postgres, mailpit]     # service names known to 13-system
auto_start:                              # v1: auto_start_projects (accepted as an alias)
  - myapp                                # ProjectId -> its primary instance
  - myapp@feature-x                      # or an InstanceId
  # - "*"                                # every enabled project that has a start command (v1 "demo")
env:                                     # overlay on instances started while active
  APP_ENV: demo
port_range: [29900, 29999]               # optional; new leases only (v1 `demo`)
stop_others: false                       # true = also stop services not listed
```

Builtins (embedded, read-only, shadowable): `light` (`services: []`), `full_stack`
(`redis, postgres, mailpit`), `demo` (same services, `auto_start: ["*"]`, `port_range: [29900, 29999]`).

Activation (`POST /api/profiles/<name>/activate`; the registry orchestrates, others execute):
set `state.active_profile`; ensure listed `services` are running (unlisted are left alone unless
`stop_others`); start `auto_start` instances that are not running (**never stops anything**); the
`env` overlay applies only to instances started afterwards, so the response lists
`restart_required: [ids]` for running ones. Final process env precedence: system < Lantern-injected
(`PORT`…) < template/manifest `run.env` < `worktree.env` < profile `env`; `PORT`/`HOST` in profile
`env` are rejected (`reserved_env`). Unknown project/service names produce a warning list, not a
failure. **Daemon boot:** after reconcile, `Registry::boot_targets()` returns
`{projects with features.auto_start} ∪ active_profile.auto_start`, minus already-running; 10-runtime
starts them. v1 started *everything*; v2 starts only what is asked for. Diagnostics follow the manifest format.

### 3.12 `lantern migrate-v1`

The CLI never touches files: `lantern migrate-v1 [--from DIR] [--dry-run] [--yes] [--force]` calls
`POST /api/import/v1 {from, dry_run, force}`; the daemon (running as the user) reads the v1 files.
Source search order: `--from`, `$LANTERN_V1_DIR`, `$LANTERN_STATE_DIR` if it holds v1 files,
`~/.config/lantern`. v1 files are **only read**. Default flow: dry-run, print the plan, ask,
apply.

v1 inputs: `settings.json`, `state.json` (`projects` map name→project, `port_assignments`,
`service_states`, `health_results`), `profiles/*.json`, `active_profile` (text), `templates/*.yml`.

| v1 | → v2 |
|---|---|
| `settings.json` `workspace_roots tld php_fpm_socket default_template port_range_*` | `config.toml` `workspaces.roots domains.tld php.fpm_socket templates.default ports.range_*` (`.glow`→`glow`). Only keys **absent** from an existing `config.toml` are added (via `toml_edit`). `caddy_mode`, `state_dir` dropped. |
| `settings.json` / `active_profile` file | `state.active_profile` (only if that profile exists) |
| `profiles/<n>.json` `{name, description, services, auto_start_projects, env}` | `profiles/<n>.yaml` (`auto_start_projects`→`auto_start`) |
| `templates/<n>.yml` | re-parsed and re-saved as `templates/<n>.yaml`; unparsable files (v1's writer emitted invalid YAML for values with quotes) are skipped with a warning |
| `state.json` project, **has a manifest at `path`** | `ProjectRecord{source: manifest}`; v1's state copy is diffed against the parsed manifest over `run.* type domain root features routing deploy depends_on tags enabled upstream_url base_url health_endpoint description kind`; differences (v1 GUI edits) become `overrides`. A `domain` equal to `<name><tld>` is dropped as default. |
| `state.json` project, no manifest (API-registered) | `source: manual`, all fields → `overrides` |
| project `path` relative (e.g. `"."`), `detection.source == "self"`, or missing on disk | skipped with a reason (v1's self-registered `lantern` entry is one) |
| v1 key (dir name / `id`) | `ProjectId` = `slugify(manifest id ?? v1 id ?? name)`, collisions suffixed; original v1 name(s) kept in `aliases` so MCP calls using v1 names still resolve |
| `port_assignments`, `service_states`, `health_results`, `status`, `pid`, `port` | **not imported** (ephemeral; v1 children died with the v1 daemon; anything still listening shows up via discovery) |

Idempotent by construction: projects are keyed by canonical `root`; a root already in v2 state is
`AlreadyPresent` and left untouched (`--force` re-merges overrides only); profiles/templates are
copied only if the target is absent or byte-identical; the run is recorded in `imports.v1`. Running
it twice yields "0 added, N already present". The response is an `ImportPlan{actions: [{kind:
Added|AlreadyPresent|Updated|Skipped, what, reason}], warnings}`. System-level v1 cleanup (system
unit `lanternd`, sudoers, dnsmasq, `/etc/caddy/sites.d`) belongs to `13-system`.

---

## 4. Interfaces

**`lantern-core` exports** (sync, no tokio; `std::fs` only inside `manifest::load`, `detect`, `scan`,
`settings::load`; the daemon calls them via `spawn_blocking`):

```rust
// ids / dirs
pub fn slugify(s: &str, max: usize) -> String;  ProjectId::parse(&str); Dirs::from_env(&dyn EnvSource) -> Dirs
// manifest
pub fn find_manifest(dir: &Path) -> Option<PathBuf>;
pub fn load_manifest(dir: &Path) -> io::Result<Option<ParsedManifest>>;
pub fn parse_manifest(src: &str, file: &Path) -> ParsedManifest;
pub fn resolve(input: ResolveInput) -> ResolvedManifest;
pub fn manifest_json_schema() -> serde_json::Value;       // + profile_json_schema(), settings_json_schema()
// settings
pub fn load_settings(dirs: &Dirs, env: &dyn EnvSource) -> Result<LoadedSettings, SettingsError>;  // value + warnings + env-overridden keys
pub fn patch_settings_file(path: &Path, patch: &SettingsPatch) -> Result<(), SettingsError>;     // toml_edit
// detection / templates / profiles / scanning
pub fn detect_dir(dir: &Path) -> Detection;
pub fn builtin_templates() -> Vec<Template>;  pub fn parse_template(name: &str, src: &str) -> Result<Template, Vec<Diagnostic>>;
pub fn parse_profile(name: &str, src: &str) -> Result<Profile, Vec<Diagnostic>>;
pub fn scan_roots(s: &WorkspaceSettings) -> ScanReport;   pub fn project_root_for(cwd: &Path) -> Option<PathBuf>;
pub fn suggest_for(root: &Path, origin: SuggestionOrigin) -> Option<Suggestion>;
```

**`registry/` persistence (daemon).**

```rust
pub trait StateStore: Send + Sync {
    fn load(&self) -> Result<LoadOutcome, StateError>;   // Loaded | RestoredFromBackup{reason} | Fresh | NeedsV1Migration
    fn save(&self, s: &StateFile) -> Result<(), StateError>;   // atomic sequence of §3.9; called only by the writer task
}
pub struct StateWriter { /* mpsc to the writer task */ }
impl StateWriter { pub fn mark_dirty(&self, snapshot: Arc<StateFile>); pub async fn flush(&self) -> Result<(), StateError>; }
pub struct SettingsHandle { /* watch::Receiver<Arc<Settings>> */ }   // current(), patch(), reload()
pub trait TemplateStore { list/get/put/delete/fork }                 // user dir + builtins
pub trait ProfileStore  { list/get/put/delete, active(), set_active(name) }
pub struct ManifestCache; // get(&InstanceId) -> Arc<ResolvedManifest>, invalidate(&ProjectId)
pub struct SuggestionStore; // offer(cwd, origin), list(), dismiss(path)
```

**Assumed from other subsystems.** `11-instances`: provides checkout paths/kinds/branches per
project and decides `InstanceId` collision suffixes (I persist them); computes
`<worktree>/<subdir>` for monorepo subdir projects; consumes `setup`, `worktree.*`, `VarContext`.
`10-runtime`: consumes `ResolvedManifest.run/deploy/depends_on/health_endpoint`, builds `VarContext`
(`PORT` leased before `setup`), owns env injection and `PORT` handling, treats `Php`/`Static` as
route-only, names units so reconcile can recover (§3.9). `12-discovery`: calls
`offer_suggestion`. `13-system`: reads `domains.tld`, `php.fpm_socket`, `serve`, `root`,
`routing`, `upstream_url`, `domain`; sets `RestartPreventExitStatus=78`. `15-api-mcp`: routes for
settings/templates/profiles/suggestions/import/schema/`ManifestReport {resolved, provenance,
diagnostics, files}`; MCP name lookups go through `aliases`. `16-clients`: renders diagnostics,
`lantern manifest check/add/schema/migrate-v1`.

---

## 5. Failure modes and edge cases

| Situation | Behavior |
|---|---|
| Manifest unreadable (EACCES), not UTF-8, > 1 MiB | `Error` diagnostic with path and reason; instance `NeedsConfig`. |
| Manifest saved half-written by an editor | Parse error → stopped instance `NeedsConfig`; **running** instance keeps its last-known-good manifest (§3.6). Fixes itself on the next stat-tick. |
| `lantern.yaml` and `lantern.yml` both exist | `.yaml` wins, `both_manifest_files` warning. |
| Tabs in indentation, bad anchors, duplicate keys | `yaml_syntax`/`duplicate_key` with line:col and help text. |
| YAML anchor bomb / 10 000-deep nesting | `yaml_limits` (budget), no memory blowup. |
| v1 manifest with `type: self`, `kind: library`, `${PATH}`, extra keys | Loads; warnings only. |
| Manifest `id` differs from registered id | Registered id wins; `id_changed` warning suggests `lantern project rename`. |
| Two projects want the same id | Second suffixed `-2`; warning names both paths. |
| Project root deleted / drive unmounted | `missing: true`, instance `NeedsConfig: path not found`; reappears automatically; never auto-deleted. |
| Branch switch inside the primary changes the manifest | Detected by mtime stamp; running instance unaffected until restart. |
| Checkout manifest invalid | No fallback to primary (§3.6 rule 2). |
| Template deleted but referenced | `unknown_template`, continue with detection. |
| Profile names a missing project/service | Activation succeeds; warnings list the unknowns. |
| `config.toml` invalid at boot / at reload | Exit 78 with message / keep old settings + health warning. |
| Env override makes a GUI setting read-only | API returns `overridden_by_env`; writes to the file still allowed, effective value unchanged. |
| TLD changed with running instances | Domains recomputed; 13-system rebuilds routes; running processes keep their old `${DOMAIN}` env until restarted (`restart_required`). |
| `state.json` corrupt / newer / v1 file / read-only dir / second daemon | §3.9: recovery chain / refuse to start / `migrate-v1` hint / in-memory + health warning / lock failure message. |
| Scan on a huge or looping tree | Depth/dir/time budget, `truncated: true`; symlink loops skipped. |
| Listener cwd is `/`, `$HOME`, `/usr/...` | No suggestion. |
| Monorepo: manifest in `packages/web/` of a larger repo | Project root is the subdir; `repo.subpath` stored for worktree path mapping (→ 02 D5); scan stops at the first manifest. |
| Windows line endings, BOM | Accepted; BOM stripped. |
| `migrate-v1` re-run, partial failure | Per-item results; already-imported items untouched; safe to re-run. |

---

## 6. Security considerations

- **A manifest is code.** `run.cmd`, `setup`, `deploy.*` execute as the user. Scanning, parsing,
  resolving, suggesting, and `manifest check` **never execute anything**. Execution happens only
  when a user/agent starts or prepares an instance. Worktrees of untrusted branches inherit that
  trust model; 2.0 has no trust-on-first-use (see §8).
- **Path confinement.** `run.cwd`, `root`, `docs[]`, `worktree.copy/link`, `deploy.env_file` are
  validated as relative without escaping `..` (a cloned repo must not make the MCP `get_project_docs`
  tool return `../../.ssh/id_rsa`; v1 did not check). Consumers must still canonicalize and verify the
  prefix, because symlinks can escape.
- **Parser limits** (1 MiB, `Budget`) protect the daemon from hostile manifests.
- `docs_auto.sources` / `api_auto.sources` URLs are validated as `http(s)`; restricting fetches to
  loopback and the project's own domain is `15-api-mcp`'s job (SSRF).
- `state.json` mode 0600 (it contains paths and PIDs); state dir 0700. Writes use same-directory
  tmp + rename so a partial write is never visible.
- Settings API rejects `workspaces.roots` containing `/`. `migrate-v1` only reads, canonicalizes
  `--from`, and caps each input at 16 MiB.
- Templates/profile names are validated against a strict regex before becoming filenames
  (no traversal).

---

## 7. Testing strategy

- **Golden/unit:** repo-root `lantern.yaml` and `lantern.yaml.example` plus every v1 test input
  from `daemon/test/lantern/config/lantern_yml_test.exs` as fixtures: must load with no errors.
  `insta` snapshots of rendered diagnostics (file:line:col must be exact). Table tests for the
  `${…}` grammar (escape, known, unknown, `$$`, `${X:-y}`, aliases, typo lint). Merge laws
  (`merge(x, empty) == x`, associativity) with `proptest`. Resolution tests for every precedence
  layer and rule in §3.6.
- **Fuzz:** `cargo fuzz` target on `parse_manifest` and `TemplateString::parse` (never panic, always
  terminates).
- **Schema:** committed `schema/*.json` equals regenerated output; every fixture validates against
  it with the `jsonschema` crate.
- **Detection:** fixture directory trees built in a tempdir per case (each row of §3.7, each
  lockfile, monorepo parent lockfile, FastAPI entrypoint variants).
- **State:** crash simulation (truncate `.tmp`, corrupt main, corrupt both, newer version, v1 file
  at v2 path), migration chain with golden files, debounce coalescing with `tokio::time::pause`,
  `try_lock` contention.
- **Settings:** precedence default < file < env, TOML error spans, `toml_edit` round-trip keeps
  comments, env-overridden reporting.
- **Scanner:** tempdir trees: stop-at-manifest, linked-worktree skip (`.git` file), symlink loop,
  hidden/ignored dirs, depth, budget truncation.
- **migrate-v1:** synthetic v1 directory (settings.json, state.json produced to match Elixir
  `Project.to_map`, profiles, templates incl. one with invalid YAML); run twice and assert the second
  run changes nothing; GUI-edit-diff becomes overrides.
- **Manual:** run against the real `~/sites`/`~/tools`; open a manifest in VS Code to confirm
  autocomplete from the modeline; edit manifests while an instance runs.

---

## 8. Open questions (with recommendations)

1. **Does `Spanned<Node>` keep nested positions through a custom `Visitor` in serde-saphyr 2.1.x?**
   Recommend: do spike S1 before anything else; fallback is a `yaml-rust2` event-based tree builder
   behind the same `Node`/`PosIndex` types.
2. **Schema hosting URL.** The public repo path is unknown (v1 lived at `dashw00d/lantern`).
   Recommend: `SCHEMA_URL` constant pointing at the raw GitHub file now; SchemaStore submission
   once the repo is public.
3. **Execute `deploy.install` / `run.pre_start` / `run.post_stop`?** Recommend no: keep them as
   `deprecated_key`/`ignored_key` warnings; `setup` covers one-time prep. Revisit if 10-runtime wants hooks.
4. **Trust model for manifests from untrusted branches** (TOFU on a hash of `run/setup/deploy`).
   Recommend: not in 2.0; document the model, and store `exec_fingerprint` per instance later.
5. **Default workspace roots.** Recommend v1's `~/sites`, `~/tools` (missing roots are fine) plus a
   first-run GUI prompt to add `~/dev`-style folders.
6. **Default PHP serving for detected Laravel/Symfony.** Recommend keeping v1's php-fpm route
   (`type: php`) with the `laravel-serve` template one click away, since php-fpm is closer to
   Valet-style production behaviour but needs a system package.
7. **Should GUI edits ever write back to `lantern.yaml`?** Recommend no: edits are overrides in
   state; "Export to lantern.yaml" (only when no file exists) is the one write path.
8. **`config.toml` fatal at boot?** Recommend yes (exit 78); revisit if users report lock-outs.
9. **`exec`-style `run.cmd` (argv list).** Recommend not now; shell string only, as v1.

---

## 9. Proposed changes to core architecture

1. **§6 template variables.** Replace "Unknown variables are a manifest validation error" with:
   *only the known names (and deprecated aliases `NAME`, `PATH`) are substituted; any other
   `${…}`/`$…` is passed to the shell untouched; `$${` is the escape; near-miss names produce a
   `possible_var_typo` warning.* Reason: `${HOME}` in `run.cmd` worked in v1 and still must.
   Also add `${BRANCH_SNAKE}` (slug with `_`) for database names, and specify `${INSTANCE}` as
   `@`→`--`.
2. **§6 `Instance.manifest`.** `ResolvedManifest` can carry large `docs`/`endpoints` lists and every
   `InstanceUpdated` event is a full snapshot. Make it `Arc<ResolvedManifest>` internally and
   serialise a `ManifestSummary` (id, serve, run.cmd, diagnostics counts, `manifest_rev`) in events;
   full manifest at `GET /api/instances/:id/manifest`.
3. **§6 domain model.** Add `ServeMode { Proxy, Php, Static, Unknown }` to the resolved manifest
   and state that `Php`/`Static` instances are route-only (no unit).
4. **§6 `Project`.** Add `aliases: Vec<String>` (legacy v1 names, accepted on lookup so v1 MCP calls
   keep working) and `subdir: Option<PathBuf>` (monorepo projects).
5. **§9 events / §10 resources.** Add `Event::SuggestionsChanged { suggestions }` and
   `/api/suggestions`; add `/api/import/v1` and `/api/schema/<name>`.
6. **§7 persistence.** Add `state.json.bak`, `state.lock`, `state.json.corrupt-*`; `active_profile`
   lives in `state.json`; profile/template files accept `.yaml` (and `.yml` for templates); built-in
   templates/profiles are embedded from `crates/lantern-core/builtin/`; add `schema/` to the repo
   layout in §4; document `LANTERN_CONFIG_DIR`/`LANTERN_STATE_DIR`/`LANTERN_DATA_DIR`
   (`LANTERN_STATE_DIR` changes meaning from v1: state only).
7. **§3 process model.** `lantern init` stays system setup; project-level commands are
   `lantern add`, `lantern manifest check`, `lantern schema`, `lantern migrate-v1` (16-clients to
   confirm). The daemon unit should use `RestartPreventExitStatus=78`.
8. **§5 YAML choice.** Resolved here: `serde-saphyr` (pinned exactly), isolated in `lantern_core::yaml`;
   add `schemars`, `serde_path_to_error`, `serde_ignored`, `globset`, `toml_edit` to the baseline list.
