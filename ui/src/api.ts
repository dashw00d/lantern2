// The daemon's REST API, same origin. Only the fields the control center uses are typed.

export type RunStatus =
  | 'stopped' | 'starting' | 'running' | 'stopping' | 'crashed' | 'failed' | 'backoff' | 'unhealthy'
  | 'external' | 'unknown' | string

export interface Instance {
  id: string
  project: string
  checkout: { path: string; kind: 'primary' | 'worktree' | string; branch: string | null; missing: boolean }
  url: string
  visible: boolean
  runtime: {
    status: RunStatus
    port: number | null
    message: string | null
    issue: { code?: string; message?: string } | string | null
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
  kind: string
  tags: string[]
  enabled: boolean
  primary: string
  status: RunStatus
  port: number | null
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
  framework: string
  name?: string
}

export interface LogLine {
  line: string
  stream: string
  ts: string
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
  instances: () => call<{ items: Instance[] }>('GET', '/api/instances').then((r) => r.items),
  projects: () => call<{ items: Project[] }>('GET', '/api/projects').then((r) => r.items),
  listeners: () => call<{ items: Listener[] }>('GET', '/api/listeners').then((r) => r.items),
  suggestions: () => call<{ items: Suggestion[] }>('GET', '/api/suggestions').then((r) => r.items),
  scan: () => call<unknown>('POST', '/api/projects/scan', {}),
  addProject: (path: string) => call<unknown>('POST', '/api/projects', { path }),
  removeProject: (id: string) => call<unknown>('DELETE', `/api/projects/${enc(id)}`),
  action: (id: string, action: 'start' | 'stop' | 'restart') =>
    call<unknown>('POST', `/api/instances/${enc(id)}/${action}?wait=false`),
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

/** The address the Open button goes to: the app's own port on this machine. */
export function openUrl(inst: Instance): string | null {
  const port = inst.runtime.port ?? inst.runtime.external?.port ?? null
  if (!port) return null
  const path = inst.manifest?.open_path ?? '/'
  return `http://127.0.0.1:${port}${path.startsWith('/') ? path : `/${path}`}`
}
