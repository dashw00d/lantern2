import {
  ExternalLink,
  GitBranch,
  Globe,
  PinOff,
  Play,
  RotateCw,
  ScrollText,
  Square,
  Trash2,
  TriangleAlert,
} from 'lucide-react'
import { openUrl, type Instance, type Project } from './api'
import { instanceStatus, problem } from './status'
import { Button, StatusBadge, Tile, cn, shortPath } from './ui'

export interface ToolActions {
  act: (inst: Instance, action: 'start' | 'stop' | 'restart') => void
  logs: (inst: Instance, title: string) => void
  pending: (id: string) => boolean
  removeProject: (project: Project, allInstances: Instance[]) => void
  openBranches: (project: Project) => void
  unpin: (inst: Instance) => void
  adopt: (inst: Instance, adopt: boolean) => void
  namedHttpsReady: boolean
}

export function ToolCard({
  project,
  primary,
  branches,
  allInstances,
  actions,
}: {
  project: Project
  primary: Instance | undefined
  branches: Instance[]
  allInstances: Instance[]
  actions: ToolActions
}) {
  const meta = instanceStatus(primary)
  const url = primary && meta.live ? openUrl(primary, actions.namedHttpsReady) : null
  const why = problem(primary)
  const pending = primary ? actions.pending(primary.id) || meta.busy : false
  const isExternal = primary?.runtime.status === 'external'
  const managed = !isExternal
  const description = primary?.manifest?.description ?? null
  const worktreeCount = Math.max(0, allInstances.length - (primary ? 1 : 0))
  const hasGit = Boolean(project.repo || primary?.checkout.branch || worktreeCount > 0)

  return (
    <article
      className={cn(
        'group flex flex-col rounded-xl border border-border bg-card p-4 shadow-card transition',
        meta.live ? 'hover:border-primary/40' : 'hover:border-foreground/15',
      )}
    >
      <header className="flex items-start gap-3">
        <Tile name={project.name} />
        <div className="min-w-0 flex-1">
          <div className="flex items-center justify-between gap-2">
            <h3 className="truncate font-semibold tracking-tight">{project.name}</h3>
            <div className="flex shrink-0 items-center gap-1">
              <StatusBadge tone={meta.tone} label={meta.label} pulse={meta.pulse} />
              <button
                type="button"
                className="rounded p-1 text-muted-foreground/60 opacity-70 transition hover:bg-danger/10 hover:text-danger group-hover:opacity-100"
                title={`Remove ${project.name} from Lantern`}
                aria-label={`Remove ${project.name}`}
                onClick={() => actions.removeProject(project, allInstances)}
              >
                <Trash2 className="h-3.5 w-3.5" />
              </button>
            </div>
          </div>
          <p className="mt-0.5 flex items-center gap-1.5 truncate font-mono text-xs text-muted-foreground" title={project.root}>
            <span className="truncate">{shortPath(project.root)}</span>
            {primary?.runtime.port ? <span className="shrink-0 text-muted-foreground/70">· :{primary.runtime.port}</span> : null}
          </p>
          <div className="mt-1 flex flex-wrap items-center gap-1.5">
            {primary?.checkout.branch ? (
              <button
                type="button"
                onClick={() => actions.openBranches(project)}
                className="inline-flex max-w-full items-center gap-1 rounded-md bg-muted/80 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground transition hover:bg-muted hover:text-foreground"
                title="View branches and worktrees"
              >
                <GitBranch className="h-3 w-3 shrink-0" />
                <span className="truncate">{primary.checkout.branch}</span>
                {worktreeCount > 0 ? (
                  <span className="rounded bg-background/80 px-1 text-[10px] font-medium text-foreground">
                    +{worktreeCount}
                  </span>
                ) : null}
              </button>
            ) : worktreeCount > 0 ? (
              <button
                type="button"
                onClick={() => actions.openBranches(project)}
                className="inline-flex items-center gap-1 rounded-md bg-muted/80 px-1.5 py-0.5 font-mono text-[11px] text-muted-foreground transition hover:bg-muted hover:text-foreground"
              >
                <GitBranch className="h-3 w-3 shrink-0" />
                <span>{worktreeCount} worktree{worktreeCount === 1 ? '' : 's'}</span>
              </button>
            ) : null}
            {actions.namedHttpsReady && primary?.domain ? (
              <span className="truncate font-mono text-[11px] text-muted-foreground/80">{primary.domain}</span>
            ) : null}
          </div>
        </div>
      </header>

      {description ? <p className="mt-3 line-clamp-2 text-sm text-muted-foreground">{description}</p> : null}

      {why ? (
        <button
          className="mt-3 flex w-full items-start gap-2 rounded-lg bg-danger/8 px-3 py-2 text-left text-xs text-danger hover:bg-danger/12"
          onClick={() => primary && actions.logs(primary, project.name)}
        >
          <TriangleAlert className="mt-px h-3.5 w-3.5 shrink-0" />
          <span className="line-clamp-2">{why}</span>
        </button>
      ) : null}

      {branches.length ? (
        <ul className="mt-3 space-y-1">
          {branches.map((b) => (
            <BranchRow key={b.id} project={project} inst={b} actions={actions} />
          ))}
        </ul>
      ) : null}

      <footer className="mt-auto flex items-center gap-1.5 pt-4">
        {url ? (
          <a
            href={url}
            target="_blank"
            rel="noreferrer"
            title={url}
            className="inline-flex h-9 items-center gap-2 rounded-lg bg-primary px-3.5 text-sm font-medium text-primary-foreground shadow-sm transition hover:brightness-110"
          >
            <ExternalLink className="h-4 w-4" />
            Open
          </a>
        ) : (
          <Button
            variant="primary"
            busy={pending}
            disabled={!primary || !project.enabled}
            onClick={() => primary && actions.act(primary, 'start')}
          >
            {pending ? null : <Play className="h-4 w-4" />}
            {pending ? meta.label : 'Start'}
          </Button>
        )}
        {meta.live && managed && primary ? (
          <>
            <Button variant="ghost" size="icon" title="Stop" aria-label="Stop" disabled={pending} onClick={() => actions.act(primary, 'stop')}>
              <Square className="h-4 w-4" />
            </Button>
            <Button variant="ghost" size="icon" title="Restart" aria-label="Restart" disabled={pending} onClick={() => actions.act(primary, 'restart')}>
              <RotateCw className="h-4 w-4" />
            </Button>
          </>
        ) : null}
        {isExternal && primary ? (
          <Button
            variant="ghost"
            size="sm"
            title={primary.adopt ? 'Stop routing domain to this external server' : 'Adopt external server onto its domain'}
            onClick={() => actions.adopt(primary, !primary.adopt)}
          >
            <Globe className="h-4 w-4" />
            {primary.adopt ? 'Unadopt' : 'Adopt'}
          </Button>
        ) : null}
        <div className="flex-1" />
        {hasGit ? (
          <Button
            variant="ghost"
            size={worktreeCount > 0 ? 'sm' : 'icon'}
            title="Branches & worktrees"
            aria-label="Branches & worktrees"
            onClick={() => actions.openBranches(project)}
          >
            <GitBranch className="h-4 w-4" />
            {worktreeCount > 0 ? worktreeCount : null}
          </Button>
        ) : null}
        {primary && managed ? (
          <Button variant="ghost" size="sm" onClick={() => actions.logs(primary, project.name)}>
            <ScrollText className="h-4 w-4" />
            Logs
          </Button>
        ) : null}
      </footer>
    </article>
  )
}

