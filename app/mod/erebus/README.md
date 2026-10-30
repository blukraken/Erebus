# erebus

Tells the Erebus desktop app when a Claude Code permission prompt waits on you.
The mod never changes a decision or a tool result.

## Install

```
claude plugin marketplace add F:\Projects\Erebus\app\mod
claude plugin install erebus@erebus
```

## Spool

Each record is one file in `%LOCALAPPDATA%\Erebus\spool\`, named `<unix_ms>-<seq>.json`.

```json
{"v":1,"kind":"ask","unix_ms":0,"session":"<id>","tool_use_id":"...","tool":"Bash","summary":"cargo test","agent":null}
{"v":1,"kind":"done","unix_ms":0,"session":"<id>","tool_use_id":"...","outcome":"ran"}
```

- `ask` is written when a call needs a person's answer. `agent` is the subagent id or null.
- `dialog` is true: the ask was put to a person in a permission dialog. The core ignores asks without it.
- `summary` is what the call asks to do, on one line of at most 200 characters: the command for Bash and PowerShell, the file for the file tools, the host for WebFetch, else the tool name.
- `done` follows when that call finishes. `outcome` is `ran` or `denied`.
- `session` is the Claude Code session id.
