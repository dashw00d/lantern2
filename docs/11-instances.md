# 11 — Checkouts & Instances (branches, worktrees)

Status: draft for review. Builds on `01-architecture.md` (authoritative). Terms (Project,
Checkout, Instance, Primary instance, Manifest, InstanceId, `Preparing`, `pinned`) are used
exactly as defined there. v1 paths are relative to `.reference/lantern-v1/`.

## 0. Plain-language summary

Lantern v2 can run **any branch of a project next to the main one**, each with its own port
and its own HTTPS name: `myapp.glow` is the main checkout, `feature-x.myapp.glow` is a
second copy of the code at branch `feature-x`.

How it works, in six points:

1. Every project that lives in a git repo is mapped to its repo. Lantern asks git for the repo's
   worktree list (`git worktree list --porcelain -z`) and turns every worktree into an
   **instance** of the project. Most are hidden until you run or pin them.
2. "Run branch X" means: reuse a checkout that already has X, or have git create a new
   worktree for X under `~/.local/share/lantern/worktrees/` (a **managed** worktree).
3. Before a non-primary instance first starts, Lantern **prepares** it: copy `.env`-style files from
   the main checkout, symlink big untracked folders, then run the project's `setup:` commands
   (or a sensible default such as `pnpm install --frozen-lockfile`). Progress streams to the
   GUI/CLI and the whole thing can be cancelled and retried.
4. The instance belongs to the **checkout (directory)**, not to the branch name. Its id and
   domain are fixed when you start or pin it and do not change if you later `git switch`
   inside that directory.
5. Removing a managed worktree refuses if there are uncommitted changes or unpushed commits
   unless you force it. Lantern never deletes a worktree directory you made yourself, and
   never deletes anything automatically; it only suggests clean-ups.
6. Monorepos work: a project may live in `repo/apps/web`; every worktree of the repo offers an
   instance for every project that exists in that worktree.

New manifest keys (semantics owned here, schema in `14-config.md`): `setup:` and
`worktree: { copy, link, env }`.

---

## 1. Purpose & scope

**In scope**

- Mapping projects to git repos and checkouts; checkout discovery and refresh.
- Deriving Instances from checkouts: kinds, visibility (`pinned`), `InstanceId` and domain
  assignment and stability, slug collisions.
- "Run branch X": resolve/create managed worktrees (local, remote-only, new branch, already
  checked out elsewhere, primary on that branch, optional PR).
- The Preparing pipeline (create → copy/link → setup → hand-off), progress, cancel, retry.
- Dependency-install defaults per ecosystem and the re-run rule (lockfile hash).
- Per-branch isolation helpers (`worktree.env`, `${BRANCH_SLUG}`), removal, GC, git edge cases.
- Semantics of manifest keys `setup` and `worktree.{copy,link,env}`.
- The `git/` module (daemon) and the checkout/instance operations of the registry.

**Not in scope** (owner in parentheses)

- Starting/stopping processes, port allocation, readiness, logs, env assembly of `run.*`
  (`10-runtime.md`). This doc only produces a prepared checkout plus the extra env layer.
- Matching listening sockets to checkouts (`12-discovery.md`). This doc exports a
  `CheckoutIndex` (§4.3).
- Caddy routes, DNS, certificates (`13-system.md`). This doc only assigns `Instance.domain`
  (§3.8) and states the contract (§3.14).
- YAML schema/validation, `config.toml`, `state.json` file format, workspace scanning
  (`14-config.md`). This doc specifies the *records* and *settings* it needs.
- REST/MCP/CLI/GUI shapes (`15`, `16`). §3.16 lists the service methods they call.

---

## 2. v1 behavior audit

v1 has **no git awareness at all**. A repo-wide grep finds `git` only in docs-ignore globs
(`daemon/lib/lantern/projects/discovery.ex:20`) and an opaque `repo_url` metadata field
(`projects/project.ex:29`). Everything below is therefore "what v1 does for projects" and
what that implies for checkouts.

| v1 behavior | File | Verdict |
|---|---|---|
| Workspace scan: one level deep under `workspace_roots`, skips hidden dirs and `node_modules`, `vendor`, etc., requires `lantern.yaml`/`.yml` | `projects/scanner.ex:8,30-50` | **Keep** (owned by 14). **Add a rule**: a directory that is a *linked worktree* of an already-known repo must not become a project, because the committed `lantern.yaml` exists in every worktree and v1's scan would register each one as a duplicate project (§3.6). |
| Project identity = map key = directory basename (or manifest `id` if it matches an existing key); `Project.path` re-pointed when the same name is found elsewhere | `projects/manager.ex:548-585` (`do_scan`, L576-577) | **Change**: identity stays `ProjectId`, but one project now has many checkouts; "moved" handling becomes `relocate_project` (§3.16, §5) and runs `git worktree repair`. |
| Project dropped on load if its directory no longer exists | `manager.ex:1004-1025,1210-1214` | **Change**: a missing *primary* keeps the project (flagged `missing`) so its worktrees and pins survive an unmounted drive (§5). |
| One runner, one Caddy file and one port per project name | `projects/port_allocator.ex:21`, `system/caddy.ex:111`, `process_runner.ex` | **Replace**: key everything by `InstanceId`. |
| Monorepo support: none. A subproject is just a directory with its own manifest registered by path (`normalize_path` defaults to `"."`) | `lantern_web/controllers/project_controller.ex:832-839` | **Extend**: project root may be a subdirectory; subpath is carried into every worktree (§3.7). |
| Package-manager detection by lockfile: bun > pnpm > yarn > npm | `projects/detector.ex:81-98` | **Keep and extend** into the default-setup table (§3.11), plus `packageManager` field, Python and PHP. |
| Run `cwd` is `project.path` joined with `run.cwd`; PHP/static Caddy roots are `project.path` + `root` | `process_runner.ex:173-175`, `system/caddy.ex:52-64,76-87` | **Keep semantics, change the base**: base is the *instance's* `checkout.path`. 13/10 must never use `project.root` for a non-primary instance. |
| Variables `${PORT} ${DOMAIN} ${NAME} ${PATH}` substituted textually into the command string | `projects/project.ex:152-158`, `config/lantern_yml.ex` `interpolate` | **Change** for shell commands: v2 exports variables as environment variables and lets the shell expand `${VAR}`; textual substitution is unsafe once `${BRANCH}` exists (a branch may be named `x$(cmd)`). See §6. |
| `run.pre_start` / `run.post_stop` are parsed but never executed anywhere | `config/lantern_yml.ex:159-160` | **Note**: `setup:` is a different thing (once per checkout, not per start). `pre_start` is 10's call. |
| Boot auto-activates every enabled project with a run command | `manager.ex:428-440,591-609` | **Change**: auto-start applies to primary instances only; a profile may name non-primary `InstanceId`s explicitly. |
| `kill -TERM -<pgid>` of a `setsid` shell; `/bin/sh -lc` | `process_runner.ex:277-279` | Informational; setup commands reuse 10's one-shot runner (same shell and toolchain environment as `run.cmd`). |

---

## 3. Design

### 3.1 Mental model

```
 repo: ~/sites/shop/.git  (common dir)            projects registered from this repo
 worktrees reported by git:                        web : subpath "apps/web"
   [P] ~/sites/shop                      main      api : subpath "apps/api"
   [W] ~/work/shop-hotfix                hotfix/urgent        (made by the user)
   [M] ~/.local/share/lantern/worktrees/web/feature-x   feature/x   (made by Lantern)

 one worktree  x  one project  =  one Instance   (only if <worktree>/<subpath> exists)

   project   primary        [W] hotfix                [M] feature-x
   web       web            web@hotfix-urgent         web@feature-x   (pinned: requested)
   api       api            api@hotfix-urgent         api@feature-x   (hidden: sibling project)
   web domains web.glow     hotfix-urgent.web.glow    feature-x.web.glow

 [P] primary   [W] user-made worktree   [M] managed worktree
```

Definitions used below:

- **Repo**: identified by its git *common dir* (`git rev-parse --path-format=absolute
  --git-common-dir`). All git commands for a repo use `git --git-dir=<common_dir> ...`, so they
  work even when the primary checkout directory is missing, and in bare repos.
- **Git root** of a checkout: the worktree top-level directory.
- **Subpath**: the project's directory relative to the git root (`""` for a normal project,
  `apps/web` in a monorepo). `Checkout.path` is always `<git_root>/<subpath>`.
- The **primary checkout** is the project's registered `root`, **not** git's "main worktree".
  If you registered `~/work/shop-hotfix` as the project, that is primary and git's main worktree
  is just another checkout. Normally they coincide.
- **Managed** = Lantern created it (kind `Managed`, lives under `worktrees_dir`). **Worktree** =
  any other linked worktree.

### 3.2 Persisted records

Everything derivable from git (branch, HEAD, lock state) is re-derived on boot and never trusted
from disk. What must persist (to keep ids/pins/stamps stable) goes into `state.json`; `14-config`
owns the file, these are the records it must carry. Unknown fields are ignored on load (forward
compatible); `state.json` is versioned per the core doc.

```rust
// lantern-core: serde types, persisted under state.json "repos" and "checkouts"
pub struct RepoRecord {
    pub common_dir: PathBuf,      // canonical; the repo key
    pub slug: String,             // dir name under worktrees_dir; = ProjectId of the first project
                                  // registered in this repo; never changes afterwards
}

pub enum CheckoutPhase { Creating, Ready }   // Creating = `git worktree add` not confirmed finished

pub struct CheckoutRecord {
    pub instance: InstanceId,             // frozen once claimed (see 3.8)
    pub project: ProjectId,
    pub path: PathBuf,                    // project dir inside the checkout (= Checkout.path)
    pub kind: CheckoutKind,               // Primary | Worktree | Managed
    pub phase: CheckoutPhase,             // always Ready for Primary/Worktree
    pub pinned: bool,
    pub created_at: Timestamp,
    pub created_branch: bool,             // Lantern ran `-b`; only then may it offer to delete the branch
    pub materialized: Vec<PathBuf>,       // relative paths created by copy:/link: (ignored by dirty check)
    pub setup: Option<SetupStamp>,        // None = never prepared
    pub first_started: Option<Timestamp>, // set by runtime on first successful start => "claimed"
    pub last_started: Option<Timestamp>,
    pub missing_since: Option<Timestamp>, // worktree no longer listed / path gone
}

pub struct SetupStamp {
    pub lock_hash: String,        // sha256 over lockfiles + version-pin files (see 3.11)
    pub commands_hash: String,    // sha256 over the resolved setup command list
    pub finished_at: Timestamp,
}
```

`claimed(record) = record.pinned || record.first_started.is_some()`.

### 3.3 Manifest keys (semantics owned here)

Names are fixed: `setup`, `worktree.copy`, `worktree.link`, `worktree.env`.

