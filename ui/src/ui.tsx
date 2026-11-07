import { forwardRef, useEffect, type ButtonHTMLAttributes, type ReactNode } from 'react'
import { Loader2, X } from 'lucide-react'

export function cn(...parts: Array<string | false | null | undefined>): string {
  return parts.filter(Boolean).join(' ')
}

const variants = {
  primary: 'bg-primary text-primary-foreground hover:brightness-110 shadow-sm',
  secondary: 'border border-border bg-card text-foreground hover:bg-muted',
  ghost: 'text-muted-foreground hover:bg-muted hover:text-foreground',
  danger: 'text-danger hover:bg-danger/10',
} as const
const sizes = {
  md: 'h-9 px-3.5 text-sm gap-2',
  sm: 'h-8 px-3 text-[13px] gap-1.5',
  icon: 'h-8 w-8',
} as const

export interface ButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  variant?: keyof typeof variants
  size?: keyof typeof sizes
  busy?: boolean
}

export const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant = 'secondary', size = 'md', busy, children, disabled, ...props }, ref) => (
    <button
      ref={ref}
      disabled={disabled || busy}
      className={cn(
        'inline-flex shrink-0 select-none items-center justify-center rounded-lg font-medium transition',
        'focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-primary',
        'disabled:pointer-events-none disabled:opacity-50',
        variants[variant],
        sizes[size],
        className,
      )}
      {...props}
    >
      {busy ? <Loader2 className="h-4 w-4 animate-spin" /> : null}
      {children}
    </button>
  ),
)
Button.displayName = 'Button'

export type Tone = 'success' | 'warning' | 'danger' | 'info' | 'muted'

const toneDot: Record<Tone, string> = {
  success: 'bg-success',
  warning: 'bg-warning',
  danger: 'bg-danger',
  info: 'bg-info',
  muted: 'bg-muted-foreground/40',
}
const toneText: Record<Tone, string> = {
  success: 'text-success',
  warning: 'text-warning',
  danger: 'text-danger',
  info: 'text-info',
  muted: 'text-muted-foreground',
}

export function StatusBadge({ tone, label, pulse }: { tone: Tone; label: string; pulse?: boolean }) {
  return (
    <span className={cn('inline-flex items-center gap-1.5 text-xs font-medium', toneText[tone])}>
      <span className="relative flex h-2 w-2">
        {pulse ? (
          <span className={cn('absolute inline-flex h-full w-full animate-ping rounded-full opacity-60', toneDot[tone])} />
        ) : null}
        <span className={cn('relative inline-flex h-2 w-2 rounded-full', toneDot[tone])} />
      </span>
      {label}
    </span>
  )
}

export function Section({
  title,
  count,
  action,
  children,
}: {
  title: string
  count?: ReactNode
  action?: ReactNode
  children: ReactNode
}) {
  return (
    <section className="mt-10 first:mt-0">
      <div className="mb-3 flex items-center justify-between gap-3">
        <h2 className="flex items-baseline gap-2 text-[15px] font-semibold tracking-tight">
          {title}
          {count !== undefined ? <span className="text-sm font-normal text-muted-foreground">{count}</span> : null}
        </h2>
        {action}
      </div>
      {children}
    </section>
  )
}

export function Modal({
  title,
  onClose,
  children,
  wide,
}: {
  title: string
  onClose: () => void
  children: ReactNode
  wide?: boolean
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && onClose()
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  return (
    <div className="fixed inset-0 z-40 flex items-start justify-center overflow-y-auto bg-black/40 p-4 pt-[10vh] backdrop-blur-[2px]" onMouseDown={onClose}>
      <div
        className={cn('w-full rounded-xl border border-border bg-card shadow-2xl', wide ? 'max-w-2xl' : 'max-w-lg')}
        onMouseDown={(e) => e.stopPropagation()}
        role="dialog"
        aria-label={title}
      >
        <div className="flex items-center justify-between border-b border-border px-5 py-3.5">
          <h3 className="font-semibold">{title}</h3>
          <Button variant="ghost" size="icon" onClick={onClose} aria-label="Close">
            <X className="h-4 w-4" />
          </Button>
        </div>
        <div className="p-5">{children}</div>
      </div>
    </div>
  )
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string
  hint?: string
  children: ReactNode
}) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-sm font-medium">{label}</span>
      {children}
      {hint ? <span className="mt-1 block text-xs text-muted-foreground">{hint}</span> : null}
    </label>
  )
}

export const inputClass =
  'h-9 w-full rounded-lg border border-border bg-background px-3 text-sm outline-none transition placeholder:text-muted-foreground/70 focus:border-primary focus:ring-2 focus:ring-primary/20'

/** A stable soft color per name, for the letter tiles. */
export function hue(name: string): number {
  let h = 0
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) % 360
  return h
}

export function Tile({ name, className }: { name: string; className?: string }) {
  const h = hue(name)
  return (
    <span
      className={cn('inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg text-sm font-semibold', className)}
      style={{ background: `hsl(${h} 70% 50% / 0.14)`, color: `hsl(${h} 65% 45%)` }}
      aria-hidden
    >
      {name.trim().charAt(0).toUpperCase() || '?'}
    </span>
  )
}

export function shortPath(path: string | null | undefined): string {
  if (!path) return ''
  return path.replace(/^\/home\/[^/]+/, '~').replace(/^\/hq\/projects\/dev/, '~/dev').replace(/\/$/, '')
}
