import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  ExternalLink,
  GitBranch,
  Globe,
  Pin,
  PinOff,
  Play,
  Plus,
  RefreshCw,
  ScrollText,
  Search,
  Square,
  Trash2,
  TriangleAlert,
} from 'lucide-react'
import {
  api,
  openUrl,
  type BranchInfo,
  type Instance,
  type Project,
  type RemovalBlocker,
  type RemovalCheck,
} from './api'
import { instanceStatus } from './status'
import { Button, Modal, StatusBadge, cn, inputClass, shortPath } from './ui'

function formatBlocker(b: RemovalBlocker): string {
  if (b === 'not_managed') return 'Created outside Lantern (unpin/forget only)'
  if (b === 'running') return 'Currently running (will stop first)'
  if (typeof b === 'object') {
    if ('locked' in b) return b.locked ? `Locked: ${b.locked}` : 'Worktree is locked'
    if ('dirty' in b) return `${b.dirty} uncommitted change${b.dirty === 1 ? '' : 's'}`
    if ('unpushed' in b) return `${b.unpushed} unpushed commit${b.unpushed === 1 ? '' : 's'}`
    if ('orphaned' in b) return `${b.orphaned} orphaned commit${b.orphaned === 1 ? '' : 's'}`
  }
  return String(b)
}