```yaml
# lantern.yaml (relative to the project directory)
setup:                      # list of shell commands
  - composer install --no-interaction
  - npm ci --prefer-offline --no-audit --no-fund
  - php artisan key:generate --force --no-interaction   # idempotent only

worktree:
  copy:                     # copied from the primary checkout, never overwrites
    - .env
    - .env.local
    - /.tool-versions       # leading "/" = relative to the git root, not the project dir
  link:                     # symlinked to the primary checkout's copy
    - storage/app/public
    - .cache/models
  env:                      # extra env for NON-primary instances only
    DB_DATABASE: myapp_${BRANCH_SLUG}
    APP_URL: ${URL}
```

#### `setup:` — list of strings

| Aspect | Rule |
|---|---|
| Type / default | List of non-empty strings. **Absent** key = use built-in defaults (§3.11). **`setup: []`** = run nothing, not even defaults. 14-config must keep "absent" and "empty" distinct (`Option<Vec<String>>`). |
| Execution | Commands run **sequentially**, each as its own `sh -c <string>` through 10-runtime's one-shot runner (same toolchain/PATH resolution as `run.cmd`). POSIX `sh` syntax is guaranteed; bashisms are not. `cd` does not persist between commands. |
| cwd | The instance's `checkout.path` (project dir in that checkout). Use `cd ../..` or an absolute `$CHECKOUT/..` in the command for repo-root work. |
| Failure | First non-zero exit aborts the pipeline; later commands do not run; no stamp is written; instance returns to `Stopped` with a message (§3.10). |
| When it runs automatically | **Non-primary instances only**: (a) before the first start (no stamp), (b) before a start if the **lock hash** changed since the last successful setup, (c) before a start if the **command list** changed (edited manifest or different branch manifest), (d) default setups only: before a start if the artifact directory (`node_modules`, `.venv`, `vendor`) is missing. |
| Primary instance | Never auto-run. If the primary's lock hash differs from its stamp, `Instance.setup_stale = true` and clients show "dependencies changed — run `lantern setup myapp`". Rationale: running installs on your main checkout after a `git pull` is the one place a surprise hurts. The primary's stamp is initialised to the current lock hash when the project is registered (it is assumed set up). |
| Manual | `lantern setup <instance>` re-runs it for any instance, including primary, and ignores the stamp. |
| Idempotency | Must be safe to run repeatedly. Lantern does not try to resume. |
| Timeout / parallelism | 15 min per command (`worktrees.setup_timeout_secs`); at most 2 setups run concurrently across the daemon (`worktrees.max_concurrent_setups`). |
| Environment | Base env as for `run.cmd`, plus `run.env`, plus (non-primary) the expanded `worktree.env`, plus the template variables **as env vars of the same name** (`PROJECT INSTANCE BRANCH BRANCH_SLUG CHECKOUT PRIMARY DOMAIN URL HOST`), plus `LANTERN_PHASE=setup`. `PORT` is **not** set (not allocated yet) and `${PORT}` in `setup` is a manifest validation error. |
| Substitution | **None.** The string is passed to `sh -c` untouched; `${BRANCH_SLUG}` works because the shell expands it from the environment. Validation only checks that plain `${NAME}` references are known names. Quote variables yourself (`"$CHECKOUT"`). |
| Output | Streamed as `LogLine` (`Stream::Setup`). Setup runs as a one-shot unit via `Runtime::run_oneshot`, so output lives in the user journal; **no `setup.log` file** (→ 02 D6). |

#### `worktree.copy:` — list of globs

- Resolved against the **primary checkout's project dir** (`${PRIMARY}`) and copied to the same
  relative path in the new checkout. A leading `/` means "relative to the git root" (for
  monorepo-wide `.env` files). Matches may be files or directories (copied recursively).
- **Never overwrites** an existing destination (it may be tracked or user-edited). Existing = skip, noted in the log.
- No match = **not an error** (`.env.local` often does not exist); noted in the log. A pattern
  with no wildcard that is missing is also just a note.
- Symlinks encountered (matched or inside a matched directory) are **skipped with a warning**; use
  `link:` for symlink semantics.
- Guard rails: no `..` components, no absolute paths, no `.git`; every source must canonicalize inside
  the repo's git root; walking skips `.git`, `node_modules`, `.venv`, `vendor`, `target` unless the
  pattern's literal prefix goes into them; limits: 500 files / 100 MiB total, else the step fails with
  "did you mean `link:`?".
- Runs for non-primary instances during prepare (§3.10) and on `lantern setup`; not on every start.
- Glob syntax: `globset` semantics with `literal_separator = true` (`*` does not cross `/`, `**` does).

#### `worktree.link:` — list of globs

- Same resolution and guard rails as `copy`, but creates a **symlink** at the destination pointing at the
  **absolute** path in the primary checkout. A matched directory is linked as a whole (no descent).
- Never replaces anything: if the destination exists (including a broken symlink; checked with
  `symlink_metadata`), skip. Missing source = note, not error. Parent directories are created.
- Intended for large *untracked* data (`storage/app/public`, model caches, fixtures). **Writes through
  the link modify the primary's files** — documented in the GUI tooltip and CLI help.
- Linked and copied relative paths are recorded in `CheckoutRecord.materialized` so an untracked
  symlink does not make the worktree look dirty (§3.13).

#### `worktree.env:` — map of string to string

- Applies **only to non-primary instances**, at setup time *and* at start time. Layering, lowest to
  highest: base env < `run.env` < `worktree.env`. Because dotenv-style loaders (phpdotenv, python-dotenv,
  Next.js, Vite) do not override already-set process variables by default, this beats a copied `.env`.
- Keys must match `[A-Za-z_][A-Za-z0-9_]*`. Values are **textually** expanded with the template variables
  (safe: values go straight into the process environment, never into a shell). `${PORT}` **is** allowed
  here and in `setup`: the port lease is taken when the instance record is created (→ 02 D1).
- Expansion happens at each start, so `${BRANCH}` reflects the branch at that moment, while
  `${BRANCH_SLUG}` is the instance's frozen slug (§3.8). On the primary instance `worktree.env` is ignored
  and `${BRANCH_SLUG}` is the slug of whatever branch is checked out now.
- `${BRANCH}` for a detached HEAD is the 7-char short sha (same as the slug).

#### Which manifest applies to which checkout

`ResolvedManifest` (core doc) is built per checkout by this rule:

1. `lantern.yaml`/`lantern.yml` in `checkout.path` exists and parses → use it (`origin: Checkout`).
2. Exists but fails to parse → instance status `NeedsConfig`, message = parse error. **No silent fallback**
   (a broken manifest on a branch is a real error).
3. Does not exist in the checkout → use the **primary's** (`origin: Primary`). This is also how people who
   keep `lantern.yaml` uncommitted get consistent behaviour in every worktree.
4. Neither → built-in detection (v1 `Detector` semantics, owned by 14), `origin: Detected`.

Project-level identity keys (`id`, `name`, `domain`, `kind`, `tags`, `enabled`) are **always read from the
primary's manifest**; if a branch's manifest differs it is logged as a warning and ignored. Everything
else (`run`, `setup`, `worktree`, `deploy`, `docs`, `endpoints`, `features`, `depends_on`...) comes from
the checkout's resolved manifest. Resolved manifests are cached by `(path, mtime, size)` and re-read at
every start and whenever the repo fingerprint changes (a branch switch can change the manifest).

### 3.4 Checkout discovery and refresh

**Command** (one per repo, not per project):

```
git --git-dir=<common_dir> worktree list --porcelain -z
```

Verified on git 2.53: with `-z` every attribute is terminated by NUL and each record by an extra NUL.
Attributes: `worktree <path>` (always first), `HEAD <sha>`, `branch refs/heads/<name>`, `detached`,
`bare`, `locked [<reason>]`, `prunable [<reason>]`. A bare repo's first record has `bare` and no
`HEAD`. Requires git >= 2.36 (`-z`); `lantern doctor` and daemon start check `git --version` and disable
worktree features with a clear message below that.

```rust
pub struct Worktree {
    pub path: PathBuf,              // from bytes (OsStrExt); non-UTF-8 paths are rejected later (see §5)
    pub head: Option<String>,       // None for bare
    pub branch: Option<String>,     // "feature/x" (refs/heads/ stripped); None if detached/bare
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,     // Some("") = locked without reason
    pub prunable: Option<String>,   // reason; path missing on disk
}
pub fn parse_worktree_list(raw: &[u8]) -> Result<Vec<Worktree>, ParseError>; // pure, fixture-tested
```

**Refresh strategy (decision): polling gated by a stat-only fingerprint, plus on-demand refresh. No
`notify`/inotify in v2.0.**

Rationale: (1) the core doc already specifies a periodic git watcher; (2) `git worktree list` costs
about 2 ms, and the fingerprint avoids even that when nothing changed; (3) inotify needs recursive watches
on `.git/worktrees/*`, re-arming when directories are replaced, debouncing, and inotify-limit handling,
which is more code and more flaky tests for the same user-visible result; (4) the only thing inotify buys is
sub-second latency, which nobody needs for a branch list. If 5 s ever feels slow, `notify` can be added as
a *wake-up hint* that calls the same `refresh(repo)` function without changing anything else.

```rust
/// Stat-only (no process spawn). None = could not compute => caller always runs git.
pub fn fingerprint(common_dir: &Path, known_checkouts: &[PathBuf]) -> Option<u64> {
    // Hash of (mtime_ns, len) for:
    //   <common>/HEAD                      (primary checkout branch switch)
    //   <common>/worktrees/               (worktree added/removed)
    //   each <common>/worktrees/<n>/      (dir mtime: lock/unlock)
    //   each <common>/worktrees/<n>/HEAD  (branch switch/rename/detach in a linked worktree)
    //   each <common>/worktrees/<n>/gitdir (git worktree move/repair)
    // plus: exists(<checkout>/.git) for every known checkout path (user rm -rf / mv)
}
```

