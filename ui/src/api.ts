// The daemon's REST API, same origin.

export type RunStatus =
  | 'stopped'
  | 'starting'
  | 'preparing'
  | 'running'
  | 'stopping'
  | 'crashed'
  | 'failed'
  | 'backoff'
  | 'unhealthy'
  | 'needs_config'
  | 'external'
  | 'unknown'
  | string

export interface Instance {
  id: string
  project: string
  domain: string
  checkout: {
    path: string
    kind: 'primary' | 'worktree' | 'managed' | string
    branch: string | null
    head?: string | null
    locked?: string | null
    missing: boolean
  }
  url: string
  pinned: boolean
  adopt: boolean
  visible: boolean
  setup_stale?: boolean
  runtime: {
    status: RunStatus
    port: number | null
    message: string | null
    issue: { code?: string; message?: string; detail?: string } | string | null
    last_exit: { code?: number | null; signal?: string | null; at?: string } | string | null
    external: { pid?: number; port?: number } | null
    started_at: string | null
  }
  manifest: { open_path?: string | null; description?: string | null; run_cmd: string | null } | null
}

export interface Project {
  id: string
  name: string
  description?: string | null
  root: string
  repo?: { common_dir: string; subpath: string } | null
  kind: string
  tags: string[]
  enabled: boolean
  primary: string
  status: RunStatus
  url?: string
  port: number | null
  instance_count?: number
}

export interface BranchInfo {
  name: string
  remote: string | null
  oid: string
  upstream: string | null
  ahead: number
  behind: number
  gone: boolean
  committed_at: number
  checked_out_at: string | null
}

export type RemovalBlocker =
  | 'not_managed'
  | 'running'
  | { locked: string }
  | { dirty: number }
  | { unpushed: number }
  | { orphaned: number }

export interface RemovalCheck {
  blockers: RemovalBlocker[]
  dirty: Array<{ path: string; kind: string }>
  unpushed: number
  orphaned: number
  has_submodules: boolean
  worktree_env: Record<string, string>
  branch: string | null
}

export interface Launcher {
  id: string
  name: string
  cmd: string
  cwd?: string
  url?: string
  status: 'running' | 'starting' | 'stopping' | 'failed' | 'stopped'
  exit_code: number | null
  started_at: string | null
  unit: string
}

export interface Listener {
  port: number
  pid: number | null
  cwd: string | null
  label: string
  managed: boolean
  exposure: string
  project: string | null
  class: string
  hidden: boolean | null
}

export interface Suggestion {
  path: string
  proposed_id?: string
  framework: string
  confidence?: string
  name?: string
}

export interface LogLine {
  line: string
  stream: string
  ts: string
}

export interface SystemInfo {
  version: string
  api_version: number
  pid: number
  boot_id: string
  started_at: string
  listen: string
  tld: string
}

export interface HealthFix {
  description: string
  command: string
  needs_root: boolean
}

export interface HealthCheck {
  id: string
  title: string
  status: 'ok' | 'warn' | 'fail' | 'skipped'
  detail: string
  fix?: HealthFix | null
}

export interface SystemHealth {
  ok: boolean
  checked_at: string
  checks: HealthCheck[]
}

export interface AddProjectOptions {
  id?: string
  name?: string
  write_manifest?: boolean
  overrides?: Record<string, unknown>
}

export interface RunBranchPayload {
  target:
    | { branch: string }
    | { new_branch: string; base?: string }
    | { detached: string }
  fetch?: boolean
  start?: boolean
  pin?: boolean
}

export class ApiError extends Error {
  code: string
  constructor(code: string, message: string) {
    super(message)
    this.code = code
  }
}

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const res = await fetch(path, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (res.status === 204) return undefined as T
  const text = await res.text()
  const data = text ? JSON.parse(text) : undefined
  if (!res.ok) {
    const err = data?.error
    throw new ApiError(err?.code ?? String(res.status), err?.message ?? res.statusText)
  }
  return data as T
}

const enc = encodeURIComponent

