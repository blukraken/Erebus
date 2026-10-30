import type { EngineInterface, Register } from 'claude-code'

const SPOOL_SUBDIR = '\\Erebus\\spool\\'
const DENIED_TEXT = /doesn't want to proceed|was rejected|has been denied|permission.{0,40}denied/i

const SUMMARY_MAX = 200

let seq = 0

// What a call asks to do, in one short line: the command, the file or the host.
export function summary(tool: string, input: unknown): string {
  const i = (input ?? {}) as Record<string, unknown>
  const str = (k: string) => (typeof i[k] === 'string' ? (i[k] as string) : undefined)
  let text: string | undefined
  if (tool === 'Bash' || tool === 'PowerShell') text = str('command')
  else if (tool === 'Read' || tool === 'Edit' || tool === 'Write' || tool === 'MultiEdit') text = str('file_path')
  else if (tool === 'NotebookEdit') text = str('notebook_path') ?? str('file_path')
  else if (tool === 'WebFetch') {
    const url = str('url')
    const m = url ? /^[a-z]+:\/\/([^/?#]+)/i.exec(url) : null
    text = m ? m[1] : url
  }
  const line = (text ?? tool).replace(/\r?\n/g, ' ⏎ ').trim()
  return line.length > SUMMARY_MAX ? line.slice(0, SUMMARY_MAX - 1) + '…' : line
}

// Spool folder: %LOCALAPPDATA%\Erebus\spool\, else %USERPROFILE%\AppData\Local.
async function spoolDir($: EngineInterface): Promise<string | undefined> {
  const local = await $.env.get('LOCALAPPDATA')
  if (local) return local + SPOOL_SUBDIR

  const profile = await $.env.get('USERPROFILE')
  return profile ? profile + '\\AppData\\Local' + SPOOL_SUBDIR : undefined
}

// Writes one record as <unix_ms>-<seq>.json. Never throws.
async function emit($: EngineInterface, record: Record<string, unknown>): Promise<boolean> {
  try {
    const dir = await spoolDir($)
    if (dir === undefined) return false

    const unixMs = await $.clock.now()
    const session = await $.session.id()
    seq += 1
    await $.fs.write(
      `${dir}${unixMs}-${seq}.json`,
      JSON.stringify({ v: 1, unix_ms: unixMs, session, ...record }),
    )
    return true
  } catch {
    return false
  }
}

// A call's identity for matching a permission dialog back to its check.
const callKey = (tool: string, input: unknown, agent: string | undefined) =>
  `${agent ?? ''}|${tool}|${JSON.stringify(input ?? null)}`

export const register: Register = on => {
  // Asks a check raised, by call identity, until a dialog shows for one or the call runs.
  const candidates = new Map<string, { id: string; tool: string; input: unknown; agent: string | null }>()
  // tool_use_ids whose ask record is written and whose done record is not.
  const pending = new Set<string>()

  // An ask here may still be settled without a person (the auto-mode classifier), so it is
  // only noted. The ask record is written once a dialog is actually shown.
  on('tool.check', async ($, e, next) => {
    const r = await next(e)

    if (r.decision === 'ask' && e.approver === 'person' && e.tool_use_id !== undefined) {
      candidates.set(callKey(e.tool, e.input, e.agentId), { id: e.tool_use_id, tool: e.tool, input: e.input, agent: e.agentId ?? null })
    }

    return r
  }).catch(($, e, next) => next(e))

  // Fires when a permission dialog is put to a person.
  on('classic.PermissionRequest', async ($, e, next) => {
    const key = callKey(e.tool_name, e.tool_input, e.agent_id)
    const c = candidates.get(key)
    if (c !== undefined && !pending.has(c.id)) {
      candidates.delete(key)
      const ok = await emit($, { kind: 'ask', dialog: true, tool_use_id: c.id, tool: c.tool, summary: summary(c.tool, c.input), agent: c.agent })
      if (ok) pending.add(c.id)
    }
    return next(e)
  }).catch(($, e, next) => next(e))

  on('tool.call', async ($, e, next) => {
    const r = await next(e)
    const id = e.tool_use_id
    if (id !== undefined) {
      for (const [k, c] of candidates) {
        if (c.id === id) candidates.delete(k)
      }
    }

    if (id !== undefined && pending.has(id)) {
      pending.delete(id)
      const isDenied =
        r.deny !== undefined || (r.isError === true && DENIED_TEXT.test(r.text ?? ''))
      await emit($, { kind: 'done', tool_use_id: id, outcome: isDenied ? 'denied' : 'ran' })
    }

    return r
  }).catch(($, e, next) => next(e))
}