Tick loop (single `tokio` task in the daemon's `JoinSet`):

```
every T seconds, T = 5 if broadcast bus has >=1 receiver (GUI/CLI listening) else 30:
    for repo in registry.repos():
        fp = fingerprint(...)
        if fp != repo.last_fp  ||  tick % 12 == 0 (self-heal)  ||  repo.force_refresh:
            list = git.list_worktrees(repo)               // IO outside the registry
            registry.apply_worktrees(repo.key, list)      // pure, in-memory, emits events
            repo.last_fp = fp
```

Additional refresh triggers (all call `refresh(repo)` directly, rate-limited to 1/s/repo):

| Trigger | Why |
|---|---|
| Before `start`, `prepare`, `remove`, `run_branch` on an instance of that repo | Never act on a stale list. |
| After any git mutation Lantern performs | Immediate UI update. |
| `GET .../checkouts?refresh=true` | Explicit user refresh (GUI button, `lantern wt ls --refresh`). |
| `InstanceService::hint_path(path)` from 12-discovery when a listener's cwd is inside a git worktree no instance knows | A server started in a brand-new worktree shows up within one scan. |

`dirty` is **not** part of the tick (a `git status` on a large monorepo can take seconds). It is computed on
demand with a 30 s cache per checkout, with `GIT_OPTIONAL_LOCKS=0` so Lantern never takes `index.lock` from
under the user's own git commands.

### 3.5 From worktrees to Instances

`apply_worktrees(repo, list)` runs inside the registry and performs no IO (the watcher passes in the result of each
`<worktree>/<subpath>` `is_dir` check along with the list). For every project in the repo:

```
for wt in list where !wt.bare:
    dir = wt.path.join(project.subpath)
    if !subpath_exists(dir):               continue      // project does not exist on that branch (precomputed by the watcher)
    kind = if wt.path == project.git_root              -> Primary
           else if wt.path.starts_with(worktrees_dir)  -> Managed    // record says Managed, or adopt (state.json lost)
           else                                        -> Worktree
    upsert CheckoutRecord/Instance keyed by dir      (path is the identity of a checkout)
    refresh checkout.{branch, head, locked, missing=false}
for records whose path is not (or no longer) in the list:
    set missing_since (if unset); instance.checkout.missing = true; ask runtime to stop it if active
    managed + prunable      -> `git worktree prune` for that entry (§3.13)
    purge records missing > missing_retention_days (30) unless pinned
```

**Kinds and defaults**

| Kind | Created by | Default `pinned` | Removable by Lantern |
|---|---|---|---|
| `Primary` | project registration | n/a (always shown) | never (deregister the project instead) |
| `Worktree` | user / other tools (`git worktree add`, agents, IDEs) | `false` | **never** (only unpin/hide; the user removes it with git) |
| `Managed` | `run_branch` (this doc) | `true` for the *requested* project, `false` for sibling projects of the same repo | yes, with safety checks (§3.13) |

**Visibility rule** (computed, exposed as `visible` by the API; hidden instances still have ids and domains and can be
started by id):

```rust
fn is_visible(i: &Instance) -> bool {
    i.checkout.kind == CheckoutKind::Primary
        || i.pinned
        || !matches!(i.runtime.status, InstanceStatus::Stopped | InstanceStatus::NeedsConfig)
}
```

So a running or `External` (a hand-started server that discovery matched) instance in an unpinned worktree
appears automatically and disappears again when it stops, unless pinned. **A successful first
start auto-pins** (the runtime calls `registry.record_started(id)`): you showed interest, so the card does not
vanish when you press Stop. `lantern wt unpin` hides it again. Pins are stored in the record, so they survive
restarts.

Many worktrees (agent tools can create dozens) therefore never flood the UI: the GUI offers a per-project
"Branches and checkouts" panel listing *all* checkouts (visible or not) with pin/run buttons.

**Non-git projects**: `Project.repo = None`; exactly one instance, the primary. All worktree operations return
`422 not_a_git_repo`.

### 3.6 Monorepos and several projects per repo

- At registration, `git.probe(root)` returns `(git_root, common_dir, subpath, is_linked_worktree)`. The
  project stores `repo = Some({common_dir, git_root, subpath})`.
- Projects with the same `common_dir` share one `RepoEntry` (one `worktree list`, one fingerprint, one mutation
  lock, one managed-worktree directory). `RepoRecord.slug` = the `ProjectId` of the first project registered in
  the repo, so single-project repos get exactly the core doc's `worktrees/<project>/<branch-slug>/`.
- **One managed worktree per branch per repo, not per project.** Creating `feature-x` for `web` creates
  `<worktrees>/shop/feature-x/` once. Because instances are derived per (project × worktree) (§3.5),
  `api@feature-x` exists immediately too (unpinned/hidden); starting it auto-pins it. Both domains follow the
  slug convention, so `web` can reference its sibling without any cross-instance feature:

  ```yaml
  # apps/web/lantern.yaml
  worktree:
    env:
      API_URL: https://${BRANCH_SLUG}.api.glow    # same slug as api@feature-x (see 3.8 on collisions)
  ```
- The subpath is applied to *every* worktree. If a branch lacks `apps/web` (older branch), no `web` instance
  exists for that worktree; `run_branch` for `web` on such a branch fails with `project_not_in_branch`.
- Two projects may not map to the same `(common_dir, subpath)`; registering a path that equals an existing
  *non-primary checkout* of another project fails with `409 path_is_checkout_of` and names the owner.
- **Scanner contract (for 14-config)**: before registering a scan candidate, call `git.probe`. If
  `is_linked_worktree` and the repo is already known (or another candidate in the same scan is that repo's
  main worktree), skip it as a project; it is a checkout. Group same-repo candidates and prefer the main worktree
  as the project root. Without this, v1's scanner would register every committed `lantern.yaml` copy as its own
  project.

### 3.7 Paths in worktrees (the subpath rule)

`checkout.path = <worktree_root>/<subpath>`; `${CHECKOUT}` is `checkout.path`, `${PRIMARY}` is the primary's
`checkout.path`. Run cwd, `copy`/`link` sources and destinations, setup cwd, manifest lookup, and PHP/static
document roots are all relative to it. Git operations (status, remove, submodules) use the worktree root
(`Checkout.git_root`, §9). Lockfile lookup searches the project dir first, then each ancestor up to the git root
(pnpm/yarn workspaces keep the lockfile at the root).

### 3.8 InstanceId, slugs and domains

**Slug function** (`lantern-core::slug`, pure, deterministic):

```rust
pub const BRANCH_SLUG_MAX: usize = 40;
/// Lowercase ASCII; every run of chars outside [a-z0-9] becomes one '-'; trim '-' at both ends. May return "".
pub fn slugify(input: &str) -> String;
pub fn branch_slug(branch: &str) -> String {
    let s = slugify(branch);
    if s.is_empty() { return format!("b-{}", hash6(branch)); }              // e.g. an emoji-only branch name
    if s.len() <= BRANCH_SLUG_MAX { return s; }
    format!("{}-{}", s[..33].trim_end_matches('-'), hash6(branch))          // ≤ 40, still unique-ish
}
pub fn detached_slug(sha: &str) -> String { sha[..7].to_string() }
```

`feature/login-form` → `feature-login-form`; `release/1.2` → `release-1-2`; `Fix/Bug#12` → `fix-bug-12`. Prefixes such
as `feature/` are **not** stripped: the slug stays a pure function of the branch name. 40 chars keeps
`<slug>.<project>.<tld>` far below DNS limits (63 per label).

```rust
impl InstanceId {
    pub fn primary(p: &ProjectId) -> Self;                      // "myapp"
    pub fn derived(p: &ProjectId, slug: &str) -> Self;          // "myapp@feature-x"
    pub fn project(&self) -> ProjectId;
    pub fn slug(&self) -> Option<&str>;                         // None for primary
    /// Filesystem/systemd-safe form: "myapp" or "myapp_feature-x". '_' never occurs in a ProjectId or
    /// slug, so it is unambiguous. ('@' is NOT usable in unit names: systemd treats it as the template separator.)
    pub fn safe_name(&self) -> String;
}
```

`${INSTANCE}` = `safe_name()`. 10-runtime names units `lantern-inst-<safe_name>`.

**Domain**: `fn instance_domain(primary_domain: &str, slug: Option<&str>) -> String` where `primary_domain` is the
manifest `domain:` override or `<project>.<tld>`. Primary → `primary_domain`; others → `<slug>.<primary_domain>`.
Manifest `routing.aliases` (v1 key) follow the same rule: primary only gets the plain alias, non-primary instances get
`<slug>.<alias>`. Total length is checked (<= 253); a domain already held by another instance is a collision.

**Collisions**: slugs are unique per project (the `InstanceId` is the key) and domains are unique globally.
`allocate_slug(project, desired)`:

```
candidates: desired, desired-2, desired-3, ...       (truncate desired to keep ≤ 40 including the suffix)
a candidate is free if no LIVE record of this project uses it, the resulting domain is unused by any instance, and
(for managed) <worktrees>/<repo-slug>/<candidate> does not exist on disk.
If candidate == slug of a MISSING (not live) record of this project -> reuse that record's InstanceId (revival):
the recreated worktree gets the same id, pin and domain as before.
Batch order when several new checkouts appear in one refresh: primary first, then by path ascending (deterministic).
```

So `feature/x` and `feature-x` (both slug `feature-x`) become `myapp@feature-x` and `myapp@feature-x-2`; the first one seen
keeps the plain name, and that choice is persisted.

**Stability: the instance follows the checkout (path), not the branch.** Decision: the id/domain are
derived from the branch (or sha) **when the checkout is first seen**, and are **frozen once the instance is
"claimed"** (pinned or started at least once). Unclaimed instances (hidden, never started) are re-derived on every
refresh, so a user's scratch worktree that moves between branches always has the right name until someone cares.

Rationale: URLs are used as OAuth redirect URIs, CORS allowlists, bookmarks and cookie hosts; the systemd unit, log
files, setup stamp and pin are keyed by id. Changing them on every `git switch` (or during an interactive rebase, when
HEAD is briefly detached) would restart servers and break sessions.

| Event inside a checkout | Unclaimed | Claimed (pinned or started) |
|---|---|---|
| `git switch other` | id/domain re-derived: `myapp@other` | unchanged; `checkout.branch = other`; GUI shows a "branch changed (was feature-x)" badge |
| `git branch -m old new` (verified: `worktree list` reports the new name at once) | re-derived | unchanged, badge |
| Detached HEAD (bisect, rebase, `git checkout <sha>`) | `myapp@<sha7>` | unchanged |
| Branch checked out in a worktree that was detached before | re-derived | unchanged |
| Directory moved/renamed (`mv`, `git worktree move`) | new unpinned instance appears at the new path | old record goes `missing`; new path is a new instance (identity is the path; a move is "remove + add") |
| Worktree deleted, later recreated for the same branch | revived (same id) | revived (same id, pin and domain kept) |

**Explicit relabel**: `rename_instance(id, new_slug)` (`lantern wt relabel myapp@feature-x other`) changes id + domain; allowed
only when the instance is `Stopped`; it moves log dir and stamp, and the runtime releases the old route. Not automatic.
`${BRANCH_SLUG}` for non-primary instances is the id's slug (frozen), so DB names in `worktree.env` do not change when
you switch branches (data stays where it was).

### 3.9 "Run branch X" (no worktree exists yet)

Entry point (service layer, called by REST/MCP/CLI):

```rust
pub struct RunBranchRequest {
    pub project: ProjectId,
    pub target: BranchTarget,
    pub start: bool,                 // default true: hand off to runtime.start after prepare
    pub pin: bool,                   // default true (requested project only)
    pub fetch: FetchPolicy,          // Auto (default) | Always | Never
    pub detach: bool,                // default false; see "primary is on that branch"
}
pub enum BranchTarget {
    Branch(String),                  // must already exist locally or on a remote
    NewBranch { name: String, base: Option<String> },   // explicit create; agents must opt in
    Detached(String),                // tag / sha / any commit-ish
    Pr(u32),                         // optional, §3.9.3
}
pub struct RunBranchResponse { pub instance: Instance, pub op: Option<OperationId>, pub outcome: Outcome }
pub enum Outcome { ReusedPrimary, ReusedCheckout, CreatedWorktree(CreateMode) }
pub enum CreateMode { ExistingLocal, TrackRemote { remote: String }, NewBranch { base: String }, Detached }
```

#### 3.9.1 Resolution algorithm (under the repo's mutation lock, so two concurrent "run feature-x" calls create one worktree)

```
0. validate: `git check-ref-format --branch <name>` (also rejects a leading '-'); project is in a repo; git >= 2.36.
1. refresh(repo)                                   // fresh worktree list; branches = for-each-ref (local + remote)
2. REUSE check (targets Branch and NewBranch): if some worktree W (any kind, incl. primary, locked, hidden) has branch == name:
       NewBranch -> error `branch_exists` (hint: run it without --new).
       Branch    -> reuse W. If W is the primary: Outcome::ReusedPrimary (the response carries the primary's domain; callers must not
                    assume "<slug>.<project>.<tld>"). Otherwise pin it and Outcome::ReusedCheckout. Go to 6.
       (git itself refuses a second checkout of one branch: "fatal: '<b>' is already used by worktree at '<path>'", verified;
        Lantern never passes --force.)
       if W is prunable/missing on disk -> error `checkout_missing` with hint `lantern wt prune`.
       exception: `detach` requested -> do not reuse; treat as target Detached(<branch tip sha>) with slug = branch_slug(name).
3. PLAN the creation by target:
   Branch(name):
     a. local branch `name` exists          -> AddMode::ExistingLocal(name)           `worktree add -- <path> <name>`
     b. else remote-tracking `<remote>/<name>` exists:
          remotes holding it: 1 -> use it; >1 -> `checkout.defaultRemote`, else `origin`, else error `ambiguous_remote` (names them)
          -> AddMode::TrackRemote            `worktree add --track -b <name> -- <path> <remote>/<name>`
     c. else if fetch == Auto: one `git fetch --prune <remote>` per remote (60 s timeout), then redo a-b once
     d. else error `branch_not_found` with up to 5 "did you mean" names (substring/edit distance)
   NewBranch{name, base}:
     name must not exist locally or on any remote (`branch_exists`). base = request.base ?? remote default branch
     (`git symbolic-ref --short refs/remotes/origin/HEAD`) ?? local main/master ?? primary's current HEAD.
     Best-effort `git fetch <base's remote>` (15 s timeout, failure = warning) unless fetch == Never.
     -> AddMode::NewBranch              `worktree add --no-track -b <name> -- <path> <base>`
     (`--no-track`: without it git makes the new branch track origin/main, and a later `git push` then targets main.)
   Detached(rev) -> AddMode::Detached   `worktree add --detach -- <path> <rev>`   (rev checked with `rev-parse --verify <rev>^{commit}`)
4. ALLOCATE: slug = branch_slug(name) (or sha7 for Detached) via allocate_slug (3.8, includes the on-disk existence check);
   path = <worktrees_dir>/<repo-slug>/<slug>; registry.begin_instance(...) creates the instance in status Preparing with
   checkout.phase = Creating, so the GUI shows it immediately.
5. CREATE (pipeline step 2, 3.10): git worktree add ... ; re-list and assert the new worktree is on `name` (or detached for
   Detached). If git silently produced a detached HEAD (e.g. `name` also matched a tag) -> fail `unexpected_detached` and roll back.
   phase = Ready.
6. PREPARE (copy/link/setup, 3.10) and, if request.start, hand off to runtime.start.
```

All git invocations pass arguments as separate argv items (never through a shell), and positional refs/paths follow `--` (verified accepted by `worktree add` in all four forms). Path = `<worktrees_dir>/<repo-slug>/<slug>`;
the slug is allocated *first* (with the on-disk existence check) so the directory name and the instance slug always agree.
Verified behaviours (git 2.53): `worktree add <path> <name>` for a remote-only name already creates a tracking branch
(explicit `--track -b` is used so the choice does not depend on `worktree.guessRemote` or multiple-remote DWIM); `-b` for
an existing branch fails with `a branch named '<b>' already exists`; an existing non-empty path fails with
`'<path>' already exists`.

Cases the brief asked about:

| Situation | Behaviour |
|---|---|
| Branch only exists as `origin/x` | Step 3 Branch-b: create local `x` tracking `origin/x` in a new managed worktree. |
| Brand-new branch name | Only with explicit `NewBranch` (CLI `--new`, API/MCP `create: true`). Otherwise `branch_not_found`. |
| Branch already checked out in a managed/user worktree | Reuse that checkout (step 2). Pinned; `Outcome::ReusedCheckout`. For a hidden/unpinned one this simply makes it visible. |
| Primary checkout currently on that branch | Reuse the primary (`ReusedPrimary`). The user can pass `detach: true` to get a separate detached worktree at that commit instead (`git worktree add --detach <path> <name>`), which gets slug `<branch-slug>` because it was explicitly requested. |
| Tag or sha | `Detached` target; slug = 7-char sha unless the caller supplies a label. |
| Bare repo | Works (`--git-dir` form). A bare clone has no `refs/remotes/*`, so only local branches exist; Lantern never fetches a bare repo that has no fetch refspec. |
| Branch exists on several remotes | Disambiguation as in step 3 Branch-b; unresolved ambiguity is an error naming the remotes (`remote` can be passed explicitly). |

#### 3.9.2 End-to-end example

```
$ lantern on myapp --branch fix/login --new
  1. POST /api/projects/myapp/instances {target:{new_branch:{name:"fix/login"}}, start:true}  -> 202 {op, instance: myapp@fix-login (Preparing)}
  2. resolve: not in worktree list, no ref -> NewBranch from origin/main  (fetch ok)
  3. slug "fix-login"; path ~/.local/share/lantern/worktrees/myapp/fix-login
  4. git --git-dir=~/sites/myapp/.git worktree add --no-track -b fix/login -- <path> origin/main
  5. prepare: copy .env, link storage/app/public, setup (composer install ...), stamp
  6. runtime.start(myapp@fix-login) -> port 41017 -> Caddy route fix-login.myapp.glow
  7. SSE: OperationProgress{...done:true}; CLI prints https://fix-login.myapp.glow
```

#### 3.9.3 GitHub PRs (optional) — recommendation: support, but after the core path works

Value: "run PR 123 to review it" is a natural fit for per-instance domains. Cost is small and needs no GitHub API for the
checkout itself:

- `Pr(n)`: `git fetch <remote> pull/<n>/head` (works for fork PRs too), then
  `git worktree add --no-track -b pr-<n> -- <path> FETCH_HEAD`; slug `pr-<n>` (`pr-123.myapp.glow`).
- `<remote>` = the remote whose URL matches the PR's base repo (`gh repo view --json url -q .url`), else `origin`.
- If `gh` is installed and authenticated, `gh pr view <n> --json number,title,headRefName,isCrossRepository,state,url`
  supplies the title for the UI. `gh` is never required for the checkout.
- PR instances are read-only snapshots; `lantern wt add --pr 123 --update` re-fetches and fast-forwards if clean.
- **Fork PRs execute foreign code** (setup scripts, dependency scripts, dev server), so `isCrossRepository == true`
  requires an explicit `confirm_foreign_code: true` in the request (CLI prompts, GUI shows a dialog). §6.
- The branch `pr-<n>` is `created_branch = true`, eligible for deletion with the worktree.

### 3.10 The Preparing pipeline

Preparation is part of a *start* (or `run_branch`) operation and runs **inside the per-instance operation lock**
(core §8), so a second start/stop request returns the in-flight operation instead of racing. 10-runtime calls
`InstanceService::ensure_prepared` first thing in `start`; the fast path (everything up to date) is a few `stat`s and one
hash and returns immediately. The pipeline (minus the create steps) also runs on the first start of a user-made `Worktree` checkout: `copy`/`link` never
overwrite anything, and running `setup` is what the user asked for by starting a server there.

```rust
pub struct PrepareCtx {
    pub cancel: CancellationToken,
    pub progress: Box<dyn ProgressSink>,     // wraps events::OperationHandle
}
pub struct Prepared {
    pub manifest: ResolvedManifest,
    pub worktree_env: BTreeMap<String, String>,   // already expanded; empty for primary
}
pub async fn ensure_prepared(&self, id: &InstanceId, ctx: PrepareCtx) -> Result<Prepared, PrepareError>;
```

**Steps** (a step is skipped when its precondition is false; `step` strings below are what `OperationProgress.step` carries):

| # | `step` label | Runs when | Action | Cancel | Failure |
|---|---|---|---|---|---|
| 1 | `Fetching` | plan says fetch (3.9.1 step 3) | `git fetch` (60 s) | SIGTERM git | warning only for `NewBranch`; error when a remote-only branch needed the fetch |
| 2 | `Creating worktree` | create plan present (run-branch only) | record `phase=Creating`, `git worktree add ...`, then `phase=Ready` | SIGTERM git (its own cleanup handler removes the half-made dir); record dropped | rollback: remove record; if `phase` stayed `Creating` the next boot/start force-removes the managed dir and retries |
| 3 | `Initializing submodules` | `.gitmodules` exists and `git submodule status` shows uninitialized entries | `git submodule update --init --recursive` in the git root | kill git | op fails; worktree kept |
| 4 | `Reading manifest` | always | resolve manifest (3.3) | n/a | parse error -> `NeedsConfig` |
| 5 | `Copying files` | non-primary and `worktree.copy` non-empty | 3.3 copy rules | between files | op fails on I/O error / limit |
| 6 | `Linking files` | non-primary and `worktree.link` non-empty | 3.3 link rules | between files | op fails on I/O error |
| 7 | `Installing dependencies (clone)` | `worktrees.clone_deps = "reflink"` and eligible (3.11) | `cp -a --reflink=always` of `node_modules` | kill cp, remove partial dir | **silent fallback** to step 8 |
| 8 | `Setup n/N: <cmd>` | non-primary and (no stamp, or lock/commands hash changed, or default-setup artifact missing) or explicit `lantern setup` | each command via the one-shot runner, in order | SIGTERM the process group, 5 s, SIGKILL | stop at first non-zero exit |
| 9 | `Ready` | all above ok | write `SetupStamp` + `materialized` to the record | n/a | n/a |

Then control returns to the runtime, which moves `Preparing -> Starting` (port lease, unit, readiness: 10's job).

```
                 start / run_branch
   Stopped ──────────────────────────────► Preparing ──ensure_prepared ok──► Starting ──► Running
      ▲  ▲                                    │   │
      │  └────────── failure ─────────────────┘   │       Instance.runtime.message = "Setup failed at step 2/3:
      │              (message = reason)            │       `pnpm install` exited 1. See setup logs. Retry: start again."
      └──────────────── cancel ───────────────────┘       or "Cancelled during: Setup 1/3"
   NeedsConfig  <── manifest parse error / no run command (set at step 4)
```

Checkout `phase` is separate: `Creating -> Ready` happens inside step 2 and survives a daemon crash.

**Progress**: every step start emits `OperationProgress { op, instance, step, done: false, error: None }`; the final event has
`done: true` (and `error: Some(msg)` on failure or `"cancelled"`). Command output is `LogLine` with `Stream::Setup` for that
instance (clients subscribed with `?logs=<id>`); history is in the journal (→ 02 D6). For `run_branch` the instance exists in the registry from the allocation
step (3.9.1 step 4, status `Preparing`, before step 1) so the GUI shows a card with a live step label immediately.

**Cancellation**: `stop` on a `Preparing` instance, or the operation-cancel endpoint, triggers the `CancellationToken`. Whatever is
running gets the signal per the table; **created artifacts are kept** (a managed worktree and any copied files are
expensive and idempotent) except a half-finished `git worktree add`. Status returns to `Stopped`, message "cancelled during ...".

**Failure and retry**: no automatic retry (a failing install is almost always a real error). The user fixes the cause and starts again;
the pipeline is **idempotent and resumes by re-evaluating each step's precondition**: the worktree exists (skip 2), copy/link skip existing
destinations (5, 6), setup reruns from its first command because there is no stamp. `lantern setup <instance>` runs steps 4-9 without
starting. A failure never deletes the worktree.

**Concurrency**: git mutations of one repo are serialized by the repo lock (`add`, `remove`, `prune`, `fetch`, `repair`); setup runs are limited by
a global semaphore (default 2) because installs saturate disk, CPU and network. A queued setup shows step `Waiting for a free setup slot`.

**Daemon restart during Preparing**: not persisted. On boot the instance is `Stopped` with message "interrupted"; records in
`Creating` are cleaned (force-remove of the managed dir; safe because the user never saw content) before the next attempt.

### 3.11 Dependency install strategy

#### Default `setup` when the key is absent (non-primary instances only)

Detection runs on the checkout, project dir first, then ancestors up to the git root; the nearest lockfile wins per ecosystem, and the install `cwd` is the directory
that holds that lockfile (so a pnpm workspace root installs the whole workspace). Several ecosystems can match (Laravel: Composer + npm); they run in the order
PHP, Python, Node. Within one ecosystem exactly one row applies. Node precedence: the `packageManager` field of `package.json`, then lockfiles in v1's
order (`projects/detector.ex:81-98`: bun, pnpm, yarn, npm). Python precedence: `uv.lock`, `[tool.uv]` without lock, `poetry.lock`, `requirements.txt`.

| Ecosystem | Evidence | Default command | Notes |
|---|---|---|---|
| pnpm | `packageManager: "pnpm@..."` in `package.json`, else `pnpm-lock.yaml` | `pnpm install --frozen-lockfile --prefer-offline` | Store is global and files are hard-linked/cloned into `node_modules`: seconds. Same-filesystem required for the cheap path. |
| npm | `package-lock.json` or `npm-shrinkwrap.json` | `npm ci --prefer-offline --no-audit --no-fund` | `npm ci` wipes `node_modules`; the npm cache (`~/.npm/_cacache`) still makes it offline-fast. |
| yarn classic | `yarn.lock` without `__metadata:`, no `.yarnrc.yml` | `yarn install --frozen-lockfile --prefer-offline` | |
| yarn berry | `.yarnrc.yml` or `yarn.lock` with `__metadata:` | `yarn install --immutable` | |
| bun | `bun.lock` or `bun.lockb` | `bun install --frozen-lockfile` | |
| uv | `uv.lock` | `uv sync --frozen` | Uses uv's global cache; `--frozen` = never rewrite `uv.lock` (keeps the tree clean). |
| uv (no lock) | `pyproject.toml` containing `[tool.uv]`, no `uv.lock` | `uv sync` | Writes `uv.lock`; the worktree becomes dirty (user's call; setup can be overridden). |
| poetry | `poetry.lock` | `poetry install --no-interaction` | |
| pip | `requirements.txt` | `uv venv --allow-existing && uv pip install -r requirements.txt` (if `uv` is not on PATH: `python3 -m venv .venv && .venv/bin/pip install -r requirements.txt`) | |
| composer | `composer.lock` | `composer install --no-interaction --prefer-dist` | Uses Composer's cache; `vendor/` is per worktree. |

Flags were checked against the installed tools (`uv 0.12`, `pnpm 11`, `npm 11`, `composer 2.9`). No default is generated for Go, Rust or Ruby (nothing slow to do before
`run`); users add `setup:` for those. A default that fails because the lockfile is out of sync with `package.json` fails the op with the tool's own message; the cure
is an explicit `setup:`.

#### When setup re-runs (the stamp)

```rust
pub fn lock_hash(project_dir: &Path, git_root: &Path) -> String;
// sha256 over, sorted by path: (relative path, file bytes) of every file found in project_dir and each ancestor up to git_root among:
//   package-lock.json npm-shrinkwrap.json yarn.lock pnpm-lock.yaml bun.lock bun.lockb
//   uv.lock poetry.lock Pipfile.lock requirements.txt composer.lock Gemfile.lock go.sum
//   .nvmrc .node-version .python-version .tool-versions .php-version        (runtime-version pins affect native builds)
```

`commands_hash` = sha256 of the resolved command list (explicit or default, joined with `\n`). A start re-runs setup when stamp is missing, or either hash differs, or (defaults
only) the artifact dir is missing. While an instance runs, a lockfile change (e.g. `git pull` inside the worktree) only sets `Instance.setup_stale = true` (evaluated lazily,
cached 10 s); the next start re-runs setup. Nothing restarts a running server automatically.

#### Optional: cloning `node_modules` (default OFF)

Setting `worktrees.clone_deps = "reflink"` enables step 7. Conditions, all required: ecosystem is npm, yarn classic or bun (**not** pnpm, uv, composer: they are already cheap); the primary has
`node_modules` with its completion marker (`.package-lock.json`, `.yarn-integrity`, or bun's equivalent); `lock_hash(worktree) == lock_hash(primary)`; the destination is on the same
filesystem and `cp --reflink=always` works (probed once per `worktrees_dir` with a tiny temp file and cached); no top-level `node_modules/*` or `node_modules/@*/*` symlink has an absolute target
inside the primary (checked, depth <= 2). Command: `cp -a --reflink=always <primary>/node_modules <checkout>/node_modules`. Any failure removes the partial copy and falls through to the normal
install. When the clone succeeds, the install command is **skipped** (hashes are identical, so a fresh install would produce the same tree) and the stamp is written.

Why default off: Ubuntu's default ext4 does **not** support reflinks (verified on this machine: ext4), so on most installs `--reflink=always` fails and nothing is gained; a plain copy of
a 500 MB `node_modules` is not reliably faster than `npm ci` from a warm cache. Pitfalls when it *is* on (btrfs/XFS-with-reflink): native `.node` addons are copied as built (fine only if
the Node version is unchanged, hence `.nvmrc`/`.node-version` are in the lock hash); absolute-path symlinks (`npm link`) would still point at the primary (hence the symlink check); and a hash
mismatch means the clone is not used at all (we never "clone then `npm install`": that can rewrite `package-lock.json` and dirty the tree).

Python virtualenvs are **never** copied (absolute paths in shebangs and `pyvenv.cfg`); `uv sync` into a fresh `.venv` is already fast because of uv's global cache. Because uv/pnpm
link from a cache, the cache and the worktree should share a filesystem; if `st_dev(worktrees_dir) != st_dev(git_root)` Lantern emits a warning in the pipeline log
("installs will copy instead of link").

### 3.12 Per-branch isolation: what Lantern does and does not do

| Resource | Isolation | Lantern's part |
|---|---|---|
| TCP port | Automatic | Runtime leases one port per instance (10). |
| Domain / cookies | Automatic per exact host | Distinct `<slug>.<project>.<tld>`. Host-only cookies are isolated; a cookie with `Domain=.myapp.glow` leaks across branches (set `SESSION_DOMAIN`/cookie domain from `${DOMAIN}` via `run.env`). |
| Build/dev caches (`.next`, `.vite`, `node_modules/.cache`, `.venv`, `vendor`) | Automatic (they live in the checkout) | none |
| Global package caches (`~/.npm`, pnpm store, uv cache, composer cache) | Intentionally shared | none (this is what makes setup fast) |
| `.env` secrets | Copy, so edits in the branch do not affect main | `worktree.copy` |
| Database / queue / cache namespace | **Not isolated by default** | `${BRANCH_SLUG}` + `worktree.env` (e.g. `DB_DATABASE: myapp_${BRANCH_SLUG}`, `REDIS_PREFIX: myapp_${BRANCH_SLUG}_`); `setup:` can create the database (`createdb "$DB_DATABASE" \|\| true`, run migrations). SQLite files inside the checkout are isolated for free. |
| Shared services (Mailpit, Redis, Postgres from 13) | Shared | none; use the env prefixing above |
| OAuth redirect URIs / CORS allowlists / webhooks | Not isolated | none; each new domain must be registered by the user (providers usually reject wildcards). |
| Migrations | A branch with a different schema on a shared DB breaks main | the user's problem; isolate the DB or do not share it |

Helpers: (1) `worktree.env` never touches the primary; (2) `${BRANCH_SLUG}` is stable (frozen) so a DB name does not change on `git switch`; (3) **one-time hint**: when a non-primary instance
had `.env*` files copied but its manifest has no `worktree.env`, the pipeline log says "no worktree.env: this instance shares databases, caches and queues with the primary"; (4) the removal check
prints the resolved `worktree.env` so the user can drop what it created (Lantern does not drop databases in v2.0, §8 Q3). Identifier note: `${BRANCH_SLUG}` contains `-`, which is legal in a
*quoted* SQL identifier (frameworks quote) but awkward in `psql`/`mysql` CLIs; §9 proposes `${BRANCH_SNAKE}`.

### 3.13 Removal and GC

#### Removal check and removal

```rust
pub struct RemovalCheck {
    pub blockers: Vec<Blocker>,                 // empty => removable without force
    pub dirty: Vec<ChangedPath>,                // tracked changes + untracked, minus CheckoutRecord.materialized
    pub unpushed: u32,
    pub orphaned: u32,                          // detached only
    pub has_submodules: bool,
    pub worktree_env: BTreeMap<String, String>, // "resources you may want to clean up"
    pub branch: Option<BranchFate>,             // what delete_branch would do
}
pub enum Blocker { NotManaged, Running, Locked(String), Dirty(usize), Unpushed(u32), Orphaned(u32) }
```

`remove_checkout(id, RemoveOpts { force, delete_branch, stop })`:

1. **Kind**: must be `Managed`. `Worktree` and `Primary` -> `409 not_managed`; `force` does **not** override this. Defence in depth: the canonical path must be a strict child of
   `worktrees_dir`, and `<path>/.git` must be a file whose `gitdir:` points into this repo's `<common_dir>/worktrees/`.
2. **Running**: `409 instance_running`, or stop it first when `stop: true`.
3. **Locked** (`git worktree lock`): `409 locked` with the reason; Lantern never uses `-f -f`. The user unlocks it.
4. **Dirty**: `git -C <git_root> status --porcelain=v2 -z --untracked-files=normal` with `GIT_OPTIONAL_LOCKS=0`; ignored files do not count (verified: `git worktree remove`
   deletes ignored files such as `node_modules` and `.env` copies without complaint). Entries equal to a `materialized` path are dropped (an untracked `link:` symlink would otherwise show
   as `?? name`, verified). In-progress rebase/merge/bisect counts as dirty. Any remaining entry is a `Dirty` blocker.
5. **Unpushed** (the rule from the brief, made precise):
   - attached HEAD and the repo has at least one remote: `git rev-list --count HEAD --not --remotes` > 0 -> `Unpushed(n)` ("branch X has n commits on no remote; the branch itself is kept").
   - attached HEAD, no remotes at all: no check (the branch ref keeps the commits).
   - detached HEAD: `git rev-list --count HEAD --not --branches --remotes --tags` > 0 -> `Orphaned(n)` (these commits become unreachable once the worktree is gone).
6. `force: true` overrides only `Dirty`, `Unpushed`, `Orphaned`. The CLI/GUI first call `removal_check`, show the list, and require explicit confirmation.
7. Execute: `git --git-dir=<common> worktree remove [--force] <path>` (`--force` only if forced, **or** `has_submodules`: git refuses to remove any worktree containing initialized submodules unless forced (verified), but the checks above already ran).
   Symlinks created by `link:` are removed as links; their targets in the primary are untouched (verified).
8. Cleanup: `rmdir` the now-empty `<worktrees_dir>/<repo-slug>/`; delete the `CheckoutRecord`, emit `InstanceRemoved`; the runtime releases port/route/unit.
9. `delete_branch: true` (default false): only if `created_branch` and the branch is no longer checked out anywhere: `git branch -d <branch>` (never `-D`; if git says not fully merged, report "branch kept"). Remote branches are never deleted.

#### `git worktree prune` and vanished directories

- `git worktree list` marks a worktree whose directory is gone as `prunable` (verified; it also does this after a plain `mv`). **Managed** entries that are `prunable`: Lantern removes the instance and
  runs `git --git-dir=<common> worktree prune` (the user deleted the dir by hand; nothing to protect). **Non-managed** prunable entries are *not* pruned (the drive may just be unmounted); the
  instance is marked `missing`, hidden, and its record purged after `missing_retention_days`. Git's own `gc` prunes stale admin entries after `gc.worktreePruneExpire` (3 months default).
- Locked worktrees are never pruned (git skips them too).

#### GC suggestions (never automatic)

`GET .../gc` returns `Vec<GcSuggestion>`; the GUI shows a dismissible banner "3 checkouts look stale", `lantern wt gc` prints them (dry-run by default; `--apply` removes only those that are `Managed` and have
**no blockers**; there is no `--force` for gc).

| Reason | Detection | Notes |
|---|---|---|
| `UpstreamGone` | `for-each-ref --format='%(upstream:track,nobracket)'` == `gone` (verified after `fetch --prune`) | Best signal for squash-merged PRs. |
| `Merged` | `git merge-base --is-ancestor <branch> <default remote branch>` | Misses squash merges; `UpstreamGone` covers them. |
| `Idle` | Managed, stopped, `last_started ?? created_at` older than `worktrees.gc_hint_days` (30) | |
| `Missing` | path gone | Action: `ForgetMissing` (drop record), or `Prune` for managed. |

Suggestions for `Worktree`-kind checkouts can only have the action `Unpin`. Fetching is never periodic (network and credentials); `lantern wt gc --fetch` does one `git fetch --prune` first.

### 3.14 Domains and Caddy (contract with 13-system)

- This module owns `Instance.domain` (formula in 3.8). It never talks to Caddy. 13-system reconciles routes from registry snapshots (preferred: full-sync from `registry.routable()` on
  every `InstanceUpdated`/`InstanceRemoved`, idempotent) using, per instance: `domain`, `runtime.port`, `manifest.type`/`root`, and **`checkout.path`** (PHP and static routes must root at the
  instance's checkout, not at `project.root`; v1's `system/caddy.ex:52-64,76-87` used the project path).
- Routes exist only while an instance is `Running`/`Unhealthy`/`External` with a port or document root (same as v1: config removed on deactivate). Unknown `*.<project>.<tld>` hosts are 13's choice;
  a per-project catch-all "this instance is not running — start it" page is a nice use of a wildcard route (see TLS below).
- **Nested subdomains and TLS** (checked against the Caddy docs, `automatic-https`):
  - DNS: the embedded resolver must answer *any depth* under `.glow`, and the systemd-resolved routing domain `~glow` already covers every depth.
  - A certificate name `*.glow` does **not** cover `feature-x.myapp.glow` (a wildcard matches exactly one label; Caddy additionally only supports a wildcard in the left-most label, so
    `*.*.glow` is impossible).
  - Slugs contain no dots, so `feature-x.myapp.glow` is exactly one label below `myapp.glow`: a per-project wildcard `*.myapp.glow` is sufficient when wanted.
  - Recommended baseline: **exact hostnames per instance route** with `tls internal`. Caddy's docs state a host matcher in a top-level route activates automatic HTTPS and that the local CA "does not
    use ACME nor does it perform any DNS validation", so the certificate for a new instance is minted locally when the route is added (no network, no new wildcard). Use the per-project wildcard only
    for the catch-all page.
  - On-demand TLS combined with `tls internal` is documented (`tls` directive page) but is not needed: routes are added explicitly, so issuance happens at route-add time and no `ask` endpoint is required.
  - **Not verified live**: no Caddy binary was available while writing this, and the Caddy docs do not say whether the internal issuer accepts wildcard subjects. 13-system should spike (a) "add a route for a new nested host via the admin API, then `curl --resolve` it over HTTPS" and (b) a `*.myapp.glow` subject, before relying on either.
  - Browsers trust the Caddy local CA only if 13's install step trusted it in the system store (and the NSS/Firefox store); unrelated to nesting.

### 3.15 The `git/` module

Location `crates/lantern-daemon/src/git/`. Shells out to `git`; no libgit2. All functions are `async` unless noted.

```rust
// git/mod.rs
#[derive(Clone)]
pub struct Git { bin: PathBuf, version: GitVersion }
impl Git {
    pub async fn detect() -> Result<Git, GitError>;               // finds `git`, requires >= 2.36
    pub async fn probe(&self, path: &Path) -> Result<Option<Probe>, GitError>;  // None => not in a repo
    pub fn repo(&self, common_dir: PathBuf) -> RepoHandle;
}
pub struct Probe {
    pub git_root: PathBuf,        // --show-toplevel (canonical)
    pub common_dir: PathBuf,      // --path-format=absolute --git-common-dir
    pub subpath: PathBuf,         // --show-prefix, trailing '/' stripped ("" at root)
    pub is_linked_worktree: bool, // absolute git-dir != common_dir
    pub head: HeadState,          // Branch(String) | Detached(String) | Unborn
}
// probe(): `git -C <path> rev-parse --path-format=absolute --show-toplevel --git-common-dir --show-prefix --absolute-git-dir`
// (output lines follow argument order; --git-common-dir is relative without --path-format; verified)

// git/repo.rs
#[derive(Clone)]
pub struct RepoHandle { git: Git, common_dir: PathBuf, mutation_lock: Arc<tokio::sync::Mutex<()>> }
impl RepoHandle {
    // reads (no lock, GIT_OPTIONAL_LOCKS=0)
    pub async fn list_worktrees(&self) -> Result<Vec<Worktree>, GitError>;
    pub async fn branches(&self, limit: usize) -> Result<Vec<BranchInfo>, GitError>;
    pub async fn remotes(&self) -> Result<Vec<String>, GitError>;
    pub async fn default_branch(&self) -> Result<Option<String>, GitError>;   // "origin/main"
    pub async fn status(&self, git_root: &Path) -> Result<StatusSummary, GitError>;
    pub async fn rev_count(&self, git_root: &Path, spec: RevCount) -> Result<u32, GitError>;
    pub async fn is_ancestor(&self, a: &str, b: &str) -> Result<bool, GitError>;
    pub fn fingerprint(&self, known: &[PathBuf]) -> Option<u64>;              // sync stat-only

    // mutations (take mutation_lock; accept a CancellationToken; kill_on_drop)
    pub async fn fetch(&self, remote: &str, timeout: Duration, cancel: &CancellationToken) -> Result<(), GitError>;
    pub async fn add_worktree(&self, path: &Path, mode: &AddMode, cancel: &CancellationToken) -> Result<(), GitError>;
    pub async fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), GitError>;
    pub async fn prune(&self) -> Result<Vec<String>, GitError>;               // `worktree prune -v`
    pub async fn repair(&self, paths: &[PathBuf]) -> Result<(), GitError>;    // `worktree repair`
    pub async fn submodule_init(&self, git_root: &Path, cancel: &CancellationToken) -> Result<(), GitError>;
    pub async fn delete_branch(&self, name: &str) -> Result<(), GitError>;   // `branch -d`
}
pub enum AddMode {
    ExistingLocal(String),                                 // worktree add -- <path> <name>
    TrackRemote { remote: String, branch: String },        // worktree add --track -b <b> -- <path> <remote>/<b>
    NewBranch { name: String, base: String },              // worktree add --no-track -b <n> -- <path> <base>
    Detached(String),                                      // worktree add --detach -- <path> <rev>
}
pub struct BranchInfo {
    pub name: String, pub remote: Option<String>, pub oid: String,
    pub upstream: Option<String>, pub ahead: u32, pub behind: u32, pub gone: bool,
    pub committed_at: i64, pub checked_out_at: Option<PathBuf>,
}
// branches(): for-each-ref --sort=-committerdate --count=<limit>
//   --format='%(refname)%00%(objectname)%00%(upstream)%00%(upstream:track,nobracket)%00%(committerdate:unix)%00%(worktreepath)' refs/heads refs/remotes
// (ref names cannot contain control chars, so newline-separated records with %00 fields are safe; verified)
pub struct StatusSummary { pub head: HeadState, pub upstream: Option<String>, pub ahead_behind: Option<(u32, u32)>, pub changed: Vec<ChangedPath> }
// status(): `status --porcelain=v2 --branch -z --untracked-files=normal`; records: "# branch.*", "1 ...", "2 ... <path>\0<orig>", "u ...", "? <path>"
pub enum RevCount { NotOnRemotes, NotOnAnyRef }   // "HEAD --not --remotes" | "HEAD --not --branches --remotes --tags"

