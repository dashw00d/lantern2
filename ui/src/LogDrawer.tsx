import { useEffect, useRef, useState } from 'react'
import { ArrowDown, Copy, X } from 'lucide-react'
import { Button, cn } from './ui'

export interface LogSource {
  title: string
  subtitle?: string
  load: () => Promise<Array<{ ts?: string; text: string; err?: boolean }>>
}

export function LogDrawer({ source, onClose }: { source: LogSource; onClose: () => void }) {
  const [lines, setLines] = useState<Array<{ ts?: string; text: string; err?: boolean }>>([])
  const [error, setError] = useState<string | null>(null)
  const [follow, setFollow] = useState(true)
  const box = useRef<HTMLDivElement>(null)

  useEffect(() => {
    let alive = true
    const tick = async () => {
      try {
        const next = await source.load()
        if (alive) {
          setLines(next)
          setError(null)
        }
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e))
      }
    }
    tick()
    const t = setInterval(tick, 1500)
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    return () => {
      alive = false
      clearInterval(t)
      window.removeEventListener('keydown', onKey)
    }
  }, [source, onClose])

  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight
  }, [lines, follow])

  return (
    <div className="fixed inset-0 z-30 flex justify-end bg-black/30" onMouseDown={onClose}>
      <aside
        className="flex h-full w-full max-w-3xl flex-col border-l border-border bg-card shadow-2xl"
        onMouseDown={(e) => e.stopPropagation()}
        aria-label={`Logs for ${source.title}`}
      >
        <header className="flex items-center gap-3 border-b border-border px-5 py-3.5">
          <div className="min-w-0 flex-1">
            <h3 className="truncate font-semibold">{source.title}</h3>
            {source.subtitle ? <p className="truncate font-mono text-xs text-muted-foreground">{source.subtitle}</p> : null}
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => navigator.clipboard.writeText(lines.map((l) => l.text).join('\n'))}
            title="Copy all lines"
          >
            <Copy className="h-4 w-4" />
            Copy
          </Button>
          <Button variant="ghost" size="icon" onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" />
          </Button>
        </header>
        <div
          ref={box}
          onScroll={(e) => {
            const el = e.currentTarget
            setFollow(el.scrollHeight - el.scrollTop - el.clientHeight < 24)
          }}
          className="relative flex-1 overflow-auto bg-background/60 px-5 py-3 font-mono text-[12.5px] leading-[1.6]"
        >
          {error ? <p className="text-danger">{error}</p> : null}
          {!error && !lines.length ? <p className="text-muted-foreground">No output yet.</p> : null}
          {lines.map((l, i) => (
            <div key={i} className={cn('whitespace-pre-wrap break-all', l.err && 'text-danger')}>
              {l.ts ? <span className="mr-3 select-none text-muted-foreground/60">{l.ts}</span> : null}
              {l.text}
            </div>
          ))}
        </div>
        {!follow ? (
          <div className="pointer-events-none absolute bottom-6 right-8">
            <Button className="pointer-events-auto shadow-lg" size="sm" onClick={() => setFollow(true)}>
              <ArrowDown className="h-4 w-4" />
              Latest
            </Button>
          </div>
        ) : null}
      </aside>
    </div>
  )
}
