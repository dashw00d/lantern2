# 13 — System Integration (Caddy, TLS, DNS, privileges, services, packaging)

Status: design draft for review. Builds on `01-architecture.md` (authoritative). Where this doc
disagrees with it, the disagreement is listed in section 9 — nothing diverges silently.
v1 paths are relative to `.reference/lantern-v1/`.

## 0. Plain-language summary

Lantern makes `https://myapp.glow` work on your laptop. That takes five pieces of system plumbing:

1. **A web proxy (Caddy).** Lantern ships its own copy of Caddy and runs it as *your* user, as a
   systemd user unit called `lantern-caddy`. The Lantern daemon is the only thing that configures
   it: whenever something starts or stops, the daemon sends Caddy the complete new config over a
   private socket. If the daemon restarts, Caddy keeps serving.
2. **HTTPS certificates.** Lantern creates its own small certificate authority (CA) once. It is
   *name-constrained*: even if its key leaked, it could only ever sign names ending in `.glow`.
   Caddy uses it to issue a certificate per hostname. The CA is trusted once in the system store,
   Chrome's store, and every Firefox profile (including the Snap).
3. **DNS.** A ~200-line DNS responder inside the daemon answers every `*.glow` lookup with
   `127.0.0.1`. One small file tells `systemd-resolved` to send `.glow` lookups (and only those)
   to it.
4. **A one-time root step.** Exactly three things need root: letting Caddy bind ports 80/443,
   installing the DNS file, and trusting the CA system-wide. `lantern init` does them through one
   graphical password prompt (`pkexec`). Every step is idempotent and re-checkable by
   `lantern doctor`. After that **nothing needs root, ever**. v1's sudoers file is deleted.
5. **Shared services and packaging.** Mailpit, Redis and Postgres run as user-level units or are
   shown read-only if the system already runs them. A `.deb` (built with `cargo-deb`) installs the
   binary, a pinned Caddy, the user units and desktop files, and migrates v1 leftovers.

### Decisions at a glance

