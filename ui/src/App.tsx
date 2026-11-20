import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Globe, Plus, Search, ShieldAlert, Trash2, WifiOff, X } from 'lucide-react'
import {
  api,
  ApiError,
  isNamedHttpsReady,
  type Instance,
  type Launcher,
  type Listener,
  type Project,
  type SystemHealth,
  type SystemInfo,
} from './api'
import { AddTools } from './AddTools'
import { BranchesModal } from './BranchesModal'
import { Commands } from './Commands'
import { HealthModal } from './HealthModal'
import { LogDrawer, type LogSource } from './LogDrawer'
import { OtherServers } from './OtherServers'
import { ToolCard, type ToolActions } from './ToolCard'
import { Button, Modal, Section, cn } from './ui'

function usePoll<T>(load: () => Promise<T>, ms: number): [T | undefined, unknown, () => void] {
  const [data, setData] = useState<T>()
  const [error, setError] = useState<unknown>(null)
  const ref = useRef(load)
  ref.current = load
  const tick = useCallback(() => {
    ref.current().then(
      (d) => {
        setData(d)
        setError(null)
      },
      (e) => setError(e),
    )
  }, [])
  useEffect(() => {
    tick()
    const t = setInterval(tick, ms)
    const onFocus = () => tick()
    window.addEventListener('focus', onFocus)
    return () => {
      clearInterval(t)
      window.removeEventListener('focus', onFocus)
    }
  }, [tick, ms])
  return [data, error, tick]
}

interface Toast {
  id: number
  text: string
}

