import { useEffect, useState } from 'react'
import { FolderPlus, RefreshCw, X } from 'lucide-react'
import { api, type AddProjectOptions, type Suggestion } from './api'
import { Button, Modal, Tile, inputClass, shortPath } from './ui'

export function AddTools({ onClose, onAdded, onError }: { onClose: () => void; onAdded: () => void; onError: (e: unknown) => void }) {
  const [items, setItems] = useState<Suggestion[] | null>(null)
  const [path, setPath] = useState('')
  const [customCmd, setCustomCmd] = useState('')
  const [showCustom, setShowCustom] = useState(false)
  const [busy, setBusy] = useState<string | null>(null)
  const [showAll, setShowAll] = useState(false)

  const load = async (rescan = false) => {
    try {
      if (rescan) {
        setItems(null)
        await api.scan()
      }
      let found = await api.suggestions()
      // A daemon that just started hasn't scanned yet: scan once instead of showing nothing.
      if (!found.length && !rescan) {
        await api.scan()
        found = await api.suggestions()
      }
      setItems(found)
    } catch (e) {
      onError(e)
      setItems([])
    }
  }
  useEffect(() => {
    load()
  }, [])

  const add = async (p: string, opts?: AddProjectOptions) => {
    setBusy(p)
    try {
      await api.addProject(p, opts)
      setItems((xs) => xs?.filter((x) => x.path !== p) ?? null)
      setPath('')
      setCustomCmd('')
      onAdded()
    } catch (e) {
      onError(e)
    } finally {
      setBusy(null)
    }
  }

  const dismiss = async (p: string) => {
    setBusy(`dismiss:${p}`)
    try {
      await api.dismissSuggestion(p)
      setItems((xs) => xs?.filter((x) => x.path !== p) ?? null)
    } catch (e) {
      onError(e)
    } finally {
      setBusy(null)
    }
  }

  const detected = items?.filter((s) => s.framework && s.framework !== 'unknown') ?? []
  const other = items?.filter((s) => !s.framework || s.framework === 'unknown') ?? []

  return (
    <Modal title="Add a tool" onClose={onClose} wide>
      <form
        className="space-y-2.5"
        onSubmit={(e) => {
          e.preventDefault()
          const p = path.trim()
          if (!p) return
          const cmd = customCmd.trim()
          add(
            p,
            cmd
              ? { overrides: { type: 'proxy', run: { cmd } } }
              : undefined,
          )
        }}
      >
        <div className="flex gap-2">
          <input className={`${inputClass} font-mono`} value={path} onChange={(e) => setPath(e.target.value)} placeholder="/path/to/project" autoFocus />
          <Button variant="primary" type="submit" busy={busy === path.trim() && !!path.trim()}>
            Add folder
          </Button>
        </div>
        {showCustom ? (
          <input
            className={`${inputClass} font-mono text-xs`}
            value={customCmd}
            onChange={(e) => setCustomCmd(e.target.value)}
            placeholder="Optional run command, e.g. pnpm dev --port ${PORT}"
          />
        ) : null}
        <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
          <p>
            Reads <span className="font-mono">lantern.yaml</span> or auto-detects Vite, Next, Astro, SvelteKit, Node, Python, Go, Rust, Rails, PHP, Docker Compose…
          </p>
          <button
            type="button"
            className="shrink-0 font-medium text-primary hover:underline"
            onClick={() => setShowCustom((v) => !v)}
          >
            {showCustom ? 'Hide command' : 'Custom command'}
          </button>
        </div>
      </form>

      <div className="mt-6 flex items-center justify-between">
        <h4 className="text-sm font-semibold">Found in your folders</h4>
        <Button variant="ghost" size="sm" onClick={() => load(true)} disabled={items === null}>
          <RefreshCw className={`h-4 w-4 ${items === null ? 'animate-spin' : ''}`} />
          Scan again
        </Button>
      </div>
      {items === null ? (
        <p className="py-6 text-center text-sm text-muted-foreground">Scanning…</p>
      ) : detected.length || other.length ? (
        <ul className="mt-2 max-h-[45vh] space-y-1 overflow-y-auto">
          {[...detected, ...(showAll ? other : [])].map((s) => {
            const parts = s.path.split('/').filter(Boolean)
            const leaf = parts.pop() ?? s.path
            // "app", "web", "frontend"… say nothing alone: name them after their parent folder.
            const name = /^(app|web|web-app|webapp|frontend|backend|server|client|ui|site|www|api)$/i.test(leaf) && parts.length
              ? `${parts.pop()}/${leaf}`
              : leaf
            return (
              <li key={s.path} className="flex items-center gap-3 rounded-lg px-2 py-2 hover:bg-muted/60">
                <Tile name={name} className="h-8 w-8" />
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium">{name}</p>
                  <p className="truncate font-mono text-xs text-muted-foreground">{shortPath(s.path)}</p>
                </div>
                {s.framework && s.framework !== 'unknown' ? (
                  <span className="rounded-md bg-muted px-2 py-0.5 text-xs font-medium text-muted-foreground">{s.framework}</span>
                ) : null}
                <Button
                  size="sm"
                  busy={busy === s.path}
                  onClick={() => add(s.path, s.proposed_id ? { id: s.proposed_id } : undefined)}
                >
                  {busy === s.path ? null : <FolderPlus className="h-4 w-4" />}
                  Add
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 text-muted-foreground hover:text-danger"
                  title="Dismiss suggestion"
                  aria-label={`Dismiss ${name}`}
                  busy={busy === `dismiss:${s.path}`}
                  onClick={() => dismiss(s.path)}
                >
                  {busy === `dismiss:${s.path}` ? null : <X className="h-4 w-4" />}
                </Button>
              </li>
            )
          })}
          {other.length && !showAll ? (
            <li>
              <button className="w-full rounded-lg px-2 py-2 text-left text-sm text-muted-foreground hover:bg-muted/60 hover:text-foreground" onClick={() => setShowAll(true)}>
                Show {other.length} more folders where Lantern couldn't tell how to run them
              </button>
            </li>
          ) : null}
        </ul>
      ) : (
        <p className="py-6 text-center text-sm text-muted-foreground">Nothing new found. Add a folder by path above.</p>
      )}
    </Modal>
  )
}
