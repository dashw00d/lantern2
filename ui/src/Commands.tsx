import { useState } from 'react'
import { ExternalLink, Pencil, Play, Plus, ScrollText, Square, SquareTerminal } from 'lucide-react'
import { api, type Launcher, type LauncherInput } from './api'
import { Button, Field, Modal, Section, StatusBadge, inputClass, shortPath, type Tone } from './ui'

function launcherStatus(l: Launcher): { tone: Tone; label: string; pulse?: boolean } {
  switch (l.status) {
    case 'running':
      return { tone: 'success', label: 'Running', pulse: true }
    case 'starting':
      return { tone: 'warning', label: 'Starting…' }
    case 'stopping':
      return { tone: 'warning', label: 'Stopping…' }
    case 'failed':
      return { tone: 'danger', label: l.exit_code != null ? `Exited ${l.exit_code}` : 'Failed' }
    default:
      return { tone: 'muted', label: 'Not running' }
  }
}

export function Commands({
  items,
  filter,
  refresh,
  onLogs,
  onError,
}: {
  items: Launcher[]
  filter: string
  refresh: () => void
  onLogs: (l: Launcher) => void
  onError: (e: unknown) => void
}) {
  const [editing, setEditing] = useState<Launcher | 'new' | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const shown = items.filter((l) => !filter || `${l.name} ${l.cmd}`.toLowerCase().includes(filter))

  const run = async (l: Launcher, fn: (id: string) => Promise<unknown>) => {
    setBusy(l.id)
    try {
      await fn(l.id)
    } catch (e) {
      onError(e)
    } finally {
      setBusy(null)
      refresh()
    }
  }

  return (
    <Section
      title="Commands"
      count={items.length || undefined}
      action={
        <Button size="sm" onClick={() => setEditing('new')}>
          <Plus className="h-4 w-4" />
          New command
        </Button>
      }
    >
      {shown.length ? (
        <ul className="divide-y divide-border overflow-hidden rounded-xl border border-border bg-card shadow-card">
          {shown.map((l) => {
            const s = launcherStatus(l)
            const live = l.status === 'running' || l.status === 'starting'
            return (
              <li key={l.id} className="flex items-center gap-3 px-4 py-3">
                <SquareTerminal className="h-5 w-5 shrink-0 text-muted-foreground" />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-3">
                    <span className="truncate font-medium">{l.name}</span>
                    <StatusBadge tone={s.tone} label={s.label} pulse={s.pulse} />
                  </div>
                  <p className="mt-0.5 truncate font-mono text-xs text-muted-foreground" title={l.cmd}>
                    {l.cwd ? <span className="text-muted-foreground/70">{shortPath(l.cwd)} $ </span> : '$ '}
                    {l.cmd}
                  </p>
                </div>
                <div className="flex items-center gap-1">
                  {live && l.url ? (
                    <a
                      href={l.url}
                      target="_blank"
                      rel="noreferrer"
                      className="inline-flex h-8 items-center gap-1.5 rounded-lg bg-primary px-3 text-[13px] font-medium text-primary-foreground hover:brightness-110"
                    >
                      <ExternalLink className="h-4 w-4" />
                      Open
                    </a>
                  ) : null}
                  {live ? (
                    <Button size="sm" variant="ghost" busy={busy === l.id} onClick={() => run(l, api.stopLauncher)}>
                      {busy === l.id ? null : <Square className="h-4 w-4" />}
                      Stop
                    </Button>
                  ) : (
                    <Button size="sm" variant="primary" busy={busy === l.id} onClick={() => run(l, api.runLauncher)}>
                      {busy === l.id ? null : <Play className="h-4 w-4" />}
                      Run
                    </Button>
                  )}
                  <Button size="icon" variant="ghost" title="Output" aria-label={`Output of ${l.name}`} onClick={() => onLogs(l)}>
                    <ScrollText className="h-4 w-4" />
                  </Button>
                  <Button size="icon" variant="ghost" title="Edit" aria-label={`Edit ${l.name}`} onClick={() => setEditing(l)}>
                    <Pencil className="h-4 w-4" />
                  </Button>
                </div>
              </li>
            )
          })}
        </ul>
      ) : (
        <button
          onClick={() => setEditing('new')}
          className="flex w-full flex-col items-center gap-1 rounded-xl border border-dashed border-border px-4 py-8 text-sm text-muted-foreground transition hover:border-primary/50 hover:text-foreground"
        >
          <SquareTerminal className="mb-1 h-6 w-6" />
          <span className="font-medium text-foreground">{items.length ? 'No commands match' : 'Save a command to run it with one click'}</span>
          <span>ComfyUI, Blender, a script — anything you'd type in a terminal.</span>
        </button>
      )}
      {editing ? (
        <CommandForm
          initial={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null)
            refresh()
          }}
          onError={onError}
        />
      ) : null}
    </Section>
  )
}

function CommandForm({
  initial,
  onClose,
  onSaved,
  onError,
}: {
  initial: Launcher | null
  onClose: () => void
  onSaved: () => void
  onError: (e: unknown) => void
}) {
  const [form, setForm] = useState<LauncherInput>({
    name: initial?.name ?? '',
    cmd: initial?.cmd ?? '',
    cwd: initial?.cwd ?? '',
    url: initial?.url ?? '',
  })
  const [saving, setSaving] = useState(false)
  const set = (k: keyof LauncherInput) => (e: React.ChangeEvent<HTMLInputElement>) => setForm({ ...form, [k]: e.target.value })

  const save = async (e: React.FormEvent) => {
    e.preventDefault()
    setSaving(true)
    try {
      if (initial) await api.updateLauncher(initial.id, form)
      else await api.createLauncher(form)
      onSaved()
    } catch (err) {
      onError(err)
    } finally {
      setSaving(false)
    }
  }
  const remove = async () => {
    if (!initial || !confirm(`Delete “${initial.name}”? It stops first if it's running.`)) return
    try {
      await api.deleteLauncher(initial.id)
      onSaved()
    } catch (err) {
      onError(err)
    }
  }

  return (
    <Modal title={initial ? `Edit ${initial.name}` : 'New command'} onClose={onClose}>
      <form onSubmit={save} className="space-y-4">
        <Field label="Name">
          <input className={inputClass} value={form.name} onChange={set('name')} placeholder="ComfyUI" autoFocus required />
        </Field>
        <Field label="Command" hint="Runs in a login shell, like a terminal would.">
          <input className={`${inputClass} font-mono`} value={form.cmd} onChange={set('cmd')} placeholder="python main.py --listen 127.0.0.1" required />
        </Field>
        <Field label="Folder" hint="Where it runs. Leave empty for your home folder.">
          <input className={`${inputClass} font-mono`} value={form.cwd} onChange={set('cwd')} placeholder="~/tools/ComfyUI" />
        </Field>
        <Field label="Web page (optional)" hint="If it serves a page, an Open button appears while it runs.">
          <input className={`${inputClass} font-mono`} value={form.url} onChange={set('url')} placeholder="http://127.0.0.1:8188" />
        </Field>
        <div className="flex items-center gap-2 pt-2">
          {initial ? (
            <Button type="button" variant="danger" onClick={remove}>
              Delete
            </Button>
          ) : null}
          <div className="flex-1" />
          <Button type="button" variant="ghost" onClick={onClose}>
            Cancel
          </Button>
          <Button type="submit" variant="primary" busy={saving}>
            {initial ? 'Save' : 'Add command'}
          </Button>
        </div>
      </form>
    </Modal>
  )
}
