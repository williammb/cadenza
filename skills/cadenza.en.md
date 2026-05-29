# Cadenza — How to use

You have access to the `cadenza-cli` CLI to manage tasks. It talks to the
Cadenza desktop app over a local socket; the app **must be running**.

## Required flow

1. **At session start:** `cadenza-cli current --json` — read the active task.
2. **While working:** `cadenza-cli log <id> "<progress>"` — report often,
   at minimum on every meaningful decision or code block touched.
3. **When you hit a derived problem** (parallel bug, blocking refactor,
   new scope): `cadenza-cli propose ...` — this command **blocks** and
   waits for a human decision. Do not invent your own fix.
4. **When done:** `cadenza-cli done <id> "<summary>"` — you **never** move a
   task to "done" yourself; this requests it from the human.

## Planning a task (plan mode)

When you are started in **plan mode**, you must NOT implement anything. The
task is still in `a_fazer`, so `cadenza-cli current` will not return it —
find it with `cadenza-cli list --json`.

1. Read the task's brief description.
2. Interview the human in the terminal: ask clarifying questions about
   scope, edge cases, and acceptance criteria — one focused batch at a time.
3. When you and the human agree, save the refined plan:

   ```bash
   cadenza-cli plan T-42 --body "## Goal
   ...
   ## Steps
   1. ...
   ## Acceptance
   - ..."
   ```

   By default the plan is appended as a `## Plano` section, preserving the
   original description. Pass `--replace` to overwrite the whole body, or
   omit `--body` to pipe the plan from stdin.
4. Do **not** call `done` and do **not** start coding. The human starts a
   separate execution run that will read your saved plan.

## Rules

- You only work on tasks with `estado: fazendo`. If `cadenza-cli current`
  returns `null`, stop and ask the human to start a task (unless you are in
  plan mode — see above).
- Always use `--json` when parsing output. `estado` values stay in PT
  canonical (`a_fazer`, `fazendo`, `aguardando_revisao`, `feito`) — they
  do **not** change with `--lang`.
- After `propose`, check the exit code:
  - `0` → accepted (output includes the new `task_id`)
  - `20` → rejected — stop and report to the human
  - `21` → timeout — stop, report that no decision was made
- If you see exit code `10` ("app not running"), ask the human to open
  the Cadenza app.
- If you see exit code `11` ("invalid token"), ask the human to use
  "Revoke CLI token" in the tray menu and try again.

## Quick examples

```bash
# Read the active task as JSON
cadenza-cli current --json

# Report progress
cadenza-cli log T-42 "validator wired up, next is the test"

# Propose a derived task (blocking)
cadenza-cli propose \
  --parent T-42 \
  --title "Validate input on another endpoint" \
  --repro "POST /api/foo with an invalid body returns 500 instead of 400" \
  --file "src/handlers/foo.rs" \
  --what-failed "missing input validation" \
  --action "wrap with the same Validator pipeline used in T-42"

# Request completion (human decides whether it really goes to "done")
cadenza-cli done T-42 "endpoint validated and covered by two new tests"
```

## Decomposing an idea (Inbox)

If the env var `CADENZA_IDEIA_ID` is set when you start, the human wants
you to break an Inbox idea into concrete tasks. The idea body is in
`CADENZA_IDEIA_BODY` (also readable via `cadenza-cli read-ideia $CADENZA_IDEIA_ID`).

For each concrete task you would create from that idea, call:

```bash
cadenza-cli new-task --titulo "..." --body "..."
```

The `--project` and `--from-ideia` flags are picked up automatically from
`$CADENZA_PROJECT_ID` and `$CADENZA_IDEIA_ID`. Each invocation prints the
newly created `task_id` on stdout. After your final task, the originating
idea is automatically marked `destrinchada`.

Aim for 3–8 actionable tasks per idea: each should be small enough to be
self-contained but big enough to deserve its own card. Don't paste the
entire idea body into a single task — the point is to slice it.