| # | Question | Decision |
|---|---|---|
| 1 | Caddy process | Transient **user unit `lantern-caddy.service`** created by the daemon with `systemd-run --user`; survives daemon restarts. Not a daemon child. |
| 2 | Caddy config | **Full `POST /load`** of the complete JSON on every change (idempotent). No incremental `/config/...` patches. |
| 3 | Admin endpoint | **Unix socket** `$XDG_RUNTIME_DIR/lantern/caddy-admin.sock` (0600), not `:2019`. |
| 4 | Binary | **Bundled upstream Caddy** at `/usr/lib/lantern/caddy` (Ubuntu's apt Caddy is 2.6.2: too old, plus a conflicting system service). |
| 5 | Privileged ports | **sysctl** `net.ipv4.ip_unprivileged_port_start=80` (survives Caddy upgrades). `setcap` accepted by `doctor` if the user prefers it. |
| 6 | Bind address | Loopback only (`127.0.0.1`); IPv6 and LAN are opt-in settings. HTTP/3 off. |
| 7 | Certificates | **Per-host managed certs** (no on-demand TLS) from a **Lantern-generated, name-constrained root**; Caddy issues leaves and the intermediate. |
| 8 | DNS server | **Hand-rolled UDP+TCP responder** on `127.0.0.1:5354` (no `hickory-server`). |
| 9 | DNS routing | A **systemd-resolved DNS delegate** file (systemd >= 258), *not* `resolved.conf` (3.6.3). Fallback: TLD `localhost` needs no DNS at all. |
| 10 | Elevation | **`pkexec lantern init --system-steps`** (polkit action shipped); the printed fallback is the equivalent `sudo` command. |
| 11 | Services | Mailpit: user unit + user-owned binary. Redis: user unit running the apt binary. Postgres: user-level cluster if 5432 is free, otherwise read-only view of the system one. PHP-FPM: Lantern-managed user pool. |
| 12 | Mail injector | Drop `.env` file editing (v1 code was never called); inject the same variables as process environment instead. |
| 13 | Daemon unit | `/usr/lib/systemd/user/lantern.service`, `Type=notify`, enabled globally by postinst; **no linger by default**. |
| 14 | Packaging | `cargo-deb`; hand-written maintainer scripts; pinned Caddy fetched at build time with sha512 check. |

---

## 1. Purpose & scope

**In scope:** Caddy lifecycle and config; the local CA and browser/system trust; the DNS responder
and resolver routing; what needs root and how it is invoked (`lantern init`, `lantern doctor`);
systemd user units for the daemon; shared services; the `.deb`; migration of v1's *system* bits;
the "app is stopped" gateway page served by the daemon.

**Not in scope** (owner in brackets): process supervision, `systemd-run` for instances and port
allocation [10-runtime]; worktrees and setup steps [11-instances]; finding listeners and the
adopt decision [12-discovery]; `config.toml`/`lantern.yaml` schema and v1 *data* import
[14-config]; REST/SSE/MCP surface and the Host/Origin middleware [15-api-mcp]; CLI/GUI rendering
of `init`/`doctor`/`services` [16-clients].

## 2. v1 behavior audit

| v1 file | What it does | Verdict |
|---|---|---|
| `daemon/lib/lantern/system/caddy.ex` | One `/etc/caddy/sites.d/<name>.caddy` per project; base Caddyfile `{ local_certs } import /etc/caddy/sites.d/*.caddy`; `sudo systemctl reload caddy` (fallback `restart`). Route kinds: `reverse_proxy 127.0.0.1:<port>`, PHP (`root * <path>/public`, `php_fastcgi unix//run/php/php8.3-fpm.sock`, `file_server`), static (`file_server`). `lighthouse.glow` → daemon :4777. `generate_config_with_websocket` hand-adds `header_up Upgrade/Connection` (never called; Caddy proxies upgrades natively). | **Replace** the mechanism (admin-API JSON, user process). **Keep** the three route kinds and the docs host (now `lantern.<tld>`). **Drop** sites.d, Caddyfile, the manual websocket headers. |
| `system/tls.ex` | `sudo caddy trust`. `trusted?/0` only checks that `~/.local/share/caddy/pki/.../root.crt` *exists* — it never checks any trust store, and a system Caddy keeps its CA under `/var/lib/caddy`, so it reports false positives. | **Replace** with Lantern-owned CA, explicit per-store trust steps, and real verification. |
| `system/dns.ex` | Writes `address=/.glow/127.0.0.1` to `/etc/NetworkManager/dnsmasq.d/lantern.conf`, then `sudo systemctl restart NetworkManager` (drops the network). Only effective if NM is configured `dns=dnsmasq`; Ubuntu's default is systemd-resolved, where that file is inert (this machine has the directory but no `dns=dnsmasq`). | **Replace** (embedded responder + resolved delegate). |
| `system/privilege.ex` | `sudo -n`, with writes staged as `/tmp/lantern_priv_<rand>` then `sudo cp`. | **Drop.** |
| `system/systemd.ex` | `sudo systemctl start/stop/...`; `install_unit/2` writes `/etc/systemd/system/*` through `sudo cp`. | **Drop** (user units, no root). |
| `system/health.ex` | `{dns, caddy, tls, daemon}` each `{status: ok/warning/error, message}`; "caddy ok" only means the binary exists. | **Replace** with `lantern doctor` checks (3.8) and the `SystemHealth` event. |
| `services/mailpit.ex` | System unit `User=nobody`, `ExecStart=/usr/local/bin/mailpit`, installed into `/etc/systemd/system` via sudo. Ports SMTP 1025, UI 8025. | **Replace** (user unit). Keep the ports. |
| `services/redis.ex`, `postgres.ex` | `sudo systemctl start/stop redis-server / postgresql`; health = TCP connect to 6379 / 5432. | **Change**: never stop a system service we did not start (3.10). |
| `services/mail_injector.ex` | Writes a marker block (`# >>> lantern mailpit >>>`) with SMTP vars into `.env` / `.env.local` per framework, with a `.lantern.bak` backup. **Nothing calls it** (grep over `daemon/lib` and `desktop/src` finds no caller; the UI's MailTab only reads `project.features.mailpit`). | **Drop the file editing**, **keep the variable tables** as process-environment injection (3.10.5). |
| `lantern_web/controllers/service_controller.ex`, `system_controller.ex` | `/services` list/start/stop; `POST /system/init` runs DNS+TLS+Caddy steps *inside the daemon via sudo*; `POST /system/shutdown` also stops mailpit/redis/postgres. | **Change**: `init` is CLI-driven (root cannot run inside the user daemon); shutdown never touches services. |
| `packaging/sudoers` | NOPASSWD for `systemctl start/stop/reload/restart/enable/disable caddy`, mailpit/redis/postgres units, `restart NetworkManager/dnsmasq`, `daemon-reload`, `caddy trust`, `mkdir/chown` on `/etc/caddy/sites.d`, and **`cp /tmp/lantern_priv_* /etc/systemd/system/*`** and `rm -f /etc/systemd/system/lantern-*`. | **Delete entirely.** sudoers(5): `*` in arguments also matches `/`, `..` and spaces, so `cp /tmp/lantern_priv_x /etc/systemd/system/../../sudoers.d/x` is allowed. Any process of the user can write `/tmp/lantern_priv_x` and become root. The `/etc/systemd/system/caddy.service` overwrite plus `systemctl start caddy` is a second route. |
| `packaging/postinst`, `prerm`, `postrm`, `lanternd.service`, `control`, `build-deb.sh`, `lantern.desktop` | Installs sudoers; rewrites `/etc/caddy/Caddyfile`; `su - $user -c "caddy trust"`; dnsmasq file + NM restart; `sed`s `User=` into a **system** unit `lanternd.service` (WantedBy multi-user); everything under `/opt/lantern`, CLI symlinked to `/usr/local/bin/lantern`. `postrm` runs `rm -rf /etc/caddy/sites.d/` and removes sudoers even on *upgrade* (no `$1` check). `control` Depends: `sudo`, Electron libs; Recommends `caddy`, `dnsmasq`. Built by hand with `dpkg-deb`. | **Replace** (3.11). v1's scripts running on upgrade is *helpful* here: they stop `lanternd`, delete sudoers and `sites.d`. |
| `install.sh`, `reinstall.sh`, `setup-dev.sh`, `dev-up.sh`, `dev-down.sh`, `toggle-server.sh` | Adds Caddy's cloudsmith apt repo, installs dnsmasq, `dpkg -i`; nukes state; Valet/nginx ↔ Caddy toggling via `systemctl stop nginx`; kills whatever holds :4777. | **Drop all.** Install = `apt install ./lantern_*.deb`. Dev = `cargo run -p lantern -- daemon` (3.9.4). nginx/Valet conflicts become `doctor` findings with printed fix commands. |

---

## 3. Design

### 3.1 Where the code lives

`lantern-core` is "pure" (no process spawning), but `lantern init` and `lantern doctor` must work
**when the daemon is down** (that is when you need them) and `init --system-steps` runs as root.
Neither can live in `lantern-daemon` (the CLI must not depend on it). Proposal (section 9.1): a new
crate, **`lantern-system`**, synchronous (`std::process`, `std::fs`; no tokio), depended on by
`lantern-daemon` and the `lantern` binary:

```
crates/lantern-system/src/
  paths.rs        # LanternPaths: config/state/data/runtime dirs from XDG; one place for every path below
  step.rs         # Step trait: check() -> StepState, apply() -> StepOutcome  (root and user steps)
  steps/          # sysctl.rs  dns_delegate.rs  system_trust.rs  nss_trust.rs  user_units.rs  v1_migrate.rs
  ca.rs           # root CA generation / inspection (rcgen, x509-parser)
  doctor.rs       # run(&DoctorCtx) -> Vec<Check>
```
Shared plain data (`Check`, `Fix`, `CheckStatus`, `SystemHealth`, `ServiceStatus`) lives in
`lantern-core::api` so the client crate can deserialize it.

The long-running parts stay in `lantern-daemon` as the `net/` and `services/` modules from
`01-architecture.md` section 4:

```
net/  mod.rs  routes.rs (RoutePlan)  caddy_config.rs (render JSON)  caddy.rs (process + admin client)
      dns.rs (responder)  tls.rs (status/ensure_ca, wraps lantern-system::ca)  gateway.rs (axum pages)
      sync.rs (registry events -> RoutePlan -> Caddy::apply)
services/  mod.rs  mailpit.rs  redis.rs  postgres.rs  php_fpm.rs
```

### 3.2 Paths and settings consumed

| What | Path |
|---|---|
| Caddy storage root (CA, certs) | `$XDG_DATA_HOME/lantern/caddy/` (core doc). CA: `pki/authorities/local/{root.crt,root.key,intermediate.crt,intermediate.key}`; leaves: `certificates/local/<host>/` |
| Last-applied Caddy config (also Caddy's start-up config) | `$XDG_STATE_HOME/lantern/caddy.json` (0600) |
| Caddy admin socket, Postgres socket dir, php-fpm socket/pid | `$XDG_RUNTIME_DIR/lantern/` (0700) |
| Downloaded binaries (mailpit) | `$XDG_DATA_HOME/lantern/bin/` |
| Service data | `$XDG_DATA_HOME/lantern/services/{mailpit,redis,postgres/<major>}/` |
| Service logs | journald: `journalctl --user -u lantern-svc-<name>`; Caddy: `-u lantern-caddy` |

`$XDG_RUNTIME_DIR` unset (e.g. a bare `ssh` without a PAM session) is a startup error with the
message "no user systemd session: log in graphically or enable lingering".

`config.toml` keys this doc reads (14-config owns the file and should adopt these names verbatim):

```toml
tld = "glow"                       # "localhost" = no-DNS mode (3.6.4)
[proxy]    http_port = 80;  https_port = 443
           bind = "loopback"       # or "all": LAN-reachable dev servers, opt-in
           ipv6 = false            # true: listen on [::1] too and answer AAAA
[caddy]    binary = ""             # "" = auto (3.4.1)
[dns]      listen = "127.0.0.1:5354"
[php]      fpm_socket = ""         # "" = managed user pool (3.10.4)
[services.mailpit]  enabled = true # likewise redis, postgres; mode = "auto"|"user"|"system"; port = <n>
```

### 3.3 Domains, hosts and the route model

A **Route** is "this hostname is served by that target". `net/sync.rs` builds the complete
`RoutePlan` from the registry on every relevant event and hands it to `Caddy::apply`.

```rust
pub struct Route { pub host: String, pub owner: RouteOwner, pub target: RouteTarget }
pub enum RouteOwner { Instance(InstanceId), Service(&'static str), System(&'static str) }
pub enum RouteTarget {
    Proxy  { upstream: SocketAddr, tls: bool },                 // managed process or adopted listener
    Php    { docroot: PathBuf, fpm_socket: PathBuf, env: Vec<(String, String)> },
    Static { docroot: PathBuf },
    Down,                                                       // known host, nothing to serve: gateway page
    Gateway,                                                    // lantern.<tld> -> the daemon
}
pub struct RoutePlan { pub routes: Vec<Route>, pub skipped: Vec<(String, String)> /* host, reason */ }
```

`build_plan` is a **pure function** (unit-tested with fixtures). Mapping rules:

| Input | Target |
|---|---|
| Instance, serve mode *process*, status `Running`/`Unhealthy`/`Starting`/`Stopping`, `runtime.port` set | `Proxy { 127.0.0.1:port, tls: manifest scheme == https }` (Starting shows the gateway page until the port opens; it auto-refreshes) |
| Instance, status `External` **and** (`adopt` or `discovery.auto_adopt`) (→ 02 D9: discovery sets `External` for every matched hand-started server; only adopted ones are routed) | `Proxy { upstream: normalize(listener.addr) }`: unspecified `0.0.0.0` / `[::]` becomes `127.0.0.1`; a specific address (`[::1]`, a LAN IP) is dialed as-is |
| Instance, serve mode *php* / *static*, status `Running` (= "activated", no process) | `Php { docroot, fpm_socket, env }` / `Static { docroot }` |
| Any other instance status (`Stopped`, `Crashed`, `NeedsConfig`, `Preparing`, no port) | `Down` |
| Service with a UI (`mailpit.<tld>`) | `Proxy { 127.0.0.1:8025 }` if running else `Down` |
| `lantern.<tld>` | `Gateway` (docs/lighthouse; `/api/*` and `/mcp*` are 404 there) |

Stopped instances **keep a route** so `myapp.glow` shows "myapp is stopped — Start" instead of a
DNS/TLS failure. Hostnames must match `^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$`,
total <= 253; violators go to `skipped` (and a log warning) instead of poisoning the whole config.
Duplicate hosts: first in sorted order wins, the rest are `skipped`. `lantern` and `mailpit` are
**reserved** labels (registry must refuse them as `ProjectId`s; v1 self-registered a project named
`lantern`).
`RoutePlan.routes` is sorted by host so the rendered JSON is deterministic.

Public URLs always go through one helper, because ports may be non-default:
`net::public_url(&settings, domain) -> "https://domain"` (plus `:<https_port>` if != 443).
`${URL}` in manifests (10-runtime) uses it.

### 3.4 Caddy

#### 3.4.1 Process: separate user unit, not a daemon child

Reasons: (a) existing browser tabs keep working while the daemon restarts or upgrades — the same
"survive the daemon" property the core doc requires of dev servers; (b) systemd restarts it on crash
and captures its log; (c) no process-tree bookkeeping in Rust. The unit is **transient**, created by
the daemon, so there is no unit file to package or path-template, and a source build works the same
as the `.deb`:

```
systemd-run --user --unit=lantern-caddy --description="Lantern Caddy (reverse proxy + local CA)" \
  --collect --quiet -p Restart=on-failure -p RestartSec=1 -p TimeoutStopSec=10 \
  -- /usr/lib/lantern/caddy run --config /home/ryan/.local/state/lantern/caddy.json
```
(Absolute paths only; escape `%` as `%%` because systemd expands specifiers in transient units.
`systemd-run` is called through the assumed `UserSystemd` interface, section 4.2.)

*Binary resolution:* `[caddy].binary` -> `/usr/lib/lantern/caddy` -> first `caddy` on `PATH` ->
`/usr/bin/caddy`. `caddy version` must report **>= 2.8.0** (needs `stream_close_delay`, added in
2.7.0; Ubuntu 26.04's apt `caddy` is 2.6.2 and fails this check). Everything below was verified
against **Caddy v2.11.9**.

*Lifecycle:*

| Event | Action |
|---|---|
| Daemon start | `Caddy::ensure_running()`: if `systemctl --user is-active lantern-caddy` is not `active`, ensure the CA exists (3.5.1), write `caddy.json` (last config, else the empty plan), run the command above, poll the admin socket (`GET /config/`) up to 5 s. Then `apply` the current plan. |
| Daemon stop/restart | **Leave Caddy running.** |
| Caddy crash | systemd restarts it; it starts from `caddy.json` (the last applied config), so routes survive even with the daemon down. |
| Health loop (every 15 s) | `systemctl --user show -p ActiveState,MainPID lantern-caddy`. Not active, or MainPID changed since the last apply, so re-apply. If `readlink /proc/<MainPID>/exe` ends in ` (deleted)` (package upgrade replaced the binary), `systemctl --user restart lantern-caddy`. |
| `lantern proxy restart` / uninstall | `systemctl --user restart lantern-caddy` / `systemctl --user stop lantern-caddy`. |

#### 3.4.2 Admin API and `apply` (full reload)

*Endpoint:* `"admin": {"listen": "unix//run/user/1000/lantern/caddy-admin.sock|0600"}` — the `|0600`
suffix sets the socket mode (verified). Not `:2019`: an apt-installed system Caddy also uses 2019
and a mis-aimed `POST /load` would overwrite *its* config. (Unix paths are limited to ~107 bytes;
`$XDG_RUNTIME_DIR/lantern/caddy-admin.sock` is far below that.) Client: `reqwest::ClientBuilder::unix_socket(path)`
(present in reqwest 0.13.5); requests use `http://localhost/...`.

*Why a full `POST /load` rather than `/config/...` patches:* every admin-API change makes Caddy
build and swap a whole new config anyway, so patches buy nothing. A full document is **idempotent**
(posting the same body twice returns 200 in <1 ms and changes nothing, measured), has no ordering
or path-index bugs, and the same JSON doubles as the on-disk start-up config.

```
apply(plan):                                   # serialized by a tokio Mutex; callers coalesce (below)
  text = serde_json::to_string_pretty(render(plan))      # BTreeMap keys: deterministic
  if text == self.last_applied { return Unchanged }
  resp = POST /load  Content-Type: application/json  body=text   (timeout 20 s)
  200            -> atomic write caddy.json (tmp + rename, 0600); last_applied = text; Applied
  400 {"error"}  -> keep last good; return Err(Rejected(error)); SystemHealth: caddy = fail(error)
  socket error   -> ensure_running(); retry once; else Err(Unavailable)
```
Write-after-success matters: if the file were written first, a rejected config would also become
Caddy's start-up config and Caddy would fail to boot. A 400 leaves the previous config running
(verified: bind conflict returned `400 {"error":"loading config: ... address already in use"}` and
the old config kept serving).

*Coalescing:* `sync.rs` listens to `InstanceUpdated/Removed`, `ListenersChanged`, `ServiceUpdated`
and settings changes; on any, it marks dirty and waits 150 ms (max 1 s) before building one plan
and applying once. An event during an in-flight apply just re-marks dirty.

*Websockets across reloads (important):* by default Caddy **closes every proxied WebSocket on every
config reload** (docs say so; reproduced: close code 1001). Every instance start/stop reloads, so
all Vite HMR sockets in all tabs would drop (and Vite then force-reloads the page). Every
`reverse_proxy` therefore carries `"stream_close_delay": "24h"`; verified that sockets survive
repeated reloads. Trade-off: a socket whose upstream route changed stays on the old upstream until
it closes. Acceptable for dev.

#### 3.4.3 The config Lantern renders

`caddy_config.rs::render(&RoutePlan, &RenderCtx) -> serde_json::Value` (built with `json!`). The
skeleton (production ports; user `ryan` for illustration). **This exact structure was loaded into
Caddy 2.11.9 and exercised with curl and a WebSocket client** (per-host certs incl. nested names,
h2, HTTP->308, static/PHP/HTTPS-upstream routes, error page, WebSocket across reloads, 400 keeps old config):

```json
{
  "admin":   { "listen": "unix//run/user/1000/lantern/caddy-admin.sock|0600", "config": { "persist": false } },
  "logging": { "logs": { "default": { "level": "INFO", "encoder": { "format": "console" }, "writer": { "output": "stderr" } } } },
  "storage": { "module": "file_system", "root": "/home/ryan/.local/share/lantern/caddy" },
  "apps": {
    "pki": { "certificate_authorities": { "local": {
      "name": "Lantern Local CA",
      "install_trust": false,
      "root": { "certificate": "/home/ryan/.local/share/lantern/caddy/pki/authorities/local/root.crt",
                "private_key": "/home/ryan/.local/share/lantern/caddy/pki/authorities/local/root.key",
                "format": "pem_file" },
      "intermediate_common_name": "Lantern Local CA Intermediate",
      "intermediate_lifetime": "720h" } } },
    "tls": { "automation": { "policies": [ { "issuers": [ { "module": "internal", "ca": "local", "lifetime": "168h" } ] } ] } },
    "http": {
      "http_port": 80, "https_port": 443,
      "servers": {
        "lantern-https": {
          "listen": ["127.0.0.1:443"],
          "protocols": ["h1", "h2"],
          "automatic_https": { "disable_redirects": true },
          "routes": [ /* one ROUTE per host, sorted */ ],
          "errors": { "routes": [ {
            "match":  [ { "expression": "{http.error.status_code} in [502, 503, 504]" } ],
            "handle": [ /* PAGE */ ] } ] }
        },
        "lantern-http": {
          "listen": ["127.0.0.1:80"],
          "routes": [ { "handle": [ { "handler": "static_response", "status_code": 308,
                       "headers": { "Location": ["https://{http.request.host}{http.request.uri}"] } } ] } ]
        }
      }
    }
  }
}
```

Notes on each choice:
- `admin.config.persist:false` — no `autosave.json`; `caddy.json` (ours) is the only persisted config.
- `install_trust:false` — Caddy must never try `sudo`/trust-store writes (log confirms "installation disabled").
- `tls.automation.policies` has **one catch-all policy with the `internal` issuer**; without it Caddy would try Let's Encrypt for `myapp.glow`. Because every route host is a literal `host` matcher, Caddy obtains a certificate for each at load time (managed, not on-demand). See 3.5.2.
- `protocols:["h1","h2"]` — HTTP/3 off (one fewer UDP listener; nothing in dev needs it).
- `disable_redirects` + our own `lantern-http` server: redirects are visible config, and the redirect also fires for unknown names (which then fail TLS with a clear error rather than hanging). 308 keeps the method. If `https_port != 443`, the `Location` becomes `https://{http.request.host}:<port>{http.request.uri}`.
- `listen`: `bind=loopback` -> `["127.0.0.1:443"]` (`ipv6=true` adds `"[::1]:443"`; `bind=all` -> `[":443"]`). Loopback by default so dev servers are not exposed to the LAN.
- `errors.routes` with the `expression` matcher limits the Lantern page to proxy failures (502/503/504). A static site's own 404 stays Caddy's plain 404 (verified).

**ROUTE** (identical wrapper for every host; `<TARGET>` per 3.3):

```json
{ "match": [ { "host": ["myapp.glow"] } ], "terminal": true,
  "handle": [ { "handler": "subroute", "routes": [
    { "match": [ { "path": ["/__lantern/*"] } ], "terminal": true, "handle": [ <GATEWAY_PROXY> ] },
    { "handle": [ <TARGET> ] } ] } ] }
```
The first inner route reserves `/__lantern/*` on every hostname for the gateway (3.4.5).

**GATEWAY_PROXY** — the daemon, with the Host header rewritten so the daemon's DNS-rebinding guard
(`Host` must be `127.0.0.1:4777`) passes; reverse_proxy already adds `X-Forwarded-Host` with the
original host:

```json
{ "handler": "reverse_proxy", "upstreams": [ { "dial": "127.0.0.1:4777" } ],
  "headers": { "request": { "set": { "Host": ["127.0.0.1:4777"] } } } }
```

**TARGET A — `Proxy`** (managed process or adopted listener):

```json
{ "handler": "reverse_proxy", "upstreams": [ { "dial": "127.0.0.1:41003" } ],
  "flush_interval": -1, "stream_close_delay": "24h",
  "transport": { "protocol": "http", "dial_timeout": "2s" } }
```
`dial` for an IPv6 listener is `"[::1]:5173"`. Websocket/HMR needs nothing extra: `reverse_proxy`
handles `Upgrade` natively (verified through TLS with a WebSocket echo server; Vite's client
connects to `wss://<page host>:443/`). `X-Forwarded-For/Proto/Host` are set by default. `flush_interval:-1`
= no buffering (SSE, streamed SSR). **TARGET B** (`tls:true`, dev server speaks HTTPS) adds
`"tls": { "insecure_skip_verify": true }` inside `transport`.

**TARGET C — `Php`.** This is Caddy's own `php_fastcgi` expansion (output of `caddy adapt` for
`root * <docroot>` + `php_fastcgi unix//<sock>` + `file_server`), with the per-instance mail
variables added as FastCGI `env`. `docroot` for a Laravel project is `<checkout>/public`
(manifest `root` override; v1 default `public`):

```json
[ { "handler": "vars", "root": "/home/ryan/sites/myapp/public" },
  { "handler": "subroute", "routes": [
    { "match": [ { "file": { "try_files": ["{http.request.uri.path}/index.php"] }, "not": [ { "path": ["*/"] } ] } ],
      "handle": [ { "handler": "static_response", "status_code": 308,
        "headers": { "Location": ["./{http.request.orig_uri.path.file_escaped}/{http.request.orig_uri.prefixed_query}"] } } ] },
    { "match": [ { "file": { "split_path": [".php"], "try_policy": "first_exist_fallback",
        "try_files": ["{http.request.uri.path}", "{http.request.uri.path}/index.php", "index.php"] } } ],
      "handle": [ { "handler": "rewrite", "uri": "{http.matchers.file.relative}" } ] },
    { "match": [ { "path": ["*.php"] } ],
      "handle": [ { "handler": "reverse_proxy",
        "transport": { "protocol": "fastcgi", "split_path": [".php"], "env": { "MAIL_HOST": "127.0.0.1" } },
        "upstreams": [ { "dial": "unix//run/user/1000/lantern/php-fpm.sock" } ] } ] },
    { "handle": [ { "handler": "file_server" } ] } ] } ]
```
**TARGET D — `Static`:** `{ "handler": "file_server", "root": "<docroot>", "index_names": ["index.html"] }`.

**PAGE** (target `Down`, and the body of the error route): rewrite to the gateway page and pass the
original URI; the daemon decides what to say from `X-Forwarded-Host`:

```json
[ { "handler": "rewrite", "uri": "/__lantern/page" },
  { "handler": "reverse_proxy", "upstreams": [ { "dial": "127.0.0.1:4777" } ],
    "headers": { "request": { "set": { "Host": ["127.0.0.1:4777"],
      "X-Lantern-Orig-Uri": ["{http.request.orig_uri}"],
      "X-Lantern-Error-Status": ["{http.error.status_code}"] } } } } ]
```
(`X-Lantern-Error-Status` is simply empty on the `Down` route.) The status code and body come
from the daemon and pass through unchanged (verified: 503 for a stopped instance, original query
string preserved).

**System routes:** `lantern.<tld>` = ROUTE whose inner routes are
`[ {match path ["/api/*","/mcp","/mcp/*"] -> static_response 404, terminal}, {handle: [GATEWAY_PROXY]} ]`.
`mailpit.<tld>` = TARGET A to `127.0.0.1:8025` (or PAGE when mailpit is down).

#### 3.4.4 Ports: sysctl, not setcap

| | `sysctl net.ipv4.ip_unprivileged_port_start=80` | `setcap cap_net_bind_service=+ep <caddy>` |
|---|---|---|
| Survives Caddy replacement | yes | **no** — file capabilities live in an xattr; any package upgrade/replacement of the file drops them |
| Works with any Caddy binary (bundled, dev build, `PATH`) | yes | per binary |
| Scope | system-wide: any local process may bind 80-1023 | one binary |
| Verification | `sysctl -n` / read `/proc/sys/net/ipv4/ip_unprivileged_port_start` | `getcap` |
| User units | n/a | `AmbientCapabilities=` is not available to unprivileged user managers, so only file caps work, and `NoNewPrivileges=` would neutralize them |

**Decision: sysctl.** The machine is single-user (core non-goal: multi-user), the setting also
covers UDP, and the failure mode of setcap (silently losing the cap after an upgrade, then Caddy
failing with `permission denied` on :443) is exactly the support burden we want to avoid. The
sysctl value applies to IPv4 and IPv6 sockets and is per network namespace.
`/etc/sysctl.d/60-lantern-ports.conf`:
```
# Managed by Lantern (lantern init). Lets unprivileged processes bind ports >= 80 so the
# per-user Caddy can listen on :80 and :443 without root or file capabilities.
net.ipv4.ip_unprivileged_port_start=80
```
The step is **Done** whenever the current value is already <= min(http_port, https_port) (nothing
written), and skipped when both configured ports are >= 1024. `doctor` also accepts the
alternative: if the resolved Caddy binary has `cap_net_bind_service` in `getcap`, `ports.bind`
passes — so users who dislike the sysctl can `setcap` by hand, and `doctor` warns that it will not
survive a Caddy upgrade.

*Port conflicts (system Caddy, nginx, apache, Valet):* the symptom is `POST /load` -> 400
`address already in use`, mapped to `NetError::PortInUse { port }`. The daemon then names the
likely holder using read-only checks (`systemctl is-active caddy nginx apache2 httpd`, plus
`/proc/net/tcp` + `/proc/<pid>/fd` for processes of the same user) and surfaces it as the
`ports.free` doctor failure with the fix `sudo systemctl disable --now nginx`. **Lantern never
stops another service itself** (needs root; surprising). A holder listening on `0.0.0.0:443`
conflicts with our `127.0.0.1:443`. The special case of a *v1-configured* system Caddy is handled
by migration (3.12).

#### 3.4.5 Gateway pages (served by the daemon)

`net::gateway::router()` is mounted by `api/` (15-api-mcp) on the daemon listener, under the fixed
prefix `/__lantern/`, **outside** the REST middleware but with its own guard: `Host` must be
`127.0.0.1:4777` or `localhost:4777` (Caddy rewrites it); state-changing methods additionally need
`Origin == https://<X-Forwarded-Host>` (exact, including a non-default port), `X-Forwarded-Host`
must be a host in the current plan, and `Sec-Fetch-Site`, if sent, must be `same-origin`.

| Endpoint | Behavior |
|---|---|
| `GET /__lantern/page` | HTML (JSON if `Accept: application/json`). Looks up the host in the live plan + registry. *Stopped/Crashed/NeedsConfig*: **503**, `Retry-After: 2`, "**myapp is stopped**", last exit reason, a **Start** button, and the CLI hint `lantern on myapp`. *Starting*: 503 + auto-refresh every 1 s. *Proxy target but connection refused/timeout* (error route): **502** "myapp is running but not answering on 127.0.0.1:41003" + link to logs. *PHP, fpm socket unreachable*: 502 with the socket path and `lantern doctor`. *Unknown host under the TLD*: 404 listing known hosts (reachable only over HTTP->TLS if a cert exists; unknown names fail TLS first). Never caches (`Cache-Control: no-store`). |
| `GET /__lantern/status` | `{host, status, message, port}` — the page polls this (same origin) and reloads when `Running` and the port answers. |
| `POST /__lantern/start` | Starts the instance owning `X-Forwarded-Host` via the same internal call as `POST /api/instances/{id}/start`. |
| `GET /__lantern/ping` | `lantern-ok` — used by the `e2e.https` doctor check. |
| `GET /__lantern/ca.crt` | Public root certificate (for phones/other trust stores). |

The page is one `include_str!` HTML template with `{{placeholder}}` substitution; **every dynamic
value is HTML-escaped** (branch names and manifest text are attacker-influenced), one inline
`<style>` and one inline `<script>` guarded by a per-response CSP nonce, no external resources.
If the daemon itself is down, the error route's upstream fails and the browser sees Caddy's bare
502 — accepted (the daemon restarts in ~2 s).

### 3.5 TLS and the local CA

#### 3.5.1 Lantern generates the root; Caddy issues everything below it

Caddy's own auto-generated root is **unconstrained**: whoever reads `root.key` (any process running
as you — an npm postinstall script, say) could mint a certificate for *any* site that this machine
then trusts. Lantern instead creates the root itself with an X.509 **Name Constraints** extension
(`permitted: DNS:glow`), and gives it to Caddy through the `pki.certificate_authorities.local.root`
config above. Caddy still does what it is good at: it creates the intermediate (signed by our
root), issues and renews leaf certificates, and serves them. Verified with Caddy 2.11.9 + OpenSSL
3.5: leaf for `feature-x.myapp.glow` verifies; a leaf for `example.com` signed with the same root
key fails with `permitted subtree violation`. NSS (Chrome, Firefox), OpenSSL and GnuTLS enforce
name constraints.

`lantern-system::ca::ensure_root(paths, tld)` (runs in `lantern init` and again before every Caddy
start; no-op when valid):

```rust
let mut p = CertificateParams::default();                       // rcgen 0.14
p.distinguished_name.push(DnType::OrganizationName, "Lantern");
p.distinguished_name.push(DnType::CommonName, format!("Lantern Local CA ({hostname})"));
p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
p.name_constraints = Some(NameConstraints {
    permitted_subtrees: vec![GeneralSubtree::DnsName(tld.to_string())], excluded_subtrees: vec![] });
p.not_before = now - 1 day;  p.not_after = now + 10 years;
let key = KeyPair::generate()?;                                 // ECDSA P-256, PKCS#8 PEM on disk
let cert = p.self_signed(&key)?;
```
Files: dir `0700`, `root.key` `0600` (created with `OpenOptions::mode(0o600)`), `root.crt` `0644`.
The DNS constraint `glow` matches `glow` and every name below it (RFC 5280), so nested
`feature-x.myapp.glow` is covered. After generating, re-parse the certificate with `x509-parser`
and **fail** if the name-constraints extension is missing or not critical.

*Regeneration* happens when the root is missing/unparseable, expires in < 90 days, or its permitted
subtree differs from the configured TLD (changing `tld` therefore requires re-running
`lantern init`). Procedure: stop `lantern-caddy`; move old files to `*.old-<date>`; delete
`pki/authorities/local/intermediate.*` and `certificates/local/` (leaves signed by the old
intermediate would not chain); write the new root; start Caddy; `doctor` then reports the new CA as
untrusted until `lantern init` re-runs the trust steps.

#### 3.5.2 Per-host certificates, no on-demand TLS

Every Route host is a literal `host` matcher, so Caddy's automatic HTTPS issues a certificate for
each at config load (verified: one `certificates/local/<host>/` per route, nested names included,
all issued within the start-up second with the internal issuer). Rejected alternative: on-demand TLS with an `ask`
endpoint on the daemon. It would add a daemon dependency to the TLS handshake and a storage-filling
vector (a web page can make your browser request `a1.glow ... aN.glow`) in exchange for nothing:
unknown hostnames cannot be served anyway. Wildcard certificates (`*.myapp.glow` for multi-tenant
apps) are an open question (section 8).

Lifetimes: leaf `168h` (7 d), intermediate `720h` (30 d); Caddy renews both on its own. They are
longer than Caddy's 12 h / 7 d defaults so that a laptop asleep over a weekend does not wake to
expired certificates before the maintenance loop (10-minute interval) catches up.

#### 3.5.3 Trusting the root

Nickname everywhere: `Lantern Local CA`. The root is installed in three kinds of store.

| Store | Who/How | Command |
|---|---|---|
| System (`/etc/ssl/certs`: curl, PHP, Python `ssl`, Go, Ruby, git) | **root step** | read `root.crt` (opened with `O_NOFOLLOW`, parsed, checked to be a self-signed CA with name constraints, **re-encoded** to PEM), write `/usr/local/share/ca-certificates/lantern-local-ca.crt` (0644), run `update-ca-certificates` |
| Chrome / Chromium / Edge / Brave (shared NSS db) | user step | `mkdir -p ~/.pki/nssdb` (+ `certutil -d sql:$HOME/.pki/nssdb -N --empty-password` if the db does not exist); then the per-db command below |
| Chromium **Snap** | user step | same, db at `~/snap/chromium/current/.pki/nssdb` (if `~/snap/chromium/current` exists; snap HOME is the revision dir, path not verified on this machine) |
| Firefox deb/tarball | user step | every `~/.mozilla/firefox/*/cert9.db` |
| Firefox **Snap** | user step | every `~/snap/firefox/common/.mozilla/firefox/*/cert9.db` (this machine: profile `sxt75stw.default`) |
| Firefox Flatpak | user step | every `~/.var/app/org.mozilla.firefox/.mozilla/firefox/*/cert9.db` |

Per NSS database `<dir>` (idempotent: delete-if-present, then add):

```
certutil -d sql:<dir> -D -n "Lantern Local CA"                       # ignore "not found"
certutil -d sql:<dir> -A -t "C,," -n "Lantern Local CA" -i <root.crt>
certutil -d sql:<dir> -L -n "Lantern Local CA" -a                    # check: PEM -> DER -> sha256 must equal root.crt
```
`certutil` comes from `libnss3-tools` (a `Depends` of the `.deb`; not installed on this machine, so
`doctor` reports `tls.certutil` with `sudo apt install libnss3-tools`). Browsers read NSS databases
at start-up, so **restart the browser** after `init`. Profiles created later are untrusted until
`lantern init` is re-run (`doctor` lists them). Browsers other than these (Epiphany/GNOME Web, Qt
WebEngine) use the system store or their own; out of scope.

Node.js ignores the system store. `Tls::root_cert_path()` lets 10-runtime export
`NODE_EXTRA_CA_CERTS=<root.crt>` (additive, safe) into instance environments. Do *not* set
`SSL_CERT_FILE`/`REQUESTS_CA_BUNDLE` (they replace the default bundle).

### 3.6 DNS

#### 3.6.1 The responder (`net/dns.rs`, `Dns::serve`)

Hand-rolled, not `hickory-server`. `hickory-server` 0.26.3 is healthy (updated Sep 2026) but pulls a
large dependency tree for ~one A record, and its API changed in each of 0.24/0.25/0.26, which is
poor footing for agents working from older training data. A UDP+TCP responder that only answers
`*.<tld>` is ~200 lines on tokio. **Escape hatch:** if it grows past ~400 lines or needs more
record types, switch to `hickory-server` (any `hickory-proto` use stays test-only).

*Listen:* `127.0.0.1:5354` UDP **and** TCP (`[dns].listen`). Not 5353 (Avahi/mDNS owns it) and not
the resolved stub addresses `127.0.0.53`/`127.0.0.54`. Port > 1023 so it needs no privilege.
Loopback-only, so there is no amplification exposure. Bind failure is non-fatal for the daemon: log,
`SystemHealth` `dns.responder = fail`.

*Algorithm* (per datagram; TCP reuses it with a 2-byte length prefix, idle timeout 5 s, <= 64
connections):
1. `len < 12` -> drop. `QR=1` -> drop. `OPCODE != 0` -> reply `NOTIMP`. `QDCOUNT != 1` -> `FORMERR`.
2. Parse `QNAME` as plain labels (each <= 63, total <= 255); a compression pointer in the question
   -> `FORMERR`. Then `QTYPE`, `QCLASS`. Walk the rest of the packet for an **OPT** RR (type 41).
3. `QCLASS` not IN/ANY -> `REFUSED`. Name (lower-cased) not equal to or below `<tld>` -> `REFUSED`.
   Name equal to `<tld>` itself -> `NOERROR`, empty answer.
4. `A` -> one record `127.0.0.1`. `AAAA` -> `::1` if `[proxy].ipv6`, else `NOERROR` with an empty
   answer (NODATA — Caddy only listens on IPv4 loopback by default). **Every other type** (notably
   `HTTPS`/type 65, which Firefox and Chrome send for every host, plus `TXT`, `MX`, `ANY`) ->
   `NOERROR`, empty answer. Never `NXDOMAIN`/`SERVFAIL` for names below the TLD.
5. Reply: same ID; flags `QR=1, AA=1, RD` copied, `RA=0`; the question section echoed byte-for-byte
   (preserves 0x20 case randomization); answer `NAME=0xC00C` (pointer to the question), `CLASS=IN`,
   **TTL 60**; if the query had an OPT RR append one (`NAME=0`, `CLASS`=1232, TTL 0, empty RDATA) so
   resolved's EDNS probing keeps the server at the highest feature level.
Every name below the TLD resolves, known project or not; unknown ones fail later at TLS (3.5.2).

#### 3.6.2 Pointing systemd-resolved at it: a DNS delegate

`/etc/systemd/dns-delegate.d/lantern.dns-delegate` (mode 0644):
```ini
# Managed by Lantern (lantern init). Do not edit; changes are overwritten.
# Sends every lookup below ".glow" to Lantern's local DNS responder (run by `lantern daemon`).
# "~" = route-only: this server is never used for any other name.
[Delegate]
DNS=127.0.0.1:5354
Domains=~glow
```
Apply = write the file, then `systemctl reload systemd-resolved` (the unit is `Type=notify-reload`;
reload re-reads delegate files — `manager_dispatch_reload_signal` calls `manager_load_delegates`).
No restart, no NetworkManager involvement, no network blip. `DNS=` uses the `resolved.conf`
syntax, so `address:port` works (`systemd.dns-delegate(5)`; parsed by
`in_addr_port_ifindex_name_from_string_auto`). Delegates need no network link, answering "does a
global routing domain work without a link?": yes, via a delegate.

#### 3.6.3 Why not `resolved.conf.d` (the obvious approach)

`DNS=127.0.0.1:5354` + `Domains=~glow` in `/etc/systemd/resolved.conf.d/` *looks* right but is
wrong. In `src/resolve/resolved-dns-scope.c`, `dns_scope_is_default_route()` ends with *"The global
DNS scope is always suitable as default route"* (`return true`), and the route-only check applies
only to link scopes. So with a global `DNS=` server, **every** lookup on the machine is also sent
to Lantern's responder; `~glow` only adds a better match for `.glow`. Delegates default to
`DefaultRoute=no`, which is exactly the behavior wanted. The cost is a hard requirement of
systemd >= 258 (`Added in version 258`); Ubuntu 26.04 ships 259.9 (checked locally). `doctor`
`dns.resolved` fails on older versions with the fallbacks in 3.6.4.

#### 3.6.4 Fallback when resolved is not in use (or too old)

`doctor` check `dns.resolved` passes when `systemctl is-active systemd-resolved` is active,
`/etc/resolv.conf` resolves to the stub (`/run/systemd/resolve/stub-resolv.conf`), and the version
is >= 258. Otherwise it fails with these options (none is applied automatically):
1. **`tld = "localhost"`** (`myapp.localhost`): no DNS component, no root step. Browsers, resolved/`nss-myhostname` and curl >= 7.78 resolve `*.localhost` to loopback themselves (RFC 6761). Costs: the CA constraint becomes `DNS:localhost` (CA regenerates once) and the `.glow` look.
2. NetworkManager with `dns=dnsmasq`: `server=/glow/127.0.0.1#5354` in `/etc/NetworkManager/dnsmasq.d/lantern.conf`, then `nmcli general reload dns-full`. Other resolvers (unbound, dnsmasq): forward `glow` to `127.0.0.1#5354`.
Browser DoH: Firefox `network.trr.mode = 3` and strict Chrome "Secure DNS" bypass the resolver; `browser.doh` (warning) reads `prefs.js` / `Local State` and says how to exempt `glow`.

#### 3.6.5 Does DNS need the daemon for existing tabs?

It does not matter enough to engineer around. `systemd-resolved` does not cache answers from
loopback servers (`CacheFromLocalhost=no`), so every *new* lookup reaches the responder; but open
tabs hold keep-alive connections to Caddy (a separate unit, unaffected), and browsers cache
lookups (~60 s, TTL 60). A daemon restart takes ~2 s. A lookup that lands in that window gets
connection-refused from loopback instantly, so it fails fast rather than hanging, and the next
reload works. (Future option if it ever matters: a `lantern-dns.socket` user unit holding the UDP
socket across restarts.)

### 3.7 Privileges and `lantern init`

#### 3.7.1 What needs root — complete inventory

| # | Step id | Root action | Why | `check()` says Done when |
|---|---|---|---|---|
| 1 | `ports` | write `/etc/sysctl.d/60-lantern-ports.conf`; `/usr/sbin/sysctl -w net.ipv4.ip_unprivileged_port_start=80` | Caddy binds :80/:443 as a user | live value <= min(http_port, https_port), or both >= 1024, or Caddy has the file cap |
| 2 | `dns` | write the delegate file (3.6.2); `systemctl reload systemd-resolved` | route `.glow` to the responder | file content equals expected for current tld+port |
| 3 | `system_trust` | install CA to `/usr/local/share/ca-certificates/lantern-local-ca.crt`; `update-ca-certificates` | curl/PHP/Python/Go trust | installed file's DER sha256 == `root.crt`'s and `/etc/ssl/certs/lantern-local-ca.pem` exists |
| 4 | `migrate_v1` | only with `--migrate-v1` (3.12) | clean v1 leftovers | nothing from the v1 list exists |

Optional, not a root script: `loginctl enable-linger $USER` (polkit prompts by itself).
User-level steps (no root): create dirs; generate CA; `nss_trust` (3.5.3); `user_units` (install a
user-level `lantern.service` override when running from a build tree, 3.9.4); `enable`/start
`lantern.service`; optional service binary downloads.

#### 3.7.2 Invocation

```
lantern init                       # as you: user steps, then elevates for pending root steps, then re-checks
lantern init --user-steps          # user steps only
sudo lantern init --system-steps   # root steps only; also what the GUI "Fix" button launches via pkexec
lantern init --check               # same as `lantern doctor --only init`
```
`lantern init` computes the root steps whose `check()` is not Done; if none, no prompt. Otherwise it
prints what will happen and why, then runs
`pkexec /usr/bin/lantern init --system-steps --uid <uid>` (path = canonical `current_exe()`;
a build-tree binary works but triggers the generic polkit message). **One mechanism, `pkexec`,
because it works from the terminal, the GUI and the tray** (KDE's `polkit-kde-agent-1` shows the
dialog) with a message we control. If `pkexec` is missing, fails (exit 126/127), or the user
cancels, `init` prints the exact `sudo lantern init --system-steps --uid <uid>` command and exits 3.
The `.deb` ships `/usr/share/polkit-1/actions/dev.lantern.init.policy`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD polkit Policy Configuration 1.0//EN" "http://www.freedesktop.org/software/polkit/policyconfig-1.dtd">
<policyconfig><vendor>Lantern</vendor>
  <action id="dev.lantern.init">
    <description>Set up Lantern local domains and HTTPS</description>
    <message>Authentication is required to let Lantern set up local domains, privileged ports and certificate trust.</message>
    <defaults><allow_any>auth_admin</allow_any><allow_inactive>auth_admin</allow_inactive><allow_active>auth_admin_keep</allow_active></defaults>
    <annotate key="org.freedesktop.policykit.exec.path">/usr/bin/lantern</annotate>
    <annotate key="org.freedesktop.policykit.exec.argv1">init</annotate>
  </action>
</policyconfig>
```
(`exec.path`/`exec.argv1` are the pkexec annotations; checked in `pkexec(1)`.) `argv1=init`
means the action also covers plain `lantern init` as root, so **root `init` without
`--system-steps` must refuse** ("run as your user").

#### 3.7.3 Rules for the root code path (`--system-steps`)

- Requires `euid == 0` (else exit 2) and `--uid N` with `N != 0` (pkexec sets `PKEXEC_UID`; `sudo`
  sets `SUDO_UID`; either is the default when `--uid` is absent). Never trust `$HOME`/`$XDG_*` (pkexec sets `HOME=/root`): resolve the user's home and `~/.config/lantern/config.toml` via `getpwuid`.
- The user's settings are *data*: tld must match `^[a-z][a-z0-9-]{0,62}$`; ports 1..=65535; `dns.listen` must be a loopback address. Nothing from them reaches a shell or a path.
- Writes only to constants: `/etc/sysctl.d/60-lantern-ports.conf`, `/etc/systemd/dns-delegate.d/lantern.dns-delegate`, `/usr/local/share/ca-certificates/lantern-local-ca.crt`, and the v1 cleanup list. Atomic (temp file in the same dir + rename), only when content differs, `root:root 0644`.
- External commands by absolute path with a cleared environment: `/usr/sbin/sysctl`, `/usr/bin/systemctl`, `/usr/sbin/update-ca-certificates`, `/usr/bin/nmcli`. No `sh -c`.
- The CA file is read from the user's directory, so treat it as untrusted input: `O_NOFOLLOW`, size limit 16 KiB, parse, require self-signed CA + critical name constraints + a permitted subtree equal to the TLD, write the re-encoded PEM — never copy bytes through.
- Output: one line per step, e.g. `[ok] ports: already 80`, `[changed] dns: wrote ... and reloaded systemd-resolved`, `[skip] system_trust: CA not created yet (run lantern init as your user first)`, `[FAIL] ...`. Exit 0 unless a step failed.
- `--from-package` (used by the `.deb` postinst, no user context): applies `ports` and `dns` *only if absent* (never overwrites an existing, possibly customized file), skips `system_trust`, always exits 0 after printing.
- `--undo`: removes the sysctl file, the delegate file (reload resolved) and, with `--untrust-ca`, the system CA entry (`rm` + `update-ca-certificates`).
- Hidden test hook: `LANTERN_FAKE_ROOT=<dir>` prefixes every root path and skips the external commands, so steps are tested without root (7).

### 3.8 `lantern doctor`

Runs entirely in the CLI process via `lantern-system::doctor` (no daemon needed; probes are
black-box: connect to the socket, send a DNS query, do a TLS handshake). When the daemon is up it
runs the same function every 60 s and publishes `Event::SystemHealth` (changes only).

```rust
pub struct Check { pub id: &'static str, pub title: String, pub status: CheckStatus, pub detail: String, pub fix: Option<Fix> }
pub enum CheckStatus { Pass, Warn, Fail, Skip, Info }
pub struct Fix { pub description: String, pub command: String, pub needs_root: bool }
pub struct SystemHealth { pub ok: bool, pub checked_at: Timestamp, pub checks: Vec<Check> }   // ok = no Fail
```

| id | Probe | Pass when | Fix command (shown verbatim) |
|---|---|---|---|
| `daemon.running` | `GET http://127.0.0.1:4777/api/system` | answers | `systemctl --user start lantern` |
| `session.systemd` | `XDG_RUNTIME_DIR` set, `systemctl --user show-environment` works, `lantern` unit enabled | yes | log in graphically; `systemctl --user enable lantern` |
| `caddy.binary` | resolve path (3.4.1), `caddy version` | exists, >= 2.8.0 | `sudo apt install --reinstall lantern` / set `[caddy].binary` |
| `caddy.running` | unit active + `GET /config/` on the socket; shows route count | yes | `systemctl --user restart lantern-caddy` |
| `ports.bind` | sysctl value vs ports, or `getcap` on binary | sufficient | `sudo lantern init --system-steps` |
| `ports.free` | `/proc/net/tcp` listeners on 80/443: ours (MainPID), another process, or free | ours or free | per holder, e.g. `sudo systemctl disable --now nginx` (v1 Valet users) |
| `dns.resolved` | `systemd-resolved` active, resolv.conf stub, `systemctl --version` >= 258 | all | text with the fallbacks of 3.6.4 |
| `dns.delegate` | delegate file equals expected | equal | `sudo lantern init --system-steps` |
| `dns.responder` | UDP query `lantern-probe.<tld>` A to `127.0.0.1:5354` | `127.0.0.1` | `systemctl --user restart lantern` |
| `dns.system` | `getaddrinfo("lantern-probe.<tld>")` (full path through resolved) | `127.0.0.1` | re-run `dns.delegate` fix |
| `tls.ca` | root exists, parses, valid > 30 d, critical NC == tld, `root.key` is 0600 | all | `lantern init` |
| `tls.system` | 3.7.1 row 3 check | Done | `sudo lantern init --system-steps` |
| `tls.certutil` | `certutil` on PATH | yes | `sudo apt install libnss3-tools` |
| `tls.nss.chrome` / `tls.nss.firefox` | one result **per database/profile** found (3.5.3) | Trusted | `lantern init` (then restart the browser) |
| `tls.serving` | TLS handshake to `127.0.0.1:443` SNI `lantern.<tld>`, verified against `root.crt` with rustls | chain valid | `systemctl --user restart lantern-caddy` |
| `e2e.https` | `curl -sS --max-time 3 https://lantern.<tld>/__lantern/ping` (system resolver + system trust, like any other program) | `lantern-ok` | whichever of the above is failing |
| `browser.doh` | Firefox `network.trr.mode`, Chrome secure DNS | not strict | warning text (Warn only) |
| `conflict.v1` | any v1 artifact from 3.12, or port 4777 held by a non-`lantern` process | none | `sudo lantern init --system-steps --migrate-v1` |
| `conflict.caddy` | system `caddy.service` active | inactive | `sudo systemctl disable --now caddy` |
| `path.shadow` | a `lantern` other than `/usr/bin/lantern` earlier on `PATH` | no | `sudo rm /usr/local/bin/lantern` |
| `services.*`, `php.fpm`, `linger` | per service: installed/mode/port conflict (3.10); php-fpm socket connectable (only if a PHP instance exists); `loginctl show-user $USER -p Linger` | Info rows, never Fail (php.fpm may Fail) | hint text; `lantern services start php-fpm`; `loginctl enable-linger $USER` |

Output:
```
$ lantern doctor
Lantern doctor — 22 checks: 19 passed, 1 warning, 2 failed

 PASS  caddy.running   lantern-caddy active (pid 5120), 7 routes loaded
 FAIL  tls.nss.firefox profile sxt75stw.default (snap) does not trust "Lantern Local CA"
       fix: lantern init                          (no root needed; then restart Firefox)
 FAIL  ports.free      :443 is held by nginx (system service)
       fix: sudo systemctl disable --now nginx    (needs root)
```
`--json` prints `SystemHealth`. Exit code 0 = no Fail, 1 = at least one Fail, 2 = doctor itself broke.
`--fix` is shorthand for running `lantern init` then re-checking.

### 3.9 systemd user units and autostart

#### 3.9.1 Unit names

`lantern.service` (packaged file), `lantern-caddy.service` and `lantern-svc-<name>.service`
(transient, this doc), `lantern-inst-<slug>.service` (transient, 10-runtime). So
`systemctl --user stop 'lantern-*'` stops everything Lantern started (used by `prerm`).

#### 3.9.2 `lantern.service`

```ini
[Unit]
Description=Lantern daemon (local dev-server manager)

[Service]
Type=notify
NotifyAccess=main
ExecStart=/usr/bin/lantern daemon
Restart=on-failure
RestartSec=2
TimeoutStartSec=30
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
```
- `Type=notify`: the daemon sends `READY=1` (crate `sd-notify` 0.5) once the API listener is bound and state is loaded — *not* waiting for Caddy — so `systemctl --user start lantern` and `lantern on --wait` know when the API is live. It sends `STOPPING=1` on shutdown.
- Stopping/restarting it **never** kills dev servers, Caddy or services: they are separate units in their own cgroups (the daemon's cgroup holds only the daemon and short-lived children such as `git`).
- No sandboxing (`NoNewPrivileges`, `ProtectHome`, ...): the daemon exists to run the user's own commands with the user's files.
- Environment caveat for 10-runtime: a user manager has a minimal `PATH` (no nvm/pyenv/cargo shims). Instance environments need the login-shell environment, not the unit's.

#### 3.9.3 Enabling, lingering, autostart

- **Enabled for every user** by the `.deb`: `systemctl --global enable lantern.service` in `postinst` (symlink in `/etc/systemd/user/default.target.wants/`). It starts at the next graphical login; a user opts out with `systemctl --user mask lantern` (`disable` cannot remove a global symlink). From a build tree, `lantern init` runs `systemctl --user enable --now lantern`.
- **No lingering by default.** The proxy and DNS only matter while you are at the machine; logging out ends your dev servers anyway. Lingering also needs polkit authentication (`org.freedesktop.login1.set-user-linger` is `auth_admin_keep`, checked locally), so it cannot be a silent step. `doctor` reports it as `Info` with the fix `loginctl enable-linger $USER`.
- **GUI/tray autostart:** `/etc/xdg/autostart/lantern-tray.desktop` (dpkg conffile; honored by KDE Plasma; users override by creating `~/.config/autostart/lantern-tray.desktop` with `Hidden=true`):
  Keys: `Type=Application`, `Name=Lantern`, `Exec=/usr/bin/lantern gui --tray`, `Icon=lantern`,
  `Terminal=false`, `StartupNotify=false`, `X-GNOME-Autostart-enabled=true`. The menu entry `/usr/share/applications/lantern.desktop` is the same with `Exec=/usr/bin/lantern gui`, `StartupWMClass=lantern` (the Wayland app-id must equal the desktop-file name; 16-clients sets it). The GUI/tray, if the daemon is down, offers `systemctl --user start lantern` (core doc).

#### 3.9.4 Running from a source build

`cargo run -p lantern -- daemon` in a terminal works unchanged (Caddy's transient unit is created from that process). For a persistent dev setup, `lantern init` detects that `current_exe()` is not `/usr/bin/lantern` and writes `~/.config/systemd/user/lantern.service` with `ExecStart=<current_exe> daemon` (user units override `/usr/lib/systemd/user`). The root steps are independent of the binary path. This replaces `setup-dev.sh`/`dev-up.sh`/`dev-down.sh`.

### 3.10 Shared services (`services/`)

#### 3.10.1 Model

```rust
pub enum ServiceName { Mailpit, Redis, Postgres, PhpFpm }           // as strings: "mailpit" | "redis" | "postgres" | "php-fpm"
pub enum ServiceState { Running, Stopped, Starting, Failed, NotInstalled }
pub enum ServiceMode  { User /* Lantern's unit */, System /* someone else runs it: read-only */ }
pub struct ServiceStatus {
    pub name: ServiceName, pub state: ServiceState, pub mode: ServiceMode,
    pub ports: BTreeMap<String, u16>,        // "smtp":1025, "ui":8025, "default":5432
    pub ui_url: Option<String>,              // https://mailpit.glow
    pub unit: Option<String>,                // lantern-svc-mailpit (User mode)
    pub controllable: bool,                  // false in System mode / NotInstalled
    pub detail: Option<String>,              // human hint ("installed? run: sudo apt install ...")
}
```
**Mode selection** (evaluated on each refresh): if the service's TCP port answers and the answer
does not come from our unit (`systemctl --user is-active lantern-svc-<n>` != active) -> `System`,
`controllable=false`, `start`/`stop` return `409 not_controllable` with the exact
`sudo systemctl stop <unit>` hint. Else if the binary is available -> `User`. Else `NotInstalled`
with an install hint. Settings `mode = "user"|"system"|"auto"` override. Status refresh: every 10 s
while any client is connected, plus immediately after `start`/`stop`; emits `ServiceUpdated`.
**The daemon never stops a service on shutdown, and never stops a `System`-mode service**
(v1's `shutdown` stopped system Postgres/Redis/Mailpit).

All `User`-mode units are transient, started with the same `UserSystemd` helper as Caddy
(`--unit=lantern-svc-<name> --collect -p Restart=on-failure -p RestartSec=2`).

#### 3.10.2 Mailpit

- Binary: `mailpit` on `PATH`, else `$XDG_DATA_HOME/lantern/bin/mailpit`. Not in Ubuntu's archive. `lantern services install mailpit` (user-initiated download) fetches `mailpit-linux-amd64.tar.gz` (or arm64) from the `axllent/mailpit` GitHub release matching a version **and sha256 pinned in the Lantern source** (their releases publish no checksum file), verifies, extracts to `bin/`.
- Command (flags checked in mailpit's source: `-l/--listen`, `-s/--smtp`, `-d/--database`, `-m/--max`; env `MP_DISABLE_VERSION_CHECK`):
  `mailpit --listen 127.0.0.1:8025 --smtp 127.0.0.1:1025 --database <data>/mailpit/mailpit.db --max 1000` with `MP_DISABLE_VERSION_CHECK=1`. Defaults to loopback, unlike mailpit's own `0.0.0.0` default.
- UI at `https://mailpit.<tld>` via a service route.

#### 3.10.3 Redis (or Valkey) and Postgres

| | Redis | Postgres |
|---|---|---|
| Binary | `redis-server`, else `valkey-server` (Ubuntu 26.04 has both packages). Not installed here: hint `sudo apt install redis-server`. | `/usr/lib/postgresql/<highest>/bin/{initdb,postgres}` (this machine: 18) |
| Port conflict | The apt package auto-starts a *system* service on 6379 -> `System` mode. To let Lantern manage it: `sudo systemctl disable --now redis-server`. | **This machine has `postgresql@18-main` running on 5432 -> `System` mode, read-only** (the common case). |
| `User` mode data | `<data>/redis/` | `<data>/postgres/<major>/`, initialized once |
| First start | none | `initdb -D <dir> --username=$USER --auth=trust --encoding=UTF8 --locale=C.UTF-8` |
| Command | `redis-server --port 6379 --bind 127.0.0.1 --dir <data>/redis --save 300 1 --daemonize no` | `postgres -D <dir> -p 5432 -c listen_addresses=127.0.0.1 -k $XDG_RUNTIME_DIR/lantern/pg` |
| Stop signal | default `SIGTERM` | **`-p KillSignal=SIGINT`**, `TimeoutStopSec=30` — Postgres treats `SIGTERM` as *smart* shutdown, which waits for every client to disconnect and makes `stop` hang |
| Auth note | n/a | `trust` on loopback + a 0700 socket dir: any local account could connect as superuser. Accepted for a single-user dev machine (stated in security, section 6). You are the superuser role, so 11-instances can `createdb myapp_feature_x` with no setup. |
| `System` mode hint | — | "System PostgreSQL: create a role for yourself once: `sudo -u postgres createuser -s $USER`", or set `[services.postgres] mode="user", port=5433` to get a private cluster alongside it. |

Postgres in user mode is the fiddly one; the decision is to support it only because `initdb` +
`postgres` are two stable commands from the system packages, and to default to read-only `System`
mode whenever 5432 is taken. Major upgrades of an existing data dir (`pg_upgrade`) are out of scope:
the data dir is versioned by major and `doctor` warns if its binaries disappeared.

#### 3.10.4 PHP-FPM (a fourth, Lantern-managed service)

A user-level Caddy cannot use the stock pool: `/run/php/php*-fpm.sock` is `www-data:www-data 0660`,
and that pool cannot read `/home/<you>/...`. (v1 papered over this by running Caddy as the `caddy`
system user.) So Lantern runs a pool as **you**, when any instance uses the PHP serve mode and a
`php-fpm*` binary exists (Ubuntu 26.04: `php8.5-fpm` -> `/usr/sbin/php-fpm8.5`):

`$XDG_STATE_HOME/lantern/php-fpm.conf`:
```ini
[global]
pid = /run/user/1000/lantern/php-fpm.pid
error_log = /proc/self/fd/2
daemonize = no
[www]
listen = /run/user/1000/lantern/php-fpm.sock
pm = ondemand
pm.max_children = 8
pm.process_idle_timeout = 30s
catch_workers_output = yes
clear_env = no
```
Run: `php-fpm8.5 --nodaemonize --fpm-config <that file>`. `ondemand` keeps idle cost at zero.
`fpm_socket` resolution for Route targets: explicit `[php].fpm_socket` -> the managed pool socket
-> a system socket **only if** `UnixStream::connect` succeeds as the user. *Not verified here* (no
php-fpm installed): that php-fpm starts as a non-root user with `user`/`group` omitted — implement
and test early.

#### 3.10.5 Mail environment (replaces the mail injector)

`services::mailpit::env_for(framework: &str) -> Vec<(String, String)>` returns v1's tables
(`services/mail_injector.ex`) unchanged, to be merged into the **process environment** of an
instance whose manifest enables mailpit (and into `RouteTarget::Php.env` for FPM): Laravel
`MAIL_MAILER=smtp MAIL_HOST=127.0.0.1 MAIL_PORT=1025 MAIL_USERNAME= MAIL_PASSWORD= MAIL_ENCRYPTION=
MAIL_FROM_ADDRESS=dev@localhost MAIL_FROM_NAME="Lantern Dev"`; Symfony `MAILER_DSN=smtp://127.0.0.1:1025`;
Next/Nuxt/Vite `SMTP_HOST=127.0.0.1 SMTP_PORT=1025 MAIL_FROM=dev@localhost`; Django
`EMAIL_BACKEND=django.core.mail.backends.smtp.EmailBackend EMAIL_HOST=127.0.0.1 EMAIL_PORT=1025
EMAIL_USE_TLS=False`; default `SMTP_HOST=127.0.0.1 SMTP_PORT=1025`. Why not edit `.env`: it dirties
git, leaves marker blocks behind, and v1 never wired it up. Laravel's dotenv loader does not
overwrite variables already in the process environment (assumption to confirm in 11-instances
testing). An explicit `lantern mail inject` file-editing command can return later if wanted.

### 3.11 Packaging

#### 3.11.1 Bundle Caddy (decided)

Depend-on-Ubuntu's-`caddy` was rejected: it is 2.6.2 (no `stream_close_delay`), and installing it
starts a system `caddy.service` on :80/:443 that fights ours. The `.deb` instead ships the official
upstream binary at `/usr/lib/lantern/caddy` (not on `PATH`), Apache-2.0, licence text in
`/usr/share/doc/lantern/`. With the sysctl approach no `setcap` is involved, so upgrading the
package replaces the file with no extra step. Cost: ~13 MB xz-compressed.

`packaging/fetch-caddy.sh` (run before `cargo deb`): downloads
`https://github.com/caddyserver/caddy/releases/download/v$VER/caddy_${VER}_linux_${ARCH}.tar.gz`,
checks it against `packaging/caddy.sha512` (pinned; for 2.11.9: amd64
`0e85544340290a16ce4bfb76fe7d65e9de1bf3a6e67f5ccfff30b30d27ea4c73e078077b7bf3e58ae60b6b12e44327705287059c775bca5782e8ee85ea7edd75`,
arm64 `9e5e76d7888542924de2c25353ae87c5f77e2a22c5b1198633cae13688922282e05383d142599205c88d8d80eee5e967d291214d7abce4835329a191b20dd6e0`),
extracts `caddy` and `LICENSE` to `packaging/build/`.

#### 3.11.2 cargo-deb metadata (`crates/lantern/Cargo.toml`)

cargo-deb 3.8.0. Its `systemd-units` feature targets system units, so user units and scripts are
hand-written.

```toml
[package.metadata.deb]
name = "lantern"                        # same name as v1 => dpkg upgrades v1 in place
section = "devel"
priority = "optional"
maintainer = "Ryan <ryan@localhost>"
extended-description = "Local development control plane: stable HTTPS names for dev servers."
depends = "$auto, systemd (>= 258), pkexec, git, ca-certificates, libnss3-tools"
recommends = "systemd-resolved, xdg-utils, curl"
suggests = "php-fpm, postgresql, redis-server | valkey-server"
maintainer-scripts = "packaging/debian/"          # postinst, prerm, postrm (below)
assets = [
  ["target/release/lantern",                     "usr/bin/lantern",                                "755"],
  ["packaging/build/caddy",                      "usr/lib/lantern/caddy",                          "755"],
  ["packaging/build/LICENSE",                    "usr/share/doc/lantern/caddy-LICENSE",            "644"],
  ["packaging/systemd/lantern.service",          "usr/lib/systemd/user/lantern.service",           "644"],
  ["packaging/desktop/lantern.desktop",          "usr/share/applications/lantern.desktop",         "644"],
  ["packaging/desktop/lantern-tray.desktop",     "etc/xdg/autostart/lantern-tray.desktop",         "644"],
  ["packaging/polkit/dev.lantern.init.policy",   "usr/share/polkit-1/actions/dev.lantern.init.policy", "644"],
  ["packaging/icons/lantern.svg",                "usr/share/icons/hicolor/scalable/apps/lantern.svg",  "644"],
  ["packaging/icons/lantern-512.png",            "usr/share/icons/hicolor/512x512/apps/lantern.png",   "644"],
]
```
(`etc/` assets are conffiles automatically.) GUI runtime libraries that `$auto` cannot see
because they are `dlopen`ed (Wayland, xkbcommon, Vulkan loader) are added by 16-clients.
Version `2.0.0` > v1's `0.1.0`. Build: `packaging/fetch-caddy.sh && cargo build --release -p lantern && cargo deb -p lantern --no-build`
-> `target/debian/lantern_2.0.0-1_amd64.deb`; install with `sudo apt install ./that.deb` (resolves dependencies, unlike `dpkg -i`).

#### 3.11.3 Maintainer scripts (`packaging/debian/`)

All three exit 0 on failure paths (a broken helper must not break `apt`). `postinst`:
```sh
#!/bin/sh
set -e
if [ "$1" = configure ]; then
    # user-independent root steps; never overwrites existing files (3.7.3)
    /usr/bin/lantern init --system-steps --from-package --migrate-v1 \
        || echo "lantern: system setup incomplete; run 'lantern doctor' after logging in" >&2
    systemctl --global enable lantern.service >/dev/null 2>&1 || true
    # upgrade: restart running daemons; dev servers, Caddy and services are separate units and stay up
    for u in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user --machine="${u}@.host" try-restart lantern.service >/dev/null 2>&1 || true
    done
    update-desktop-database -q /usr/share/applications 2>/dev/null || true
    gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor 2>/dev/null || true
fi
exit 0
```
`prerm` (only `remove|deconfigure`; upgrades do nothing): for each logged-in user
`systemctl --user --machine="${u}@.host" stop 'lantern-*.service' lantern.service`, then
`systemctl --global disable lantern.service`. `postrm` (the binary is already gone: plain shell
only): on `remove|purge` delete `/etc/systemd/dns-delegate.d/lantern.dns-delegate` and
`/etc/sysctl.d/60-lantern-ports.conf`, then `systemctl reload systemd-resolved` (the live sysctl
value reverts at next boot); on `purge` also delete `/usr/local/share/ca-certificates/lantern-local-ca.crt`
and run `update-ca-certificates`; finally `update-desktop-database`. (Reaching a user manager as
root with `systemctl --user --machine=<user>@.host` is the documented route; confirm on first build.)

#### 3.11.4 Uninstall behavior

`apt remove lantern`: stops everything Lantern started for logged-in users, removes the DNS
delegate and sysctl file; the **CA stays trusted** (its key is still in your data dir, but it is
name-constrained to `.glow`) so a reinstall needs no new prompt. `apt purge lantern`: also removes
the system trust entry. Per-user cleanup is a separate user command because a package must not
edit home directories: **`lantern uninstall [--purge-data]`** (run *before* removing the package)
stops units, deletes the NSS entries (`certutil -D`), the user unit override and the autostart
override, and with `--purge-data` deletes `$XDG_{DATA,STATE}_HOME/lantern` (CA key included).

### 3.12 v1 -> v2 migration of the system bits

The v2 package keeps the name `lantern`, so `apt install ./lantern_2.0.0...deb` upgrades v1 in
place; dpkg runs **v1's** `prerm upgrade` (stops and disables `lanternd`) and `postrm upgrade`
(deletes `/etc/sudoers.d/lantern` and `/etc/caddy/sites.d/`), then removes the v1 files
(`/opt/lantern/**`, `/usr/local/bin/lantern`, `/etc/systemd/system/lanternd.service`). The v2
`postinst` then runs `lantern init --system-steps --from-package --migrate-v1` for what v1's
scripts did not clean, and for v1 installs that were not dpkg-managed. `--migrate-v1` is
idempotent; every action is detect-then-act, and it prints what it did:

| v1 artifact | Detected by | Action |
|---|---|---|
| System daemon | `/etc/systemd/system/lanternd.service` exists | `systemctl disable --now lanternd`; delete the unit; `systemctl daemon-reload` |
| sudoers (the local privilege escalation) | `/etc/sudoers.d/lantern` exists | delete it; print a one-line security note |
| Caddy site files | `/etc/caddy/sites.d/*.caddy` | delete those files (the directory only if then empty) |
| System Caddy left running on :80/:443 | `/etc/caddy/Caddyfile` contains `import /etc/caddy/sites.d` (v1's signature) **and** `caddy.service` is active/enabled | `systemctl disable --now caddy`. The package and the Caddyfile are **left alone** (harmless once disabled; restore with `apt install --reinstall -o Dpkg::Options::=--force-confask caddy`). If the signature is absent it is the user's own Caddy: not touched, `conflict.caddy` fails instead. |
| NetworkManager/dnsmasq DNS | `/etc/NetworkManager/dnsmasq.d/lantern.conf` or `/etc/dnsmasq.d/lantern.conf` | delete; `nmcli general reload dns-full` (NM) or `systemctl reload-or-restart dnsmasq`. NetworkManager itself is **not** restarted. |
| Orphaned v1 install / leftovers | `/opt/lantern/{daemon,desktop,cli,packaging}` not owned by any package (`dpkg -S` fails); `/usr/local/bin/lantern` symlink into `/opt/lantern` | delete |

Reported but never removed: an old trusted `Caddy*.crt` in `/usr/local/share/ca-certificates`, and a v1
dev daemon (`dev-up.sh`) holding port 4777 (the v2 daemon logs "port 4777 held by pid N" and exits).
v1 user data (`~/.config/lantern/settings.json`, project store) is imported by 14-config on the
first v2 start. The v2 CA is new: browsers need the one-time `lantern init`.

---

## 4. Interfaces

### 4.1 Provided (`lantern-daemon`)

```rust
// net/caddy.rs
impl Caddy {
    pub fn new(cfg: CaddyConfig, sd: Arc<dyn UserSystemd>, paths: LanternPaths) -> Self;
    pub async fn ensure_running(&self) -> Result<(), NetError>;                 // 3.4.1
    pub async fn apply(&self, plan: &RoutePlan) -> Result<ApplyOutcome, NetError>; // Applied|Unchanged; Err: Rejected|Unavailable|PortInUse
    pub async fn status(&self) -> CaddyStatus;     // { running, pid, version, admin_ok, route_count, last_error }
    pub async fn restart(&self) -> Result<(), NetError>;
}
// net/routes.rs
pub fn build_plan(input: &PlanInput) -> RoutePlan;     // pure; PlanInput = instances + listeners + service statuses + NetSettings
pub fn public_url(settings: &NetSettings, domain: &str) -> String;
pub const RESERVED_LABELS: [&str; 2] = ["lantern", "mailpit"];
// net/dns.rs
pub async fn serve(cfg: DnsConfig, cancel: CancellationToken) -> Result<(), NetError>;   // returns on cancel
// net/tls.rs
impl Tls {
    pub fn ensure_ca(&self, tld: &str) -> Result<CaState, NetError>;           // 3.5.1
    pub fn status(&self) -> TlsStatus;   // { ca: Missing|Ok{sha256,not_after,permitted}|Invalid(msg), system_trusted, stores: Vec<{kind,path,state: Trusted|Missing|Stale|NoCertutil}> }; blocking, cache 60 s
    pub fn root_cert_path(&self) -> PathBuf;                                   // for NODE_EXTRA_CA_CERTS
}
// net/gateway.rs
pub fn router(state: GatewayState) -> axum::Router;                            // mounted at "/__lantern" by api/
// net/sync.rs
pub async fn run(reg: RegistryHandle, caddy: Arc<Caddy>, services: Arc<Services>, cancel: CancellationToken);
// services/mod.rs
impl Services {
    pub fn list(&self) -> Vec<ServiceStatus>;
    pub async fn start(&self, n: ServiceName) -> Result<ServiceStatus, ServiceError>;   // NotControllable | NotInstalled | Failed
    pub async fn stop(&self, n: ServiceName)  -> Result<ServiceStatus, ServiceError>;
    pub async fn install(&self, n: ServiceName) -> Result<(), ServiceError>;            // mailpit download
    pub fn mail_env(&self, framework: &str) -> Vec<(String, String)>;
}
// lantern-system
pub fn doctor::run(ctx: &DoctorCtx) -> Vec<Check>;
pub fn init::run_user_steps(ctx: &InitCtx) -> Vec<StepOutcome>;
pub fn init::run_system_steps(ctx: &SystemInitCtx) -> Vec<StepOutcome>;   // root only
```
Daemon boot order (`net` + `services`): `Tls::ensure_ca` -> spawn `dns::serve` -> `Caddy::ensure_running`
-> spawn `sync::run` -> spawn health loop -> mount gateway router -> `READY=1`. Only the API bind
gates `READY`.

| Consumer | Uses |
|---|---|
| 10-runtime | `net::public_url` for `${URL}`; `Tls::root_cert_path()` for `NODE_EXTRA_CA_CERTS`; `Services::mail_env`; `Services::list` for profile `services:` dependencies |
| 11-instances | `Services::list` (is Postgres reachable before creating a per-branch DB) |
| 12-discovery | nothing direct: sets `status=External` + `runtime.external` for every stable matched listener; `net::sync` routes it only when `adopt`/`auto_adopt` (→ 02 D9) |
| 15-api-mcp | mounts `net::gateway::router()` outside its Host/Origin middleware; exposes `/api/services*` over `Services`, `/api/system` (`SystemHealth`, `CaddyStatus`, `TlsStatus`); treats `Origin: https://lantern.<tld>` as same-origin for the docs pages |
| 16-clients | runs `lantern init`/`doctor`/`uninstall` through `lantern-system` directly; GUI "Fix" button spawns `pkexec lantern init --system-steps --uid <uid>` |
| 14-config | adopts the `config.toml` keys of 3.2; reserves `lantern`/`mailpit` labels |

### 4.2 Consumed (assumed)

- **10-runtime:** `trait UserSystemd { async fn start_transient(&self, u: &TransientUnit) -> Result<()>; async fn stop(&self, unit: &str) -> Result<()>; async fn state(&self, unit: &str) -> UnitState /* + MainPID */; async fn restart(&self, unit: &str) -> Result<()>; }` where `TransientUnit { name, description, argv, env, cwd, restart: RestartPolicy, properties: Vec<(String,String)> }`. If 10 names these differently only the call sites in `net/caddy.rs` and `services/` change.
- **registry:** `RegistryHandle::snapshot() -> { instances: Vec<Instance>, listeners: Vec<Listener> }` plus the `Event` stream of core section 9.
- **14-config:** `NetSettings` (3.2) and `ResolvedManifest::serve_mode() -> Process | Php { docroot } | Static { docroot }`, `run.scheme`, `features.mailpit`, `framework`.

## 5. Failure modes and edge cases

| Situation | Intended behavior |
|---|---|
| Bundled Caddy missing or < 2.8.0 | `caddy.binary` fails with the reason; daemon runs without a proxy (SystemHealth red); `lantern on` still works, URL falls back to `http://127.0.0.1:<port>` in output |
| Admin socket not answering after `ensure_running` (5 s) | `apply` returns `Unavailable`; health loop retries every 15 s; journal path shown in `doctor` |
| `POST /load` 400: port in use | `NetError::PortInUse`; `ports.free` names the holder (3.4.4); old config keeps serving |
| `POST /load` 400: other | `Rejected(msg)`; last good config stays; SystemHealth shows msg; typically a bug, logged with the rendered JSON path |
| Caddy killed/crashes | systemd restarts it from `caddy.json`; health loop sees MainPID change and re-applies |
| Caddy binary replaced by upgrade | `(deleted)` exe detected, unit restarted (WebSockets drop once) |
| Daemon down | Caddy keeps serving the last routes; new lookups of `*.glow` fail fast until the daemon is back (3.6.5); stopped-instance pages show Caddy's bare 502 |
| Daemon port 4777 taken | exits with "port 4777 held by pid N (cmdline)"; v1 BEAM is called out |
| Responder port 5354 taken | DNS disabled, `dns.responder` fails, rest of the daemon runs |
| resolved absent / < 258 | `dns.resolved` fails with the options of 3.6.4; nothing is half-applied |
| TLD changed in config | `ensure_ca` regenerates the root (3.5.1); `dns.delegate` and trust checks fail until `lantern init` |
| Root CA missing/regenerated | Caddy cannot start with an absent root: `ensure_ca` runs first; `tls.system`/`tls.nss.*` show *Stale* |
| `certutil` missing | `tls.certutil` fail; NSS steps `Skip` with the apt command; system trust still done |
| Firefox profile created later / browser open during `init` | `tls.nss.firefox` fails for the new profile until `lantern init` re-runs; NSS writes retry once on `database is locked`; restart the browser |
| Upstream listens only on `::1` | adopted listeners dial `[::1]:port`; managed instances get `HOST=127.0.0.1` from 10-runtime |
| Dev server rejects unknown `Host` (Vite >= 5.4.12 `allowedHosts`, webpack-dev-server, Next `allowedDevOrigins`) | Caddy preserves `Host` on purpose (apps build absolute URLs from it). Hand-off to 10-runtime/14-config: framework templates set `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS=.<tld>` / `allowedHosts` etc. |
| IPv6 loopback unavailable but `ipv6=true` | `/load` fails to bind `[::1]`; `Rejected`; `doctor` explains and suggests `ipv6=false` |
| VPN with a catch-all DNS route (`~.`) | `.glow` still reaches Lantern: the delegate's `~glow` is a longer match than `~.` |
| `ip_unprivileged_port_start` overridden by a later sysctl file | `ports.bind` fails (reads the live value); fix text names the offending file when found in `/etc/sysctl.d` |
| Two users on one machine both running Lantern | the second Caddy fails on :443 (`PortInUse`); unsupported by design (core non-goal) |
| Service port busy | `System` mode, read-only, with the stop hint (3.10.1) |

## 6. Security considerations

- **No sudoers, no runtime root.** The only root code is `lantern init --system-steps`, reached through polkit (admin password) and constrained by 3.7.3; it executes nothing the user supplies.
- **Name-constrained CA** (3.5.1) limits a stolen `root.key` to `*.glow`. Key `0600`, dir `0700`. It is still a root your browsers trust for `.glow`: do not copy it to other machines.
- **Admin socket** `0600` in a `0700` runtime dir; no TCP admin port, so a web page cannot reach it.
- **Gateway:** Host pinned to loopback; state-changing requests need an exact same-origin `Origin` for a known hostname; escaped output + CSP. Cross-site pages cannot start instances. A dev app at `myapp.glow` can call its own `/__lantern/start` (its own origin, its own instance).
- **Loopback only:** Caddy (unless `bind="all"`, an explicit LAN-exposure opt-in), DNS responder (fixed A/AAAA, no recursion, bounds-checked parsing), Mailpit, Redis, Postgres.
- **`ip_unprivileged_port_start=80`** lets any local process bind 80-1023 (3.4.4); single-user assumption, stated in the `init` output.
- **Postgres `trust` / unauthenticated Redis and Mailpit:** any local account can use them; acceptable on a single-user dev machine.
- **Pinned downloads:** Caddy (build time) and Mailpit (user request) by version + sha512/sha256 in the repo.
- **Root reads user files** (CA, config) as untrusted data (3.7.3).
- `stream_close_delay: 24h` keeps old proxy handlers alive for long-lived sockets; bounded by socket lifetime.

## 7. Testing strategy

**Unit (no root, no network):**
- `build_plan`: table-driven fixtures for every row of 3.3 (statuses, adopted `[::1]`, php/static, invalid/duplicate hosts, reserved labels, sorted output).
- `render`: golden JSON per scenario; a CI job runs `caddy validate` on each golden file with the **bundled** Caddy (catches schema drift when the pin is bumped).
- DNS codec: golden bytes for A, AAAA (both settings), HTTPS (type 65), ANY, apex, out-of-zone (`REFUSED`), EDNS OPT echo, 0x20 case, truncated/garbage, compression pointer, label > 63, TCP framing and pipelining; a randomized never-panics loop. `hickory-proto` is allowed as a **dev-dependency** to decode replies independently.
- CA: generate, assert with `x509-parser` (CA:TRUE, critical NC `DNS:glow`, key usage); shell to `openssl verify -x509_strict` for an in-zone leaf (pass) and out-of-zone leaf (`permitted subtree violation`).
- Steps under `LANTERN_FAKE_ROOT=<tmpdir>`: apply twice = second is a no-op; `--from-package` never overwrites a customized file; refuses when not root; tld/port validation. NSS discovery over temp `$HOME` trees (deb/snap/flatpak); `certutil` behind a trait with a fake. Doctor classification against fake probe results.

**Integration (real processes, unprivileged):** the bundled Caddy with `https_port=18443, http_port=18080` and a temp XDG tree, through `Caddy::apply`: proxy + nested host + Down page + PHP-without-fpm 502 -> page + static 404 stays plain + HTTP redirect + a WebSocket echo surviving a second `apply` (the scenarios this design was verified with); `/load` 400 on a bound port keeps the old config; kill Caddy and watch it restart with its routes. DNS: bind `127.0.0.1:0`, query with `dig` and `hickory-proto`.

**Manual, fresh Ubuntu 26.04 KDE/Wayland VM:** install the `.deb`; log in; `lantern init` (one polkit prompt); `lantern doctor` green; Firefox Snap, Chromium Snap and Chrome load `https://lantern.glow/__lantern/ping` with no warning; `curl` and PHP `file_get_contents` likewise; Vite HMR through the proxy while starting/stopping other instances; `systemctl --user restart lantern` with a tab open; `apt install` over a v1 install; `apt purge` leaves nothing in `/etc/systemd/dns-delegate.d`, `/etc/sysctl.d`, `/usr/local/share/ca-certificates`.

## 8. Open questions (with recommendations)

1. **Wildcard subdomains** (`tenant1.myapp.glow` -> myapp, Valet-style multi-tenant apps)? *Recommend:* not in 2.0; later a manifest flag `wildcard: true` adds a `*.myapp.glow` host matcher and a wildcard leaf (Caddy's internal issuer supports wildcard subjects; the CA constraint already permits it).
2. **More than one TLD** (e.g. `glow` and `test`)? *Recommend:* single TLD in 2.0; the constraint list would simply gain entries.
3. **Auto-install `libnss3-tools` in `init`?** *Recommend:* no, it is a `.deb` dependency; `init` only reports it from a source build.
4. **`stream_close_delay` 24 h** vs shorter? *Recommend:* keep 24 h; revisit if Caddy issue #7222 (preserving sockets for unrelated route updates) lands.
5. **Redis vs Valkey preference** when both exist? *Recommend:* prefer `redis-server` (client compatibility is identical; keeps `redis-cli` naming), fall back to `valkey-server`.
6. **Default for `ipv6`?** *Recommend:* off. Turn on only if someone has IPv6-only tooling; one switch controls Caddy's `[::1]` listeners and AAAA answers together.

## 9. Proposed changes to core architecture (`01-architecture.md`)

1. **New crate `lantern-system`** (sync, no tokio) for `init`, `doctor`, CA and trust-store code, shared by `lantern-daemon` and the `lantern` binary. Dependency rule becomes `core <- system <- {daemon, lantern bin}`; `cli` may call it for `init`/`doctor`/`uninstall` (they must work with the daemon down and as root). Section 4 layout and the dependency paragraph should reflect this.
2. **Process model (section 3):** Caddy is the transient user unit `lantern-caddy.service`, admin API on a **Unix socket** (`$XDG_RUNTIME_DIR/lantern/caddy-admin.sock`), not `localhost:2019` (a system Caddy owns 2019 and a mis-aimed `POST /load` would overwrite it). Update the diagram label.
3. **DNS (section 3):** "resolved drop-in" -> **systemd-resolved DNS delegate** (`/etc/systemd/dns-delegate.d/lantern.dns-delegate`, systemd >= 258); `resolved.conf` would make Lantern's responder a default route for every lookup (3.6.3). Responder answers **A only by default; AAAA is opt-in** (`[proxy].ipv6`) because Caddy listens on IPv4 loopback by default. The "A/AAAA" wording in section 3 should say so.
4. **Root (sections 1 and 3):** "setcap/sysctl" is decided: **sysctl**. Elevation is `pkexec lantern init --system-steps`; the `.deb` postinst also runs the user-independent steps.
5. **Persistence (section 7):** add `$XDG_STATE_HOME/lantern/caddy.json` (last-applied Caddy config), `$XDG_DATA_HOME/lantern/{bin,services}/`, `$XDG_RUNTIME_DIR/lantern/`. The CA under `caddy/pki/authorities/local/` is **generated by Lantern** (name-constrained), not by Caddy.
6. **Types:** define `ServiceStatus` (3.10.1), `SystemHealth`/`Check` (3.8) in `lantern-core::api` (referenced by `Event` in section 9 but not defined).
7. **Domains (section 6):** `lantern` and `mailpit` are reserved labels (`ProjectId` validation). `${URL}` must be produced by `net::public_url` (non-default ports).
8. **Daemon surface (sections 3/10):** the daemon also serves the gateway under `/__lantern/*` (3.4.5), outside the REST Host/Origin middleware with its own guard.
9. **Goals ("no runtime root"):** confirmed achievable; additionally stated: the daemon never stops a service it did not start.