export function App() {
  const [projects, projErr, refreshProjects] = usePoll<Project[]>(api.projects, 2000)
  const [instances, , refreshInstances] = usePoll<Instance[]>(api.instances, 1500)
  const [launchers, , refreshLaunchers] = usePoll<Launcher[]>(api.launchers, 2000)
  const [listeners] = usePoll<Listener[]>(api.listeners, 5000)
  const [systemInfo] = usePoll<SystemInfo>(api.systemInfo, 60000)
  const [health, , refreshHealth] = usePoll<SystemHealth>(api.systemHealth, 15000)
  const [query, setQuery] = useState('')
  const [logs, setLogs] = useState<LogSource | null>(null)
  const [adding, setAdding] = useState(false)
  const [healthOpen, setHealthOpen] = useState(false)
  const [branchesProjectId, setBranchesProjectId] = useState<string | null>(null)
  const [removingProject, setRemovingProject] = useState<{ project: Project; managedWorktrees: number } | null>(null)
  const [removeWorktrees, setRemoveWorktrees] = useState(false)
  const [removingBusy, setRemovingBusy] = useState(false)
  const [toasts, setToasts] = useState<Toast[]>([])
  const [pending, setPending] = useState<Record<string, number>>({})
  const search = useRef<HTMLInputElement>(null)

  const namedHttpsReady = useMemo(() => isNamedHttpsReady(health), [health])
  const tld = systemInfo?.tld ?? 'glow'

  const toast = useCallback((e: unknown) => {
    const text = e instanceof ApiError || e instanceof Error ? e.message : String(e)
    const id = Date.now() + Math.random()
    setToasts((t) => [...t, { id, text }])
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 6000)
  }, [])

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === '/' && document.activeElement?.tagName !== 'INPUT') {
        e.preventDefault()
        search.current?.focus()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  // A click marks the instance pending until its status moves (or 20 s pass).
  const lastStatus = useRef<Record<string, string>>({})
  useEffect(() => {
    if (!instances) return
    const now = Date.now()
    setPending((p) => {
      const next = { ...p }
      for (const inst of instances) {
        const was = lastStatus.current[inst.id]
        if (next[inst.id] && ((was && was !== inst.runtime.status) || now - next[inst.id] > 20000)) delete next[inst.id]
        lastStatus.current[inst.id] = inst.runtime.status
      }
      return next
    })
  }, [instances])

  const openLogs = useCallback(
    (inst: Instance, title: string) =>
      setLogs({
        title,
        subtitle: inst.id,
        load: async () =>
          (await api.logs(inst.id)).map((l) => ({
            ts: new Date(l.ts).toLocaleTimeString([], { hour12: false }),
            text: l.line,
            err: l.stream === 'stderr',
          })),
      }),
    [],
  )

  const actions: ToolActions = useMemo(
    () => ({
      act: async (inst, action) => {
        lastStatus.current[inst.id] = inst.runtime.status
        setPending((p) => ({ ...p, [inst.id]: Date.now() }))
        try {
          await api.action(inst.id, action)
        } catch (e) {
          setPending((p) => {
            const { [inst.id]: _, ...rest } = p
            return rest
          })
          toast(e)
        }
        refreshInstances()
      },
      logs: openLogs,
      pending: (id) => id in pending,
      removeProject: (project, allInstances) => {
        const managedWorktrees = allInstances.filter((i) => i.checkout.kind === 'managed').length
        setRemoveWorktrees(false)
        setRemovingProject({ project, managedWorktrees })
      },
      openBranches: (project) => setBranchesProjectId(project.id),
      unpin: async (inst) => {
        try {
          await api.pinInstance(inst.id, false)
          refreshInstances()
        } catch (e) {
          toast(e)
        }
      },
      adopt: async (inst, adopt) => {
        try {
          if (adopt) await api.adoptInstance(inst.id)
          else await api.unadoptInstance(inst.id)
          refreshInstances()
        } catch (e) {
          toast(e)
        }
      },
      namedHttpsReady,
    }),
    [namedHttpsReady, openLogs, pending, refreshInstances, toast],
  )

  const confirmRemoveProject = async () => {
    if (!removingProject) return
    setRemovingBusy(true)
    try {
      await api.removeProject(removingProject.project.id, {
        stop: true,
        remove_worktrees: removeWorktrees,
      })
      setRemovingProject(null)
      refreshProjects()
      refreshInstances()
    } catch (e) {
      toast(e)
    } finally {
      setRemovingBusy(false)
    }
  }

  const q = query.trim().toLowerCase()
  const tools = (projects ?? [])
    .filter((p) => !q || `${p.name} ${p.id} ${p.tags.join(' ')}`.toLowerCase().includes(q))
    .sort((a, b) => a.name.localeCompare(b.name))
  const byProject = new Map<string, Instance[]>()
  for (const i of instances ?? []) byProject.set(i.project, [...(byProject.get(i.project) ?? []), i])
  const running =
    (instances ?? []).filter((i) => ['running', 'unhealthy', 'external', 'starting', 'preparing'].includes(i.runtime.status)).length +
    (launchers ?? []).filter((l) => l.status === 'running').length
  const offline = projErr instanceof TypeError || (projErr !== null && projects === undefined)
  const branchesProject = branchesProjectId ? (projects ?? []).find((p) => p.id === branchesProjectId) ?? null : null
  const issueCount = health ? health.checks.filter((c) => c.status === 'fail' || c.status === 'warn').length : 0

  return (
    <div className="min-h-screen">
      <header className="sticky top-0 z-20 border-b border-border bg-background/85 backdrop-blur">
        <div className="mx-auto flex h-14 max-w-6xl items-center gap-4 px-5">
          <div className="flex items-center gap-2.5">
            <img src="/favicon.svg" alt="" className="h-8 w-8" />
            <span className="text-[15px] font-semibold tracking-tight">Lantern</span>
          </div>
          <div className="relative ml-2 max-w-sm flex-1">
            <Search className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <input
              ref={search}
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search tools and commands"
              className="h-9 w-full rounded-lg border border-border bg-muted/50 pl-9 pr-8 text-sm outline-none transition placeholder:text-muted-foreground focus:border-primary focus:bg-background focus:ring-2 focus:ring-primary/20"
            />
            {query ? (
              <button className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-1 text-muted-foreground hover:text-foreground" onClick={() => setQuery('')} aria-label="Clear search">
                <X className="h-3.5 w-3.5" />
              </button>
            ) : (
              <kbd className="pointer-events-none absolute right-2.5 top-1/2 -translate-y-1/2 rounded border border-border px-1.5 font-mono text-[10px] text-muted-foreground">/</kbd>
            )}
          </div>
          <div className="ml-auto flex items-center gap-3">
            {health ? (
              <button
                type="button"
                onClick={() => setHealthOpen(true)}
                className={cn(
                  'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs font-medium transition',
                  namedHttpsReady
                    ? 'border-success/30 bg-success/10 text-success hover:bg-success/15'
                    : 'border-border bg-muted/60 text-muted-foreground hover:border-warning/40 hover:text-foreground',
                )}
                title={namedHttpsReady ? `*.${tld} HTTPS active` : `*.${tld} HTTPS setup (${issueCount} check${issueCount === 1 ? '' : 's'} need attention)`}
              >
                {namedHttpsReady ? <Globe className="h-3.5 w-3.5" /> : <ShieldAlert className="h-3.5 w-3.5 text-warning" />}
                <span>*.{tld}</span>
              </button>
            ) : null}
            <div className="text-sm text-muted-foreground">{running ? `${running} running` : 'Nothing running'}</div>
          </div>
        </div>
      </header>

      {offline ? (
        <div className="border-b border-danger/30 bg-danger/10">
          <div className="mx-auto flex max-w-6xl items-center gap-2 px-5 py-2.5 text-sm text-danger">
            <WifiOff className="h-4 w-4" />
            Can't reach the Lantern daemon. Start it with <code className="font-mono">systemctl --user start lantern</code>.
          </div>
        </div>
      ) : null}

      <main className="mx-auto max-w-6xl px-5 py-8">
        <Section
          title="Tools"
          count={projects ? tools.length : undefined}
          action={
            <Button size="sm" onClick={() => setAdding(true)}>
              <Plus className="h-4 w-4" />
              Add tool
            </Button>
          }
        >
          {projects === undefined ? (
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {[0, 1, 2].map((i) => (
                <div key={i} className="h-40 animate-pulse rounded-xl border border-border bg-card" />
              ))}
            </div>
          ) : tools.length ? (
            <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
              {tools.map((p) => {
                const all = byProject.get(p.id) ?? []
                const primary = all.find((i) => i.id === p.primary)
                const branches = all.filter(
                  (i) =>
                    i.id !== p.primary &&
                    (i.pinned ||
                      i.visible ||
                      ['running', 'unhealthy', 'external', 'starting', 'preparing', 'crashed'].includes(i.runtime.status)),
                )
                return <ToolCard key={p.id} project={p} primary={primary} branches={branches} allInstances={all} actions={actions} />
              })}
            </div>
          ) : (
            <button
              onClick={() => setAdding(true)}
              className="w-full rounded-xl border border-dashed border-border px-4 py-10 text-sm text-muted-foreground hover:border-primary/50 hover:text-foreground"
            >
              {q ? 'No tools match.' : 'No tools yet. Add one.'}
            </button>
          )}
        </Section>

        <Commands
          items={launchers ?? []}
          filter={q}
          refresh={refreshLaunchers}
          onError={toast}
          onLogs={(l) =>
            setLogs({
              title: l.name,
              subtitle: l.cmd,
              load: async () => (await api.launcherLogs(l.id)).map((text) => ({ text })),
            })
          }
        />

        <OtherServers
          items={listeners ?? []}
          onAdded={() => {
            refreshProjects()
            refreshInstances()
          }}
          onError={toast}
        />
      </main>

      {logs ? <LogDrawer source={logs} onClose={() => setLogs(null)} /> : null}
      {adding ? (
        <AddTools
          onClose={() => setAdding(false)}
          onAdded={() => {
            refreshProjects()
            refreshInstances()
          }}
          onError={toast}
        />
      ) : null}
      {branchesProject ? (
        <BranchesModal
          project={branchesProject}
          instances={byProject.get(branchesProject.id) ?? []}
          namedHttpsReady={namedHttpsReady}
          onClose={() => setBranchesProjectId(null)}
          onRefresh={() => {
            refreshProjects()
            refreshInstances()
          }}
          onLogs={openLogs}
          onError={toast}
        />
      ) : null}
      {healthOpen ? (
        <HealthModal
          info={systemInfo}
          health={health}
          onRefresh={refreshHealth}
          onClose={() => setHealthOpen(false)}
        />
      ) : null}
      {removingProject ? (
        <Modal title={`Remove ${removingProject.project.name}`} onClose={() => setRemovingProject(null)}>
          <p className="text-sm text-muted-foreground">
            Unregister <strong className="text-foreground">{removingProject.project.name}</strong> from Lantern? Any running instances will be stopped first. Your project files on disk are not deleted.
          </p>
          {removingProject.managedWorktrees > 0 ? (
            <label className="mt-4 flex cursor-pointer items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={removeWorktrees}
                onChange={(e) => setRemoveWorktrees(e.target.checked)}
                className="accent-danger"
              />
              Also remove {removingProject.managedWorktrees} Lantern-managed worktree{removingProject.managedWorktrees === 1 ? '' : 's'}
            </label>
          ) : null}
          <div className="mt-5 flex items-center justify-end gap-2">
            <Button variant="ghost" onClick={() => setRemovingProject(null)} disabled={removingBusy}>
              Cancel
            </Button>
            <Button variant="danger" busy={removingBusy} onClick={confirmRemoveProject}>
              {removingBusy ? null : <Trash2 className="h-4 w-4" />}
              Remove
            </Button>
          </div>
        </Modal>
      ) : null}

      <div className="fixed bottom-4 right-4 z-50 flex w-80 flex-col gap-2">
        {toasts.map((t) => (
          <div key={t.id} className="rounded-lg border border-danger/30 bg-card px-4 py-3 text-sm shadow-lg">
            <span className="font-medium text-danger">Something went wrong. </span>
            {t.text}
          </div>
        ))}
      </div>
    </div>
  )
}
