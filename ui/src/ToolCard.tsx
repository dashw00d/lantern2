import { ExternalLink, GitBranch, Play, RotateCw, ScrollText, Square, TriangleAlert } from 'lucide-react'
import { openUrl, type Instance, type Project } from './api'
import { instanceStatus, problem } from './status'
import { Button, StatusBadge, Tile, cn, shortPath } from './ui'

export interface ToolActions {
  act: (inst: Instance, action: 'start' | 'stop' | 'restart') => void
  logs: (inst: Instance, title: string) => void
  pending: (id: string) => boolean
}

export function ToolCard({
  project,
  primary,
  branches,
  actions,
}: {
  project: Project
  primary: Instance | undefined
  branches: Instance[]
  actions: ToolActions
}) {
  const meta = instanceStatus(primary)
  const url = primary && meta.live ? openUrl(primary) : null
  const why = problem(primary)
  const pending = primary ? actions.pending(primary.id) || meta.busy : false
  const managed = primary?.runtime.status !== 'external'
  const description = primary?.manifest?.description ?? null

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
            <StatusBadge tone={meta.tone} label={meta.label} pulse={meta.pulse} />
          </div>
          <p className="mt-0.5 truncate font-mono text-xs text-muted-foreground" title={project.root}>
            {shortPath(project.root)}
            {primary?.runtime.port ? <span className="text-muted-foreground/70"> · :{primary.runtime.port}</span> : null}
          </p>
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
            <BranchRow key={b.id} inst={b} actions={actions} />
          ))}
        </ul>
      ) : null}

      <footer className="mt-auto flex items-center gap-1.5 pt-4">
        {url ? (
          <a
            href={url}
            target="_blank"
            rel="noreferrer"
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
        <div className="flex-1" />
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

function BranchRow({ inst, actions }: { inst: Instance; actions: ToolActions }) {
  const meta = instanceStatus(inst)
  const url = meta.live ? openUrl(inst) : null
  const name = inst.checkout.branch ?? inst.id.split('@')[1] ?? inst.id
  return (
    <li className="flex items-center gap-2 rounded-lg bg-muted/60 px-2.5 py-1.5 text-xs">
      <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
      <span className="min-w-0 flex-1 truncate font-mono" title={inst.checkout.path}>
        {name}
      </span>
      <StatusBadge tone={meta.tone} label={meta.tone === 'info' ? 'outside' : meta.label} />
      {url ? (
        <a href={url} target="_blank" rel="noreferrer" className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground" title="Open" aria-label={`Open ${name}`}>
          <ExternalLink className="h-3.5 w-3.5" />
        </a>
      ) : null}
      {meta.live && inst.runtime.status !== 'external' ? (
        <button className="rounded p-1 text-muted-foreground hover:bg-card hover:text-foreground" title="Stop" aria-label={`Stop ${name}`} onClick={() => actions.act(inst, 'stop')}>
          <Square className="h-3.5 w-3.5" />
        </button>
      ) : null}
    </li>
  )
}
