import { useState } from 'react'
import { ChevronRight, ExternalLink, Eye, EyeOff, FolderPlus } from 'lucide-react'
import { api, type Listener } from './api'
import { Button, Section, cn, shortPath } from './ui'

const HIDDEN_KEY = 'lantern.hiddenListeners.v1'

function listenerKey(l: Listener): string {
  return `${l.label}|${l.cwd ?? `:${l.port}`}`
}

function loadHidden(): string[] {
  try {
    const raw = localStorage.getItem(HIDDEN_KEY)
    const parsed = raw ? JSON.parse(raw) : []
    return Array.isArray(parsed) ? parsed.filter((x): x is string => typeof x === 'string') : []
  } catch {
    return []
  }
}

/** Listening servers Lantern didn't start and can't match to a tool. */
export function OtherServers({
  items,
  onAdded,
  onError,
}: {
  items: Listener[]
  onAdded: () => void
  onError: (e: unknown) => void
}) {
  const [open, setOpen] = useState(false)
  const [showHidden, setShowHidden] = useState(false)
  const [hidden, setHidden] = useState<string[]>(() => loadHidden())
  const [adding, setAdding] = useState<string | null>(null)

  const candidates = items
    .filter((l) => !l.managed && !l.project && !l.hidden && l.exposure !== 'none')
    .sort((a, b) => a.port - b.port)

  const hiddenSet = new Set(hidden)
  const visible = candidates.filter((l) => !hiddenSet.has(listenerKey(l)))
  const hiddenItems = candidates.filter((l) => hiddenSet.has(listenerKey(l)))
  const shown = showHidden ? candidates : visible

  if (!candidates.length) return null

  const toggleHide = (l: Listener) => {
    const k = listenerKey(l)
    const next = hiddenSet.has(k) ? hidden.filter((x) => x !== k) : [...hidden, k]
    setHidden(next)
    try {
      localStorage.setItem(HIDDEN_KEY, JSON.stringify(next))
    } catch {
      // ignore storage errors
    }
  }

  const addFromListener = async (l: Listener) => {
    if (!l.cwd) return
    setAdding(l.cwd)
    try {
      await api.addProject(l.cwd)
      onAdded()
    } catch (e) {
      onError(e)
    } finally {
      setAdding(null)
    }
  }

  return (
    <Section
      title="Also running on this machine"
      count={visible.length}
      action={
        <div className="flex items-center gap-3">
          {open && hiddenItems.length ? (
            <button
              className="text-xs text-muted-foreground hover:text-foreground"
              onClick={() => setShowHidden((v) => !v)}
            >
              {showHidden ? 'Hide ignored' : `Show ${hiddenItems.length} hidden`}
            </button>
          ) : null}
          <button className="inline-flex items-center gap-1 text-sm text-muted-foreground hover:text-foreground" onClick={() => setOpen(!open)}>
            {open ? 'Hide' : 'Show'}
            <ChevronRight className={cn('h-4 w-4 transition', open && 'rotate-90')} />
          </button>
        </div>
      }
    >
      {open ? (
        <ul className="divide-y divide-border overflow-hidden rounded-xl border border-border bg-card text-sm shadow-card">
          {shown.map((l) => {
            const isHidden = hiddenSet.has(listenerKey(l))
            return (
              <li key={`${l.port}-${l.pid}`} className={cn('flex items-center gap-3 px-4 py-2.5', isHidden && 'opacity-55')}>
                <span className="w-16 shrink-0 font-mono text-xs text-muted-foreground">:{l.port}</span>
                <span className="min-w-0 flex-1 truncate font-medium">{l.label}</span>
                <span className="hidden min-w-0 max-w-[36%] truncate font-mono text-xs text-muted-foreground sm:block" title={l.cwd ?? ''}>
                  {shortPath(l.cwd)}
                </span>
                <div className="flex shrink-0 items-center gap-1">
                  {l.cwd ? (
                    <Button
                      size="icon"
                      variant="ghost"
                      className="h-7 w-7"
                      busy={adding === l.cwd}
                      title={`Add ${shortPath(l.cwd)} as a tool`}
                      aria-label={`Add ${l.label} folder as a tool`}
                      onClick={() => addFromListener(l)}
                    >
                      {adding === l.cwd ? null : <FolderPlus className="h-4 w-4" />}
                    </Button>
                  ) : null}
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
                  <button
                    type="button"
                    className="rounded p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground"
                    title={isHidden ? 'Unhide server' : 'Hide server from this list'}
                    aria-label={isHidden ? `Unhide ${l.label}` : `Hide ${l.label}`}
                    onClick={() => toggleHide(l)}
                  >
                    {isHidden ? <Eye className="h-4 w-4" /> : <EyeOff className="h-4 w-4" />}
                  </button>
                </div>
              </li>
            )
          })}
        </ul>
      ) : (
        <p className="text-sm text-muted-foreground">
          {visible.length} other server{visible.length === 1 ? '' : 's'} listening — dev servers you started by hand, apps, games.
          {hiddenItems.length ? ` (${hiddenItems.length} hidden)` : ''}
        </p>
      )}
    </Section>
  )
}
