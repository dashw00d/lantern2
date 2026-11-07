import type { Instance } from './api'
import type { Tone } from './ui'

export interface StatusMeta {
  tone: Tone
  label: string
  live: boolean
  busy: boolean
  pulse?: boolean
}

export function instanceStatus(inst: Instance | undefined): StatusMeta {
  switch (inst?.runtime.status) {
    case 'running':
      return { tone: 'success', label: 'Running', live: true, busy: false, pulse: true }
    case 'unhealthy':
      return { tone: 'warning', label: 'Not responding', live: true, busy: false }
    case 'starting':
      return { tone: 'warning', label: 'Starting…', live: false, busy: true }
    case 'preparing':
      return { tone: 'warning', label: 'Setting up…', live: false, busy: true }
    case 'stopping':
      return { tone: 'warning', label: 'Stopping…', live: false, busy: true }
    case 'crashed':
      return { tone: 'danger', label: 'Crashed', live: false, busy: false }
    case 'needs_config':
      return { tone: 'danger', label: 'Needs setup', live: false, busy: false }
    case 'external':
      return { tone: 'info', label: 'Running outside Lantern', live: true, busy: false }
    default:
      return { tone: 'muted', label: 'Stopped', live: false, busy: false }
  }
}

/** Why an instance is not running, in one line, if Lantern knows. */
export function problem(inst: Instance | undefined): string | null {
  if (!inst) return null
  const s = inst.runtime.status
  if (s !== 'crashed' && s !== 'needs_config' && s !== 'unhealthy') return null
  const issue = inst.runtime.issue
  const detail = typeof issue === 'string' ? issue : (issue as { detail?: string } | null)?.detail
  return inst.runtime.message ?? detail ?? (s === 'crashed' ? 'The process exited.' : null)
}
