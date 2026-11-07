import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Plus, Search, WifiOff, X } from 'lucide-react'
import { api, ApiError, type Instance, type Launcher, type Listener, type Project } from './api'
import { AddTools } from './AddTools'
import { Commands } from './Commands'
import { LogDrawer, type LogSource } from './LogDrawer'
import { OtherServers } from './OtherServers'
import { ToolCard, type ToolActions } from './ToolCard'
import { Button, Section } from './ui'

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
  const [query, setQuery] = useState('')
  const [logs, setLogs] = useState<LogSource | null>(null)
  const [adding, setAdding] = useState(false)
  const [toasts, setToasts] = useState<Toast[]>([])
  const [pending, setPending] = useState<Record<string, number>>({})
  const search = useRef<HTMLInputElement>(null)

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
      logs: (inst, title) =>
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
      pending: (id) => id in pending,
    }),
    [pending, refreshInstances, toast],
  )

  const q = query.trim().toLowerCase()
  const tools = (projects ?? [])
    .filter((p) => !q || `${p.name} ${p.id} ${p.tags.join(' ')}`.toLowerCase().includes(q))
    .sort((a, b) => a.name.localeCompare(b.name))
  const byProject = new Map<string, Instance[]>()
  for (const i of instances ?? []) byProject.set(i.project, [...(byProject.get(i.project) ?? []), i])
  const running = (instances ?? []).filter((i) => ['running', 'unhealthy', 'external', 'starting'].includes(i.runtime.status)).length
    + (launchers ?? []).filter((l) => l.status === 'running').length
  const offline = projErr instanceof TypeError || (projErr !== null && projects === undefined)

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
          <div className="ml-auto text-sm text-muted-foreground">{running ? `${running} running` : 'Nothing running'}</div>
        </div>
      </header>

      {offline ? (
        <div className="border-b border-danger/30 bg-danger/10">
          <div className="mx-auto flex max-w-6xl items-center gap-2 px-5 py-2.5 text-sm text-danger">
            <WifiOff className="h-4 w-4" />
            Can't reach the Lantern daemon. Start it with <code className="font-mono">systemctl --user start lantern-dev</code>.
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
                  (i) => i.id !== p.primary && ['running', 'unhealthy', 'external', 'starting', 'crashed'].includes(i.runtime.status),
                )
                return <ToolCard key={p.id} project={p} primary={primary} branches={branches} actions={actions} />
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

        <OtherServers items={listeners ?? []} />
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