function BranchRow({
  project,
  inst,
  actions,
}: {
  project: Project
  inst: Instance
  actions: ToolActions
}) {
  const meta = instanceStatus(inst)
  const url = meta.live ? openUrl(inst, actions.namedHttpsReady) : null
  const name = inst.checkout.branch ?? inst.id.split('@')[1] ?? inst.id
  const pending = actions.pending(inst.id) || meta.busy
  const isExternal = inst.runtime.status === 'external'

  return (
    <li className="flex items-center gap-1.5 rounded-lg bg-muted/60 px-2.5 py-1.5 text-xs">
      <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate font-mono" title={inst.checkout.path}>
        {name}
      </span>
      <StatusBadge tone={meta.tone} label={meta.tone === 'info' ? 'outside' : meta.label} pulse={meta.pulse} />
      {url ? (
        <a
          href={url}
          target="_blank"
          rel="noreferrer"
          className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground"
          title={`Open ${url}`}
          aria-label={`Open ${name}`}
        >
          <ExternalLink className="h-3.5 w-3.5" />
        </a>
      ) : !isExternal ? (
        <button
          type="button"
          disabled={pending}
          className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground disabled:opacity-50"
          title={`Start ${name}`}
          aria-label={`Start ${name}`}
          onClick={() => actions.act(inst, 'start')}
        >
          <Play className="h-3.5 w-3.5" />
        </button>
      ) : null}
      {meta.live && !isExternal ? (
        <button
          type="button"
          disabled={pending}
          className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground disabled:opacity-50"
          title="Stop"
          aria-label={`Stop ${name}`}
          onClick={() => actions.act(inst, 'stop')}
        >
          <Square className="h-3.5 w-3.5" />
        </button>
      ) : null}
      {!isExternal ? (
        <button
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground"
          title="Logs"
          aria-label={`Logs for ${name}`}
          onClick={() => actions.logs(inst, `${project.name} (${name})`)}
        >
          <ScrollText className="h-3.5 w-3.5" />
        </button>
      ) : null}
      {inst.pinned && !meta.live ? (
        <button
          type="button"
          className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground"
          title="Unpin from card"
          aria-label={`Unpin ${name}`}
          onClick={() => actions.unpin(inst)}
        >
          <PinOff className="h-3.5 w-3.5" />
        </button>
      ) : null}
    </li>
  )
}
