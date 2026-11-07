# 15 — REST API, SSE, MCP server, Intel

Status: draft for review. Builds on `01-architecture.md` (authoritative; this doc follows its glossary and
type names). Facts about third-party crates were checked against source on 2026-11-06:
**rmcp 3.9.0** (read from the `modelcontextprotocol/rust-sdk` repo, tag `v3.9.0`), **axum 0.8.9**,
**reqwest 0.13**. Client config formats were checked against the Claude Code, Codex and Cursor docs on the same date.
v1 paths are relative to `.reference/lantern-v1/`.

---

## 0. Plain-language summary

The daemon listens on `127.0.0.1:4777` and speaks three things through one axum server:

1. **REST** under `/api/...` (JSON). The CLI, GUI and tray use it. Starting a branch is
   `POST /api/projects/myapp/instances {"branch":"feature-x"}`. Anything slow returns `202` plus an
   *operation* you can poll or watch.
2. **SSE** at `/api/events`. One long-lived connection that first sends the full current state, then every change.
   If a client falls behind, the daemon tells it to reconnect (which re-sends the state). No replay logic.
3. **MCP** at `/mcp` for AI agents (Claude Code, Codex, Cursor, ...). All 17 v1 tools keep their names and
   arguments; they now act on the *primary* checkout by default, and accept `branch` or `instance` to act on
   another one. Four new tools: `list_instances`, `start_branch`, `list_listeners`, `get_operation`.
   MCP runs **statelessly** (no sessions). That removes v1's worst failure mode: after a daemon restart every
   agent call hung until the user ran `/mcp` (`TESTING-MCP.md`).

Plus **intel**: the daemon reads each checkout's docs (README, AGENTS.md, ...) and API descriptions
(OpenAPI) so agents can ask "how does this tool work?" and "what endpoints does it have?".

Security is simple: reject any request whose `Host` or `Origin` is not ours, send no CORS headers, serve repo
files only as inert text, and let `call_tool_api` reach registered projects only. No token in v2.0.

---

## 1. Purpose & scope

Owns: the full REST + SSE surface, the `lantern-core::api` wire types, the local-API security layer, the MCP server
(tools, resources, prompts, long-operation handling), intel (docs/OpenAPI ingestion and the `/<project>/docs`
pages), `call_tool_api` proxy rules, and what `lantern mcp install` writes.

Not in scope (owner): process supervision, readiness, logs storage (`10-runtime`); worktree and branch
mechanics (`11-instances`); listener scanning and adoption logic (`12-discovery`); Caddy/DNS/services/systemd
(`13-system`); manifest and settings schemas, templates, profiles (`14-config`); CLI command surface, GUI,
tray, client reconnect (`16-clients`). Where I need something from them, section 4 states the interface I assume.

---

## 2. v1 behavior audit

### 2.1 What v1 exposed, and where it goes

| v1 (`daemon/lib/lantern_web/router.ex`) | v2 |
|---|---|
| `GET/POST /api/projects`, `GET/PUT/PATCH/DELETE /api/projects/:name` | `GET/POST /api/projects`, `GET/PATCH/DELETE /api/projects/{id}`. PUT dropped (PATCH is a merge). `new_name` rename dropped (ids are stable; re-register instead). |
| `POST /api/projects/scan` | `POST /api/projects/scan`, now **additive only** (v1 scan dropped manual projects: `RUNTIME_TESTING_AND_REGRESSIONS.md` regression A). |
| `POST .../activate`, `.../deactivate`, `.../restart` | `POST /api/instances/{id}/start`, `/stop`, `/restart` (the primary instance id equals the project id). |
| `POST .../reset` | `POST /api/projects/{id}/reload`. |
| `GET .../logs` (SSE of one project) | `GET /api/instances/{id}/logs` (snapshot) and `GET /api/events?logs={id}` (follow). |
| `GET .../endpoints`, `.../discovery`, `POST .../discovery/refresh`, `POST /api/projects/discovery/refresh` | `GET /api/instances/{id}/endpoints`, `GET .../intel`, `POST .../intel/refresh`, `POST /api/intel/refresh`. |
| `GET .../dependencies`, `.../dependents`, `GET /api/dependencies` | `GET /api/dependencies[?project=]`. |
| `GET /api/tools`, `/api/tools/:id`, `/api/tools/:id/docs` | **Dropped.** `GET /api/projects?kind=tool,capability,service,website` plus `GET /api/instances/{id}/intel` carry the same data. |
| `POST .../deploy/{start,stop,restart}`, `GET .../deploy/{logs,status}` | **Dropped.** `deploy.*` overrides are applied inside runtime start/stop/logs (`10-runtime`). |
| `GET .../docs`, `.../docs/*file` | `GET /api/instances/{id}/docs`, `.../docs/{*path}`. |
| `GET /api/health`, `GET .../health`, `POST .../health/check` | `InstanceView.runtime.health`, `GET /api/system/health`, `POST /api/instances/{id}/health/check`. |
| `GET /api/ports` | `GET /api/ports` (new shape, section 3.3). |
| `GET /api/services`, `GET /:name/status`, `POST /:name/{start,stop}` | `GET /api/services`, `GET /{name}`, `POST /{name}/{start,stop,restart}`. |
| `GET /api/system/health` | same path. |
| `POST /api/system/init` | **Dropped.** Needs root; `lantern init` does it outside the daemon (`13-system`). |
| `POST /api/system/shutdown` | `POST /api/system/stop-all` (stops dev servers, optionally services). It never exits the daemon; systemd does that. |
| `GET/PUT /api/system/settings` | `GET/PATCH /api/settings`. |
| `/api/templates` CRUD, `/api/profiles` list + activate | Same, plus `GET /{name}`, profile `PUT/DELETE`. |
| `GET /` | `GET /api`. |
| `GET /:project/docs[/*file]` (Lighthouse, also at `lighthouse.glow`) | Kept at `/{project}/docs[/{*path}]` on any allowed host. The separate `lighthouse.<tld>` host is dropped. |
| Phoenix channels `project:lobby`, `project:<n>`, `services:lobby`, `system:health` | **Dropped.** Replaced by `GET /api/events`. |
| `forward /mcp` (Hermes, sessions) | `/mcp` (rmcp 3.9, stateless). |
| LiveDashboard `/dev/dashboard` | Dropped. |

### 2.2 Defects found in v1 that this design fixes

1. **Stale MCP sessions hang forever.** `TESTING-MCP.md`: "After restarting the daemon the MCP session becomes stale ... every MCP tool call will hang indefinitely." Fix: stateless MCP (3.7.1).
2. **`mix patch_hermes`** (`mix/tasks/patch_hermes.ex`) patched Hermes' `GenServer.call` timeout from 5s to 60s, because tool calls longer than 5s hung then failed. The lesson is not a Rust problem, but its rules carry over: no hidden short internal timeout under a longer tool deadline; every handler has an explicit deadline (3.7.5); long work is an operation, not a held-open call.
3. **`call_tool_api` URL built by string concatenation** (`mcp/tools/call_tool_api.ex`, `build_url`: `base <> path`). `path: "@evil.com/x"` turns `http://127.0.0.1:4100` into a userinfo URL for `evil.com`; no validation of the destination host, port or resolved IP; `base_url` from a repo-controlled manifest can point at Caddy's admin API or the daemon itself (v1 even self-registered the daemon as a callable tool, `self_registration.ex`). Fix: section 6.3.
4. **`risk`, `requires_confirmation`, `max_concurrent` were never enforced.** They are parsed (`config/lantern_yml.ex` `:routing`) and shown (`tool_controller.ex`) and that is all. Fix: 6.4.
5. **Any file in the project dir was readable** through `DocServer.read` (`projects/doc_server.ex`: path-traversal check only), including `.env`; and `.html` was served as `text/html` on the API origin (`doc_controller.ex`, `lighthouse_controller.ex`), so a repo's HTML would run script same-origin with the API. Fix: only listed docs are readable, served inert (3.8.6).
6. **Discovery ran inside the registry GenServer call** (`manager.ex` calls `Discovery.enrich` on register/update/scan, with 30s call timeouts) and probed every candidate OpenAPI URL of stopped services, producing error noise. Fix: intel is a separate background component; stopped instances are skipped (3.8).
7. **`async` flag hidden inside the JSON body string** of `call_tool_api` (`async_requested?`) and also forwarded upstream. Fix: first-class `async` argument; the old form still works (3.7.4).
8. **InFlight/Timeout machinery** (`mcp/in_flight.ex`, `tools/timeout.ex`) existed because Hermes did not cancel tool tasks. rmcp passes a cancellation token to every handler, so InFlight is not needed (3.7.5).
9. Pagination, auth, CORS: none in v1; `GET /api/projects` returned the full 30-field project map for each project.

### 2.3 Keep / change / drop

Keep: tool, resource and prompt names and argument names; `docs`, `docs_auto`, `endpoints`, `api_auto`, `routing` manifest keys and defaults (patterns, `max_files` 100, `max_bytes` 1 MiB, candidates, `timeout_ms` 2500, `max_endpoints` 400, 10-minute refresh); the "manual entries win over discovered" merge; risk inference from the HTTP method; async-job pattern (as operations).
Change: instance awareness; operations instead of blocking 45-65s calls; snapshot-then-diff SSE instead of Phoenix channels; error envelope; security.
Drop: self-registration, `lighthouse.glow`, deploy/* routes, WebSocket channels, `/api/tools*`, stdio bridge (`mcp-remote`).

---

## 3. Design

### 3.1 Conventions (all REST endpoints)

- Base `http://127.0.0.1:4777`. `Content-Type: application/json; charset=utf-8` both ways, except logs/docs text responses.
- **Success** returns the resource itself, no `{"data": ...}` wrapper. **Lists** are `{"items": [...]}` (no pagination: tens of items; logs use `tail`/`since`).
- **Errors**: `{"error":{"code":"snake_case","message":"human text","details":{}}}` with the HTTP status from the table below. `message` is for people; clients branch on `code`.
- **Ids in paths**: `ProjectId`/`InstanceId` as-is; `@` may be percent-encoded (`%40`) or not. `GET /api/instances/myapp` is the primary instance of `myapp` (core: `InstanceId == ProjectId`).
- **Timestamps**: RFC 3339 UTC with milliseconds, `2026-11-06T17:52:03.412Z`.
- **Request bodies** must be `application/json` (else `415`; requests with an empty body need no `Content-Type`), at most 1 MiB (else `413`). Unknown JSON fields are ignored; missing optional fields take the documented default.
- **Idempotent verbs**: `start` on a running instance and `stop` on a stopped one succeed with `200` and `operation.noop = true`.
- **Versioning**: paths are unversioned; `GET /api` returns `api_version` (integer, starts at 1). `lantern-client` refuses a daemon whose `api_version` is higher than it was built for.
- **Every call into another module has a deadline** (default 15s, `tokio::time::timeout`). On expiry: `504 timeout`, and the log line names the module.

| code | HTTP | when |
|---|---|---|
| `bad_request` / `invalid_json` | 400 | malformed query, path id or JSON |
| `validation_failed` | 422 | well-formed but invalid; `details.fields = {"path":"not a directory"}` |
| `not_found`, `project_not_found`, `instance_not_found`, `service_not_found`, `operation_not_found`, `doc_not_found` | 404 | |
| `already_exists`, `operation_in_progress`, `cannot_remove_primary`, `dirty_worktree`, `port_unavailable`, `builtin_template`, `reserved_id` | 409 (`reserved_id`: 422) | `operation_in_progress` carries `details.operation` |
| `needs_config`, `branch_invalid` | 422 | |
| `forbidden_host`, `forbidden_origin` | 403 | section 6.1 |
| `unsupported_media_type` / `payload_too_large` / `method_not_allowed` | 415 / 413 / 405 | |
| `timeout` | 504 | module deadline or `wait` expired for an operation that then failed to start |
| `unavailable` | 503 | daemon starting or shutting down; too many SSE streams |
| `operation_failed`, `internal` | 500 | |

### 3.2 `lantern-core::api` (shared by daemon and `lantern-client`)

Plain `serde` types, no tokio. Types marked (core), (14), (12), (13) come from the core doc or another subsystem doc.

```rust
pub const API_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)] pub struct List<T> { pub items: Vec<T> }
#[derive(Serialize, Deserialize)] pub struct ErrorEnvelope { pub error: ApiError }
#[derive(Serialize, Deserialize)] pub struct ApiError {
    pub code: String, pub message: String, #[serde(default)] pub details: serde_json::Value }

// ---------- projects & instances (what REST and SSE actually carry)
pub struct ProjectSummary {                       // GET /api/projects items
    #[serde(flatten)] pub project: Project,       // (core)
    pub primary: InstanceId, pub status: InstanceStatus, pub url: String, pub port: Option<u16>,
    pub instance_count: u32, pub docs_count: u32, pub endpoints_count: u32,
    pub routing: Option<RoutingMeta> }
pub struct ProjectDetail { #[serde(flatten)] pub summary: ProjectSummary,
    pub instances: Vec<InstanceView>, pub depends_on: Vec<ProjectId>, pub manifest: ManifestSummary }
pub struct InstanceView {                         // Instance minus the full ResolvedManifest
    pub id: InstanceId, pub project: ProjectId, pub checkout: Checkout, pub domain: String,
    pub url: String, pub pinned: bool, pub runtime: RuntimeState, pub manifest: ManifestSummary }
pub struct ManifestSummary { pub kind: ProjectKind, pub description: Option<String>, pub tags: Vec<String>,
    pub run_cmd: Option<String>, pub health_endpoint: Option<String>, pub routing: Option<RoutingMeta> }
pub struct RoutingMeta { pub triggers: Vec<String>, pub risk: Option<Risk>,
    pub requires_confirmation: bool, pub max_concurrent: Option<u32>, pub agents: Vec<String> }
#[serde(rename_all = "snake_case")] pub enum Risk { Low, Medium, High, Critical, Unknown }  // ordered

pub struct RegisterProjectRequest { pub path: PathBuf, pub id: Option<ProjectId>, pub name: Option<String>,
    pub enabled: Option<bool>, pub overrides: Option<ManifestOverrides> }   // ManifestOverrides (14)
pub struct PatchProjectRequest { pub name: Option<String>, pub enabled: Option<bool>,
    pub tags: Option<Vec<String>>, pub overrides: Option<ManifestOverrides> }
pub struct UnregisterParams { pub stop: bool /*default true*/, pub remove_worktrees: bool /*default false*/ }
pub struct ScanRequest { pub roots: Option<Vec<PathBuf>> }                  // None = settings.workspaces
pub struct ScanReport { pub registered: Vec<ProjectSummary>, pub already_known: u32, pub skipped: Vec<ScanSkip> }
pub struct ReloadRequest { pub discard_overrides: bool }

