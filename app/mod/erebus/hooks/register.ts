import type { EngineInterface, Register } from 'claude-code'

const SPOOL_SUBDIR = '\\Erebus\\spool\\'
const DENIED_TEXT = /doesn't want to proceed|was rejected|has been denied|permission.{0,40}denied/i

let seq = 0

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

export const register: Register = on => {
  // tool_use_ids whose ask record is written and whose done record is not.
  const pending = new Set<string>()

  on('tool.check', async ($, e, next) => {
    const r = await next(e)

    if (r.decision === 'ask' && e.approver === 'person' && e.tool_use_id !== undefined) {
      const ok = await emit($, {
        kind: 'ask',
        tool_use_id: e.tool_use_id,
        tool: e.tool,
        agent: e.agentId ?? null,
      })
      if (ok) pending.add(e.tool_use_id)
    }

    return r
  }).catch(($, e, next) => next(e))

  on('tool.call', async ($, e, next) => {
    const r = await next(e)
    const id = e.tool_use_id

    if (id !== undefined && pending.has(id)) {
      pending.delete(id)
      const isDenied =
        r.deny !== undefined || (r.isError === true && DENIED_TEXT.test(r.text ?? ''))
      await emit($, { kind: 'done', tool_use_id: id, outcome: isDenied ? 'denied' : 'ran' })
    }

    return r
  }).catch(($, e, next) => next(e))
}