export function BranchesModal({
  project,
  instances,
  namedHttpsReady,
  onClose,
  onRefresh,
  onLogs,
  onError,
}: {
  project: Project
  instances: Instance[]
  namedHttpsReady: boolean
  onClose: () => void
  onRefresh: () => void
  onLogs: (inst: Instance, title: string) => void
  onError: (e: unknown) => void
}) {
  const [branches, setBranches] = useState<BranchInfo[] | null>(null)
  const [syncing, setSyncing] = useState(false)
  const [filter, setFilter] = useState('')
  const [branchInput, setBranchInput] = useState('')
  const [createNew, setCreateNew] = useState(false)
  const [baseBranch, setBaseBranch] = useState('')
  const [launching, setLaunching] = useState(false)
  const [busyId, setBusyId] = useState<string | null>(null)
  const [removing, setRemoving] = useState<{ inst: Instance; check: RemovalCheck | null } | null>(null)
  const [forceRemove, setForceRemove] = useState(false)
  const [deleteBranch, setDeleteBranch] = useState(false)

  const loadBranches = useCallback(async () => {
    try {
      const list = await api.projectBranches(project.id)
      setBranches(list)
    } catch {
      setBranches([])
    }
  }, [project.id])

  useEffect(() => {
    loadBranches()
  }, [loadBranches])

  const sync = async () => {
    setSyncing(true)
    try {
      await api.syncProject(project.id)
      await loadBranches()
      onRefresh()
    } catch (e) {
      onError(e)
    } finally {
      setSyncing(false)
    }
  }

  const runBranch = async (e: React.FormEvent) => {
    e.preventDefault()
    const name = branchInput.trim()
    if (!name) return
    setLaunching(true)
    try {
      await api.runBranch(project.id, {
        target: createNew
          ? { new_branch: name, ...(baseBranch.trim() ? { base: baseBranch.trim() } : {}) }
          : { branch: name },
        fetch: true,
        start: true,
        pin: true,
      })
      setBranchInput('')
      setBaseBranch('')
      setCreateNew(false)
      onRefresh()
    } catch (err) {
      onError(err)
    } finally {
      setLaunching(false)
    }
  }

  const act = async (inst: Instance, fn: () => Promise<unknown>) => {
    setBusyId(inst.id)
    try {
      await fn()
      onRefresh()
    } catch (e) {
      onError(e)
    } finally {
      setBusyId(null)
    }
  }

  const startRemove = async (inst: Instance) => {
    setForceRemove(false)
    setDeleteBranch(false)
    if (inst.checkout.kind !== 'managed') {
      // User-made worktrees are only unpinned/forgotten, never deleted from disk.
      await act(inst, () => api.removeInstance(inst.id))
      return
    }
    setBusyId(inst.id)
    try {
      const check = await api.removalCheck(inst.id)
      setRemoving({ inst, check })
    } catch (e) {
      onError(e)
    } finally {
      setBusyId(null)
    }
  }

  const confirmRemove = async () => {
    if (!removing) return
    const target = removing.inst
    setBusyId(target.id)
    try {
      await api.removeInstance(target.id, {
        force: forceRemove,
        delete_branch: deleteBranch,
        stop: true,
      })
      setRemoving(null)
      onRefresh()
      await loadBranches()
    } catch (e) {
      onError(e)
    } finally {
      setBusyId(null)
    }
  }

  const sortedInstances = useMemo(() => {
    const q = filter.trim().toLowerCase()
    return [...instances]
      .filter((inst) => {
        if (!q) return true
        const hay = `${inst.id} ${inst.checkout.branch ?? ''} ${inst.checkout.path} ${inst.checkout.kind}`.toLowerCase()
        return hay.includes(q)
      })
      .sort((a, b) => {
        if (a.id === project.primary) return -1
        if (b.id === project.primary) return 1
        const aLive = ['running', 'unhealthy', 'external', 'starting', 'preparing'].includes(a.runtime.status) ? 0 : 1
        const bLive = ['running', 'unhealthy', 'external', 'starting', 'preparing'].includes(b.runtime.status) ? 0 : 1
        if (aLive !== bLive) return aLive - bLive
        if (a.pinned !== b.pinned) return a.pinned ? -1 : 1
        return a.id.localeCompare(b.id)
      })
  }, [instances, filter, project.primary])

  const uniqueBranchNames = useMemo(() => {
    const seen = new Set<string>()
    const names: string[] = []
    for (const b of branches ?? []) {
      if (!seen.has(b.name)) {
        seen.add(b.name)
        names.push(b.name)
      }
    }
    return names
  }, [branches])

  const hasDirtyOrUnpushed =
    removing?.check?.blockers.some(
      (b) => typeof b === 'object' && ('dirty' in b || 'unpushed' in b || 'orphaned' in b),
    ) ?? false

  return (
    <Modal title={`${project.name} — Branches & worktrees`} onClose={onClose} wide>
      <form onSubmit={runBranch} className="space-y-2.5 rounded-xl border border-border bg-muted/35 p-3">
        <div className="flex flex-wrap items-center gap-2">
          <div className="relative min-w-[200px] flex-1">
            <GitBranch className="pointer-events-none absolute left-3 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
            <input
              list={`branches-${project.id}`}
              className={`${inputClass} pl-9 font-mono text-xs`}
              value={branchInput}
              onChange={(e) => setBranchInput(e.target.value)}
              placeholder={createNew ? 'new-branch-name' : 'Pick or type a branch to run…'}
            />
            <datalist id={`branches-${project.id}`}>
              {uniqueBranchNames.map((name) => (
                <option key={name} value={name} />
              ))}
            </datalist>
          </div>
          {createNew ? (
            <input
              list={`branches-${project.id}`}
              className={`${inputClass} w-36 font-mono text-xs`}
              value={baseBranch}
              onChange={(e) => setBaseBranch(e.target.value)}
              placeholder="base (default)"
            />
          ) : null}
          <Button type="submit" variant="primary" size="sm" busy={launching} disabled={!branchInput.trim()}>
            {launching ? null : <Plus className="h-4 w-4" />}
            Run branch
          </Button>
        </div>
        <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
          <label className="inline-flex cursor-pointer items-center gap-1.5 select-none">
            <input
              type="checkbox"
              checked={createNew}
              onChange={(e) => setCreateNew(e.target.checked)}
              className="rounded border-border accent-primary"
            />
            Create new branch
          </label>
          <Button type="button" variant="ghost" size="sm" onClick={sync} busy={syncing}>
            {syncing ? null : <RefreshCw className="h-3.5 w-3.5" />}
            Sync git
          </Button>
        </div>
      </form>

      {removing ? (
        <div className="mt-4 rounded-xl border border-danger/35 bg-danger/8 p-3.5 text-sm">
          <div className="flex items-start gap-2.5">
            <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-danger" />
            <div className="min-w-0 flex-1 space-y-2">
              <p className="font-medium">
                Remove managed worktree <code className="font-mono">{removing.inst.id}</code>?
              </p>
              {removing.check?.blockers.length ? (
                <ul className="list-disc space-y-0.5 pl-4 text-xs text-danger">
                  {removing.check.blockers.map((b, i) => (
                    <li key={i}>{formatBlocker(b)}</li>
                  ))}
                </ul>
              ) : (
                <p className="text-xs text-muted-foreground">Clean worktree. Safe to remove.</p>
              )}
              <div className="flex flex-wrap items-center gap-4 pt-1 text-xs">
                {hasDirtyOrUnpushed ? (
                  <label className="inline-flex cursor-pointer items-center gap-1.5 text-danger">
                    <input
                      type="checkbox"
                      checked={forceRemove}
                      onChange={(e) => setForceRemove(e.target.checked)}
                      className="accent-danger"
                    />
                    Force remove (discard unsaved work)
                  </label>
                ) : null}
                {removing.check?.branch ? (
                  <label className="inline-flex cursor-pointer items-center gap-1.5 text-muted-foreground">
                    <input
                      type="checkbox"
                      checked={deleteBranch}
                      onChange={(e) => setDeleteBranch(e.target.checked)}
                      className="accent-primary"
                    />
                    Also delete local branch <code className="font-mono">{removing.check.branch}</code>
                  </label>
                ) : null}
              </div>
              <div className="flex items-center gap-2 pt-1">
                <Button
                  size="sm"
                  variant="danger"
                  busy={busyId === removing.inst.id}
                  disabled={hasDirtyOrUnpushed && !forceRemove}
                  onClick={confirmRemove}
                >
                  Remove worktree
                </Button>
                <Button size="sm" variant="ghost" onClick={() => setRemoving(null)}>
                  Cancel
                </Button>
              </div>
            </div>
          </div>
        </div>
      ) : null}

      <div className="mt-4 flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="pointer-events-none absolute left-3 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-muted-foreground" />
          <input
            className={`${inputClass} h-8 pl-8 text-xs`}
            value={filter}
            onChange={(e) => setFilter(e.target.value)}
            placeholder={`Filter ${instances.length} checkout${instances.length === 1 ? '' : 's'}…`}
          />
        </div>
        <span className="text-xs text-muted-foreground">
          {sortedInstances.length} of {instances.length}
        </span>
      </div>

      <ul className="mt-3 max-h-[46vh] divide-y divide-border overflow-y-auto rounded-xl border border-border">
        {sortedInstances.map((inst) => {
          const isPrimary = inst.id === project.primary
          const meta = instanceStatus(inst)
          const url = meta.live ? openUrl(inst, namedHttpsReady) : null
          const isExternal = inst.runtime.status === 'external'
          const name = inst.checkout.branch ?? (isPrimary ? 'primary' : inst.id.split('@')[1] ?? inst.id)
          const busy = busyId === inst.id || meta.busy

          return (
            <li key={inst.id} className="flex items-center gap-2.5 px-3 py-2.5 text-sm hover:bg-muted/30">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  <span className="truncate font-mono text-xs font-medium">{name}</span>
                  <span
                    className={cn(
                      'rounded px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide',
                      isPrimary
                        ? 'bg-primary/12 text-primary'
                        : inst.checkout.kind === 'managed'
                          ? 'bg-info/12 text-info'
                          : 'bg-muted text-muted-foreground',
                    )}
                  >
                    {isPrimary ? 'primary' : inst.checkout.kind}
                  </span>
                  {inst.checkout.missing ? (
                    <span className="rounded bg-danger/12 px-1.5 py-0.5 text-[10px] font-medium text-danger">
                      missing
                    </span>
                  ) : null}
                  <StatusBadge tone={meta.tone} label={meta.tone === 'info' ? 'outside' : meta.label} pulse={meta.pulse} />
                </div>
                <p className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground" title={inst.checkout.path}>
                  {shortPath(inst.checkout.path)}
                  {inst.runtime.port ? ` · :${inst.runtime.port}` : ''}
                  {namedHttpsReady && inst.domain ? ` · ${inst.domain}` : ''}
                </p>
              </div>

              <div className="flex shrink-0 items-center gap-1">
                {url ? (
                  <a
                    href={url}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex h-7 items-center gap-1 rounded-md bg-primary px-2 text-xs font-medium text-primary-foreground hover:brightness-110"
                    title={url}
                  >
                    <ExternalLink className="h-3.5 w-3.5" />
                    Open
                  </a>
                ) : !isExternal ? (
                  <Button
                    size="sm"
                    variant="secondary"
                    className="h-7 px-2 text-xs"
                    busy={busy}
                    disabled={inst.checkout.missing}
                    onClick={() => act(inst, () => api.action(inst.id, 'start'))}
                  >
                    {busy ? null : <Play className="h-3.5 w-3.5" />}
                    Start
                  </Button>
                ) : null}

                {meta.live && !isExternal ? (
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    title="Stop"
                    disabled={busy}
                    onClick={() => act(inst, () => api.action(inst.id, 'stop'))}
                  >
                    <Square className="h-3.5 w-3.5" />
                  </Button>
                ) : null}

                {isExternal ? (
                  <Button
                    size="sm"
                    variant="ghost"
                    className="h-7 px-2 text-xs"
                    busy={busy}
                    title={inst.adopt ? 'Stop routing domain to this external server' : 'Route domain to this external server'}
                    onClick={() => act(inst, () => (inst.adopt ? api.unadoptInstance(inst.id) : api.adoptInstance(inst.id)))}
                  >
                    <Globe className="h-3.5 w-3.5" />
                    {inst.adopt ? 'Unadopt' : 'Adopt'}
                  </Button>
                ) : null}

                {!isExternal ? (
                  <Button
                    size="icon"
                    variant="ghost"
                    className="h-7 w-7"
                    title="Logs"
                    onClick={() => onLogs(inst, `${project.name} (${name})`)}
                  >
                    <ScrollText className="h-3.5 w-3.5" />
                  </Button>
                ) : null}

                {!isPrimary ? (
                  <>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      title={inst.pinned ? 'Unpin from card' : 'Pin to card'}
                      disabled={busy}
                      onClick={() => act(inst, () => api.pinInstance(inst.id, !inst.pinned))}
                    >
                      {inst.pinned ? <PinOff className="h-3.5 w-3.5 text-primary" /> : <Pin className="h-3.5 w-3.5" />}
                    </Button>
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7 text-muted-foreground hover:text-danger"
                      title={
                        inst.checkout.kind === 'managed'
                          ? 'Remove managed worktree'
                          : 'Hide / forget worktree (does not delete files)'
                      }
                      disabled={busy}
                      onClick={() => startRemove(inst)}
                    >
                      <Trash2 className="h-3.5 w-3.5" />
                    </Button>
                  </>
                ) : null}
              </div>
            </li>
          )
        })}
      </ul>
    </Modal>
  )
}