// → 02 D8: the request is 11's richer form; the flat {branch, base} is accepted as shorthand.
pub struct CreateInstanceRequest {                // "run branch X"
    pub target: RunTarget,                        // Branch(String) | NewBranch{name, base: Option<String>} | Detached(String) | Pr(u32)
    pub fetch: bool /*default true*/, pub start: bool /*default true*/, pub pin: Option<bool> }
pub struct OperationResponse { pub operation: Operation, pub instance: Option<InstanceView>,
    pub created: Option<bool> /*only for CreateInstance: false = reused an existing worktree*/ }
pub struct StopAllRequest { pub include_services: bool }
pub struct WaitParams { pub wait: bool, pub timeout: Option<u32> }          // seconds, default 60, max 600
pub struct RemoveInstanceParams { pub force: bool, pub delete_branch: bool } // + WaitParams
pub struct AdoptRequest { pub port: Option<u16> }                            // needed if >1 listener matches

// ---------- operations
pub struct Operation { pub id: OperationId, pub kind: OperationKind, pub instance: Option<InstanceId>,
    pub status: OperationStatus, pub noop: bool, pub step: Option<String>, pub steps: Vec<OperationStep>,
    pub created_at: Timestamp, pub finished_at: Option<Timestamp>,
    pub error: Option<ApiError>, pub result: Option<serde_json::Value> }
#[serde(rename_all = "snake_case")]
pub enum OperationKind { Start, Stop, Restart, Prepare, CreateInstance, RemoveInstance, StopAll, ProfileActivate, ApiCall }
pub enum OperationStatus { Queued, Running, Succeeded, Failed, Cancelled }
pub struct OperationStep { pub name: String, pub status: OperationStatus,
    pub started_at: Option<Timestamp>, pub finished_at: Option<Timestamp> }

// ---------- logs, ports
pub struct LogParams { pub tail: Option<u32> /*200, max 5000*/, pub since: Option<String>, // "15m" | "2h" | RFC3339
    pub stream: Option<Stream> /*core; default both*/, pub grep: Option<String>, pub format: Option<LogFormat> }
pub enum LogFormat { Json, Text }
pub struct LogPage { pub instance: InstanceId, pub lines: Vec<LogLine>, pub buffered: u64, pub truncated: bool }
pub struct LogLine { pub ts: Timestamp, pub stream: Stream, pub line: String }
pub struct PortsView { pub instances: BTreeMap<InstanceId, PortEntry>,
    pub unmanaged: Vec<Listener>, pub conflicts: Vec<PortConflict> }        // Listener (core), PortConflict (12)
pub struct PortEntry { pub port: u16, pub status: InstanceStatus, pub health_status: String, pub url: String }

// ---------- intel
pub struct IntelSnapshot { pub instance: InstanceId, pub refreshed_at: Option<Timestamp>, pub stale: bool,
    pub docs: Vec<DocEntry>, pub endpoints: Vec<EndpointEntry>, pub discovery: DiscoveryMeta }
pub struct DocEntry { pub path: String, pub kind: String, pub source: IntelSource,
    pub exists: bool, pub size: Option<u64>, pub mtime: Option<Timestamp> }
pub struct DocContent { pub path: String, pub content: String, pub offset: u64, pub total_chars: u64, pub truncated: bool }
pub struct EndpointEntry { pub method: String, pub path: String, pub description: Option<String>,
    pub category: Option<String>, pub risk: Risk, pub body_hint: Option<String>,
    pub params: Vec<ParamHint>, pub source: IntelSource }
