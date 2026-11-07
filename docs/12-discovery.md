# 12 — Discovery: listening-server scanner

Status: draft for review. Builds on `01-architecture.md` (authoritative). Vocabulary: **Listener**,
**Checkout**, **Instance**, **Unmanaged instance**, **managed** — exactly as in its glossary.
v1 paths are relative to `.reference/lantern-v1/`.

## 0. Plain-language summary

Lantern asks the Linux kernel "which TCP ports are in the LISTEN state?", works out which process
owns each one, reads that process's working directory, and checks whether the directory is inside
any project, worktree or Lantern-created checkout it knows. The result is a list such as
"port 5173 — Vite — `myapp` on branch `feature-x` — started by hand". That list is the
original goal of the whole project: *show every dev server on the machine and say which project
and branch it belongs to*.

Decisions, in one paragraph each:

- **Kernel query**: netlink `sock_diag` (LISTEN-filtered dump), with automatic fallback to parsing
  `/proc/net/tcp{,6}`. Measured on this machine: 0.06–0.15 ms versus 4–6 ms (§3.3).
- **Socket → process**: scan `/proc/<pid>/fd` only for *new* sockets, only in our own processes,
  newest process first, and cache the answer. Sockets of other users or root keep `pid: None`.
- **Process → checkout**: longest-prefix match of `/proc/<pid>/cwd` against all checkout paths;
  if that misses, look at up to 5 ancestor processes (stopping at an interactive shell or
  terminal), then at absolute paths in the command line. Lantern's own processes are identified
  *exactly* by their systemd cgroup (`lantern-inst-*.service`).
- **Unmanaged instance**: a matched, non-managed listener makes its Instance `External`.
  *Adopting* it means routing its domain to the port. The route follows the listener's life.
- **Probe**: one cheap HTTP `GET /` per interesting listener, never on the scan path.
- **Docker**: v2.0 does not touch the Docker socket; `docker-proxy` listeners are labelled from
  the world-readable `/proc/<pid>/cmdline`. The Docker API is a v2.1 opt-in.
- **Secrets**: command lines are redacted before they leave the discovery module.

Nothing in this module needs root, capabilities, or `ptrace` privileges. Not seeing the owner of a
root-owned socket is the correct and intended outcome.

## 1. Purpose & scope

**In scope**: enumerating listening TCP sockets; socket → process → checkout mapping; managed
detection by cgroup; listener classification and hide rules; unmanaged instances and the adoption
contract; the HTTP probe; port-holder lookup for conflicts; Docker-published port labelling; scan
scheduling, diffing and performance; redaction of what is exposed.

**Not in scope** (owner in brackets):
- Allocating ports, sticky port leases, starting/stopping units, readiness loops, health checks
  (`10-runtime.md`). Discovery only *answers questions* for them (§4.1).
- Checkout/worktree discovery and Instance lifecycle (`11-instances.md`). Discovery consumes a
  read-only `CheckoutIndex` and emits `ExternalChange`s.
- Caddy routes, DNS, unit naming for services (`13-system.md`).
- `config.toml` schema and persistence of hide rules (`14-config.md`).
- REST/SSE/MCP wire formats (`15-api-mcp.md`). Docs/OpenAPI ingestion is **intel**, not discovery.
- Rendering (`16-clients.md`).

## 2. v1 behaviour audit

| v1 file | What it does | Verdict |
|---|---|---|
| `daemon/lib/lantern_web/controllers/infrastructure_controller.ex:7-22` (`GET /ports`), `daemon/lib/lantern/mcp/tools/get_ports.ex` | Returns only `PortAllocator.assignments()` merged with health status: `{name: {port, health_status}}`. **Nothing is observed**; a hand-started server is invisible. | **Replace** with real observation. Keep the *shape* for MCP `get_ports` compatibility (§4.3). |
| `daemon/lib/lantern/projects/port_allocator.ex:103-125` | Sticky name→port map in 41000–42000; "free" test is a bind attempt (`:gen_tcp.listen`). | Allocator moves to 10-runtime (`ports/`). The bind test stays the authority for "can I bind?". Discovery adds *who holds it* (§3.9). |
| `daemon/lib/lantern/projects/manager.ex:1073-1126` (`wait_for_port`, `tcp_port_open?`) | Readiness = a TCP connect to `127.0.0.1:port` succeeds within 250 ms. | **Bug-prone**: any process on that port, including a foreign one, makes the instance "ready". Replaced by `port_binding(port, expected_unit)` (§3.8). |
| `manager.ex:1128-1160` (`stop_listener_on_port` via `lsof -tiTCP:<port> -sTCP:LISTEN`, then `kill -TERM/-KILL`) | Stop = kill whatever listens on the port. | **Drop.** It can kill an unrelated process. v2 stops units (10-runtime) and never kills by port. Discovery never signals processes. |
| `daemon/lib/lantern/system/caddy.ex:37-46` (`upstream_url` → `reverse_proxy <url>`) | A project with a fixed `upstream_url` gets a route to an already-running service. | Closest ancestor of *adoption*. v2 derives the upstream from an observed listener instead of config. |
| `daemon/lib/lantern/projects/discovery.ex`, `discovery_worker.ex` | Docs/OpenAPI ingestion. | Not this doc (→ intel). |
| `daemon/lib/lantern/projects/scanner.ex` | Workspace scan for `lantern.yaml`. | Not this doc (→ 14-config). Discovery *reads* the resulting workspace roots (§3.9). |

Lesson from v1: "which port does the project use" and "what is listening on this machine" are two
different questions; v1 answered only the first.

## 3. Design

### 3.1 Pipeline and module layout

```
 ┌── blocking thread (spawn_blocking), the ONLY place that touches /proc or netlink ──┐
 │ 1 SocketSource  → Vec<RawSocket>       LISTEN rows: addr, inode, uid               │
 │ 2 owners        → inode → (pid, fd)    cache; fd scan only for new own-uid inodes  │
 │ 3 ProcReader    → Arc<ProcInfo>        cache keyed (pid, start_ticks)              │
 │ 4 docker-proxy  → Option<DockerInfo>   only for new foreign listeners              │
 └───────────────────────────────┬────────────────────────────────────────────────────┘
                                 ▼  Observation (plain data)
 ┌── async side, pure functions, no I/O ──────────────────────────────────────────────┐
 │ build_snapshot(obs, CheckoutIndex, settings, probe_cache) -> Snapshot              │
 │   match → classify → hide → group → redact                                         │
 └───────────────┬──────────────────────────────┬─────────────────────────────────────┘
                 ▼                              ▼
     diff → Event::ListenersChanged     adopt.rs → ExternalChange → registry
     watch::Sender<Arc<Snapshot>>       probe.rs (spawned tasks) → results back into cache
```

Splitting "observe" (I/O) from "build" (pure) means a registry change (new worktree, branch
switch) re-runs matching from the cached `Observation` with zero syscalls, and the whole
matching/classification logic is unit-testable with no `/proc`.

```
crates/lantern-daemon/src/discovery/
  mod.rs       DiscoveryHandle, spawn(), Snapshot, PortBinding, PortHolder
  source.rs    trait SocketSource; SockDiag (primary); ProcNetTcp (fallback + tests)
  owners.rs    inode → Owner resolution and caches
  procinfo.rs  ProcReader { root }, ProcInfo, ProcKey, cgroup/stat/cmdline parsing
  matcher.rs   cwd / ancestor / cmdline-path matching, boundary list
  classify.rs  RULES table: dev-server / infra labels
  hide.rs      HideRule, DEFAULT_HIDE
  build.rs     build_snapshot() — pure
  probe.rs     HTTP probe + cache + scheduler
  docker.rs    docker-proxy cmdline index
  redact.rs    cmdline redaction
  adopt.rs     snapshot → ExternalChange (appear delay, grace, ranking)
  sched.rs     the scan loop: demand levels, on-demand, debounce
```

