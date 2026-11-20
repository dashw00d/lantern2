import { useState } from 'react'
import { Check, CheckCircle2, Copy, MinusCircle, RefreshCw, ShieldAlert, TriangleAlert } from 'lucide-react'
import { isNamedHttpsReady, type HealthCheck, type SystemHealth, type SystemInfo } from './api'
import { Button, Modal, cn } from './ui'

function CheckIcon({ status }: { status: HealthCheck['status'] }) {
  switch (status) {
    case 'ok':
      return <CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-success" />
    case 'warn':
      return <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0 text-warning" />
    case 'fail':
      return <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0 text-danger" />
    default:
      return <MinusCircle className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
  }
}

export function HealthModal({
  info,
  health,
  onRefresh,
  onClose,
}: {
  info: SystemInfo | undefined
  health: SystemHealth | undefined
  onRefresh: () => void
  onClose: () => void
}) {
  const [copied, setCopied] = useState<string | null>(null)
  const [showPassing, setShowPassing] = useState(false)
  const httpsReady = isNamedHttpsReady(health)
  const tld = info?.tld ?? 'glow'

  const checks = health?.checks ?? []
  const issues = checks.filter((c) => c.status === 'fail' || c.status === 'warn')
  const passing = checks.filter((c) => c.status === 'ok' || c.status === 'skipped')

  const copy = (cmd: string) => {
    navigator.clipboard?.writeText(cmd)
    setCopied(cmd)
    setTimeout(() => setCopied((c) => (c === cmd ? null : c)), 1800)
  }

  return (
    <Modal title={`System & *.${tld} HTTPS`} onClose={onClose} wide>
      <div
        className={cn(
          'flex items-center justify-between gap-3 rounded-xl border px-4 py-3 text-sm',
          httpsReady
            ? 'border-success/30 bg-success/8 text-success'
            : 'border-warning/35 bg-warning/8 text-foreground',
        )}
      >
        <div>
          <p className="font-medium">
            {httpsReady
              ? `Named HTTPS (*.${tld}) is active — Open buttons use https://<tool>.${tld}`
              : `Named HTTPS (*.${tld}) isn't ready yet — Open buttons fall back to 127.0.0.1:<port>`}
          </p>
          {info ? (
            <p className="mt-0.5 font-mono text-xs text-muted-foreground">
              Lantern {info.version} · {info.listen} · TLD .{info.tld}
            </p>
          ) : null}
        </div>
        <Button size="sm" variant="secondary" onClick={onRefresh}>
          <RefreshCw className="h-3.5 w-3.5" />
          Recheck
        </Button>
      </div>

      {!health ? (
        <p className="py-8 text-center text-sm text-muted-foreground">Running system checks…</p>
      ) : (
        <div className="mt-4 max-h-[55vh] space-y-4 overflow-y-auto pr-1">
          {issues.length ? (
            <div className="space-y-2">
              <h4 className="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
                Needs attention ({issues.length})
              </h4>
              <ul className="space-y-2">
                {issues.map((c) => (
                  <li key={c.id} className="rounded-xl border border-border bg-muted/30 px-3.5 py-2.5 text-sm">
                    <div className="flex items-start gap-2.5">
                      <CheckIcon status={c.status} />
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="font-medium">{c.title}</span>
                          <span className="font-mono text-[11px] text-muted-foreground">{c.id}</span>
                        </div>
                        <p className="mt-0.5 text-xs text-muted-foreground">{c.detail}</p>
                        {c.fix?.command ? (
                          <div className="mt-2 flex items-center gap-2">
                            <code className="flex-1 truncate rounded-md border border-border bg-background px-2.5 py-1 font-mono text-xs">
                              {c.fix.command}
                            </code>
                            <Button
                              size="sm"
                              variant="ghost"
                              className="h-7 px-2 text-xs"
                              onClick={() => copy(c.fix!.command)}
                            >
                              {copied === c.fix.command ? (
                                <>
                                  <Check className="h-3.5 w-3.5 text-success" />
                                  Copied
                                </>
                              ) : (
                                <>
                                  <Copy className="h-3.5 w-3.5" />
                                  Copy
                                </>
                              )}
                            </Button>
                          </div>
                        ) : null}
                      </div>
                    </div>
                  </li>
                ))}
              </ul>
            </div>
          ) : (
            <p className="text-sm text-success">All system checks are passing.</p>
          )}

          {passing.length ? (
            <div>
              <button
                className="text-xs font-medium text-muted-foreground hover:text-foreground"
                onClick={() => setShowPassing((s) => !s)}
              >
                {showPassing ? 'Hide' : 'Show'} {passing.length} passing / skipped checks
              </button>
              {showPassing ? (
                <ul className="mt-2 divide-y divide-border rounded-xl border border-border">
                  {passing.map((c) => (
                    <li key={c.id} className="flex items-start gap-2.5 px-3 py-2 text-xs">
                      <CheckIcon status={c.status} />
                      <div className="min-w-0 flex-1">
                        <span className="font-medium">{c.title}</span>
                        <span className="ml-2 text-muted-foreground">{c.detail}</span>
                      </div>
                    </li>
                  ))}
                </ul>
              ) : null}
            </div>
          ) : null}
        </div>
      )}
    </Modal>
  )
}