pub struct ParamHint { pub name: String, pub r#in: String, pub required: bool }
pub enum IntelSource { Manual, Discovered }
pub struct DiscoveryMeta {                        // same shape as v1 `discovery`, so v1 consumers keep working
    pub refreshed_at: Option<Timestamp>,
    pub docs: DocsMeta,   // { enabled, count, source_count, sources: [pattern], skipped_large, truncated }
    pub api: ApiMeta }    // { enabled, count, source_count, sources: [url|path], errors: [{source, error}] }
pub struct IntelRefreshReport { pub refreshed: u32, pub errors: Vec<IntelError>, pub took_ms: u64 }

// ---------- misc
pub struct DependencyGraph(pub BTreeMap<ProjectId, DependencyEntry>);       // { depends_on, depended_by }
pub struct SystemInfo { pub version: String, pub api_version: u32, pub pid: u32, pub boot_id: String,
    pub started_at: Timestamp, pub listen: String, pub tld: String, pub paths: SystemPaths }
// Settings, Profile, Template (14); ServiceStatus, SystemHealth (13): referenced, not redefined.
```

`WireEvent` (the SSE payload) is in 3.5. Pure helpers for `mcp install` live in `lantern-core::mcp_install` (3.9).

### 3.3 REST endpoints

Status columns list success codes first. All endpoints can also return the generic errors in 3.1.
`?wait=true&timeout=N` is defined in 3.4 and applies wherever the response type is `OperationResponse`.

**Root, system, settings**

| Method & path | Request | Response | Status / notes |
|---|---|---|---|
| `GET /api` | | `{name,version,api_version,mcp:"/mcp",links:{...}}` | 200. Cheap liveness probe: touches no module. |
| `GET /api/system` | | `SystemInfo` | 200 |
| `GET /api/system/health` | | `SystemHealth` (13) | 200 |
| `POST /api/system/reconcile` | | `{reconciled: true}` | 200. Re-runs registry reconcile + Caddy route sync. For "something is out of sync". |
| `POST /api/system/stop-all` | `StopAllRequest` + wait | `{operations: [Operation]}` | 202, or 200 with `wait=true`. One operation per running instance. |
| `GET /api/settings` | | `Settings` (14) | 200 |
| `PATCH /api/settings` | JSON merge-patch (RFC 7386) | `{settings: Settings, restart_required: ["api.port"]}` | 200; 422 with `details.fields`. Changing `api.port`/`tld` needs a daemon restart (the Host allowlist is built at startup). |

**Projects**

| Method & path | Request | Response | Status / notes |
|---|---|---|---|
| `GET /api/projects` | `kind=a,b` `tag=` `enabled=true\|false\|any` (default true) `q=` | `List<ProjectSummary>` | 200 |
| `POST /api/projects` | `RegisterProjectRequest` | `ProjectSummary` | 201; 409 `already_exists`; 422 `validation_failed` / `reserved_id` |
| `POST /api/projects/scan` | `ScanRequest` | `ScanReport` | 200. Additive: never removes or alters projects. |
| `GET /api/projects/{id}` | | `ProjectDetail` | 200; 404 |
| `PATCH /api/projects/{id}` | `PatchProjectRequest` | `ProjectDetail` | 200; 404; 422 |
| `DELETE /api/projects/{id}` | `UnregisterParams` as query | | 204; 404 |
| `POST /api/projects/{id}/reload` | `ReloadRequest` | `ProjectDetail` | 200; 422 on invalid manifest (`details` has the parse error) |
| `POST /api/projects/{id}/sync` | | `List<InstanceView>` | 200. Re-lists git worktrees now (after you ran `git worktree add` by hand). |
| `GET /api/projects/{id}/instances` | | `List<InstanceView>` | 200 (same as `GET /api/instances?project={id}`) |
| `POST /api/projects/{id}/instances` | `CreateInstanceRequest` + wait | `OperationResponse` | 202 (200 if reused and already running, or `wait` finished). Creates the worktree if there is no instance for `branch`, prepares it, starts it unless `start=false`. 422 `branch_invalid`; 409 `operation_in_progress`. |
| `GET /api/dependencies` | `project=` optional | `DependencyGraph` | 200 |

**Instances**

| Method & path | Request | Response | Status / notes |
|---|---|---|---|
| `GET /api/instances` | `project=` `status=a,b` `checkout=primary\|worktree\|managed` `q=` | `List<InstanceView>` | 200. Includes `external` (unmanaged) instances. |
| `GET /api/instances/{id}` | | `InstanceView` | 200; 404 |
| `PATCH /api/instances/{id}` | `{"pinned": bool}` | `InstanceView` | 200 |
| `DELETE /api/instances/{id}` | `RemoveInstanceParams` + wait | `OperationResponse` | 202/200. Stops, then removes the worktree of a `managed` checkout; for a `worktree` (user-made) checkout it **only ever forgets** it (unpin + hide; 200 `forgotten: true`); Lantern never deletes a user-made worktree, even with `force` (→ 02 D8). 409 `cannot_remove_primary`, `dirty_worktree` (managed only, unless `force=true`). |
| `POST /api/instances/{id}/start` `/stop` `/restart` `/prepare` | `{}` + wait | `OperationResponse` | 202/200. A repeat of the in-flight kind returns the **same** operation; a different kind returns 409 `operation_in_progress`. `prepare` re-runs setup steps without starting. 422 `needs_config`. |
| `POST /api/instances/{id}/adopt` | `AdoptRequest` | `InstanceView` | 200 (synchronous: a Caddy route add); 409 if not `external` or ambiguous (`details.ports`) |
| `POST /api/instances/{id}/unadopt` | | `InstanceView` | 200 |
| `GET /api/instances/{id}/logs` | `LogParams` | `LogPage`; with `format=text` or `Accept: text/plain`: `<ts> <stream> <line>` lines | 200 |
| `POST /api/instances/{id}/health/check` | | `HealthSummary` (core) | 200 (runs a check now, 10s cap) |
| `GET /api/instances/{id}/manifest` | | `ResolvedManifest` (14), unredacted | 200 |
| `GET /api/instances/{id}/intel` | `max_age=` seconds (default 300) | `IntelSnapshot` | 200 |
| `POST /api/instances/{id}/intel/refresh` | | `IntelSnapshot` | 200 (30s cap) |
| `GET /api/instances/{id}/docs` | | `List<DocEntry>` | 200 |
| `GET /api/instances/{id}/docs/{*path}` | `format=json` `offset=` `max_chars=` | raw text (inert content type) or `DocContent` | 200; 404 `doc_not_found` (unlisted or missing) |
| `GET /api/instances/{id}/endpoints` | `q=` `limit=` (200) | `List<EndpointEntry>` | 200 |
| `POST /api/intel/refresh` | | `IntelRefreshReport` | 200 (30s overall cap; partial results on cap) |

**Operations, listeners, services, profiles, templates**

| Method & path | Request | Response | Status / notes |
|---|---|---|---|
| `GET /api/operations` | `instance=` `status=` `limit=` (50) | `List<Operation>` | 200, newest first |
| `GET /api/operations/{id}` | `wait=true&timeout=N` | `Operation` | 200 (failed operations are still 200: this is a resource read); 404 |
| `POST /api/operations/{id}/cancel` | | `Operation` | 200; 409 if finished. Best effort. |
| `GET /api/listeners` | `matched=true\|false` `project=` `port=` | `List<Listener>` | 200 (cached snapshot) |
| `POST /api/listeners/scan` | | `List<Listener>` | 200 (scans now) |
| `GET /api/ports` | `project=` | `PortsView` | 200 |
| `GET /api/services`, `GET /api/services/{name}` | | `List<ServiceStatus>` / `ServiceStatus` | 200; 404 |
| `POST /api/services/{name}/start` `/stop` `/restart` | | `ServiceStatus` | 200 (synchronous, 30s cap); 502 `operation_failed` with systemd's message |
| `GET /api/profiles`, `GET /api/profiles/{name}`, `PUT /api/profiles/{name}`, `DELETE /api/profiles/{name}` | `Profile` for PUT | `List<Profile>` / `Profile` / 204 | 200/201/204 |
| `POST /api/profiles/{name}/activate` | + wait | `{operations: [Operation]}` | 202/200 |
| `GET /api/templates`, `GET .../{name}`, `POST`, `PUT .../{name}`, `DELETE .../{name}` | `Template` | `Template` / 204 | 409 `builtin_template` on built-in PUT/DELETE |

**Not under `/api`**: `GET /{project}/docs` and `GET /{project}/docs/{*path}` (3.8.6) and `/mcp` (3.7). The segment may be a project or an instance id (`/myapp@feature-x/docs`).
`api` and `mcp` are reserved as ids (see section 9), so these routes can never shadow `/api/*` or `/mcp`.

**Example: run a branch, wait for it**

```
POST /api/projects/myapp/instances?wait=true&timeout=120
{"branch": "feature-x"}

200 OK
{"operation": {"id":"op_3f9a1c07b2e4","kind":"create_instance","instance":"myapp@feature-x","status":"succeeded",
  "noop":false,"step":null,"steps":[
    {"name":"create_worktree","status":"succeeded","started_at":"2026-11-06T17:52:03.412Z","finished_at":"2026-11-06T17:52:04.020Z"},
    {"name":"setup: pnpm install","status":"succeeded","started_at":"...","finished_at":"..."},
    {"name":"start","status":"succeeded","started_at":"...","finished_at":"..."}],
  "created_at":"2026-11-06T17:52:03.401Z","finished_at":"2026-11-06T17:52:31.877Z","error":null,"result":null},
 "instance": {"id":"myapp@feature-x","project":"myapp","domain":"feature-x.myapp.glow","url":"https://feature-x.myapp.glow",
   "checkout":{"path":"/home/ryan/.local/share/lantern/worktrees/myapp/feature-x","kind":"managed","branch":"feature-x","head":"9c1e...","dirty":false},
   "pinned":false,"runtime":{"status":"running","port":41031,"unit":"lantern-inst-myapp-feature-x","restarts":0,"...":"..."},
   "manifest":{"kind":"project","run_cmd":"pnpm dev --port ${PORT}","...":"..."}},
 "created": true}
```

Same request without `wait`: `202 Accepted` with `operation.status = "running"`.
A failure with `wait=true`: `422` `{"error":{"code":"needs_config","message":"no run command for myapp@feature-x","details":{"operation_id":"op_..."}}}`.

> **Reconciled → 02 D8:** routes added to the table above:
> `GET /api/projects/{id}/checkouts`, `GET /api/projects/{id}/branches`,
> `POST /api/instances/{id}/pin` `/unpin` `/rename`, `GET /api/instances/{id}/removal-check` (semantics: 11);
> `GET /api/suggestions`, `POST /api/suggestions/dismiss`, `POST /api/import/v1`, `GET /api/schema/{name}`,
> `PATCH /api/projects/{id}` with `{id}` to rename (old id kept in `aliases`) (semantics: 14).
> The re-run-setup verb is `prepare` (the CLI's `lantern setup` maps to it). `ProjectId`s `api`, `mcp`,
> `lantern`, `mailpit` are reserved (D11).

### 3.4 Operations and `wait`

- Any call that may take longer than ~2s (start, stop, restart, prepare, create/remove instance, stop-all, profile activate, `call_tool_api async`) creates an `Operation` in the daemon's `OperationTracker` (assumed from 10/11, section 4) and returns `202`.
- `?wait=true` holds the HTTP request until the operation finishes or `timeout` seconds pass (default 60, max 600). Result: finished OK `200`; finished with failure the standard error envelope with `details.operation_id`, status mapped from the error code (`needs_config` 422, `port_unavailable` 409, `branch_invalid` 422, `timeout` 504, else 500); still running `202` with the current operation.
- If the HTTP client disconnects, the **operation keeps running**. Only the wait is abandoned. The same is true for MCP (3.7.5).
- Retention: finished operations stay queryable for 1 hour, at most 200 kept. Progress is also broadcast as `operation_progress` SSE events.

### 3.5 SSE: `GET /api/events`

Query parameters (all optional):

| param | meaning |
|---|---|
| `logs=<id>[,<id>...]` | also stream log lines of these instances (max 8). Without it, **no** log lines are sent. |
| `tail=N` | backlog lines per `logs` instance sent before live lines (default 100, max 1000) |
| `state=false` | skip state events and the state snapshot (log-only follow, e.g. `lantern logs -f`). Default `true`. |
| `op=<operation-id>` | with `state=false`, still deliver `operation_progress` for this operation (e.g. `lantern on --wait`). |

**Wire format.** Standard SSE. `event:` is the snake_case kind; `data:` is one line of JSON that also contains `"type"` with the same kind, so a client may ignore the `event:` line. No `id:` and no `Last-Event-ID` support (state is re-sent on reconnect instead). Keepalive: a `: ping` comment every 15s.

```rust
#[derive(Serialize, Deserialize)] #[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    Hello { api_version: u32, daemon_version: String, boot_id: String, now: Timestamp },
    ProjectUpdated { project: ProjectSummary },   ProjectRemoved { id: ProjectId },
    InstanceUpdated { instance: InstanceView },   InstanceRemoved { id: InstanceId },
    ListenersChanged { listeners: Vec<Listener> },
    ServiceUpdated { service: ServiceStatus },    SystemHealth { health: SystemHealth },
    OperationProgress { op: OperationId, instance: InstanceId, step: String, done: bool, error: Option<String> },
    LogLine { instance: InstanceId, stream: Stream, line: String, ts: Timestamp },
    SnapshotComplete,
    Resync { reason: String, skipped: Option<u64> },   // the stream ends right after this
}
```

All variants except `Hello`, `SnapshotComplete` and `Resync` mirror core `Event` one-to-one (`impl From<Event> for WireEvent`); those three exist only on the wire.

**Initial snapshot: yes.** On connect the daemon sends: `hello`; one `project_updated` per project; one `instance_updated` per instance; one `listeners_changed`; one `service_updated` per service; one `system_health`; the `logs` backlogs as `log_line`; then `snapshot_complete`; then live events. Why: a client needs exactly one connection and one code path ("apply events"), there is no GET-then-subscribe race, and recovering from anything (lag, network drop, daemon restart) is the same: reconnect. The cost is one burst of roughly 50-200 KB for tens of projects. The daemon subscribes to the event bus **before** it builds the snapshot, so nothing is lost; a duplicate (an update that is both in the snapshot and then re-delivered) is harmless because every event is a full snapshot of its object (core section 9).

**Lag.** The bus is a `broadcast` channel (capacity 1024). If a client's receiver lags, the daemon sends `resync {reason:"lagged", skipped:n}` and **closes the stream**. The client reconnects immediately (no backoff for a `resync`) and gets a fresh snapshot. This replaces core section 9's "tell it to re-fetch" with the simpler "reconnect = re-fetch" (see section 9). Log subscriptions lag independently, same rule.

**Other rules.** At most 32 concurrent SSE connections (`503 unavailable` beyond). The stream ends on daemon shutdown. Behind Caddy (`https://lantern.glow`), the route needs `flush_interval -1` (note for `13-system`); the GUI/CLI use `127.0.0.1:4777` directly and are unaffected.

```
GET /api/events?logs=myapp@feature-x&tail=2        Accept: text/event-stream

event: hello
data: {"type":"hello","api_version":1,"daemon_version":"2.0.0","boot_id":"7d1c8e5a","now":"2026-11-06T17:52:03.412Z"}

event: instance_updated
data: {"type":"instance_updated","instance":{"id":"myapp","project":"myapp","...":"..."}}

event: log_line
data: {"type":"log_line","instance":"myapp@feature-x","stream":"stdout","line":"ready on :41031","ts":"2026-11-06T17:52:30.101Z"}

event: snapshot_complete
data: {"type":"snapshot_complete"}

: ping

event: operation_progress
data: {"type":"operation_progress","op":"op_3f9a1c07b2e4","instance":"myapp@feature-x","step":"start","done":true,"error":null}
```

### 3.6 axum structure

```
lantern-daemon/src/api/
  mod.rs        router(), serve(), AppState
  guard.rs      Host / Origin / Sec-Fetch-Site middleware, response-header layer      (section 6.1)
  error.rs      ApiError + IntoResponse + From<RegistryError|RuntimeError|...>        (one match per error enum)
  extract.rs    ApiJson<T>, ApiQuery<T>, ApiPath<T>: extractors whose rejections become ApiError
  projects.rs instances.rs operations.rs listeners.rs services.rs config.rs system.rs
  events.rs     the SSE handler
  docs_pages.rs /{project}/docs (HTML index + inert files)
```

```rust
#[derive(Clone)]
pub struct AppState {
    pub registry: Registry, pub runtime: Runtime, pub instances: Instances, pub discovery: Discovery,
    pub system: SystemOps, pub config: ConfigHandle, pub intel: IntelHandle,
    pub ops: OperationTracker, pub bus: EventBus,
    pub security: Arc<Security>, pub sse_slots: Arc<Semaphore>, pub shutdown: CancellationToken,
}

pub fn router(app: AppState, mcp: McpService) -> Router {
    let api = Router::new()
        .route("/", get(system::root))                 // nested at /api: serves GET /api (axum 0.8 nest docs)
        .route("/system", get(system::info))
        .route("/projects", get(projects::list).post(projects::register))
        .route("/projects/scan", post(projects::scan))
        .route("/projects/{id}", get(projects::get).patch(projects::patch).delete(projects::unregister))
        .route("/projects/{id}/instances", get(projects::instances).post(instances::create))
        .route("/instances", get(instances::list))
        .route("/instances/{id}", get(instances::get).patch(instances::patch).delete(instances::remove))
        .route("/instances/{id}/start", post(instances::start))
        .route("/instances/{id}/docs/{*path}", get(instances::doc))
        .route("/events", get(events::stream))
        /* ... every row of 3.3 ... */;
    Router::new()
        .nest("/api", api)
        .nest_service("/mcp", mcp)
        .route("/{project}/docs", get(docs_pages::index))
        .route("/{project}/docs/{*path}", get(docs_pages::file))
        // layers: the LAST one added is the OUTERMOST. Order at runtime: Trace -> headers -> guard -> body limit -> routes
        .layer(DefaultBodyLimit::max(1 << 20))
        .layer(middleware::from_fn_with_state(app.security.clone(), guard::check))  // Host/Origin/Sec-Fetch-Site
        .layer(middleware::from_fn(guard::response_headers))                         // nosniff, no-store; also stamps 403s
        .layer(TraceLayer::new_for_http())
        .with_state(app)
}

pub async fn serve(app: AppState, mcp: McpService) -> anyhow::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], app.config.settings().api.port));
    let listener = tokio::net::TcpListener::bind(addr).await
        .with_context(|| format!("cannot bind {addr}: is another lantern daemon running?"))?;
    let shutdown = app.shutdown.clone();
    axum::serve(listener, router(app, mcp)).with_graceful_shutdown(shutdown.cancelled_owned()).await?;
    Ok(())
}
```

axum 0.8 path syntax is `{id}` / `{*rest}` (not `:id` / `*rest`). Do not put a `TimeoutLayer` on the router (it would kill SSE and `/mcp`); handlers apply their own deadlines.

Handler pattern (all lifecycle endpoints look like this):

```rust
async fn start(State(app): State<AppState>, ApiPath(id): ApiPath<InstanceId>, ApiQuery(w): ApiQuery<WaitParams>)
    -> Result<(StatusCode, Json<OperationResponse>), ApiError>
{
    app.registry.require_instance(&id).await?;                         // 404 instance_not_found
    let handle = within(&app, "runtime", app.runtime.start(&id, StartOpts::default())).await??; // joins in-flight start
    operation_response(&app, handle, &w).await
}

async fn operation_response(app: &AppState, h: OperationHandle, w: &WaitParams) -> Result<(StatusCode, Json<OperationResponse>), ApiError> {
    let op = if w.wait { app.ops.wait(h.id(), w.timeout_secs()).await } else { app.ops.get(h.id()).await? };
    let instance = match &op.instance { Some(i) => app.registry.view(i).await, None => None };
    let body = |op, instance| Json(OperationResponse { operation: op, instance, created: None });
    match op.status {
        OperationStatus::Failed if w.wait => Err(ApiError::from_operation(&op)),  // error code -> HTTP status table
        OperationStatus::Queued | OperationStatus::Running => Ok((StatusCode::ACCEPTED, body(op, instance))),
        _ => Ok((StatusCode::OK, body(op, instance))),  // succeeded, cancelled, or failed-and-not-waiting
    }
}
```

`ApiJson<T>`, `ApiQuery<T>` and `ApiPath<T>` are one-line `#[derive(FromRequest)] #[from_request(via(axum::Json), rejection(ApiError))]` wrappers (needs axum's `macros` feature) so that malformed input produces the error envelope instead of axum's plain text. A *failed* operation is data, not an HTTP error, unless the caller asked to `wait`.