Everything that reads `/proc` goes through `ProcReader { root: PathBuf }` (default `/proc`), so
tests build a fake tree in a temp dir (symlinks for `cwd`, `exe`, `fd/N → socket:[123]`).

### 3.2 Types

`lantern-core::listener` (serde, no tokio). Field names from `01-architecture.md` §6 are kept; the
additions are listed again in §9.

```rust
pub struct Listener {
    // --- core §6, unchanged ---
    pub addr: SocketAddr,              // primary bind address (§3.10)
    pub pid: Option<u32>,              // None: other user's/root's socket, or owner not readable
    pub exe: Option<PathBuf>,          // readlink /proc/pid/exe, " (deleted)" stripped
    pub cmdline: Vec<String>,          // ALWAYS REDACTED (§6). Raw never leaves discovery/.
    pub cwd: Option<PathBuf>,          // canonical (kernel-resolved)
    pub matched: Option<InstanceId>,
    pub managed: bool,                 // process is inside a lantern-inst-* unit
    // --- additions ---
    pub port: u16,
    pub other_addrs: Vec<SocketAddr>,  // same port + same owner, e.g. 0.0.0.0 and ::
    pub uid: u32,
    pub exposure: Exposure,            // Loopback | AllInterfaces | Interface
    pub started_at: Option<Timestamp>, // process start (btime + starttime/CLK_TCK)
    pub project: Option<ProjectId>,    // denormalized from the matched checkout
    pub branch: Option<String>,        // ditto; None = detached or unmatched
    pub matched_by: Option<MatchSource>,
    pub unit: Option<String>,          // "lantern-inst-myapp.service", or a user's own *.service
    pub class: ListenerClass,
    pub label: String,                 // "Vite", "uvicorn", "node server.mjs", ...
    pub probe: Option<ProbeInfo>,
    pub docker: Option<DockerInfo>,
    pub adopted: bool,                 // a route currently points here
    pub hidden: Option<HiddenReason>,  // set only in include_hidden responses
}
pub enum ListenerClass { Project, DevServer, Docker, Infra, Other, Foreign }
pub enum MatchSource { Unit, Cwd, AncestorCwd { depth: u8 }, CmdlinePath }
pub enum Exposure { Loopback, AllInterfaces, Interface }
```

Daemon-internal (never serialized):

```rust
struct RawSocket { addr: SocketAddr, inode: u64, uid: u32 }
struct ProcKey   { pid: u32, start_ticks: u64 }                 // stat field 22
struct ProcInfo {                                               // AncestorInfo = same fields, no ancestors
    key: ProcKey, ppid: u32, uid: u32, comm: String, exe: Option<PathBuf>,
    cmdline_raw: Vec<String>, cwd: Option<PathBuf>,
    unit: Option<String>,                  // from /proc/pid/cgroup (§3.8)
    ancestors: Vec<AncestorInfo>,          // <= MAX_ANCESTOR_DEPTH, nearest first (§3.6)
    refreshed: Instant,
}
struct Observation { taken_at: SystemTime, source: SocketSourceKind, timings: Timings,
                     sockets: Vec<(RawSocket, Option<Arc<ProcInfo>>, Option<DockerInfo>)> }
pub struct HiddenReason { pub origin: HideOrigin /* Default | User | Lantern */, pub why: String }

pub struct Snapshot { pub seq: u64, pub taken_at: SystemTime,
                      pub listeners: Vec<Listener>,   // visible, sorted (port, addr, pid)
                      pub hidden: Vec<Listener>,
                      pub source: SocketSourceKind, pub scan_ms: f32 }
```

`seq` increments only when `listeners` actually changed (§3.14).

### 3.3 Enumerating listening sockets

**Scope.** TCP, IPv4 and IPv6, state LISTEN only. Non-loopback binds (`0.0.0.0`, `::`, a LAN IP) are
**included**: `vite --host` and `php artisan serve --host=0.0.0.0` are exactly what a developer
wants to see, and "this is reachable from your network" is information, not noise. They carry
`exposure: AllInterfaces | Interface`; the GUI should badge them. Setting
`discovery.include_non_loopback = false` hides them (reason `non_loopback`). **UDP and Unix
sockets are out of scope** for v2.0 (dev servers are TCP; php-fpm's unix socket is a `run.cmd`
matter). HTTP/3 dev servers are rare enough to wait.

**Comparison** (measured on this machine: 60 GB RAM, 506 pids, 15 listeners, ~85 TCP rows):

| | `/proc/net/tcp{,6}` | netlink `sock_diag` |
|---|---|---|
| Measured time per scan | **4–6 ms** (read + parse, 3 runs) | **0.06–0.15 ms** (both families) |
| Scales with | all TCP sockets *and*, it seems, the size of the kernel's RAM-sized hash tables (probable reason an ~85-row file costs 5 ms) | number of listeners (state filter `1<<10`) |
| Gives | local addr, state, uid, inode | local addr, state, uid, inode |
| Complexity | ~60 lines of text parsing; hex addresses are **host-endian** per 32-bit word | ~100 lines: fixed binary structs, network-order addresses |
| Debuggable by hand | `cat` | `ss -ltn` |
| Privilege | none | none (verified as uid 1000) |
| Failure modes | none | `EPERM`/`EPROTONOSUPPORT` under a seccomp/`RestrictAddressFamilies` sandbox or a kernel without `INET_DIAG` |

**Decision: `SockDiag` is primary; `ProcNetTcp` is the automatic fallback and the test oracle.**
5 ms every 2 s is only 0.25 % of a core, but it also scales with machine size, takes kernel hash
locks, and contradicts the "idle at near-zero" goal for no benefit. The fallback keeps the
daemon working in odd sandboxes; a parity test (§7) keeps the two honest. `Snapshot.source` says
which one ran, and the first fallback is logged once at `warn`.

```rust
trait SocketSource: Send {
    fn listening_tcp(&mut self) -> io::Result<Vec<RawSocket>>;   // v4 + v6, LISTEN only
}
```

**`SockDiag` wire facts** (stable kernel ABI; `man 7 sock_diag`; prototype verified here):
- Socket: `AF_NETLINK`(16), `SOCK_DGRAM`, protocol `NETLINK_SOCK_DIAG`(4). Use `rustix`
  (features `net`; the constant `rustix::net::netlink::SOCK_DIAG` exists) or five `unsafe` lines of
  `libc`. Do **not** pull in `netlink-packet-sock-diag` (0.4.x and 0.5.x both seen on docs.rs, tied to
  `netlink-packet-core` versions): it is an abstraction layer over one fixed 72-byte struct.
- Request = `nlmsghdr`(16: len u32, type u16 = `SOCK_DIAG_BY_FAMILY`(20), flags u16 = `NLM_F_REQUEST|NLM_F_DUMP` = 0x301, seq u32, pid u32 = 0)
  followed by `inet_diag_req_v2`(56: family u8 = `AF_INET`(2) or `AF_INET6`(10), protocol u8 = 6, ext u8 = 0,
  pad u8, `states` u32 = `1 << 10` (TCP_LISTEN), `inet_diag_sockid` 48 zero bytes). Send one request
  per family. All integers are native-endian, except addresses and ports below.
- Response datagram = repeated `nlmsghdr` (align each message to 4 bytes) until `NLMSG_DONE`(3);
  type 2 is `NLMSG_ERROR`. Payload = `inet_diag_msg`(72): family u8 @0, state u8 @1, `id` @4..52
  (`sport` u16 **big-endian** @4, `src` 16 bytes network order @8 — IPv4 uses the first 4),
  `uid` u32 @64, `inode` u32 @68. Receive buffer 64 KiB.