pub enum GitError {
    NotARepo, GitTooOld(String),
    BranchCheckedOut { path: PathBuf }, BranchExists, PathExists, InvalidRef(String), RefNotFound(String),
    Locked(String), Timeout, Cancelled, Spawn(std::io::Error),
    Other { code: Option<i32>, stderr: String },
}
```

Runner rules (`git/runner.rs`):

- `tokio::process::Command`, `stdin = null`, `kill_on_drop(true)`, per-call timeout (reads 10 s, local mutations 120 s, network 60 s), optional cancellation. argv as `OsString`s, never a shell.
- Environment: inherit, then **remove** `GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_INDEX_FILE GIT_PREFIX GIT_OBJECT_DIRECTORY GIT_NAMESPACE`; set `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0`, and for read-only
  calls `GIT_OPTIONAL_LOCKS=0`. `SSH_*`/`HOME` are untouched so agents and credential helpers keep working.
- **Prefer structured data over stderr text.** Pre-checks (worktree list, for-each-ref) decide the plan; stderr is matched only as a fallback to classify races (`is already used by worktree at` / older
  `is already checked out at` -> `BranchCheckedOut`; `already exists` -> `BranchExists`/`PathExists`; `invalid reference` -> `RefNotFound`), anything else is `Other` with the raw stderr in the error details.
- Parsers (`parse_worktree_list`, `parse_status`, `parse_branches`) are pure functions with fixture tests; fixtures are real git output.

### 3.16 Registry operations and service surface

`registry/` stays pure state; the IO-driven logic lives in a new daemon module `instances/` (proposal in §9): `watcher.rs` (tick), `derive.rs` (3.5), `resolve.rs` (3.9), `prepare.rs` +
`files.rs` + `deps.rs` (3.10-3.11), `remove.rs` (3.13). Logical registry operations (actor commands or methods under a lock — `registry/` chooses; each emits events after commit):

| Operation | Purpose |
|---|---|
| `register_project_repo(project, probe)` / `unregister_project_repo` | create/attach `RepoEntry`; set `Project.repo`; init primary record + stamp baseline |
| `apply_worktrees(repo, Vec<Worktree>) -> Vec<Change>` | 3.5; allocate slugs; mark missing; emits `InstanceUpdated`/`InstanceRemoved` |
| `allocate_slug(project, desired, kind) -> Slug` | 3.8 collision logic |
| `begin_instance(project, slug, path, kind, pinned) -> InstanceId` | creates the `Preparing` instance before the worktree exists; `rollback_instance(id)` on failed create |
| `set_pinned(id, bool)` | pin/unpin |
| `record_started(id)` | runtime hook: sets `first_started`/`last_started`, auto-pins |
| `record_prepared(id, SetupStamp, materialized)` | step 9 |
| `rename_instance(id, new_slug)` | 3.8, Stopped only |
| `remove_instance(id)` | after removal or purge |
| `relocate_project(project, new_root)` | re-probe, `git worktree repair` (run in the new location), keep ids; used when the primary moved |
| `instances(filter)` / `checkouts(repo)` / `routable()` / `checkout_index()` | reads |

Service methods that REST/MCP/CLI call (route and flag names are 15/16's choice; suggestions in the right column):

| `InstanceService` method | Suggested REST | Suggested CLI |
|---|---|---|
| `list_checkouts(project, refresh)` | `GET /api/projects/{id}/checkouts` | `lantern wt ls` |
| `branches(project)` | `GET /api/projects/{id}/branches` | `lantern wt branches` (also shell completion) |
| `run_branch(RunBranchRequest)` | `POST /api/projects/{id}/instances` (202 + `OperationId`) | `lantern on myapp --branch x [--new] [--base b] [--detach] [--pr n]` |
| `pin(id)` / `unpin(id)` | `POST /api/instances/{id}/pin`, `.../unpin` | `lantern wt pin/unpin` |
| `rename_instance(id, slug)` | `POST /api/instances/{id}/rename` | `lantern wt relabel` |
| `setup(id, force)` | `POST /api/instances/{id}/setup` | `lantern setup <instance>` |
| `removal_check(id)` | `GET /api/instances/{id}/removal-check` | first step of `lantern wt rm` |
| `remove_checkout(id, opts)` | `DELETE /api/instances/{id}?force=&delete_branch=&stop=` | `lantern wt rm [--force] [--delete-branch]` |
| `gc_suggestions(project)` | `GET /api/projects/{id}/gc` | `lantern wt gc [--apply] [--fetch]` |
| `hint_path(path)` | (internal, from 12) | |

For MCP (15): the v1-compatible `start_project(name)` keeps working (primary instance). The instance-aware extension is `start_project{ name, branch?, create? }`, which calls `run_branch`; the response must
return the actual `domain`/`url` because a reused checkout can have a different domain than `<branch-slug>.<project>.<tld>`.

---

## 4. Interfaces

### 4.1 Exposed by this module

- `InstanceService` (3.16), `ensure_prepared` (3.10), `lantern_core::slug::*` and `InstanceId` helpers (3.8), `git::*` (3.15), registry operations (3.16).
- Events emitted: `InstanceUpdated`, `InstanceRemoved`, `OperationProgress`, `LogLine{Stream::Setup}`. `ProjectUpdated` when `Project.repo` changes.
- `Instance` fields this module populates beyond the core doc: `checkout.git_root`, `checkout.locked`, `checkout.missing`, `setup_stale` (§9).

### 4.2 Assumed from other modules

| From | Interface assumed |
|---|---|
| 10-runtime | `runtime.start(id)` calls `instances.ensure_prepared(id, ctx)` before port lease; calls `registry.record_started(id)` after the first successful start; `runtime.stop(id)` on a `Preparing` instance cancels the prepare; `runtime.stop(id, reason)` is called by this module when a checkout goes missing or is removed; a **one-shot runner**: `OneShotRunner::run(OneShotSpec { cwd, shell_cmd: String, env: BTreeMap, timeout, cancel, on_line: FnMut(Stream, &str) }) -> Result<ExitStatus>` that applies the same shell/toolchain (nvm/fnm/volta, uv, PATH) resolution as `run.cmd` and kills the whole process group on cancel; unit names `lantern-inst-<InstanceId::safe_name()>`; env assembly order base < `run.env` < `Prepared.worktree_env`; for shell-command contexts, template variables are exported as env vars rather than substituted into the string (recommended, §6). |
| 12-discovery | consumes `registry.checkout_index()` (4.3); calls `hint_path` for unknown worktrees. |
| 13-system | consumes `registry.routable()` / events; builds PHP/static roots from `checkout.path`; `Settings.tld`; per-instance exact-host routes with `tls internal`; may add a per-project wildcard catch-all (3.14). |
| 14-config | `Manifest::load(dir) -> Result<Option<Manifest>, ManifestError>` (None = no file) with `setup: Option<Vec<String>>` and `worktree: Option<WorktreeSection{copy, link, env}>`; validation of template names per context (3.3); `Settings` section `[worktrees]` (below); `state.json` sections `repos` and `checkouts` (3.2); workspace scanner uses `git.probe` and skips linked worktrees (3.6); `Detector` for fallback `Detected` manifests. |
| 15-api-mcp | exposes 3.16 methods; `GET /api/operations/{id}` (steps + status, last ~50 kept in memory) and a cancel endpoint, backed by `events::OperationHandle { id, progress(step), cancel_token(), finish(Result) }`; adds `visible` to instance JSON and `?include=hidden`. |
| 16-clients | shows `Preparing` step labels from `OperationProgress`; branch-changed badge when `slug(id) != slug(checkout.branch)`; `setup_stale` hint; pin/unpin; "Branches and checkouts" panel; removal dialog from `RemovalCheck`. |

`[worktrees]` settings in `config.toml` (all optional):

```toml
[worktrees]
dir = "~/.local/share/lantern/worktrees"   # default $XDG_DATA_HOME/lantern/worktrees
clone_deps = "never"                       # "never" | "reflink"
max_concurrent_setups = 2
setup_timeout_secs = 900
gc_hint_days = 30
missing_retention_days = 30
```

### 4.3 `CheckoutIndex` for 12-discovery

```rust
pub struct CheckoutIndex { /* immutable snapshot, Arc-swapped on change */ }
impl CheckoutIndex {
    /// cwd -> deepest matching checkout. Includes hidden and unpinned instances.
    pub fn lookup(&self, cwd: &Path) -> Lookup;
}
pub enum Lookup {
    Instance(InstanceId),               // cwd is inside some <checkout.path>
    RepoRoot(Vec<InstanceId>),          // cwd is inside a worktree but in no project dir (e.g. monorepo root): candidates, one per project
    None,
}
```

Matching is by longest path prefix, so a managed or user worktree nested inside the primary directory (some tools create `<repo>/.worktrees/x`) matches its own instance, not the primary. Paths are canonical; the index
is rebuilt on every change from `apply_worktrees`.

---

## 5. Failure modes & edge cases

| Situation | Behaviour |
|---|---|
| `git` missing or < 2.36 | Worktree features disabled; primary-only mode; `lantern doctor` and a `SystemHealth` entry say why. Probe failures never block project registration. |
| Project not in a git repo | Single primary instance; `422 not_a_git_repo` for worktree ops. |
| Project inside a repo but registered path is a subdirectory | Monorepo handling (3.6/3.7). |
| **Bare repo** | `list` shows a first record with `bare` and no HEAD; it is skipped (no instance). Projects live in the linked worktrees (the popular `myapp.git` + `myapp/main` layout); all git calls use `--git-dir`, so they work. A bare clone has no `refs/remotes/*`: only local branches are offered and Lantern does not fetch it. Managed worktrees still go under `worktrees_dir`. |
| **Submodules (in the repo)** | A new worktree has submodule dirs empty (verified); step 3 runs `git submodule update --init --recursive`. Removal needs `--force` internally (verified), after Lantern's own dirty check. |
| **Project root inside a submodule** | Treated as its own repo (its common dir is `.git/modules/<x>` of the superproject). Supported on a best-effort basis; worktree add failures are surfaced verbatim. |
| **Locked worktree** | Fully usable as an instance; shows the lock reason; cannot be removed or pruned by Lantern (409 `locked`). |
| **Worktree on another filesystem** | Works. Warning in the pipeline log: package-manager links degrade to copies; reflink clone disabled for that checkout. Setting `worktrees.dir` to the repo's filesystem avoids it. Paths are compared by `st_dev`. |
| **Worktree path is not valid UTF-8** | Listed in the checkouts view with a warning, but no instance is created (state.json and URLs are UTF-8). |
| Branch name has `/`, `.`, uppercase, emoji | Slug rules (3.8); the raw name is only used as an argv item and in `${BRANCH}`. |
| Slug collision (`feature/x` vs `feature-x`) | Numeric suffix, persisted (3.8). |
| Branch deleted upstream | Instance keeps working; `UpstreamGone` gc suggestion. |
| **Manifest differs per branch** | Per-checkout resolution (3.3). Changing it takes effect on the next start. |
| **Manifest missing on a branch** | Primary's manifest is used. |
| Manifest invalid on a branch | `NeedsConfig` with the parse error. |
| **Primary checkout moved** | The registry sees the primary path missing: project flagged `missing`, all its checkouts `missing` (the linked worktrees' `.git` files still point at the old location, so git fails inside them; verified: `fatal: not a git repository`). The user gives Lantern the new path (a "relocate project" action; name is 15/16's) -> `relocate_project` re-probes and runs `git worktree repair` in the new location (verified: after `mv repo repo2`, `git -C repo2 worktree repair` re-links the linked worktrees). Ids, pins and stamps are kept. Workspace scanning (14) may trigger the same action when it finds the project's `id` at a new path, as v1 did (`manager.ex:576`). |
| **Primary checkout deleted** (non-bare: this deletes `.git` and every worktree's admin data) | Project is kept flagged `missing`, its checkouts are marked `missing` and stopped; Lantern does not delete managed dirs (the user may recover from backup). The user can deregister or relocate. For bare-repo layouts the common dir survives, so linked worktrees keep working and one of them can be made primary by re-registering. |
| Worktree directory deleted by hand | `prunable` -> managed: instance removed and `git worktree prune`; user worktree: `missing`, record kept 30 days. |
| Worktree directory moved by hand | Old path `missing`, new path appears as a new (unpinned) instance (3.8). |
| Daemon restart mid-`git worktree add` | `phase = Creating` -> managed dir force-removed on next start and recreated. |
| Daemon restart mid-setup | Instance `Stopped`, message "interrupted"; setup reruns on next start. |
| Two "run feature-x" at once | Repo lock + step 2 of the resolver: second call finds the first's worktree and reuses (or joins the in-flight op). |
| `git fetch` hangs on credentials | `GIT_TERMINAL_PROMPT=0`, stdin null, 60 s timeout, `kill_on_drop`; error surfaces `fetch_failed`. |
| Out of disk during checkout/install | The git/setup error is reported verbatim; no partial-state cleanup beyond rolling back an unfinished `worktree add`. |
| Install lifecycle scripts modify shared git config (e.g. husky sets `core.hooksPath`) | Config is shared by all worktrees of the repo; nothing Lantern can fix. Mentioned in docs. |
| Hidden instance receives a request (`lantern on myapp@old`) | Works (ids exist for every known worktree); starting auto-pins. |

---

## 6. Security considerations

- **Running a branch runs its code.** `setup`, dependency lifecycle scripts, and the dev server execute with the user's privileges, exactly as if the user had done `git checkout && npm install && npm run dev`. Lantern
  adds no sandbox in v2.0. It makes one explicit exception: **fork PRs** (`isCrossRepository`) need `confirm_foreign_code: true` (3.9.3). Branches in your own repo are treated as trusted.
- **No shell injection through branch names.** Valid git branch names may contain `;`, `$`, `(`, `)`, `&`, `|`, `>`, `'`, `"` and backticks (verified with `git check-ref-format --branch`: `x$(id)`, `a;b` and ``a`id` `` are accepted; whitespace, `\`, `*`, `~`, `^`, `:`, `?`, `[`, `..`, `@{` and a leading `-` are refused). Therefore: all git calls use argv arrays; `setup` strings are never textually
  substituted (variables arrive as environment variables, expanded by the shell from data); `worktree.env` values are substituted textually only because they go to the environment, not a shell. 10-runtime should follow the
  same rule for `run.cmd` (v1 substituted text, `projects/project.ex:152-158`). Branch names starting with `-` are rejected (`git check-ref-format --branch`) and positional refs/paths follow `--`.
- **Path traversal and symlink escape** in `copy`/`link`: no `..`, no absolute patterns, canonicalize every source and require it inside the repo's git root; never follow symlinks while copying directories; never write outside the
  checkout (destination canonicalized parent must be inside `checkout.path`).
- **Deletion safety**: removal only for `Managed`, containment check on `worktrees_dir`, `.git` file check, no `-f -f`, symlinks removed not followed (verified), no automatic deletion anywhere.
- **Git environment hygiene**: scrubbed `GIT_*` variables so a polluted daemon environment cannot redirect operations to another repository; `GIT_OPTIONAL_LOCKS=0` for reads so Lantern cannot wedge the user's `index.lock`.
- **`worktree.copy` copies secrets** (`.env`) into another directory: documented; the managed dir is under the user's data dir (mode from umask; 0700 recommended and set on `<worktrees_dir>`).
- Setup logs may contain secrets echoed by commands; they live in the user journal (readable only by the user and admins) and are not served over MCP by default (15 decides).

---

## 7. Testing strategy

**Unit (pure, no git)**
- `parse_worktree_list`: fixtures from real git output: normal, detached, bare, `locked` with/without reason, `prunable`, path with spaces and with an embedded newline (`-z`), non-UTF-8 path, empty output.
- `parse_status` (v2 porcelain with renames `2`, unmerged `u`, untracked `?`, branch headers with/without upstream), `parse_branches` (including `gone`, empty upstream).
- `slugify`/`branch_slug`: property tests (charset `[a-z0-9-]`, no `--`, no leading/trailing `-`, length <= 40, deterministic, idempotent on its own output); table tests for `feature/login-form`, `Fix/Bug#12`, `release/1.2`, emoji, very long names, names differing only in punctuation.
- Slug allocation: collisions, revival of a missing record, batch ordering determinism, domain-collision with a manifest `domain:` override.
- `instance_domain`: default TLD, override, alias prefixing, length limit.
- Visibility rule table; claimed/unclaimed re-derivation table (3.8).
- Glob copy/link: wildcard vs literal, `/`-prefix repo-root patterns, `..` and absolute rejection, symlink skip, no-overwrite, limits, `.git` exclusion.
- `lock_hash`, default-setup detection per fixture directory (pnpm workspace root lockfile for `apps/web`, yarn classic vs berry, Laravel Composer+npm ordering, `packageManager` precedence).
- Removal-check decision table incl. `materialized` exclusion and attached/detached/no-remote variants.

**Integration (real `git` in temp dirs; the experiments behind this doc become test cases)**
- Repo + linked worktrees: derive instances; add/remove/lock/`mv`/`rm -rf`; branch switch/rename/detach; fingerprint changes in each case and does not change when idle.
- Monorepo with two projects: one `run_branch` creates one worktree and two instances; branch lacking one subpath.
- `run_branch` matrix: local, remote-only (single and multiple remotes), new branch (with/without remote), already checked out elsewhere, primary on the branch, `detach`, tag, nonexistent (suggestions), invalid name, name with `$()`.
- Bare repo layout; submodule repo (init on create, force-on-remove path); locked worktree; worktree dir on a second tmpfs (st_dev warning).
- Removal: clean, modified, untracked, materialized symlink (not dirty), unpushed, detached orphan, submodule, running, locked, `delete_branch` merged/unmerged; verify the symlink target survives.
- Pipeline with stub commands: success; failing command (no stamp, message set, retry resumes); `sleep` cancelled mid-step (process group dead, worktree kept); lockfile change triggers rerun; `setup: []`; daemon "crash" between `Creating` and `Ready`.
- Concurrency: two parallel `run_branch` for one branch; setup semaphore limits to 2.

**Manual**
- GUI: appearance/disappearance of hidden instances, branch-changed badge, progress label stream, removal dialog.
- Real stacks: Laravel + php-fpm worktree (`.env` copy, `worktree.env` DB), pnpm monorepo, uv/FastAPI, Vite static; btrfs loopback for reflink clone.
- Caddy spike for nested hosts (3.14) and a user worktree created by another tool inside the primary directory.

---

## 8. Open questions (with recommendations)

| # | Question | Recommendation |
|---|---|---|
| Q1 | Add an `InstanceStatus::PrepareFailed`? | **No** in v2.0. `Stopped` + `runtime.message` + the retained operation record is enough; revisit if the GUI cannot distinguish "never started" from "failed". |
| Q2 | Auto-run `setup` on the primary after its lockfile changes? | **No**; badge + `lantern setup`. |
| Q3 | A `worktree.teardown: [cmds]` key (run before removal, same env) so `dropdb "$DB_NAME"` can run? | **Yes, in v2.1.** v2.0 prints the resolved `worktree.env` in the removal summary instead. |
| Q4 | Cross-instance references for monorepos (`${SIBLING_URL:api}`)? | **Defer.** The `https://${BRANCH_SLUG}.api.glow` convention covers it today. |
| Q5 | Support `--pr <n>`? | **Yes**, git-only checkout, after the core run-branch path ships (3.9.3). |
| Q6 | Add `notify` as a wake-up hint? | Only if users report latency; the design allows it without other changes. |
| Q7 | Default `worktrees.dir` on the repo's filesystem when `$XDG_DATA_HOME` is on another device? | Keep the core default; warn on `st_dev` mismatch (done); expose the setting. |
| Q8 | Should `lantern wt rm` also offer to delete the remote branch? | **No.** |
| Q9 | Should unpinned `Worktree`-kind checkouts of *other tools* (e.g. agent worktree dirs) be hideable entirely (ignore patterns)? | Add a `worktrees.ignore_globs` setting only if the checkouts panel gets noisy. |
| Q10 | Default clone of `node_modules` on reflink-capable filesystems? | Keep **off** until measured; users on btrfs/XFS can turn it on. |

---

## 9. Proposed changes to core architecture

1. **Project/Checkout types (§6).** `Project.is_git: bool` -> `Project.repo: Option<ProjectRepo { common_dir: PathBuf, git_root: PathBuf, subpath: PathBuf }>`; clarify `Project.root` = the project directory of the primary
   checkout (possibly a subdirectory of the repo). `Checkout.path` = project directory inside that checkout (`<git_root>/<subpath>`). Add `Checkout.git_root: Option<PathBuf>`, `Checkout.locked: Option<String>`,
   `Checkout.missing: bool`. Add `Instance.setup_stale: bool`.
2. **Managed worktree path (§7).** Key by repo, not project: `$XDG_DATA_HOME/lantern/worktrees/<repo-slug>/<instance-slug>/`, where `<repo-slug>` is the `ProjectId` of the first project registered in that repo (identical to the core text for
   single-project repos). Needed so a monorepo gets one worktree per branch.
3. **`InstanceId::safe_name()`** = `myapp` / `myapp_feature-x` (§3.8) as the definition of "filesystem-safe form" for `${INSTANCE}`, unit names, log dirs. `@` cannot be used in systemd unit names.
4. **Template variables (§6).** Add `${BRANCH_SNAKE}` (slug with `-` -> `_`, for SQL identifiers). Define `${BRANCH}` for a detached HEAD as the 7-char sha. ~~State that `${PORT}` is not available in `setup` or `worktree.env`.~~ (Superseded: `02` D1 makes `${PORT}` available everywhere.) State that in shell-command
   contexts (`setup`, and recommended for `run.cmd`/`deploy.*`) variables are exported as environment variables of the same name instead of textually substituted (injection safety).
5. **Events (§9).** Add `Stream::Setup` to the log `Stream` enum so setup output is a normal log line.
6. **Code layout (§4).** Add `crates/lantern-daemon/src/instances/` (watcher, derive, resolve, prepare, files, deps, remove). `registry/` stays pure state; `git/` stays the command wrapper.
7. **Workers (§8).** The "git watcher" is a stat-fingerprint-gated poll with on-demand refresh, not an unconditional periodic `git worktree list`; interval 5 s while any client is subscribed, 30 s otherwise.
8. **Scanner rule (§2/14).** Workspace scanning must not register a *linked worktree* of a known repo as a project (3.6).
9. **Auto-start (v1 carry-over).** "Auto-start" and profile `auto-start` apply to primary instances by default; non-primary instances are included only when named by `InstanceId`.
10. **Persistence (§7).** `state.json` gains `repos` and `checkouts` sections (3.2) and a `[worktrees]` settings section (4.2).