---

### 3.7 MCP server

#### 3.7.1 Transport: rmcp 3.9, streamable HTTP, **stateless**

Facts verified in the rmcp source (`crates/rmcp/src/transport/streamable_http_server/tower.rs`, README):

- Crate `rmcp = "3"` (3.9.0, edition 2024, MSRV 1.88). Features needed: `server`, `macros` (both default) and `transport-streamable-http-server`. It implements MCP `2026-07-28` and stays compatible with `2025-11-25` and earlier (it negotiates; we do not restrict versions).
- `StreamableHttpService` is a tower `Service`; mount with `Router::nest_service("/mcp", service)`.
- `StreamableHttpServerConfig::legacy_session_mode` (formerly `stateful_mode`) defaults to **true**. `2026-07-28` clients are always served statelessly; setting it to `false` serves *older* clients (Claude Code, Codex, Cursor today) statelessly too: no `Mcp-Session-Id` is issued, any session header a client still sends is ignored, `GET /mcp` and `DELETE /mcp` answer `405` (allowed by the spec: "return HTTP 405 Method Not Allowed, indicating that the server does not offer an SSE stream").
- In stateless mode the `service_factory` closure runs **per request**, so the handler must be a cheap clone of `AppState`. Handlers are given a `RequestContext` whose `ct` token is cancelled when the HTTP response stream is dropped (`CancelOnDisconnect` in `tower.rs`).
- rmcp's own `allowed_hosts` defaults to `localhost, 127.0.0.1, ::1` and would reject `Host: lantern.glow`. We call `.disable_allowed_hosts()` and let our `guard` middleware (6.1) be the single place that decides Host/Origin.
- Macros: `#[tool_router]`, `#[tool]`, `#[tool_handler]`, `#[prompt_router]`, `#[prompt]`, `#[prompt_handler]`; tool fns take `Parameters<T>` (`T: Deserialize + schemars::JsonSchema`, schema generation is JSON Schema 2020-12) and may also take `RequestContext<RoleServer>`. `get_info()` returns `ServerConfig` (the old name `ServerInfo` is deprecated). Tool-level failures are `Ok(CallToolResult::error(vec![ContentBlock::text(..)]))` so the agent sees the message; `Err(McpError)` is for protocol problems and clients render it opaquely.

**Decision: stateless.** Why: (1) it removes v1's stale-session hang class entirely, since there is nothing to go stale; the daemon can restart at any time and the next call just works; (2) Lantern needs no server-to-client pushes (no `list_changed`, no subscriptions); (3) there is no session table to leak or recover. Cost: no server-initiated requests (so no MCP *elicitation* for confirmations: 6.4 uses an explicit `confirm` argument) and no `Last-Event-ID` resume. Progress notifications still work: they ride the SSE response of the POST that carries the call (rmcp switches that response from JSON to `text/event-stream` when the handler emits a notification). Even if some client turns out to require sessions, the fallback is safe: flip `legacy_session_mode` to `true`; an unknown session id then gets `404` and a spec-compliant client MUST re-initialize (spec 2025-11-25, Session Management 4).

```rust
// crates/lantern-daemon/src/mcp/mod.rs
pub type McpService = StreamableHttpService<LanternMcp, NeverSessionManager>;   // rmcp::transport::streamable_http_server::session::never

pub fn service(app: AppState) -> McpService {
    let cfg = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)               // stateless for every protocol version
        .with_json_response(true)                      // plain JSON for quick tools (curl-friendly); SSE only when progress is sent
        .disable_allowed_hosts()                       // guard.rs owns Host/Origin (6.1)
        .with_max_request_body_bytes(1 << 20)
        .with_sse_keep_alive(Some(Duration::from_secs(15)))
        .with_cancellation_token(app.shutdown.child_token());
    StreamableHttpService::new(
        move || Ok(LanternMcp { app: app.clone() }),   // per request: must be cheap
        Arc::new(NeverSessionManager::default()),      // "sessions disabled" (rmcp's README example uses LocalSessionManager; equivalent here)
        cfg)
}

#[derive(Clone)]
pub struct LanternMcp { app: AppState }

#[tool_router]                                          // generates Self::tool_router(); #[tool_handler] calls it
impl LanternMcp { /* the 21 tools, 3.7.4 */ }
#[prompt_router]
impl LanternMcp { /* 2 prompts, 3.7.7 */ }

#[tool_handler]
#[prompt_handler]
impl ServerHandler for LanternMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().enable_resources().build())
            .with_server_info(Implementation::new("lantern", env!("CARGO_PKG_VERSION")))
            .with_instructions(SERVER_INSTRUCTIONS)
    }
    async fn list_resources(&self, _r: Option<PaginatedRequestParams>, _c: RequestContext<RoleServer>) -> Result<ListResourcesResult, McpError> { /* 3.7.6 */ }
    async fn list_resource_templates(&self, ..) -> Result<ListResourceTemplatesResult, McpError> { /* 3.7.6 */ }
    async fn read_resource(&self, req: ReadResourceRequestParams, _c: RequestContext<RoleServer>) -> Result<ReadResourceResponse, McpError> {
        /* build ReadResourceResult::new(vec![ResourceContents::text(json, &req.uri)]) and `.into()` it */ }
}
```