**`ProcNetTcp` facts** (`proc(5)`): data lines are
`sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode ...`. Keep only `st == 0A`.
`local_address` is `ADDR:PORT`, port = 4 hex digits (host order). IPv4 `0100007F` = one native
`u32` printed in hex → `Ipv4Addr::from(u32::from_str_radix(h,16)?.to_ne_bytes())`. IPv6 is four such
words → concatenate four `to_ne_bytes()` (`00000000000000000000000001000000` is `::1`; verified).
Read each file with a single `read_to_string` (the `seq_file` can skip or repeat rows across
`read(2)` calls); de-duplicate by inode.

### 3.4 Socket → process (inode → pid)

The kernel exposes no "socket → pid" table, so the only way is: for each candidate process list
`/proc/<pid>/fd`, `readlink` each entry, look for `socket:[<inode>]`. (`ss -p` and `lsof` do the same.)

Cost on this machine: 183 of 506 processes are ours, with 6 517 fds; a **full scan is 22–25 ms**
(`readlink` ≈ 3 µs each). Doing that every tick would be the dominant cost, so:

1. **Only own-uid sockets are searched.** The sock_diag/`/proc` row carries the socket's `uid`.
   `uid != me` ⇒ `Owner = None` immediately, no scan (Docker's root-owned `docker-proxy`, CUPS,
   resolved, a system Postgres). Those listeners still appear, with `pid: None`, and `class: Foreign`.
2. **Positive cache** `inode → Owner{pid, fd}`. Each tick, verify with one `readlink
   /proc/<pid>/fd/<fd>` (≈ 3 µs) and compare to `socket:[inode]`. Equal ⇒ still valid (this
   also proves the pid was not recycled). Otherwise drop the entry and treat it as new.
3. **Search only for unresolved own-uid inodes**, newest process first: sort pids descending
   (pids are allocated increasing, so a just-started server is among the first scanned), skip pids
   whose `/proc/<pid>` is not owned by us, `read_dir(fd)` (EACCES ⇒ skip), stop as soon as all wanted
   inodes are found. Measured: a server started minutes ago resolved in **2.4 ms** (16 processes
   scanned), an old one in 5–13 ms, an unresolvable inode costs the full ≈ 24 ms.
4. **Topmost holder.** Pre-forking servers (gunicorn, nginx, `uvicorn --reload`) share the socket
   across processes. After finding holder `P`, check `P`'s ancestors (≤ 5, own uid) for the same
   inode and report the **topmost** one: it is the process whose death ends the server, and the
   one a human wants to `kill`.
