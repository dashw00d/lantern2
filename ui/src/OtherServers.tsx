import { useState } from 'react'
import { ChevronRight, ExternalLink } from 'lucide-react'
import type { Listener } from './api'
import { Section, cn, shortPath } from './ui'

/** Listening servers Lantern didn't start and can't match to a tool. */
export function OtherServers({ items }: { items: Listener[] }) {
  const [open, setOpen] = useState(false)
  const shown = items
    .filter((l) => !l.managed && !l.project && !l.hidden && l.exposure !== 'none')
    .sort((a, b) => a.port - b.port)
  if (!shown.length) return null
  return (
    <Section
      title="Also running on this machine"
      count={shown.length}
      action={
        <button className="inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground" onClick={() => setOpen(!open)}>
          {open ? 'Hide' : 'Show'}
          <ChevronRight className={cn('h-4 w-4 transition', open && 'rotate-90')} />
        </button>
      }
    >
      {open ? (
        <ul className="divide-y divide-border overflow-hidden rounded-xl border border-border bg-card text-sm shadow-card">
          {shown.map((l) => (
            <li key={`${l.port}-${l.pid}`} className="flex items-center gap-3 px-4 py-2.5">
              <span className="w-16 shrink-0 font-mono text-xs text-muted-foreground">:{l.port}</span>
              <span className="min-w-0 flex-1 truncate font-medium">{l.label}</span>
              <span className="hidden min-w-0 max-w-[40%] truncate font-mono text-xs text-muted-foreground sm:block" title={l.cwd ?? ''}>
                {shortPath(l.cwd)}
              </span>
              <a
                href={`http://127.0.0.1:${l.port}/`}
                target="_blank"
                rel="noreferrer"
                className="rounded p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                title={`Open http://127.0.0.1:${l.port}`}
                aria-label={`Open port ${l.port}`}
              >
                <ExternalLink className="h-4 w-4" />
              </a>
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-sm text-muted-foreground">
          {shown.length} other servers are listening — dev servers you started by hand, apps, games.
        </p>
      )}
    </Section>
  )
}