Smoke test without any MCP client (replaces v1's `test-browser-mcp.sh` steps 1-6; no session id to track):

```bash
H='Content-Type: application/json'; A='Accept: application/json, text/event-stream'
curl -s localhost:4777/mcp -H "$H" -H "$A" -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
curl -s localhost:4777/mcp -H "$H" -H "$A" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"check_health","arguments":{}}}'
```

#### 3.7.2 Server instructions (shown to the agent at connect; Claude Code truncates at 4096 chars)

```
Lantern manages the dev servers on this machine. A *project* is a codebase (id like `myapp`); each git
checkout/branch of it is an *instance* (`myapp` = primary checkout, `myapp@feature-x` = a branch worktree) with
its own port and HTTPS name (`feature-x.myapp.glow`).

Typical flow: list_projects or list_instances -> start_project -> get_project_logs if it fails ->
get_project_docs / get_project_endpoints to learn how a tool works -> call_tool_api to use it.

Every project tool takes `name` and, optionally, `branch` or `instance` to act on a non-primary checkout;
with neither it acts on the primary. To run a branch that has no instance yet, use start_branch (creates the
git worktree, runs setup, starts it).

Slow work (worktree creation, installs, starts) returns an operation_id after `wait_seconds`; the work keeps
going. Poll with get_operation. Never guess a port: use get_ports or list_listeners.

call_tool_api reaches registered projects only. Endpoints marked risk high/critical, or projects marked
requires_confirmation, are not sent until you pass confirm=true, so ask the user first.
```

#### 3.7.3 Target resolution (shared by every project tool)

`AppState::resolve_target(name, instance, branch) -> Result<InstanceId, ToolError>`:

1. `instance` given: must exist; if `name` also names a project it must be that instance's project (else `instance_mismatch`).
2. else `name` contains `@`: treated as an instance id.
3. else `name` is a project: exact `ProjectId`, else case-insensitive display-name match, else a "did you mean ...?" error listing the closest ids.
4. `branch` given: the project's instance whose `checkout.branch == branch` or whose slug equals `slug(branch)`; none: error `No instance of 'myapp' for branch 'x'. Existing: myapp (main, running), myapp@feature-x (stopped). To run it, call start_branch.`
5. else the primary instance.

Every error ends with a suggested next call. This is the rule for all tool errors.

#### 3.7.4 Tools (21): 17 from v1 (same names), 4 new

Argument structs (the JSON Schema is derived from these with `schemars`; doc comments become `description`s). The three target fields are **repeated** in each struct, not `#[serde(flatten)]`ed, to keep the emitted schema flat and friendly to every client. Integer fields use a lenient deserializer that also accepts `"30"` (models often quote numbers). Unknown fields are ignored.

```rust
// T = target:  name: String (required) "Project name or id, e.g. `myapp`; an instance id `myapp@feature-x` also works"
//              instance: Option<String> "Instance id (project@branch). Wins over `branch`."
//              branch: Option<String>   "Branch of an EXISTING instance of this project. Omit = primary checkout."
struct ListProjectsArgs   { tag: Option<String>, kind: Option<String>, status: Option<String> }          // v1
struct SearchProjectsArgs { query: String }                                                               // v1
struct GetProjectArgs     { T }                                                                           // v1 (+T)
struct ListInstancesArgs  { project: Option<String>, status: Option<String>, branch: Option<String> }    // NEW
struct LifecycleArgs      { T, wait_seconds: Option<u32> }   // start/stop/restart; default 30/20/40, max 55   v1 (+T, wait)
struct StartBranchArgs    { name: String, branch: String, base: Option<String>, start: Option<bool> /*true*/, wait_seconds: Option<u32> /*45, max 55*/ } // NEW
struct GetOperationArgs   { operation_id: String, wait_seconds: Option<u32> /*0, max 55*/ }              // NEW
struct GetLogsArgs        { T, tail: Option<u32> /*200, max 2000*/, since: Option<String>, stream: Option<String>, grep: Option<String> } // v1 (+)
struct GetDocsArgs        { T, path: Option<String>, offset: Option<u32>, max_chars: Option<u32> /*40000*/ }  // v1 (+)
struct GetEndpointsArgs   { T, q: Option<String>, limit: Option<u32> /*200*/ }                           // v1 (+)
struct GetDiscoveryArgs   { T }                                                                           // v1 (+T)
struct RefreshArgs        { name: Option<String>, instance: Option<String>, branch: Option<String> }     // v1; none = refresh all
struct CheckHealthArgs    { name: Option<String>, instance: Option<String>, branch: Option<String>, refresh: Option<bool> } // v1 (+)
struct GetDependenciesArgs{ name: Option<String> }                                                        // v1
struct GetPortsArgs       { project: Option<String> }                                                     // v1 had none
struct ListListenersArgs  { project: Option<String>, port: Option<u16>, unmatched_only: Option<bool> }   // NEW
struct ListToolsArgs      { tag: Option<String>, status: Option<String>, kind: Option<String> }          // v1 (+kind)
struct CallToolApiArgs {                                                                                  // v1 (+)
    tool: String,                      // project name (v1 key kept). Also accepts `myapp@branch`.
    instance: Option<String>, branch: Option<String>,
    method: Option<String>,            // GET (default) POST PUT PATCH DELETE
    path: String,                      // starts with `/`, may contain `?query`
    body: Option<Value>,               // JSON string (v1) OR a JSON object/array. schema: anyOf [string, object, array]
    r#async: Option<bool>,             // return a job_id now; also honoured as `{"async":true}` inside a JSON body (v1)
    timeout_seconds: Option<u32>,      // sync only; default 50, max 300
    max_bytes: Option<u32>,            // response cap; default 100_000, max 1_000_000
    confirm: Option<bool>,             // required for risk >= high / requires_confirmation (6.4)
}
struct GetJobResultArgs   { job_id: String }                                                              // v1
```

Derived schema example (`start_branch`), as served by `tools/list`:

```json
{"name":"start_branch","description":"Run a git branch ...","inputSchema":{"type":"object","properties":{
  "name":{"type":"string","description":"Project name or id, e.g. `myapp`."},
  "branch":{"type":"string","description":"Git branch to run, e.g. `feature-x`. Created from `base` if it does not exist."},
  "base":{"type":["string","null"],"description":"Start point when creating a new branch. Default: the primary checkout's current branch."},
  "start":{"type":["boolean","null"],"description":"Start the server after preparing it. Default true."},
  "wait_seconds":{"type":["integer","null"],"minimum":0,"description":"How long to wait before returning an operation_id. Default 45, max 55."}},
 "required":["name","branch"]},
 "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}
```

The one schema that needs a hand-written fragment is `call_tool_api.body`, which accepts a JSON string (v1) or an object/array (`#[schemars(schema_with = "body_schema")]`):

```json
{"type":"object","properties":{
  "tool":{"type":"string","description":"Project name, e.g. `browser`. `browser@feature-x` also works."},
  "instance":{"type":["string","null"]}, "branch":{"type":["string","null"]},
  "method":{"type":["string","null"],"enum":["GET","POST","PUT","PATCH","DELETE",null],"description":"Default GET."},
  "path":{"type":"string","description":"Path on the project's server, starting with `/`, may include ?query. Example: /tasks/execute"},
  "body":{"anyOf":[{"type":"string"},{"type":"object"},{"type":"array"},{"type":"null"}],"description":"JSON request body, as a JSON string or an object."},
  "async":{"type":["boolean","null"],"description":"Return a job_id immediately for slow calls."},
  "timeout_seconds":{"type":["integer","null"],"minimum":1,"maximum":300,"description":"Sync calls only. Default 50."},
  "max_bytes":{"type":["integer","null"],"minimum":1,"maximum":1000000,"description":"Response cap. Default 100000."},
  "confirm":{"type":["boolean","null"],"description":"Set true only after the user approved a risky call."}},
 "required":["tool","path"]}
```

Annotations: read tools `readOnlyHint=true`; `start_*`/`stop_*`/`restart_*`/`refresh_discovery` `readOnlyHint=false, destructiveHint=false, idempotentHint=true`; `call_tool_api` `readOnlyHint=false, destructiveHint=true, openWorldHint=true`. These are hints only; the real gate is 6.4.

**Descriptions.** These strings are what agents read, so they are specified here and live in `mcp/descriptions.rs` as `const &str`. Rules used: first sentence says what the tool does and when to reach for it; key search words appear early (Claude Code defers MCP tools and matches queries against the start of the description; it cuts descriptions at 4096 chars if loaded upfront); mention side effects, defaults and limits; name the sibling tool to use instead when relevant; stay under ~600 chars.

| Tool | Description |
|---|---|
| `list_projects` | List projects registered in Lantern (local dev servers, tools, websites) with status, URL and port. Start here to see what exists and what is running. Shows the primary checkout's status and how many branch instances exist; use `list_instances` for per-branch detail, `get_project` for full config. Filters: `tag`; `kind` (project, service, tool, capability, website); `status` (running, stopped, starting, stopping, crashed, unhealthy, needs_config, external; `error` also accepted). |
| `search_projects` | Find projects by free text (case-insensitive substring of name, description, tags, or git branch names). Use when you remember only part of a name or want projects related to a topic such as "browser" or "postgres". |
| `get_project` | Full details of one project: path, URLs, status, run command, health endpoint, dependencies, risk/confirmation metadata, docs and endpoint counts, and all its instances (branches). Pass `branch` or `instance` for that specific checkout instead of the primary. |
| `list_instances` | List instances (one per git checkout) with branch, status, port, URL, health, dirty flag and checkout path. Filter by `project`, `status` or `branch`. Use to see which branches are running side by side, or to find the instance id for other tools. |
| `start_project` | Start a project's dev server. Safe to repeat: if it is already running, reports its URL. Acts on the primary checkout unless `branch` (an existing instance) or `instance` is given; to run a branch with no instance yet use `start_branch`. Waits up to `wait_seconds` (default 30, max 55) for the server to accept connections; if still starting it returns an `operation_id` for `get_operation`. |
| `stop_project` | Stop a project's dev server (its whole process group). Safe to repeat. Acts on the primary unless `branch`/`instance` is given. Deletes nothing. Check `get_dependencies` first if other projects rely on it. |
| `restart_project` | Restart a project's dev server (stop, then start). Acts on the primary unless `branch`/`instance` is given. Waits up to `wait_seconds` (default 40, max 55), then returns an `operation_id`. |
| `start_branch` | Run a git branch of a project side by side with the others: creates a git worktree for `branch` if there is none (new branches start from `base`, default the primary's current branch), runs setup (copies .env, installs dependencies), starts it, and returns its own URL such as https://feature-x.myapp.glow. Safe to repeat; an existing instance is reused. Can take minutes: after `wait_seconds` (default 45, max 55) you get an `operation_id` and the work continues; poll `get_operation`. `start=false` only prepares the worktree. |
| `get_operation` | Check a long operation started by `start_project`, `start_branch`, `restart_project` or `call_tool_api` with `async=true`. Returns status (running, succeeded, failed, cancelled), the current step, and the error if it failed. `wait_seconds` (max 55) blocks until it finishes or the time passes, so you do not need to poll in a loop. |
| `get_project_logs` | Recent log output (stdout and stderr, newest last) of a project's dev server. First stop when something fails to start or returns errors. `tail` lines (default 200, max 2000), `since` ("15m", "2h" or an ISO time), `stream` (stdout, stderr or all), `grep` (case-insensitive substring filter). Targets the primary unless `branch`/`instance` is given. |
| `get_project_docs` | List or read a project's documentation files (README, AGENTS.md, CLAUDE.md, docs/**/*.md, plus anything its manifest lists). Without `path`: the list with kind and size. With `path`: the file text, paged by `offset`/`max_chars`. Reads the targeted branch's checkout, so the docs match that branch. Only listed files can be read. |
| `get_project_endpoints` | List the HTTP API endpoints of a project (from its manifest and its OpenAPI/Swagger document): method, path, description, category, risk, body hint. Use before `call_tool_api`. `q` filters by substring of path, description or category; `limit` caps the list (default 200). |
| `get_project_discovery` | Show how Lantern found a project's docs and endpoints: the auto-discovery settings (docs_auto, api_auto), discovered versus manual entries, sources tried, errors, last refresh time. Use to debug missing docs or endpoints. |
| `refresh_discovery` | Re-scan docs and re-fetch OpenAPI documents now (normally refreshed every 10 minutes and when a server starts). Pass a project/instance, or nothing to refresh all. Takes up to 30 seconds. |
| `check_health` | Health of Lantern itself (dns, caddy, tls, daemon) and of every instance (healthy, unhealthy, unreachable, unknown; latency; last check). With `name` returns only that instance. `refresh=true` runs a check now instead of using the cached result. |
| `get_dependencies` | Dependency graph between projects (`depends_on` in manifests): what each needs and what depends on it. Pass `name` for one project. Check it before stopping or restarting something others rely on. |
| `get_ports` | Which ports are in use: Lantern's assignment per instance (with status, health, URL), processes Lantern did not start that listen on localhost (pid, command, working directory, the project they appear to belong to), and port conflicts. Never guess a port; use this. |
| `list_listeners` | Raw list of TCP sockets listening on localhost with owning process, command line, working directory, and the matched project/instance. Finds servers started by hand in a terminal. Filters: `project`, `port`, `unmatched_only`. |
| `list_tools` | List the tools agents can call through Lantern: projects of kind `tool` or `capability` (browser automation, data services, ...). Shows description, status, domain, risk, whether calls need confirmation, trigger keywords and endpoints. Then use `get_project_docs`, `get_project_endpoints` and `call_tool_api`. Filters: `tag`, `status`, `kind`. |
| `call_tool_api` | Call an HTTP API of a registered project through Lantern, e.g. tool="browser", method="POST", path="/tasks/execute", body={...}. Lantern finds the running server's address; you give the project and a path. Returns the response body (cut at `max_bytes`, default 100000). Only registered projects are reachable. Endpoints marked risk high/critical, or projects needing confirmation, are not sent until `confirm=true`: ask the user first. For calls over ~50s set `async=true` to get a `job_id`, then use `get_operation` or `get_job_result`. See `get_project_endpoints` for what exists. |
| `get_job_result` | Fetch the result of an async `call_tool_api` job by `job_id`. While running: status and elapsed time. When done: the response body. On failure: the error. Jobs are kept for 1 hour after they finish. (`get_operation` returns the same job as an operation.) |

**`call_tool_api` behavior, in order**

1. Resolve `tool`/`instance`/`branch` (3.7.3). Method must be one of GET POST PUT PATCH DELETE (case-insensitive), else a tool error.
2. Validate `path` and build the URL (6.3). Choose the upstream: the primary instance's manifest `upstream_url` if set (fixed service); else `http://127.0.0.1:{runtime.port}` when the instance is running; else manifest `base_url`; else `"myapp is stopped; start it with start_project"`. It never auto-starts anything.
3. Policy check (6.3), then risk gate (6.4), then, if the manifest sets `routing.max_concurrent`, a per-instance semaphore (wait up to 5s, then `busy`). Unset means unlimited (v1 never enforced it).
4. Send with a fresh `reqwest::Client` (`no_proxy()`, `redirect(Policy::none())`, `connect_timeout(3s)`, `timeout(timeout_seconds)`, addresses pinned to the vetted IPs). Headers: `Accept: application/json`, `Content-Type: application/json` when there is a body, `User-Agent: lantern/<ver>`, `X-Lantern-Caller: mcp`. `body` as a string is sent verbatim (v1); an object/array is serialized.
5. Read the body as a stream up to `max_bytes`; on overflow stop reading, drop the connection, append `\n[truncated at 100000 bytes; narrow the request or raise max_bytes (max 1000000)]`. Non-UTF-8 or binary: `image/png|jpeg|webp|gif` up to 1 MiB are returned as an MCP image block; anything else as `[binary response: <content-type>, <n> bytes, not shown]`.
6. `2xx/3xx`: body text (exactly what v1 returned). `>=400`: tool error `HTTP 404: <body, first 2000 chars>`. Connect failure: `Connection refused: is 'browser' running? Try start_project.`
7. `async=true`: create an `Operation{kind: ApiCall}`, run steps 4-6 in a spawned task (900s cap), return `{"status":"submitted","job_id":"op_...","message":"Use get_job_result or get_operation with this id."}` (the v1 shape plus the id). The result body (cut at `max_bytes`) is stored in `operation.result` for 1 hour. v1's in-body `{"async": true}` still triggers async when the `async` argument is absent, and is left in the forwarded body as in v1.

#### 3.7.5 Deadlines, cancellation, progress, jobs

Every tool body runs inside one helper. It replaces v1's `Timeout.run` + `InFlight` + `Jobs`.

```rust
async fn run_tool<F>(ctx: &RequestContext<RoleServer>, limit: Duration, body: F) -> Result<CallToolResult, McpError>
where F: Future<Output = Result<String, ToolError>> {
    let _ticker = ProgressTicker::start(ctx, limit);           // spawns only if the request has a progressToken; aborts on drop
    let out = tokio::select! {
        r = body => r,
        _ = ctx.ct.cancelled() => Err(ToolError::new("Cancelled by the client.")),
        _ = tokio::time::sleep(limit) => Err(ToolError::new(format!(
            "Timed out after {}s. Any operation it started keeps running; use get_operation.", limit.as_secs()))),
    };
    Ok(match out {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(cap_chars(text, 40_000))]),
        Err(e)   => CallToolResult::error(vec![ContentBlock::text(e.message)]),
    })
}
// ProgressTicker: every 3s peer.notify_progress(ProgressNotificationParam::new(token, elapsed_secs)
//     .with_total(limit_secs).with_message(current operation step or "Working (12s)")). Same cadence as v1.
```

| Tool group | v1 deadline | v2 default deadline | Notes |
|---|---|---|---|
| start / restart / stop | 45s / 65s / 35s (blocking) | `wait_seconds` 30 / 40 / 20, hard max 55, plus 5s slack | returns `operation_id` when exceeded; the work continues |
| `start_branch` | n/a | `wait_seconds` 45, max 55 | |
| `get_operation` | n/a | `wait_seconds` 0, max 55 | |
| logs, docs, endpoints, projects, ports, listeners, deps | 10-15s | 10s | |
| `check_health` | 10s | 10s (`refresh`: 15s) | |
| `refresh_discovery` | 35s | 30s | |
| `call_tool_api` sync | 120s | `timeout_seconds` default 50, max 300 | the 55s ceiling elsewhere is deliberate: **Codex's default `tool_timeout_sec` is 60** and `mcp install` raises it to 120 for Codex only; unknown clients may be stricter |

- **Cancellation.** Stateless means a `notifications/cancelled` POST cannot be matched to the call it names (there is no session; rmcp answers `202` and drops it). What rmcp does cancel is `ctx.ct` when the HTTP response stream closes, and clients abort the request when the user cancels. So `ctx.ct` is the only signal we use. That is safe because cancelling a tool call only abandons the *wait*: operations started by the call keep running (stop them with `stop_project` or `POST /api/operations/{id}/cancel`). The one tool where cancellation does real work is sync `call_tool_api`, which drops the upstream request.
- **A retry joins, never doubles.** If a client's connection drops during `start_project` and the agent calls it again, the runtime returns the in-flight operation (core section 8 per-instance lock).
- **Jobs.** v1 `Jobs` (in-memory map, 5-minute TTL) becomes `OperationTracker` entries of kind `api_call`: id `op_...`, retained 1 hour / 200 entries. `get_job_result(job_id)` accepts that id and keeps v1's output shapes (running: `{"status":"running","elapsed_ms":n,"message":"Still processing. Poll again shortly."}`; completed: the body text; failed: tool error).
- **Output size.** Claude Code warns above 10k tokens, caps at 25k (`MAX_MCP_OUTPUT_TOKENS`), and writes text results over 50,000 characters to a file. So tool text is capped at 40,000 characters with a `[truncated ...; <how to get the rest>]` marker, and list tools default to compact JSON (no pretty-printing).
- **Redaction.** MCP output never includes values of `run.env` keys whose names match `(?i)key|token|secret|passw|credential|auth`; they show as `"***"`. (REST returns the manifest unredacted: its callers are the user's own tools.)
- **Result encoding.** JSON results are returned as one text block of compact JSON, as v1 did (`Response.json`). We do not also set `structuredContent`: some clients would put both in the model's context.
- **Logging.** One `tracing` span per call: tool, target instance, duration, outcome. Arguments and bodies are never logged.

#### 3.7.6 Resources

Claude Code surfaces resources through `@` mentions; Codex's docs mention only tools and server instructions. So **nothing is resource-only**: every resource has a tool equivalent.

| URI | v1? | Content (`application/json`) | Tool equivalent |
|---|---|---|---|
| `lantern:///projects` | yes | `ProjectBrief[]` for all projects | `list_projects` |
| `lantern:///projects/docs` | yes | all docs of all primary instances, `[{name, docs:[{path,kind,source,content}]}]`; each doc cut at 64 KiB, total at 1 MiB | `get_project_docs` |
| `lantern:///projects/discovery` | yes | per project: `docs_auto, api_auto, discovered_docs, discovered_endpoints, docs_available, endpoints_available, discovery` | `get_project_discovery` |
| template `lantern:///projects/{name}/docs` | v1 accepted it but never advertised it | docs of one project/instance (`name` may be `myapp@feature-x`) | `get_project_docs` |
| template `lantern:///projects/{name}/docs/{+path}` | new | one doc, `text/markdown` or `text/plain` | `get_project_docs` |
| `lantern:///instances` | new | `InstanceView[]` | `list_instances` |
| `lantern:///listeners` | new | `Listener[]` | `list_listeners` |

Unknown URI: `McpError::resource_not_found("resource_not_found", Some(json!({"uri": uri})))`.

#### 3.7.7 Prompts (v1 names and text kept; optional `instance`)

Prompt arguments are always strings in MCP, so both are `Option<String>`/`String` (no numeric arguments).

- `diagnose_service {name, instance?}`: user message, v1 text with the steps: check health (`check_health`), details (`get_project`), logs (`get_project_logs`), dependents (`get_dependencies`); then v2 adds "if the port looks wrong, call `get_ports`/`list_listeners`" and passes `instance` to each call when given. The closing analysis bullets are unchanged.
- `dependency_trace {name}`: v1 text unchanged (get_dependencies, get_project for dependents, check_health, blast-radius summary).

#### 3.7.8 Output compatibility with v1

| Tool | v1 output | v2 output |
|---|---|---|
| `list_projects`, `search_projects` | array of full `Project.to_map` | array of `ProjectBrief`: `name, id, path, domain, url, kind, status, port, description, tags, enabled, instances (count), branches[], docs_count, endpoints_count`. `get_project` has the rest (smaller output; v1 returned ~30 fields per project). `status` of a project = its primary instance's. |
| `get_project` | full map | v1 keys kept where they still exist (`name, path, domain, kind, status, port, run_cmd, run_env (redacted), health_endpoint, base_url, upstream_url, tags, enabled, depends_on, routing, docs_available, endpoints_available, discovery`) plus `primary_instance`, `instances[]`. Dropped: `detection`, `pid` (now per instance), `deploy` (runtime-internal). |
| `start/stop/restart_project` | text `Started myapp (status: running)` / `myapp is already running (port: N)` | same sentences, extended: `Started myapp (status: running, port: 41023, url: https://myapp.glow)`; still running after the wait: `{"status":"in_progress","operation_id":"op_..","instance":"..","step":"waiting for port"}` |
| `get_project_logs` | log lines joined with `\n` | a header `# myapp@feature-x: last 200 of 1534 lines, status running` then lines (`[stderr] ` prefix on stderr lines) |
| `check_health` | no name: `{dns, caddy, tls, daemon, projects:{name:{status,latency_ms,checked_at,error,history}}}`; name: that entry | identical keys. `projects` is keyed by **InstanceId**, so primary keys equal v1's project names. Names resolving to no health data: `{status:"unknown", message}` as before. |
| `get_ports` | `{name: {port, health_status}}` | `{"instances": {id: {port, status, health_status, url}}, "unmanaged": [Listener], "conflicts": [..]}` |
| `get_dependencies` | `{name: {depends_on, depended_by}}` | unchanged |
| `list_tools` | `{name, description, domain, status, tags, endpoints_available}` for `kind == tool` | same keys plus `instance, url, risk, requires_confirmation, triggers`; includes `capability` projects (core glossary) |
| `get_project_discovery`, `refresh_discovery` | `docs_auto, api_auto, discovered_*, *_available, discovery` | same keys plus `instance`; the `discovery` object has v1's shape |

---

### 3.8 Intel (docs and API ingestion)

v1 called this "discovery" (`projects/discovery.ex`, `discovery_worker.ex`, `doc_server.ex`). In v2 *discovery* means finding listening servers (`12-discovery`); this is **intel**. User-facing names stay v1's where they are manifest keys or MCP tool names (`docs_auto`, `api_auto`, `get_project_discovery`, `refresh_discovery`).

#### 3.8.1 Per-instance, not per-project (decision)

Intel is computed and stored **per instance**. Docs are files in a checkout and differ by branch; the manifest (`docs:`, `endpoints:`, `docs_auto`, `api_auto`) is resolved per checkout (core glossary); OpenAPI is served by *that instance's* process on *its* port. A project-level question ("`get_project_docs name=myapp`") means the primary instance. Cost control: the periodic refresh covers primary instances of enabled projects plus any instance that is running or pinned; other instances are computed lazily on first request and then cached.

```rust
// crates/lantern-daemon/src/intel/{mod,docs,openapi,cache,worker}.rs
#[derive(Clone)] pub struct IntelHandle { /* Arc<Inner>: RwLock<HashMap<InstanceId, Arc<IntelSnapshot>>>, request channel */ }
impl IntelHandle {
    pub async fn snapshot(&self, id: &InstanceId, max_age: Duration) -> Result<Arc<IntelSnapshot>, IntelError>; // inline refresh if missing/older
    pub async fn refresh(&self, id: &InstanceId) -> Result<Arc<IntelSnapshot>, IntelError>;                    // 30s cap
    pub async fn refresh_all(&self) -> IntelRefreshReport;                                                      // 30s overall cap
    pub async fn read_doc(&self, id: &InstanceId, rel: &str, offset: u64, max_chars: usize) -> Result<DocContent, IntelError>;
    pub fn forget(&self, id: &InstanceId);
}
```

Snapshots are immutable `Arc`s swapped atomically on refresh, so readers never block on a refresh. Doc *content* is never cached: it is read from disk on request.

#### 3.8.2 Docs ingestion

Inputs from the resolved manifest (14-config owns the YAML schema; v1 shapes assumed): `docs: [path | {path, kind}]` (manual) and `docs_auto: {enabled, patterns, ignore, max_files, max_bytes}`.

| Setting | Default | vs v1 |
|---|---|---|
| `docs_auto.enabled` | **true** | v1: off unless `docs_auto` was present. On by default so `get_project_docs` works for every project; `docs_auto: {enabled: false}` opts out. |
| `patterns` | `README.md CHANGELOG.md AGENTS.md CLAUDE.md BRAIN.md SKILL.md docs/**/*.md` | same, checkout-root relative |
| `ignore` | `.git/** node_modules/** deps/** _build/**` | same, plus directories `.git node_modules target .venv __pycache__` pruned at any depth |
| `max_files` | 100 | applied **after** the size filter (v1 truncated first, then dropped big files) |
| `max_bytes` | 1 MiB per file | same; larger files are counted in `skipped_large`, not listed |

Algorithm (`intel/docs.rs`, uses `globset` + `walkdir`; no gitignore handling, because `CLAUDE.md`/`BRAIN.md` are often gitignored and are exactly what we want):

1. For each pattern, split off its literal directory prefix (`docs/**/*.md` -> walk `docs/` only; `README.md` has no glob -> a single `stat`). Scanning cost is proportional to the doc directories, not to the repo (v1's `Path.wildcard` over the whole tree was O(repo) per pattern).
2. Walk with `follow_links(false)`, `max_depth(8)`, pruning ignored directories; keep regular files that match a pattern and not an ignore glob.
3. Drop files over `max_bytes`; order by (pattern index, path); take `max_files`; set `truncated` if more existed.
4. Merge with manual `docs:` entries: manual first, discovered entries with the same path dropped. `kind` is inferred from the name as in v1 (readme, changelog, contributing, openapi, markdown, text, yaml, json, unknown). `exists/size/mtime` come from `stat`; a missing manual doc is listed with `exists:false`.

#### 3.8.3 API (endpoint) ingestion

Inputs: manual `endpoints: [{method, path, description, category, risk, body_hint, params}]` and `api_auto: {enabled, sources, candidates, timeout_ms, max_endpoints}`. `api_auto` stays **opt-in** (it makes network requests).

- **Sources** (explicit): each `http(s)://` URL or checkout-relative file path is loaded; failures are reported per source.
- **Candidates** (probed against the instance's own address: the same upstream as `call_tool_api`, 3.7.4 step 2): tried in order, **stopping at the first that parses**. Candidate misses (404, refused) are not errors; if none succeeds the meta gets one entry `no_openapi_found` listing the URLs tried. (v1 reported every miss.) Defaults `/openapi.json /swagger.json /docs/openapi.json`.
- **Not running**: if the instance is not `running`/`external`, candidate probing is skipped silently; the previous endpoints stay and `stale: true` is set. Explicit file sources are still read.
- **Fetch rules**: `GET`, `Accept: application/json, application/yaml`, `timeout_ms` (2500), no redirects, body cap `max_openapi_bytes` (5 MiB, new), destination checked by the same policy as `call_tool_api` (6.3). File sources must resolve inside the checkout.
- **Parse**: try JSON, then YAML (the YAML crate chosen in `14-config`). Both OpenAPI 3.x and Swagger 2.0 have `paths`. Per `paths.{path}.{method}` (methods `get post put patch delete options head`): `description` = first line of `summary` or `description` (cut at 200 chars); `category` = `tags[0]`; `risk` = valid `x-risk` else inferred (GET/HEAD/OPTIONS low, POST/PUT/PATCH medium, DELETE high; same as v1); `body_hint` = content types of `requestBody.content` (or `consumes`); `params` = `parameters[]` `{name,in,required}` merged from path and operation level, resolving only local `#/components/parameters/X` / `#/parameters/X` one level deep. Path items that are bare `$ref`s are skipped.
- **Merge**: dedupe on `(METHOD, path)`, manual entries win (v1 rule), sort by path then method, cut at `max_endpoints` (400).

#### 3.8.4 Refresh schedule

One tokio task (`intel/worker.rs`) owned by the daemon's task tree. It refreshes:

- 2s after daemon start (primary instances of enabled projects, stagger by 50 ms) and then every `intel.refresh_interval_minutes` (default 10; v1: 10);
- when an `InstanceUpdated` event shows an instance **became Running**: after 1s and again after 10s (frameworks like FastAPI build their OpenAPI lazily or after warm-up);
- on `ProjectUpdated` / manifest reload: that project's instances;
- on demand: `refresh_discovery`, `POST .../intel/refresh`, or inline when `snapshot()` finds nothing cached or older than `max_age` (docs only; OpenAPI is fetched inline only if there is no snapshot at all);
- never more than 4 refreshes at once (`Semaphore`), one per instance at a time, each capped at 30s; an `InstanceRemoved` event calls `forget`.

#### 3.8.5 Storage

In memory (`IntelHandle`), plus an optional cache file `$XDG_STATE_HOME/lantern/intel-cache.json` (`{"version":1,"instances":{id:{refreshed_at, endpoints, api_meta}}}`, atomic write via tmp+rename, at most once per 60s after a change). It exists so that after a daemon restart agents see the last known endpoints immediately (`stale: true` until the first refresh) and OpenAPI endpoints of services are not re-fetched at every restart. Docs are not cached (a rescan is cheap). The file is disposable: if it is missing or has an unknown `version` it is ignored, and entries for instances that no longer exist are discarded. It is deliberately **not** part of `state.json` (core-owned, versioned, small).

#### 3.8.6 Reading docs and the `/<project>/docs` pages

- `read_doc` serves **only files that appear in the instance's current docs list**. The resolved path (`canonicalize`) must stay inside the canonical checkout root, so a symlink leading out is refused. Binary files (NUL in the first 8 KiB) are refused. Content is read up to `max_bytes`, decoded lossily as UTF-8, and paged by `offset`/`max_chars`. Errors: `doc_not_found` (not listed or missing), `forbidden` (escapes root).
- **Lighthouse pages: keep, trimmed.** `GET /{project}/docs` renders a small HTML index for a human (path, kind, size, link) built with `format!` and an explicit `escape_html`; `GET /{project}/docs/{*path}` returns the file. They are served by the daemon on any allowed host (`http://127.0.0.1:4777/myapp/docs`, `https://lantern.glow/myapp/docs`); the separate `lighthouse.<tld>` site and its Caddy route are dropped. They are the only place a browser meets repo content, so: content type is `text/markdown` for `.md`, `application/json` for `.json`, and `text/plain; charset=utf-8` for everything else **including `.html`**; every response carries `X-Content-Type-Options: nosniff` and `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; sandbox`. Markdown is shown as text, not rendered (as in v1).

---

### 3.9 `lantern mcp install <client>`

`16-clients` owns the command line (flags, output, prompts). This section owns **what gets written**. Pure logic lives in `lantern-core::mcp_install` (string in, string out, unit-testable); the CLI does file IO (read, back up to `<file>.lantern-bak`, write atomically, keep file mode).

**URL written everywhere: `http://127.0.0.1:<port>/mcp`** (port from settings, default 4777). Not `https://lantern.glow/mcp`: the loopback URL works without DNS, Caddy or CA trust (Node-based clients such as Claude Code do not use the system trust store by default, so Caddy's local CA would break them). No bridge: all three clients speak streamable HTTP natively, so v1's `npx mcp-remote` stdio bridge is dropped.

| Client | Where / how (verified against the vendors' docs, 2026-11-06) | What is written |
|---|---|---|
| `claude-code` | **Runs the client's own CLI**: `claude mcp add --transport http --scope user lantern http://127.0.0.1:4777/mcp` (`--scope project` writes the repo's `.mcp.json`; user/local scopes live in `~/.claude.json`). If `claude mcp get lantern` already shows the same URL: "already installed". Different URL: with `--force`, `claude mcp remove lantern -s user` then add. | Entry shape for reference/`--print`: `{"mcpServers":{"lantern":{"type":"http","url":"http://127.0.0.1:4777/mcp"}}}` (`type` is required; a `url` without `type` is treated as stdio and skipped). We **never edit `~/.claude.json` ourselves**: it is large, holds session state and is rewritten constantly by Claude Code. If `claude` is not on PATH we print the command and, for `--scope project`, write `./.mcp.json` directly (merge into `mcpServers`). |
| `codex` | Edits `~/.codex/config.toml` with `toml_edit` (preserves comments and order; the docs show no env override of this path). Project scope: `.codex/config.toml` (trusted projects only). | `[mcp_servers.lantern]` table: `url = "http://127.0.0.1:4777/mcp"`, `startup_timeout_sec = 10`, `tool_timeout_sec = 120`. The timeout is raised from Codex's default of 60 because `call_tool_api`/`start_branch` can approach it. |
| `cursor` | Merges into `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (`--scope project`). | `{"mcpServers":{"lantern":{"url":"http://127.0.0.1:4777/mcp"}}}`, no `type`, as in Cursor's remote-server examples. |
| `json` / `--print` | Prints the generic JSON snippet and the three clients' variants; writes nothing. For any other client (OpenCode, Windsurf, ...). | |

Rules: idempotent (re-running changes nothing); never deletes other servers or unrelated keys; a file that is not valid JSON/TOML is left untouched with an error naming the file and offering `--print`; the CLI re-checks afterwards with an `initialize` POST to the URL and says so if the daemon is down (installing still succeeds). v1's `claude` (Claude Desktop path `~/.config/claude/mcp.json`, which is not a real Claude Desktop location) and `opencode` targets are dropped; they remain reachable through `--print`. If the security option `require_token` is ever enabled (section 8, Q1), `--token` adds `--header "Authorization: Bearer ..."` (Claude Code), `bearer_token_env_var` (Codex) or `headers` (Cursor).

### 3.10 Self-registration: dropped

v1 (`self_registration.ex`) registered the daemon as a `tool` project named `lantern` (proxy to `127.0.0.1:4777`, domain `lantern.glow`, a hand-written list of its own REST endpoints, re-activated on every boot). It did three things: gave the daemon a domain, made the REST API callable through `call_tool_api`, and showed the daemon in the GUI list. In v2 the first is a fixed Caddy route for `lantern.<tld>` created by `13-system` (no registry entry), the second is dangerous (it is an SSRF into the control plane, section 6.3) and redundant (MCP exposes management directly), and the third is not needed. `lantern`, `api` and `mcp` become reserved ids (section 9). A developer working on Lantern's own repo sets `id: lantern-dev` in its `lantern.yaml`.

---

## 4. Interfaces

### 4.1 Provided

| Item | Used by |
|---|---|
| `api::router(AppState, McpService) -> Router`, `api::serve(...)` | `lantern-daemon` main |
| `mcp::service(AppState) -> McpService` | `api::router` |
| `intel::{IntelHandle, spawn(app, shutdown)}` | api, mcp, daemon main |
| `UpstreamPolicy` (6.3): `vet(url, scope)`, `build_target(base, path)` | mcp `call_tool_api`, intel |
| `lantern-core::api::*` (3.2, 3.5) | daemon, `lantern-client` |
| `lantern-core::mcp_install::{claude_code_args, cursor_merge, codex_merge, snippet}` | `lantern-cli` |

### 4.2 Consumed (assumed; names indicative, behavior is what matters)

All are async handle methods on cheap-to-clone structs held in `AppState`. If a sibling doc names things differently, the intent here wins and the call sites adapt.

```text
registry  (core §8 actor)
  list_projects(filter) / get_project(&ProjectId) / require_instance(&InstanceId)
  list_instances(filter) / view(&InstanceId) -> Option<InstanceView> / resolve(&str) -> ResolveOutcome
  register_project(RegisterProjectRequest) / patch_project / unregister_project(UnregisterParams)
  reload_project(&ProjectId, discard_overrides) / scan_workspaces(Option<Vec<PathBuf>>) -> ScanReport
  set_pinned(&InstanceId, bool)
runtime   (10-runtime)
  start|stop|restart|prepare(&InstanceId, opts) -> Result<OperationHandle>   // joins an in-flight op of the same kind;
                                                                            // different kind -> Err(OperationInProgress{op})
  stop_all(include_services) -> Vec<OperationHandle>
  logs(&InstanceId, LogQuery{tail,since,stream,grep}) -> LogPage
  tail_and_subscribe(&InstanceId, tail) -> (Vec<LogLine>, broadcast::Receiver<LogLine>)   // atomic: no gap, no duplicate
  check_health_now(&InstanceId) -> HealthSummary
instances (11-instances)
  ensure_branch(&ProjectId, CreateInstanceRequest) -> Result<{instance: InstanceId, created: bool, op: OperationHandle}>
  remove(&InstanceId, RemoveInstanceParams) -> Result<OperationHandle>
  sync(&ProjectId) -> Vec<InstanceId>
discovery (12-discovery)
  listeners() -> Vec<Listener> / scan_now() / conflicts() -> Vec<PortConflict>
  adopt(&InstanceId, Option<u16>) -> Result<()> / unadopt(&InstanceId)
system    (13-system)
  health() -> SystemHealth / services() / service_start|stop|restart(name) -> ServiceStatus / reconcile()
config    (14-config)
  settings() -> Arc<Settings> / patch_settings(Value) -> (Settings, Vec<String> restart_required)
  profiles, templates CRUD / profile_activate(name) -> Vec<OperationHandle>
  ResolvedManifest exposes: kind, description, tags, run.cmd, health_endpoint, base_url, upstream_url, depends_on,
    docs, docs_auto, endpoints, api_auto, routing{aliases,paths,websocket,triggers,risk,requires_confirmation,max_concurrent,agents}
operations (shared, daemon/src/operations.rs; first caller creates it)
  begin(kind, Option<InstanceId>) -> OperationHandle{id, step(..), finish(Ok|Err), cancel_token()}
  get(id) / wait(id, Duration) / list(filter) / cancel(id)
events    (core §9)
  EventBus::subscribe() -> broadcast::Receiver<Event> (state events only; log lines are not on this bus, see §9)
```

Settings keys I need in `config.toml` (names for `14-config` to adopt):

| key | default | meaning |
|---|---|---|
| `api.port` | 4777 | listen port (restart required) |
| `tld` | `glow` | used for the Host allowlist and `*.tld` upstream rule |
| `mcp.allowed_upstream_hosts` | `[]` | extra non-loopback hostnames `call_tool_api`/intel may reach (6.3) |
| `mcp.allow_critical_calls` | `false` | permit `risk: critical` calls with `confirm=true` (6.4) |
| `intel.refresh_interval_minutes` | 10 | |
| `intel.cache_file` | `true` | write `intel-cache.json` |

---

## 5. Failure modes & edge cases

| Situation | Behavior |
|---|---|
| Daemon down | REST clients get connection refused (16-clients handles). An MCP client's call fails at connect; it works again on the next call after the daemon is back, with no re-initialize or `/mcp` needed (stateless). |
| Daemon restarts during a long `wait` or tool call | The HTTP connection resets; operations whose supervisors outlive the daemon (systemd units) continue, the OperationTracker entry is lost, so a `get_operation` afterwards returns `operation_not_found`; the agent should call `list_instances` to see the real state. |
| Client disconnects mid-`wait` | Only the wait is abandoned; operations continue. Repeating the call joins the in-flight operation. |
| Two clients start the same instance | Same operation returned to both (`202` each). `stop` during `start`: `409 operation_in_progress` with the running operation. |
| `start` on `needs_config` | Operation fails fast: `422 needs_config`, `message` is `runtime.message` (e.g. "no run command; add run.cmd to lantern.yaml"). MCP: tool error with the same text plus "see get_project". |
| Branch with no instance, MCP `branch` arg | Tool error naming existing instances and suggesting `start_branch` (3.7.3). |
| Worktree creation fails (branch checked out elsewhere, bad base, dirty) | Operation `failed` with the code from `11-instances` (`branch_invalid`, `dirty_worktree`, `port_unavailable`...); `details.stderr` holds the last 2000 chars of git's output. |
| `call_tool_api` on a stopped instance | Tool error `'browser' is stopped; start it with start_project`. Never auto-starts. |
| Upstream returns huge or binary body | Truncated / summarized (3.7.4 step 5). Connection is dropped after the cap. |
| Upstream hangs | `timeout_seconds` elapses: `Timed out after 50s`, upstream request dropped. For long calls use `async`. |
| Path tricks in `call_tool_api` (`@evil.com`, `//host`, `http://...`, `\`, control chars) | Rejected before any DNS or connect: `path must start with a single '/' and contain no scheme or host`. |
| OpenAPI missing / invalid / too large / timing out | Recorded in `discovery.api.errors`; previous endpoints kept, `stale: true`; logged at debug (not warn: expected for most projects). |
| Manifest `docs:` entry missing on disk | Listed with `exists:false`; reading it is `doc_not_found`. |
| Doc deleted between list and read | `doc_not_found` (404). |
| Request with disallowed `Host`/`Origin` | `403 forbidden_host` / `forbidden_origin`; the message names the allowed hosts (helps when someone opens the API via a LAN IP). |
| Wrong content type / oversized body | `415` / `413` with the envelope. |
| SSE client too slow | `resync` then close (3.5). More than 32 streams: `503 unavailable`. |
| Registry/runtime call hangs | Handler deadline (15s) returns `504 timeout`; the log names the module. |
| Project unregistered during a request | The race resolves to `404` on the next lookup; no handler holds a registry lock across `.await`. |
| `PATCH /api/settings` changes `api.port` or `tld` | Saved; response lists `restart_required`; the running daemon keeps the old Host allowlist until restart. |
| `GET /mcp` or `DELETE /mcp` | `405` (stateless). Clients that probe for a standalone SSE stream accept this per the spec. |
| Behind Caddy, SSE buffered | Events arrive late or in bursts; fix is `flush_interval -1` on the `lantern.<tld>` route (13-system). |

---

## 6. Security considerations

Threat model: one user, one machine, browsers and dev servers that are *less trusted than the user*. In scope: a web page the user visits, a compromised or sloppy dev-server page (served from `myapp.glow`), a repo whose manifest or files are hostile, an agent that is prompt-injected. Out of scope: other OS users (core non-goal) and any process already running as the user (it can read the user's files and start processes anyway).

### 6.1 Host, Origin, CORS

One middleware (`api/guard.rs`) in front of **everything** (REST, SSE, MCP, docs pages):

1. **Host** (DNS rebinding). `Host` must equal `127.0.0.1:<port>` or `localhost:<port>`, or be `lantern.<tld>` (any port: Caddy forwards the original Host, which carries no port for 443). Anything else, or a missing/malformed Host: `403 forbidden_host`. `X-Forwarded-*` headers are ignored.
2. **Origin.** If an `Origin` header is present it must be exactly one of `http://127.0.0.1:<port>`, `http://localhost:<port>`, `https://lantern.<tld>`, `http://lantern.<tld>` (each with `:<port>` if Caddy listens on a non-default port); else `403 forbidden_origin`. `Origin: null` is rejected. A missing `Origin` is allowed (CLI, GUI, curl, MCP clients never send one; browsers always send one on cross-origin POST/PUT/PATCH/DELETE and on `fetch` with CORS).
3. **Sec-Fetch-Site.** If present, it must be `same-origin` or `none`; else `403 forbidden_origin`. Covers browsers where Origin is absent (e.g. a cross-site top-level `GET`) and `myapp.glow` -> `lantern.glow` requests.
4. **No CORS, ever.** No `Access-Control-*` headers are sent and `OPTIONS` is not handled (405), so a browser's preflight fails. Combined with the mandatory `Content-Type: application/json` on bodies (a "simple" cross-origin form/text POST is therefore impossible) and the Origin check, a web page can neither send nor read anything.
5. Rule for implementers: **no state-changing endpoint may be reachable by `GET`** (the GET exceptions to Origin checking rely on it).
6. Response headers on everything: `X-Content-Type-Options: nosniff`, `Cache-Control: no-store`, `Referrer-Policy: no-referrer`.

This implements MCP's own requirement (spec 2025-11-25, Transports, "Security Warning": servers MUST validate `Origin` and respond 403, SHOULD bind to localhost) for every route, not just `/mcp`.

### 6.2 Token: not in v2.0 (recommendation)

All three target clients can send a bearer header (Claude Code `--header`, Codex `bearer_token_env_var`, Cursor `headers` with `${env:...}`), so friction is *possible* but real: a token must be distributed into three config formats, kept in sync after rotation, and read by CLI, GUI and tray. What would it buy? Against a web page: nothing beyond 6.1. Against a hostile dev server or any other process of the same user: nothing, because it can read the token file (or just run commands). Against other OS users: that is a non-goal. So v2.0 ships **without** a token; the guard middleware is the single place one would be added (`Authorization: Bearer` compare in constant time, token in `$XDG_STATE_HOME/lantern/api-token` mode 0600, a `require_token` setting). Revisit if multi-user or non-loopback binding ever becomes a goal.

### 6.3 `call_tool_api` and intel: no SSRF

Rules, implemented once in `UpstreamPolicy` and used by both `call_tool_api` and OpenAPI fetches:

1. **Only registered projects.** The target is derived from the instance (3.7.4 step 2); callers never supply a host, scheme or port.
2. **Path building.** `path` must start with exactly one `/`, contain no `\` or control characters. The URL is built by parsing the base with `url::Url`, setting path and query, then asserting scheme, host and port still equal the base's. (v1 concatenated strings.)
3. **Destination class.**
   - Loopback (`127.0.0.0/8`, `::1`), `localhost` and `*.<tld>` names: allowed, but never the daemon's own port, Caddy's admin port (2019) or the embedded resolver's port (the `infra_ports` set). This blocks pointing a manifest's `upstream_url` at the control plane.
   - Any other hostname or address: allowed only if listed in the **user's** `mcp.allowed_upstream_hosts` setting. A manifest cannot widen it, because manifests come from repos. (This is a deliberate v1 break: a manifest with an external `base_url` such as `https://ghost.paidfor.net` now needs that host in the setting; the error says exactly that.)
   - Always refused, even if allow-listed: unspecified (`0.0.0.0`, `::`), link-local (`169.254.0.0/16`, `fe80::/10`, which includes cloud metadata at `169.254.169.254`), multicast.
4. **Resolve once, connect to the vetted address.** Names are resolved with `tokio::net::lookup_host`; every address must pass 3; the `reqwest` client is built per call with `.resolve_to_addrs(host, &addrs)` so a second DNS answer cannot redirect it (rebinding).
5. **No redirects** (`redirect::Policy::none()`: a `302` to `169.254.169.254` is returned to the agent, not followed), **no proxies** (`.no_proxy()`, so `HTTP_PROXY` cannot divert localhost calls), `connect_timeout` 3s, total `timeout_seconds`.
6. **Methods** GET, POST, PUT, PATCH, DELETE only. **Headers** are fixed (3.7.4 step 4); agents cannot set any (v1 had none either).
7. **Size**: request body at most 1 MiB; response cap `max_bytes` (default 100 KB, max 1 MB); OpenAPI documents 5 MiB.
8. Global ceiling of 16 concurrent `call_tool_api` requests.

### 6.4 `risk`, `requires_confirmation`, `max_concurrent`

v1 parsed and displayed these but enforced nothing. v2 enforces:

- Effective risk of a call = the matching declared/discovered endpoint's `risk` (path templates `:id` and `{id}` match one segment), or, when nothing matches, inferred from the method (GET low; POST/PUT/PATCH medium; DELETE high); then raised to the project's `routing.risk` if that is higher.
- If effective risk is `high` or `critical`, or `routing.requires_confirmation` is true, and `confirm` is not `true`: **nothing is sent**; the tool returns an error such as `Not sent: DELETE /jobs/42 on 'browser' is risk=high (from its manifest). Ask the user to confirm, then repeat with confirm=true.` `critical` additionally needs the user's setting `mcp.allow_critical_calls = true`.
- This is a speed bump for the *agent*, not a security boundary: an agent can pass `confirm=true` by itself. Its value is that agents are told to ask the human, and clients that prompt before each MCP tool call (Claude Code and Codex do by default) show the explanation. The real boundary remains the client's own tool-permission prompt. MCP elicitation would be the proper mechanism but needs server-to-client requests, which stateless mode cannot do (revisit with Q8).
- `max_concurrent`: enforced per instance only when the manifest sets it (3.7.4 step 3).

### 6.5 Files, secrets, other

- Only listed docs are readable and are served inert (3.8.6); `.env` and other repo files can never be read through Lantern. This closes v1's any-file read and same-origin HTML hole.
- MCP output redacts secret-looking `run.env` values (3.7.5). `GET /api/instances/{id}/manifest` returns them unredacted; it is a REST-only call for the user's own tools.
- A `docs` file or API response can contain text aimed at the agent ("ignore previous instructions ..."). Lantern marks neither as trusted; the descriptions of `get_project_docs` and `call_tool_api` do not claim they are safe. Nothing in Lantern executes content from either.
- `start_branch` runs the branch's `setup` and `run.cmd`, i.e. code from the user's own repo, as the user. Branch names are validated (`^[A-Za-z0-9._/-]{1,200}$`, no leading `-`, no `..`) before reaching git. Agents can create worktrees but **cannot delete them** over MCP (REST/CLI only): removing a worktree can destroy uncommitted work.
- Logging never records request bodies, MCP arguments or env values.

---

## 7. Testing strategy

**Unit** (pure, fast)
- `mcp_install`: golden before/after files for each client, including existing unrelated servers, comments in TOML, invalid input left untouched, idempotence.
- `UpstreamPolicy` table test: paths `@evil.com`, `//evil.com`, `http://x`, `\x`, `/a\nb`; IPs `169.254.169.254`, `0.0.0.0`, `::1`, `127.0.0.1:4777`, `:2019`; hostnames not in the allowlist; redirect response not followed.
- `guard`: Host x Origin x Sec-Fetch-Site matrix (including `lantern.glow`, missing Host, `Origin: null`).
- Target resolution (3.7.3) and status-filter mapping; risk inference and path-template matching.
- OpenAPI parser on fixtures: 3.0, 3.1, 2.0, YAML, refs, huge, malformed; docs scanner on a temp tree (symlink escape, node_modules pruning, `max_files`, oversize).
- `ApiError` mapping: one test per error code to HTTP status.

**Integration** (in-process, real axum on `127.0.0.1:0`, registry/runtime built over a tempdir with the fake process backend that `10-runtime` provides for tests)
- REST: every row of 3.3 at least once; `wait` semantics (200/202/error mapping); duplicate `start` joins the operation; conflicting op returns 409.
- SSE: snapshot content and order; `state=false`; log backlog then live lines with no duplicate/gap; forced lag produces `resync` then close; 33rd stream refused.
- MCP via the `rmcp` client over `StreamableHttpClientTransport` (feature `transport-streamable-http-client-reqwest`, dev-dependency): `initialize`, `tools/list` compared with a **golden file** (`tests/golden/tools.json`: all 21 names, descriptions, schemas; a rename or schema change fails CI), each tool once against fixtures, `resources/list`/`read`, `prompts/get`. Assert no `Mcp-Session-Id` header is ever issued. Restart the server on the same port between two calls with the *same client*: the second call must succeed (the v1 regression).
- Progress: call a slow fake tool with a `progressToken`, assert notifications arrive; drop the connection, assert `ctx.ct` fired and the operation still runs.

**Manual** (checklist in `docs/manual-tests.md` owned by 20-roadmap)
- `lantern mcp install claude-code|codex|cursor`, then in each client list tools, run `list_projects`, `start_branch`, restart the daemon, call again **without reconnecting**. Confirms the stateless assumption per client; if one fails, flip `legacy_session_mode` (3.7.1).
- Open `https://lantern.glow/myapp/docs` in a browser; open a malicious page that tries `fetch('http://127.0.0.1:4777/api/projects', {method:'POST'})` and a DNS-rebinding page; both must fail.
- Per v1 `TESTING-MCP.md`: debug MCP problems with curl and `journalctl --user -u lantern`, never by calling MCP tools from the agent being debugged.

---

## 8. Open questions

1. **Token auth** (recommend: no, 6.2). Revisit only for multi-user or LAN binding.
2. **MCP Tasks extension** for operations (rmcp implements SEP-2663; Claude Code understands task handles). Would let a client track `start_branch` natively instead of polling `get_operation`. Recommend: defer to after v2.0; our `Operation` maps cleanly (`id`, `status`, `step`), but stateless-mode support and client behavior need testing first. Tools-based polling works everywhere today.
3. **Should agents be able to remove worktrees over MCP?** Recommend: no in v2.0 (data-loss risk). If added later, a `remove_branch` tool must refuse dirty worktrees and require `confirm=true`.
4. **Custom headers for `call_tool_api`** (some tools may need an auth header). Recommend: no; if a tool needs auth, put it in the tool's own config. Revisit on demand.
5. **Docs on by default** (3.8.2). Recommend: yes. If privacy matters for some project, `docs_auto.enabled: false`.
6. **Real-client verification of stateless mode** for Claude Code, Codex and Cursor. I could not run them from this environment. Recommend: first task of the MCP milestone; the fallback is a one-line config flip (3.7.1).
7. **Return `structuredContent` too?** Recommend: no for now (duplicate tokens in some clients); add per tool with an `outputSchema` if a client benefits.
8. **MCP elicitation for risky calls** (a real "are you sure?" dialog instead of the `confirm` argument). Needs server-to-client requests, hence stateful sessions, which would bring back stale-session handling. Recommend: stay stateless; use `confirm` plus the client's own permission prompt.

---

## 9. Proposed changes to core architecture

1. **Events carry views, not full domain objects.** `Event::InstanceUpdated { instance: InstanceView }` and `ProjectUpdated { project: ProjectSummary }` (types in `lantern-core::api`). `Instance` holds a full `ResolvedManifest` (run env, endpoint lists); sending it on every status change is wasteful and leaks env values to every SSE subscriber. `GET /api/instances/{id}/manifest` serves the full manifest.
2. **Log lines leave the shared bus.** A noisy dev server can lag every SSE client if `LogLine` shares the `broadcast` channel with state events. Keep `LogLine` in the wire format, but `10-runtime` serves it per instance through `tail_and_subscribe` (4.2); `Event::LogLine` is removed from the in-process enum. This also gives the atomic backlog-plus-live handoff the SSE handler needs.
3. **Lag handling is "reconnect = re-fetch" (core §9).** `resync` is followed by closing the stream; the new connection begins with a full snapshot (3.5). Same intent as §9, one recovery path.
4. **Add `operations.rs` to the daemon layout** (`OperationTracker`, `Operation` types in `lantern-core::api`). §8/§10 already imply it ("returns the in-flight operation's result", `OperationId`, `GET /api/operations/{id}`) but no module owns it. `OperationProgress.instance` stays an `InstanceId`; multi-instance actions (stop-all, profile activate) create one operation per instance.
5. **§10 Host/Origin rule, refined.** Allowed Hosts: `127.0.0.1:<port>`, `localhost:<port>`, and `lantern.<tld>` (the Caddy route for the daemon; any port). Origin: absent, or one of the daemon's own origins. Additionally reject `Sec-Fetch-Site` other than `same-origin`/`none`, send no CORS headers, require `application/json` on bodies, and forbid state change on `GET`.
6. **MCP is stateless streamable HTTP** (`legacy_session_mode = false`). §3 diagram and §5 table: `rmcp` is `3.x` (verified 3.9.0), and the daemon never initiates requests to MCP clients.
7. **Reserved ids.** `ProjectId` must not be `api`, `mcp` or `lantern` (they collide with routes and the daemon's own domain). The registry returns `reserved_id` (422). Needed from `14-config`/registry.
8. **Drop `lighthouse.<tld>`.** §3 diagram keeps `/<project>/docs`, served on any allowed host. `13-system` creates no `lighthouse` Caddy site.
9. **Intel is per instance** and lives in `intel/`; add `intel-cache.json` (disposable) to §7's persistence table: `$XDG_STATE_HOME/lantern/intel-cache.json`, JSON, versioned, daemon-written, safe to delete.
10. **`?wait=true` semantics (§10).** `200` when the operation finished OK, error envelope (status from the error code) when it failed, `202` with the current operation if `timeout` elapsed first. Without `wait`: always `202`, except true no-ops (`200`, `operation.noop = true`).
11. **`lantern-core` also hosts `mcp_install`** (pure string functions) next to `api`; still no tokio.
12. **Additional dependencies for this subsystem** (§5 "may add crates"): `async-stream`, `globset`, `walkdir`, `url`, `percent-encoding`, `toml_edit` (CLI side), `tokio-stream` (sync feature), `bytes`; `axum` with feature `macros`; `rmcp` with `transport-streamable-http-server`; `reqwest` with `json` and `stream`.