5. **Negative cache.** An own-uid inode with no readable holder (non-dumpable processes: binaries with
   file capabilities such as a `setcap`'d Caddy, some sandboxes) is retried on the next tick,
   then at most every 60 s. Until found: `pid: None`, `uid == me`, `class: Other`.
6. Run on `spawn_blocking`; never on the async executor.

`/proc/<pid>/fd` access for same-uid processes works under default Ubuntu settings
(`kernel.yama.ptrace_scope` restricts attach, not these reads). No capability is requested.

### 3.5 Process attributes and the cache

`ProcReader::read(pid)` returns a `ProcInfo` using plain `std::fs`:

| Field | Source | Notes |
|---|---|---|
| `ppid`, `start_ticks`, `comm` | `/proc/<pid>/stat` | Parse **after the last `)`**: `comm` may contain spaces and parens (real example here: `npm exec vite -`). `ppid` = field 4, `starttime` = field 22 (`proc(5)`). |
| `cmdline_raw` | `/proc/<pid>/cmdline` | NUL-separated; cap 16 KiB; empty for kernel threads/zombies; processes that rewrite their title (`next-server (v15…)`) arrive as one string. |
| `cwd` | `readlink /proc/<pid>/cwd` | `" (deleted)"` suffix ⇒ `None`. Already canonical. |
| `exe` | `readlink /proc/<pid>/exe` | Strip `" (deleted)"`; EACCES ⇒ `None`. |
| `unit` | `/proc/<pid>/cgroup` | §3.8. |
| `started_at` | `btime` (`/proc/stat`) + `start_ticks / CLK_TCK` | `CLK_TCK` via `rustix::param::clock_ticks_per_second()` (feature `param`) or `libc::sysconf`. |

Measured: all five reads ≈ **35 µs** per pid.

Cache: `HashMap<u32, Arc<ProcInfo>>` keyed by pid but **valid only while `/proc/<pid>/stat`
still reports the same `start_ticks`** (one 7 µs read per listener pid per tick). Mismatch ⇒ rebuild.
`cwd` and `cmdline_raw` can change under a live process (`chdir`, `setproctitle`), so the entry is
refreshed after `PROC_TTL = 30 s`. The ancestor chain is computed at (re)build time, not each tick.
Entries not referenced by any listener or chain for 60 s are dropped.

I deliberately do **not** use the `procfs` crate (0.18.0, last release Aug 2025, alive but slow-moving):
it cannot be pointed at a fixture directory, and `Process::new` opens a directory fd per call. The
parsing above is ~80 lines and fully testable. (Change to the baseline table: §9.)

### 3.6 Process → checkout matching

Input: the owner's `ProcInfo` plus `CheckoutIndex` (published by the registry on every change):

```rust
// lantern-core, pure
pub struct CheckoutIndex {
    pub by_path: HashMap<PathBuf, CheckoutRef>,   // canonical checkout path -> ref
    pub by_unit: HashMap<String, InstanceId>,     // "lantern-inst-myapp.service" -> instance
    pub home: PathBuf,
    pub workspace_roots: Vec<PathBuf>,            // from Settings, canonical
    pub any_adopted: bool,                        // some instance wants its route kept
}
pub struct CheckoutRef { pub instance: InstanceId, pub project: ProjectId,
                         pub kind: CheckoutKind, pub branch: Option<String>, pub path: PathBuf }
impl CheckoutIndex {
    /// Longest-prefix match, component-wise: deepest ancestor of `p` that is a checkout path.
    pub fn lookup(&self, p: &Path) -> Option<&CheckoutRef> {
        p.ancestors().find_map(|a| self.by_path.get(a))
    }
}
```

`Path::ancestors()` walks `/a/b/c → /a/b → /a → /` deepest-first and compares whole components, so
`/dev/app2` never matches checkout `/dev/app`, and a worktree nested inside its primary
(`app/.worktrees/x`) resolves to the worktree. O(depth) per lookup, no scan of all checkouts.
`CheckoutIndex::build` must **drop degenerate roots**: `/`, `$HOME`, or anything above `$HOME`;
otherwise every process in the home directory would match.

Paths must be canonical on both sides. Real case on this machine: `~/dev` is a symlink to
`/hq/projects/dev`; the kernel reports `/hq/projects/dev/...` as cwd, so the registry must index the
canonical form (core §6 already says so) and clients may map back to the user's spelling for display.

Algorithm `match_owner(p0) -> Option<(CheckoutRef, MatchSource)>`, first hit wins:

1. **Unit** — `p0.unit ∈ index.by_unit` ⇒ `(instance, Unit)`. Authoritative; ignores cwd (a managed
   command may `cd` anywhere).
2. **Own cwd** — `index.lookup(p0.cwd)` ⇒ `Cwd`.
3. **Ancestor cwd** — for `a` in `p0.ancestors` (nearest first, at most `MAX_ANCESTOR_DEPTH = 5`):
   if `is_boundary(a)` stop; else `index.lookup(a.cwd)` ⇒ `AncestorCwd{depth}`.
4. **Cmdline path** — for each absolute path in `p0.cmdline_raw` (also `--flag=/abs/path`) and in
   non-boundary ancestors' cmdlines: `index.lookup(path)` ⇒ `CmdlinePath`. Catches
   `python3 /home/u/app/manage.py runserver` started from `~`, and `.venv/bin/uvicorn` launched
   from elsewhere.
5. Otherwise unmatched.

*Why walk ancestors at all?* The listening process is usually a child: here
`node` (vite) ← `sh -c vite` ← `npm exec vite` all share cwd `/hq/projects/dev/games/rigsmith`, so
step 2 hits. Ancestors rescue the cases where the listener changed directory or was daemonised
(cwd `/`), but its launcher did not. *Why stop at 5?* Real chains are 3–4 deep (`turbo → pnpm → sh
→ vite` is the long end) and a longer walk starts attributing unrelated processes.

`is_boundary(a)` — an ancestor whose cwd says nothing about the project, so the walk stops there:
`pid <= 1` or `ppid == 0`; `comm` in `systemd`, `init`; `uid != me`; the daemon's own pid; an
**interactive shell** (`bash zsh fish sh dash ksh nu xonsh` whose cmdline has no `-c` and no script
argument, or whose `argv[0]` starts with `-`); terminals/multiplexers/remote entry points
(`konsole gnome-terminal-server kitty alacritty wezterm-gui foot xterm tilix ptyxis tmux: server
screen sshd login su code cursor zed`). Non-interactive `sh -c` wrappers (what `npm` spawns) are
*not* boundaries. The list is one `const` in `matcher.rs`.

**cwd `/` or `$HOME`** simply misses the index (those roots are never indexed), so the algorithm
falls through to ancestors and cmdline paths; if everything misses the listener is unmatched, which
is correct (Steam, Discord, a `python -m http.server` run from `~`).

Rejected signal: `/proc/<pid>/environ` (`PWD`, `INIT_CWD`, `VIRTUAL_ENV`). It is the most
informative, but it is the file most likely to contain secrets, and cwd + ancestors + cmdline
paths already cover the common cases.

### 3.7 Classification and labels

For each listener compute `(class, label)`; first rule wins:

1. `docker` info present, or `exe` basename in `rootlesskit rootlessport` ⇒ `Docker` ("Docker", "Docker (rootless)").
2. Unit starts with `lantern-svc-` ⇒ `Infra`, label = service name (13-system's naming; assumption §4.2).
3. `matched.is_some()` ⇒ `Project`.
4. Dev-server rule hits ⇒ `DevServer`.
5. cwd is under a workspace root (but not the root itself) ⇒ `DevServer` (an *unregistered* project directory;
   the GUI can offer "register project").
6. Infra rule hits ⇒ `Infra`.
7. `uid != me` ⇒ `Foreign`; else `Other` (Steam, Discord, VS Code helpers…).

Rules are evaluated against the listener's own argv first, then non-boundary ancestors'. A rule
matches on *file stems* of arguments (`.../node_modules/.bin/vite`, `vite.js`, `vite` all have
stem `vite`) plus sub-command words. Table in `classify.rs` (`const RULES: &[Rule]`), grown by
adding fixture + row:

| Class | Signal (argv stems / words) | Label |
|---|---|---|
| DevServer | `vite` (+`preview`) | Vite |
| DevServer | `next` + (`dev`\|`start`), or argv0 starts `next-server` | Next.js |
| DevServer | `nuxt`\|`nuxi` + `dev`; `astro dev`; `remix`; `svelte-kit`; `ng serve`; `webpack serve`\|`webpack-dev-server`; `parcel`; `storybook`; `expo start`; `wrangler dev`; `gatsby develop`; `vitepress dev`; `docusaurus start`; `react-scripts start` | the tool name |
| DevServer | `uvicorn`, `gunicorn`, `hypercorn`, `daphne`, `granian`; `manage.py runserver`; `flask run`\|`-m flask`; `fastapi dev`\|`run`; `streamlit run`; `mkdocs serve`; `-m http.server` | uvicorn, Django, Flask… |
| DevServer | `artisan serve` (own or ancestor), `php -S` (+`server.php`), `symfony serve` | Laravel serve / PHP built-in |
| DevServer | `rails s[erver]`, `puma`, `rackup`, `jekyll serve`, `hugo server` | Rails, Puma… |
| DevServer | `serve`, `http-server`, `live-server`, `browser-sync`, `caddy file-server` | Static server |
| DevServer | `bun\|deno\|node\|tsx\|nodemon` running a script inside a checkout; `air`, `cargo run`, `go run`, or `argv0` inside a checkout (`target/debug/x`) | `node server.mjs`, `x` |
| Infra | `postgres`, `redis-server`, `mysqld`, `mariadbd`, `mongod`, `memcached`, `nginx`, `apache2`, `php-fpm:`, `caddy`, `dnsmasq`, `sshd`, `cupsd`, `dockerd`, `containerd`, `mailpit`, `minio` | the daemon name |

Fallback label: `basename(exe)` plus the script's basename for interpreters (`node server.mjs`).
The probe may later *confirm* a framework (§3.12) but never overrides a cmdline label.

### 3.8 Managed detection and `port_binding`

**cgroup parsing.** Unified cgroup v2 (Ubuntu 26.04) has one line `0::<path>`; split at the first two
`:` only (paths may contain `:` and parentheses; real example here:
`.../app-org.kde.konsole-165786.scope/tab(165834).scope`). Verified Lantern shape from the runtime
tests on this machine:
`0::/user.slice/user-1000.slice/user@1000.service/lantern.slice/lantern-test.slice/lantern-inst-zz-a1.service`.

```rust
fn unit_of(cgroup_path: &str) -> Option<String> {
    let comps = cgroup_path.split('/').filter(|c| !c.is_empty());
    // 1. any component named lantern*.service / lantern*.scope, wherever slice nesting puts it
    // 2. else the innermost *.service (a user's own unit, shown as info; managed = false)
}
fn is_lantern_inst(unit: &str) -> bool {   // lantern-inst-<slug>.service | .scope
    unit.starts_with("lantern-inst-") && (unit.ends_with(".service") || unit.ends_with(".scope"))
}
```
Unit names are compared after `\xNN` unescaping on both sides (systemd escapes unusual characters in
cgroup directory names). cgroup v1/hybrid is not supported (`unit = None`); Ubuntu 26.04 is v2-only.
`Listener.managed = is_lantern_inst(unit)`. Because descendants of a unit stay in its cgroup, the `node`
grandchild of `npm` under `lantern-inst-myapp.service` is managed too.

**Readiness question** — the function 10-runtime calls:

```rust
pub enum PortBinding {
    /// Nothing is listening on that TCP port.
    Free,
    /// Listening, and a holder is inside `expected_unit`'s cgroup.
    BoundByUnit { pid: u32, addrs: Vec<SocketAddr> },
    /// Listening, but no holder is in `expected_unit` (or none was expected).
    HeldByOther(PortHolder),
}
pub struct PortHolder {
    pub port: u16, pub addr: SocketAddr,
    pub pid: Option<u32>, pub uid: u32,
    pub exe: Option<PathBuf>, pub cmdline: Vec<String>,   // redacted
    pub cwd: Option<PathBuf>, pub unit: Option<String>,
    pub instance: Option<InstanceId>,                      // matched instance, if any
    pub label: String,
}
impl PortHolder { pub fn describe(&self) -> String }       // §3.9

impl DiscoveryHandle {
    /// Fresh (never cached) answer. `expected_unit` is the full unit name, e.g.
    /// "lantern-inst-myapp.service". Cheap enough to poll every 100-250 ms.
    pub async fn port_binding(&self, port: u16, expected_unit: Option<&str>) -> PortBinding;
}
```

Algorithm: (1) `listening_tcp()` — fresh, ~0.1 ms — filter by port; none ⇒ `Free` with **no `/proc`
walk at all**, which is the common case while a server is still booting. (2) For each socket resolve
the owner via the shared cache or §3.4 (foreign-uid ⇒ no scan). (3) Read the owner's `unit`. (4) Any
holder with `unit == expected_unit` ⇒ `BoundByUnit` (addrs = all of that unit's sockets on the port).
(5) Else `HeldByOther` for the first holder. It shares `ScanState` (behind one `std::sync::Mutex`,
held only inside `spawn_blocking`) with the periodic scan, so the first sighting costs 2–13 ms once
and later polls ≈ 0.2 ms.

How 10-runtime should use it (their doc decides): after `systemd-run` returns, loop
`port_binding(port, Some(unit))` — `BoundByUnit` ⇒ ready; `Free` ⇒ keep waiting while the unit is
alive; `HeldByOther` ⇒ if `holder.unit` is another `lantern-inst-*` (old generation still stopping)
keep waiting until the deadline, otherwise fail fast with `PortConflict` and `holder.describe()`.
`addrs` lets them require an address Caddy can dial (a server bound to `[::1]` only is not reachable
at `127.0.0.1`). This replaces v1's "connect succeeds" test, which a foreign process could satisfy.

### 3.9 Visibility, hide rules, port conflicts

**What is shown.** The snapshot and `ListenersChanged` contain every listener that is **not hidden**,
each with a `class`. Clients default to `Project | DevServer | Docker` ("relevant") and offer
"show all" for `Infra | Other | Foreign`. Hidden listeners are only returned by
`GET /api/listeners?include_hidden=true`, with `hidden.why`, so the user can always find out why
something is missing.

**Default hide list** (`DEFAULT_HIDE`, compiled in, `hide_defaults = false` turns it off). Rules are
the same `HideRule` struct users write:

| Rule | Reason |
|---|---|
| `addr` 127.0.0.53 or 127.0.0.54, port 53 | systemd-resolved stub |
| port 631, uid 0 | CUPS |
| port 22, uid 0 | sshd |
| pid == daemon's own pid; unit `lantern.service`; unit `lantern-caddy.service`; any port in `SelfPorts` (API 4777, Caddy admin 2019, Caddy 80/443 and configured alternates, Lantern DNS) | Lantern infrastructure |

Port numbers alone would be fragile, so Lantern's own processes are matched by pid/unit first;
`SelfPorts` (supplied by 13-system via a `watch`) is the belt-and-braces second check.
**Listeners with `managed == true` and a `lantern-inst-*` unit are never hidden** by any rule.
Hidden listeners are never adopted.

**User rules** (`config.toml`; schema is 14-config's; rules are AND within a rule, OR across rules):

```toml
[discovery]
hide_defaults = true
hide = [
  { exe = "steamwebhelper*" },                                # glob on exe basename
  { port = "49152-65535", cmdline = "*--type=renderer*" },    # number or range; glob on the raw cmdline
  { cwd = "~/dev/scratch" },                                  # path prefix
]
```

```rust
pub struct HideRule { pub port: Option<PortSpec>, pub addr: Option<IpAddr>, pub uid: Option<u32>,
                      pub exe: Option<Glob>, pub cmdline: Option<Glob>, pub cwd: Option<PathBuf>,
                      pub reason: Option<String> }        // at least one match field required
```
The GUI's "Hide" button posts a rule `{port, exe}` (15-api → 14-config writes it). Pairing the
exe prevents hiding a different server that later reuses the port.

**Port conflicts.** A conflict = an instance's sticky port (allocated by 10-runtime) is LISTENed by
a process that is not that instance's unit. Discovery supplies the *who*:

```rust
impl Snapshot {
    /// reservations: (instance, sticky port) for every instance that is not currently Running.
    pub fn conflicts(&self, reservations: &[(InstanceId, u16)]) -> Vec<PortConflict>;
}
pub struct PortConflict { pub instance: InstanceId, pub holder: PortHolder }
```
`PortHolder::describe()` yields the text for the UI/CLI/MCP:
`` port 41003 is held by `node` (pid 8123) in ~/dev/other `` — home-relative cwd, `exe` basename,
plus ` (instance other-app)` if matched and ` (unit …)` for managed. For `pid: None`:
`port 5432 is held by a process owned by uid 113 (not visible to you)`. For Docker:
`port 8080 is published by Docker (container 172.17.0.2:80)`. The allocator still uses a bind test
as the final word on "free" (it is what the dev server will experience); discovery explains *why*
the bind failed.

### 3.10 Grouping into `Listener`s and choosing the primary address

One `Listener` per **(port, owner)**, where owner = `pid` if known, else `(uid)`. A Node server that binds
both `0.0.0.0:3000` and `[::]:3000` is one row with `other_addrs`. `addr` (primary) is the first by
preference `127.0.0.1`, `::1`, other loopback, `0.0.0.0`, `::`, other. Two different pids on the
same port (`127.0.0.1` and `::1`, a real Vite/Node-18 pitfall) stay two rows.

Routing needs an **upstream** that Caddy can actually dial, not merely the primary address:

```rust
fn upstream(addrs: &[SocketAddr], reached_via: Option<SocketAddr>) -> SocketAddr {
    // 1. the address a successful probe actually answered on
    // 2. 127.0.0.1:port if any addr is 127.0.0.1, 0.0.0.0 or [::] (dual-stack assumed)
    // 3. [::1]:port if any addr is ::1
    // 4. the first specific address (a LAN IP)
}
```

### 3.11 Unmanaged instances and adoption

**Unmanaged instance** = a matched listener with `managed == false`. `adopt.rs` turns snapshots
into `ExternalChange` messages for the registry; it never touches Caddy.

```rust
pub enum ExternalChange {
    Appeared { instance: InstanceId, external: ExternalState },
    Updated  { instance: InstanceId, external: ExternalState },
    Gone     { instance: InstanceId },
}
pub struct ExternalState {
    pub port: u16, pub upstream: SocketAddr, pub scheme: Scheme,   // Http | Https (probe saw TLS)
    pub pid: Option<u32>, pub label: String, pub extra_ports: Vec<u16>, pub since: Timestamp,
}
```

Rules (registry/11-instances applies; they are the contract):

| Instance status when a matched unmanaged listener is stable | Effect |
|---|---|
| `Stopped`, `Crashed`, `NeedsConfig` | → `External`; `runtime.port = external.port`, `main_pid`, `message = "started outside Lantern"`. |
| `External` | `Updated` (port or pid changed). |
| `Preparing`, `Starting`, `Running`, `Unhealthy`, `Stopping` | **No change.** A Lantern-managed process wins; the stray listener is just a row in the listener list (`managed: false`, same `matched`). |

- **Primary listener** when a checkout has several (frontend + API + Storybook): rank by
  `(probe says HTTP first, DevServer-class first, lowest port)`; the rest go in `extra_ports`.
  `POST /api/instances/{id}/adopt {port?}` lets the user pick another.
- **Appear delay 1.5 s**: a candidate must still exist in a forced follow-up scan 1.5 s after first
  sight before `Appeared` is sent, so test runners that open a port for 300 ms do not flap instances.
- **Grace 5 s** on disappearance before `Gone` (Vite restarts on config changes; the port is back in
  < 1 s). `Gone` ⇒ registry sets `Stopped`, clears `external`.
- **Adopt** = the user's persisted intent `Instance.adopt: bool` (set via
  `POST /api/instances/{id}/adopt`; global `discovery.auto_adopt`, default **false**, sets it for
  everyone). An instance is *routed* iff `status == External && (adopt || auto_adopt)`. The route
  reconciler in 13-system is **level-triggered** from Instance state: routable = `Running | Unhealthy`
  (as today) **or** `External` and adopted, upstream = `runtime.external.upstream`, scheme from
  `external.scheme`. Discovery never calls Caddy, so a missed event cannot leave a stale route; when the
  listener vanishes and the grace expires, the state change removes the route. When the same checkout's
  server returns, `adopt` still being set re-routes it with no click.
- **Start while External**: recommend 10-runtime returns `409 external_running` (body: pid, port)
  unless `force`; **Stop on External** returns `409 not_managed` with "started outside Lantern
  (pid N)". Lantern does not kill what it did not start (v1 did; §2).

```
 Stopped ──listener stable 1.5s──► External ──user adopts / auto_adopt──► External+routed
    ▲                                 │                                        │
    └────── gone for 5 s ─────────────┴────────────────────────────────────────┘ (route removed)
```

### 3.12 HTTP probe

Purpose: `is_http`, `<title>`, framework hint (Vite/Next/…), TLS-or-not, and which address
answers. Probing is **never on the scan path**; `probe.rs` keeps `ProbeCache` and spawns tasks;
results come back over an mpsc and mark the snapshot dirty (coalesced, §3.14).

- **Eligibility**: `class ∈ {Project, DevServer}`, `uid == me`, not hidden, `discovery.probe = true`.
  Never `Infra`/`Docker`/`Foreign`/`Other`: sending `GET /` to Postgres floods its log, and poking
  Steam or Discord is rude.
- **Request**: `reqwest` client built once: `.no_proxy()` (else `HTTP_PROXY` would route a localhost probe
  elsewhere), `redirect(Policy::none())`, `pool_max_idle_per_host(0)`, `connect_timeout(300 ms)`,
  `Accept-Encoding: identity`. One `GET /` with `Host: 127.0.0.1:<port>` (Vite rejects unknown hosts),
  `Accept: text/html`, `User-Agent: lantern-probe/2`, `X-Lantern-Probe: 1` (so a developer can filter it
  from their dev-server log). Target = the listener's `upstream()` candidates, loopback first.
- **Budget**: whole probe wrapped in `tokio::time::timeout(1500 ms)`; body read stops at 16 KiB or
  `</title>`; global `Semaphore(4)`. If plain HTTP fails with something other than refused/timeout
  (reset, invalid HTTP), try `https://` once (accept invalid certs) ⇒ `tls = true`.
- **Frequency**: *once per listener identity* `(addr, inode)` — a restarted server gets a fresh probe.
  Success is final. Failures (timeout, reset: Next.js compiles on first request) retry at +3 s, +15 s,
  +60 s, then stop; a manual `POST /api/listeners/probe` resets that. It is **not periodic**: a probe
  executes application code (an SSR page, a DB query, a log line in the user's terminal), so the fewer the
  better. Health checking is 10-runtime's business, not discovery's.
- **Result**:

```rust
pub struct ProbeInfo { pub http: bool, pub status: Option<u16>, pub title: Option<String>,
    pub server: Option<String>, pub powered_by: Option<String>, pub hint: Option<String>,
    pub tls: bool, pub reached_via: Option<SocketAddr>, pub at: Timestamp }
```
  `title`: case-insensitive search for `<title…>…</title>`, decode `&amp; &lt; &gt; &quot; &#39;`, strip
  control characters, collapse whitespace, cap 120 chars. No HTML parser crate. `hint`: body contains
  `/@vite/client` ⇒ `vite`; `/_next/` or `x-powered-by: Next.js` ⇒ `next`; `/_nuxt/` ⇒ `nuxt`;
  `Server: WSGIServer` ⇒ `django`; `Werkzeug` ⇒ `flask`; `uvicorn` ⇒ `uvicorn`;
  `X-Powered-By: PHP` ⇒ `php`. (Verify each against a real server when writing the fixtures.)
  A TLS-only server gives `tls: true` and `scheme: Https` in `ExternalState`, which 13-system turns
  into an `https` upstream with `tls_insecure_skip_verify`.

### 3.13 Docker-published ports

**Decision: v2.0 does not use the Docker API socket; it is deferred to v2.1 as an opt-in.** The socket is
root-equivalent (this user is in the `docker` group), the integration needs a unix-socket HTTP client,
and the user-visible gain in v2.0 is naming, not attribution.

What v2.0 does without it, using only world-readable files:
- A published port is held by root's `docker-proxy` (uid 0 ⇒ `pid: None`, `Foreign`). When a *new* foreign
  listener appears, `docker.rs` makes one pass over `/proc/*/comm`, and for each `docker-proxy` parses
  `/proc/<pid>/cmdline`: `-proto -host-ip -host-port -container-ip -container-port [-use-listen-fd]`
  (flags confirmed in `moby/cmd/docker-proxy/main_linux.go`). A match on proto+host-port(+host-ip)
  gives `DockerInfo { proxy_pid, container_ip, container_port }`, class `Docker`. Cost ≈ 3 ms, once per
  new foreign inode, cached.
- Rootless Docker/Podman: the holder is our own `rootlesskit`/`rootlessport`, so the normal path finds its
  pid; class `Docker`, label "Docker (rootless)".
- Gap, documented: with `"userland-proxy": false` there is no listening socket at all, so those ports are
  invisible to socket scanning.

v2.1 sketch (kept compatible): `discovery.docker_socket = "/var/run/docker.sock"`, `GET /containers/json`,
map `Ports[].PublicPort` to listeners, fill `DockerInfo.container` (name, image) and use
`com.docker.compose.project.working_dir` through `CheckoutIndex::lookup` to set `matched`. Enrichment
sits behind `trait ListenerEnricher` so it plugs into `build_snapshot` without touching the pipeline.

### 3.14 Scheduling, performance and diffing

**Demand levels** (the scanner task owns a `tokio::time::Interval`):

| Level | When | Interval |
|---|---|---|
| Active | ≥ 1 `InterestGuard` alive (an SSE client, the GUI/tray) | **2 s** |
| Watching | no guard, but `index.any_adopted` or `discovery.auto_adopt` | **5 s** |
| Idle | neither | **no timer** (zero wakeups); on-demand only |

On-demand: `fresh_snapshot(max_age)` returns the cached snapshot if younger than `max_age`, else runs one scan
(single-flight: concurrent callers share it). REST/MCP handlers call it with 2 s, so an agent calling
`get_ports` after starting a server by hand sees it. The first scan runs at daemon start (reconciliation
needs it). Creating an `InterestGuard` triggers an immediate scan. There is no unprivileged push
notification for "a socket started listening" (the proc-connector needs `CAP_NET_ADMIN`), so polling it is.

**Cost per scan** (measured here: 506 pids / 183 own / 6 517 fds / 15 listeners / 60 GB RAM):

| Phase | Steady state | When a new listener appears |
|---|---|---|
| sock_diag dump v4+v6 | 0.06–0.15 ms | same |
| verify cached owners (1 `readlink` each) | ~3 µs × 15 | — |
| `stat` per listener pid (start-time check) | ~7 µs × 15 | — |
| fd search for new own-uid inode | 0 | **2–13 ms** (newest-first, early exit); ≤ 25 ms worst case, then backed off |
| `ProcInfo` build (+ ≤ 5 ancestors) | 0 | 35 µs × ≤ 6 |
| `build_snapshot` (pure) | < 0.1 ms | same |
| **Total** | **≈ 0.3 ms** (budget 5 ms) | **≤ 15 ms** typical, 30 ms hard-cap warning |

If a scan exceeds 50 ms, log `warn` with the per-phase `Timings` (also in `Snapshot.scan_ms` and
`GET /api/system` diagnostics). Ticks use `MissedTickBehavior::Delay`.

**Diffing and debouncing.** After each scan/rematch/probe result, build the new `Vec<Listener>` (sorted by
`(port, addr, pid)`) and compare with `PartialEq` to the last published one. `Listener` has *no
per-tick-changing fields* (no uptime, CPU, memory) so equality is stable. Different ⇒ `seq += 1`, send
`watch` update (in-process consumers) and `Event::ListenersChanged` (SSE). Equal ⇒ nothing. At most one
event per 250 ms: a change inside the window sets `dirty` and arms a one-shot timer, so a burst (a probe
title arriving just after a new listener) collapses into one event. Removal of adopted routes is
debounced separately (5 s grace, §3.11), not here.

The loop (all arms cheap, scan runs on the blocking pool):

```rust
loop { select! {
    _ = tick.tick(), if level != Idle => self.scan().await,
    Some(req) = rx.recv()             => self.on_demand(req).await,      // scan_now / fresh_snapshot
    _ = index.changed()               => self.rematch(),                 // no I/O
    _ = settings.changed()            => self.reconfigure(),
    Some(p) = probe_rx.recv()         => self.apply_probe(p),
    _ = publish_timer, if self.dirty  => self.publish(),
    _ = cancel.cancelled()            => break,
} }
```

## 4. Interfaces

### 4.1 Provided (`discovery/mod.rs`)

```rust
pub fn spawn(deps: DiscoveryDeps, tasks: &mut JoinSet<()>, cancel: CancellationToken) -> DiscoveryHandle;

#[derive(Clone)]
pub struct DiscoveryHandle { /* Arc inner */ }
impl DiscoveryHandle {
    pub fn snapshot(&self) -> Arc<Snapshot>;                                  // never blocks, may be stale
    pub async fn fresh_snapshot(&self, max_age: Duration) -> Arc<Snapshot>;   // single-flight scan if older
    pub async fn scan_now(&self) -> Arc<Snapshot>;                            // always scans
    pub fn subscribe(&self) -> watch::Receiver<Arc<Snapshot>>;                // in-process consumers
    pub fn interest(&self) -> InterestGuard;                                  // hold while an SSE stream is open
    pub async fn port_binding(&self, port: u16, expected_unit: Option<&str>) -> PortBinding;   // §3.8
    pub async fn reprobe(&self, addr: SocketAddr);                            // POST /api/listeners/probe
}
```

| Consumer | Uses |
|---|---|
| 10-runtime | `port_binding` for readiness and pre-start conflict check; `PortHolder::describe()` for error text. Ports allocator: `snapshot().listeners` ∪ `hidden` to skip busy ports in its range (then bind-test). |
| 11-instances / registry | `ExternalChange` stream (apply rules in §3.11); publishes `CheckoutIndex`. |
| 15-api-mcp | `GET /api/listeners?include_hidden&class&project&port&fresh` → `{seq, taken_at, source, listeners}`; `POST /api/listeners/refresh`, `/probe`, `/hide`; SSE holds an `InterestGuard` per connection; MCP `get_ports`. |
| 16-clients | `Listener` JSON via `ListenersChanged` + `GET /api/listeners`; `describe()` text arrives pre-rendered in conflict errors. |
| events.rs | receives `Event::ListenersChanged { listeners }` (visible, non-hidden). |

### 4.2 Consumed (assumptions, to be checked against the sibling docs)

| From | Interface assumed |
|---|---|
| registry (11-instances) | `watch::Receiver<Arc<CheckoutIndex>>`, rebuilt on any checkout/instance/branch/adopt change; **every known checkout has an Instance record** (so `by_path` entries carry an `InstanceId`); `trait ExternalSink: Send+Sync { fn apply(&self, c: ExternalChange); }` (sync, non-blocking; implemented by an mpsc send into the registry actor); ownership of `Instance.adopt` and the status rules in §3.11. |
| 14-config | `watch::Receiver<Arc<DiscoverySettings>>` (`[discovery]`: `enabled`, `include_non_loopback`, `probe`, `auto_adopt`, `hide_defaults`, `hide`, `cmdline = "redacted"\|"hidden"`); workspace roots live in `CheckoutIndex`. |
| 13-system | `watch::Receiver<Arc<SelfPorts>>` (API, Caddy admin/http/https, DNS); unit-name convention below; level-triggered route reconciliation from Instance state incl. `External`+adopted; user unit must not set `ProtectProc=invisible` and, if it sets `RestrictAddressFamilies=`, must allow `AF_NETLINK`. |
| 10-runtime | `RuntimeState.unit` equals the cgroup unit name (`lantern-inst-<slug>.service`, `.scope` accepted); Lantern services use `lantern-svc-<name>.service`; daemon `lantern.service`; Caddy `lantern-caddy.service`. |
| events.rs | `EventBus::publish(Event)`. |

### 4.3 MCP `get_ports` compatibility (recommendation to 15-api-mcp)

Keep v1's shape `{ "<name>": { "port": N, "health_status": "…" } }`, keyed by `InstanceId` (project id for the
primary), including External instances, adding optional keys per entry (`source: "managed"|"external"`, `pid`,
`branch`). Add a **new** tool `list_listeners` (15's name, → 02 D10) for the full list so v1 clients never see a changed entry shape.

## 5. Failure modes & edge cases

| Situation | Behaviour |
|---|---|
| netlink unavailable (`EPERM`, no `INET_DIAG`) | Fall back to `/proc/net/tcp{,6}`; `Snapshot.source = ProcNetTcp`; warn once. |
| Process exits between steps (ENOENT/ESRCH on any per-pid read) | Treat that field as `None`; never panic; next tick is consistent. |
| Own-uid socket, owner unreadable (file-capability binary, sandbox) | `pid: None, uid == me, class: Other`; fd search retried next tick then every 60 s. |
| Socket owned by root/other user | `pid: None`, class `Foreign` (or `Docker`), shown unless hidden. No scan attempted. |
| cwd deleted (`… (deleted)`), `/`, `$HOME`, unreadable | `cwd = None` or misses the index → ancestors → cmdline paths → unmatched. |
| `~/dev` is a symlink (real here) | Match on canonical paths; registry stores canonical; display mapping is the client's job. |
| Server restarts on the same port | Same `(port, owner)` row, new pid/inode; probe re-runs; adoption sees `Updated`, not `Gone`, if back inside the 5 s grace. |
| Two dev servers in one checkout | Both listed with the same `matched`; ranking picks the adopt/primary port (§3.11). |
| Server bound only to `[::1]` (Vite on Node 18+) | `upstream()` picks `[::1]` or the probe's `reached_via`; Caddy never gets a 502 from a v4/v6 mismatch. |
| `SO_REUSEPORT` or pre-forking servers (shared socket) | One row; the pid is the topmost holder (§3.4 item 4). |
| Managed unit exists but state.json has no instance (orphan) | `managed = true`, matched by cwd if possible; 10-runtime's reconcile adopts the unit. |
| A user's own systemd unit (`rigsmith-dev.service`, real here) | `unit` shown (info), `managed = false`, normal cwd matching. |
| Probe target is slow, silent, or not HTTP | Timeout 1.5 s in a detached task; scan unaffected; retries bounded (§3.12). |
| > 1 000 listeners (k8s-in-docker machine) | Cap at 1 000, drop `Other/Foreign` beyond it, warn. |
| Daemon restart | Nothing persisted except `Instance.adopt`; first scan re-emits `Appeared` for running servers. |
| Dev server inside a container/flatpak/distrobox | Different mount namespace; paths may not match ⇒ unmatched. Accepted. |
| Test runner opens a port for 300 ms | Seen by the list at most; never becomes `External` (1.5 s appear delay). |

## 6. Security considerations

- **Command lines contain secrets** (`--token=…`, `DATABASE_URL=postgres://u:pw@…` as an argument,
  `-p hunter2`). `/proc/<pid>/cmdline` is world-readable, so same-user processes can already see it; the
  new risk is **aggregation and egress**: REST/SSE feed the GUI, and **MCP output goes into LLM context
  and transcripts**, possibly to a cloud model.
- **Decision: always redact; never expose raw.** `cmdline_raw` lives only in daemon memory for
  classification and is dropped at `Listener` construction. No `environ`, ever.
  `redact.rs` (applied to every outward path: REST, SSE, MCP, `PortHolder`, logs):
  1. `key=value` where `key` matches `(?i)token|secret|passw(or)?d|pwd|api[-_]?key|auth|credential|bearer|cookie|session|private|dsn` ⇒ `key=***`.
  2. A `-`/`--` flag whose name matches the same pattern and has no `=` ⇒ the **next** argument becomes `***`.
  3. URL userinfo: `scheme://user:pass@host` ⇒ `scheme://user:***@host`; `scheme://token@host` ⇒ `scheme://***@host`.
  4. Standalone token-looking arguments (≥ 32 chars of `[A-Za-z0-9_\-.+/=]`, ≥ 2 of lower/upper/digit,
     not starting with `/`, `.`, `~`) ⇒ `***`.
  5. Each argument truncated to 200 chars, at most 40 arguments (`…` marker).
  Best-effort by nature; the corpus test (§7) is the contract. Setting `discovery.cmdline = "hidden"`
  drops the argument list entirely and exposes only `exe` + `label`.
- **MCP** returns the same redacted list with home-relative `cwd`; `pid: None` listeners expose port, uid
  and class only. **Titles and labels are untrusted text from local servers** (a page title can say "ignore
  previous instructions"): lengths capped, control characters stripped, and 15-api should present them as
  data in tool output. Clients render them as plain text, never markup.
- **Probing** targets only addresses taken from the local socket table (loopback, wildcard→loopback, a
  local interface IP): no URL ever comes from the user or the network, so no SSRF surface. Bodies are not stored.
- **Impersonation**: a local process can fake its cwd to look like a project. Same-uid already means full
  trust and routing needs an explicit adopt (`auto_adopt` defaults off), so there is no escalation.
- **No signals, no privileges**: discovery never kills or signals a process (v1 did, §2); no capabilities, no
  ptrace, no Docker socket in v2.0. Listener data is readable by whatever can reach the API; core §10's
  Host/Origin checks (15-api) keep a web page from reading it.

## 7. Testing strategy

**Unit (no OS access)**
- Parsers against fixtures: sock_diag responses as hex captured from a real dump (v4, `::`, uid ≠ 0,
  multi-message, `NLMSG_DONE`/`ERROR`) plus golden request bytes; `/proc/net/tcp{,6}` (`0100007F`, `::1`,
  `::`, v4-mapped, non-LISTEN rows, duplicate inodes); `stat` with `comm` containing spaces/parens
  (`npm exec vite -`); `cmdline` (empty, 16 KiB cap); `cgroup` (parentheses, nested `lantern.slice`,
  `.scope`, `\x2d` escapes, v1-style lines).
- `CheckoutIndex::lookup`: `/dev/app` vs `/dev/app2`, nested worktree, degenerate roots dropped, deleted cwd.
- `match_owner` over a fake `/proc` tree: own cwd, ancestor hit, stop at interactive `bash`, `sh -c`
  passes through, cmdline-path fallback, cwd `/` and `$HOME`, unit beats cwd.
- `classify`: one real-cmdline fixture per table row. `redact`: ≥ 40 cases both ways (secrets removed;
  `node ./node_modules/.bin/vite --port 5173` and long paths untouched).
- `build_snapshot`: stable order, `seq` unchanged for equal input, hidden never in `listeners`, managed
  never hidden. `adopt.rs` with a manual clock: appear delay, grace, flap suppression, ranking, managed-wins.

**Integration (real OS, run in CI)**
- Bind a `TcpListener` on `127.0.0.1:0` inside the test, register a temp checkout containing the test's
  cwd, run the real scanner: expect `pid == std::process::id()`, `matched_by = Cwd`.
- Fork a child that inherits the socket: expect the *parent* as owner (topmost holder).
- **Parity**: `SockDiag` and `ProcNetTcp` return the same `(addr, inode, uid)` set (retry once on a race).
- A `Fixed` socket source with `uid != me`: expect `pid: None`, class `Foreign`, no fd scan performed.
- `#[ignore]` systemd test: `systemd-run --user --unit=lantern-inst-test-N sleep`-style listener ⇒
  `port_binding(port, Some(unit)) == BoundByUnit`, a second foreign listener ⇒ `HeldByOther`.
- Probe: tiny local server returning `<title>x</title>`; one that accepts and never answers (must time out in
  ≈ 1.5 s while a concurrent scan completes in < 20 ms); one TLS server.
- Perf guard (`#[ignore]`, prints numbers): steady-state scan < 5 ms, with a fixture tree of 500 processes.

**Manual checklist**: start `vite` by hand in a worktree ⇒ it appears with project + branch within 2 s;
`adopt` ⇒ `https://feature-x.myapp.glow` works; Ctrl-C ⇒ route gone after ~5 s; `docker run -p 8080:80 nginx` ⇒
class Docker with container IP; `python -m http.server 41003` then start the instance that owns 41003 ⇒ conflict
message names `python` and its directory; run `vite --host` ⇒ network-exposed badge.

## 8. Open questions (with recommendations)

1. **Default for `auto_adopt`.** Recommend **false** (explicit adopt, then sticky via `Instance.adopt`). Revisit
   after dogfooding; the owner may want true for primary instances only.
2. **Terminate an external process from the UI.** Recommend **not in v2.0**; v2.1 may add a confirmed
   `SIGTERM` to the listener's topmost holder. The conflict message shows `kill <pid>` text to copy.
3. **Should `Other`/`Foreign` listeners appear in the GUI by default?** Recommend hidden behind "show all";
   the data is always in the API.
4. **UDP and Unix sockets.** Recommend out of v2.0. Revisit if a user runs HTTP/3 dev servers or wants
   php-fpm's socket listed.
5. **Docker API** in v2.1: recommend opt-in setting, compose `working_dir` label → checkout matching.
6. **`CheckoutIndex` placement.** Recommend a pure type in `lantern-core` (shared by registry and tests).
7. **Probe default.** Recommend **on**, eligible classes only, once per listener (§3.12).
8. **Where GUI-created hide rules live.** Recommend `config.toml` via 14-config (user preferences), not `state.json`.

## 9. Proposed changes to core architecture

1. **`Listener` (§6)**: keep the six existing fields; document that `cmdline` is **redacted**; add the fields in
   §3.2 (`port, other_addrs, uid, exposure, started_at, project, branch, matched_by, unit, class, label, probe,
   docker, adopted, hidden`). Add `ListenerClass`, `MatchSource`, `Exposure`, `ProbeInfo`, `DockerInfo`.
2. **`RuntimeState` / `Instance` (§6)**: add `RuntimeState.external: Option<ExternalState>` (ephemeral) and
   `Instance.adopt: bool` (persisted user intent). Define `InstanceStatus::External` precisely as in §3.11
   (set only when no managed process exists for the instance).
3. **`Event::ListenersChanged` (§9)**: payload is the **non-hidden** snapshot; add `seq: u64`.
4. **Baseline technology (§5)**: replace "Process info: `/proc` via the `procfs` crate" with "`/proc` read directly
   with `std::fs` behind an injectable root (fixture-testable); socket enumeration via netlink `sock_diag`
   using `rustix` (features `net`, `param`) with a `/proc/net/tcp` fallback". `procfs` may still be used
   elsewhere but discovery does not depend on it.
5. **Registry contract (§8)**: add the `watch::Sender<Arc<CheckoutIndex>>` publication and the
   "every checkout has an Instance record" invariant.
6. **Route reconciliation (§3/`13-system`)**: level-triggered from Instance state, routing `External` +
   adopted instances to `runtime.external.upstream`; unit-name conventions `lantern-inst-*`, `lantern-svc-*`,
   `lantern.service`, `lantern-caddy.service` become part of the contract.
7. **Daemon unit hardening (§3)**: must not use `ProtectProc=invisible`; if `RestrictAddressFamilies=` is set it
   must include `AF_NETLINK`, `AF_INET`, `AF_INET6`, `AF_UNIX`.
8. **Glossary**: *Listener* = TCP LISTEN socket (v4/v6, loopback **and** non-loopback), UDP/Unix excluded in v2.0.