export const api = {
  systemInfo: () => call<SystemInfo>('GET', '/api/system'),
  systemHealth: () => call<SystemHealth>('GET', '/api/system/health'),
  instances: () => call<{ items: Instance[] }>('GET', '/api/instances').then((r) => r.items),
  projects: () => call<{ items: Project[] }>('GET', '/api/projects').then((r) => r.items),
  listeners: () => call<{ items: Listener[] }>('GET', '/api/listeners').then((r) => r.items),
  suggestions: () => call<{ items: Suggestion[] }>('GET', '/api/suggestions').then((r) => r.items),
  dismissSuggestion: (path: string) => call<void>('POST', '/api/suggestions/dismiss', { path }),
  scan: () => call<unknown>('POST', '/api/projects/scan', {}),
  addProject: (path: string, opts?: AddProjectOptions) =>
    call<Project>('POST', '/api/projects', { path, ...opts }),
  removeProject: (id: string, opts?: { stop?: boolean; remove_worktrees?: boolean }) => {
    const q = new URLSearchParams()
    if (opts?.stop !== undefined) q.set('stop', String(opts.stop))
    if (opts?.remove_worktrees) q.set('remove_worktrees', 'true')
    const qs = q.toString()
    return call<void>('DELETE', `/api/projects/${enc(id)}${qs ? `?${qs}` : ''}`)
  },
  syncProject: (id: string) =>
    call<{ items: Instance[] }>('POST', `/api/projects/${enc(id)}/sync`, {}).then((r) => r.items),
  projectBranches: (id: string) =>
    call<{ items: BranchInfo[] }>('GET', `/api/projects/${enc(id)}/branches`).then((r) => r.items),
  projectCheckouts: (id: string) =>
    call<{ items: Instance[] }>('GET', `/api/projects/${enc(id)}/checkouts`).then((r) => r.items),
  runBranch: (projectId: string, body: RunBranchPayload) =>
    call<unknown>('POST', `/api/projects/${enc(projectId)}/instances?wait=false`, body),

  action: (id: string, action: 'start' | 'stop' | 'restart') =>
    call<unknown>('POST', `/api/instances/${enc(id)}/${action}?wait=false`),
  pinInstance: (id: string, pinned: boolean) =>
    call<Instance>('POST', `/api/instances/${enc(id)}/${pinned ? 'pin' : 'unpin'}`, {}),
  adoptInstance: (id: string) =>
    call<Instance>('POST', `/api/instances/${enc(id)}/adopt`, {}),
  unadoptInstance: (id: string) =>
    call<Instance>('POST', `/api/instances/${enc(id)}/unadopt`, {}),
  removalCheck: (id: string) =>
    call<RemovalCheck>('GET', `/api/instances/${enc(id)}/removal-check`),
  removeInstance: (id: string, opts?: { force?: boolean; delete_branch?: boolean; stop?: boolean }) => {
    const q = new URLSearchParams({ wait: 'true' })
    if (opts?.force) q.set('force', 'true')
    if (opts?.delete_branch) q.set('delete_branch', 'true')
    if (opts?.stop) q.set('stop', 'true')
    return call<unknown>('DELETE', `/api/instances/${enc(id)}?${q.toString()}`)
  },
  logs: (id: string, tail = 400) =>
    call<{ lines: LogLine[] }>('GET', `/api/instances/${enc(id)}/logs?tail=${tail}`).then((r) => r.lines),

  launchers: () => call<{ items: Launcher[] }>('GET', '/api/launchers').then((r) => r.items),
  createLauncher: (l: LauncherInput) => call<Launcher>('POST', '/api/launchers', l),
  updateLauncher: (id: string, l: LauncherInput) => call<Launcher>('PUT', `/api/launchers/${enc(id)}`, l),
  deleteLauncher: (id: string) => call<void>('DELETE', `/api/launchers/${enc(id)}`),
  runLauncher: (id: string) => call<Launcher>('POST', `/api/launchers/${enc(id)}/start`),
  stopLauncher: (id: string) => call<Launcher>('POST', `/api/launchers/${enc(id)}/stop`),
  launcherLogs: (id: string) =>
    call<{ lines: string[] }>('GET', `/api/launchers/${enc(id)}/logs?tail=400`).then((r) => r.lines),
}

export interface LauncherInput {
  name: string
  cmd: string
  cwd?: string
  url?: string
}

/** True when Caddy, DNS, and local TLS are all working so named .glow URLs resolve cleanly. */
export function isNamedHttpsReady(health: SystemHealth | undefined): boolean {
  if (!health?.checks?.length) return false
  const byId = new Map(health.checks.map((c) => [c.id, c.status]))
  if (byId.get('e2e.https') === 'ok') return true
  const caddyOk = byId.get('caddy.running') === 'ok'
  const dnsOk = byId.get('dns.system') === 'ok' || byId.get('dns.system') === 'skipped'
  const tlsOk = byId.get('tls.serving') === 'ok'
  return caddyOk && dnsOk && tlsOk
}

/** The address the Open button goes to: named HTTPS URL when Caddy/DNS is ready, else loopback port. */
export function openUrl(inst: Instance, namedHttpsReady = false): string | null {
  const path = inst.manifest?.open_path ?? '/'
  const suffix = path.startsWith('/') ? path : `/${path}`
  const isExternal = inst.runtime.status === 'external'
  if (namedHttpsReady && inst.url && (!isExternal || inst.adopt)) {
    const base = inst.url.replace(/\/$/, '')
    return suffix === '/' ? base : `${base}${suffix}`
  }
  const port = inst.runtime.port ?? inst.runtime.external?.port ?? null
  if (!port) return null
  return `http://127.0.0.1:${port}${suffix}`
}
