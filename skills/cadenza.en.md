# Cadenza — How to use

You have access to the `cadenza-cli` CLI to manage tasks. It talks to the
Cadenza desktop app over a local socket; the app **must be running**.

## Know your task

When the app starts you for a task, it injects two environment variables
into your shell:

- `$TASKAI_TASK_ID` — the task you were started for (e.g. `T-42`).
- `$TASKAI_PROJECT_ID` — the project that task belongs to.

**Always identify your task from `$TASKAI_TASK_ID`** — there can be several
tasks in `fazendo` at once (one per running agent), so `current` is
ambiguous and may return someone else's card. Fetch *your* task by id:

```bash
cadenza-cli get "$TASKAI_TASK_ID" --json
```

`get` returns only that one task (or exits `30`, `task_not_found`, if the id
doesn't exist). Fall back to `cadenza-cli current --json` only when
`$TASKAI_TASK_ID` is unset (you were run outside the app's terminal).

## Required flow

1. **At session start:** `cadenza-cli get "$TASKAI_TASK_ID" --json` — read
   your task. Only work on it if its `estado` is `fazendo`.
2. **While working:** `cadenza-cli log "$TASKAI_TASK_ID" "<progress>"` —
   report often, at minimum on every meaningful decision or code block
   touched.
3. **When you hit a derived problem** (parallel bug, blocking refactor,
   new scope): `cadenza-cli propose ...` — this command **blocks** and
   waits for a human decision. Do not invent your own fix.
4. **When done:** gather validation evidence and call `cadenza-cli done`
   (see "Finishing with evidence" below) — you **never** move a task to
   "done" yourself; this requests it from the human.

## Planning a task (plan mode)

When you are started in **plan mode**, you must NOT implement anything. The
task stays in `a_fazer` (so `current` won't return it), but
`$TASKAI_TASK_ID` is still set — read it the same way:

```bash
cadenza-cli get "$TASKAI_TASK_ID" --json
```

1. Read the task's brief description from that output.
2. Interview the human in the terminal: ask clarifying questions about
   scope, edge cases, and acceptance criteria — one focused batch at a time.
3. When you and the human agree, save the refined plan:

   ```bash
   cadenza-cli plan "$TASKAI_TASK_ID" --body "## Goal
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

## Finishing with evidence

When you finish, "it's done" isn't enough — you assemble an **evidence
package** that Cadenza independently verifies and shows to the human
reviewer. The flow:

1. **Read the project's quality contract** — the checks you must run:

   ```bash
   cadenza-cli quality --json
   ```

   Returns `{ contract_version, checks: [{id, name, cmd, required,
   required_if_changed}] }`. `--task` and `--project` are resolved from
   the environment; pass them only if needed. An empty list means the
   project has no contract (proceed with no checks).

2. **Run each check** exactly as its `cmd` says, capturing the
   **command, the exit code, and a short log excerpt** (the relevant last
   lines — errors, test summary).

3. **Assemble an `evidence.json`** with the results, within the caps:

   ```json
   {
     "contract_version": "sha256:…",
     "checks": [{"id": "clippy", "exit": 0, "log_excerpt": "…(last lines)…"}],
     "groups": [{"label": "feat: review package", "files": ["src-tauri/src/review.rs"]}],
     "open_questions": ["…questions for the reviewer…"]
   }
   ```

   - Use the exact `contract_version` that `quality` returned.
   - Each check's `id` and `exit` are required; `log_excerpt` is the log
     snippet.
   - `groups` (optional) maps files → an intent label, so the diff is
     grouped by intent.
   - `open_questions` (optional) flags doubts to the reviewer.
   - **Caps:** ≤ 64 checks, ≤ 64 groups, `log_excerpt` ≤ 8 KiB per
     check, whole file ≤ 256 KiB. Exceeding any = exit 2, nothing changes.

4. **Request completion, attaching the evidence:**

   ```bash
   cadenza-cli done "$TASKAI_TASK_ID" \
     --summary "endpoint validated; clippy and tests passing" \
     --evidence evidence.json
   ```

   The idempotency key is auto-generated (or pass `--idempotency-key
   <k>`); it is echoed to stderr, so re-running with the same key is a
   safe no-op. `--summary` is equivalent to the positional summary.
   Without `--evidence`, `done` still works (it becomes a `no_validation`
   package).

5. **`done` always succeeds** — it is not a gate. Cadenza independently
   derives the diff, the risks, and an **evidence state** from what you
   reported and shows them to the human reviewer, who decides.

## Rules

- You only work on tasks with `estado: fazendo`. If `get "$TASKAI_TASK_ID"`
  shows a different state (and you are not in plan mode), stop and ask the
  human.
- If `$TASKAI_TASK_ID` is unset, fall back to `cadenza-cli current --json`;
  if that returns `null`, stop and ask the human to start a task.
- Always use `--json` when parsing output. `estado` values stay in PT
  canonical (`a_fazer`, `fazendo`, `aguardando_revisao`, `feito`) — they
  do **not** change with `--lang`.
- After `propose`, check the exit code:
  - `0` → accepted (output includes the new `task_id`)
  - `20` → rejected — stop and report to the human
  - `21` → timeout — stop, report that no decision was made
- `get` exits `30` (`task_not_found`) if the id doesn't exist.
- If you see exit code `10` ("app not running"), ask the human to open
  the Cadenza app.
- If you see exit code `11` ("invalid token"), ask the human to use
  "Revoke CLI token" in the tray menu and try again.

## Quick examples

```bash
# Read your task as JSON (preferred over `current`)
cadenza-cli get "$TASKAI_TASK_ID" --json

# Report progress
cadenza-cli log "$TASKAI_TASK_ID" "validator wired up, next is the test"

# Discover project IDs (for new-task / create-ideia)
cadenza-cli projects --json

# Propose a derived task (blocking)
cadenza-cli propose \
  --parent "$TASKAI_TASK_ID" \
  --title "Validate input on another endpoint" \
  --repro "POST /api/foo with an invalid body returns 500 instead of 400" \
  --file "src/handlers/foo.rs" \
  --what-failed "missing input validation" \
  --action "wrap with the same Validator pipeline used in the parent task"

# Read the quality contract (checks to run before done)
cadenza-cli quality --json

# Request completion with evidence (human decides whether it goes to "done")
cadenza-cli done "$TASKAI_TASK_ID" \
  --summary "endpoint validated and covered by two new tests" \
  --evidence evidence.json
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
`$TASKAI_PROJECT_ID` and `$CADENZA_IDEIA_ID`. (If `$TASKAI_PROJECT_ID` isn't
set, pass `--project` explicitly — run `cadenza-cli projects` to find the id.)
Each invocation prints the newly created `task_id` on stdout. After your
final task, the originating idea is automatically marked `destrinchada`.

Aim for 3–8 actionable tasks per idea: each should be small enough to be
self-contained but big enough to deserve its own card. Don't paste the
entire idea body into a single task — the point is to slice it.

## Project memory

Each project has an **official memory**: a curated list of facts,
decisions and conventions that hold for that project. The **human is the
curator** — nothing you suggest enters memory until they approve it.

- **When a task starts**, the memory is already injected into your initial
  prompt. To re-read it at any time:

  ```bash
  cadenza-cli memory list --json
  ```

- **When you finish a task** (before `done`), if you learned something
  **genuinely reusable** for future tasks in this project — a convention,
  an architecture decision, a gotcha — propose it as a learning.
  Repeatable and **optional**; don't propose trivial learnings:

  ```bash
  cadenza-cli memory suggest "IPC handlers live in ipc.rs; business logic goes in the modules."
  ```

  The learning stays **pending** until the human promotes it in the task
  review. `--task` defaults to `$TASKAI_TASK_ID`; `--project` to
  `$TASKAI_PROJECT_ID`.

### Memory reevaluation mode

If the env var `CADENZA_MEMORY_REEVAL` is set when you start, the human
wants you to **reevaluate the project's current memory**. Read it with
`cadenza-cli memory list --json` and emit review suggestions — **without
changing anything directly**. Each suggestion stays pending until the
human approves it in the Memory tab.

```bash
# remove an obsolete item
cadenza-cli memory revise --op remover --target M-abc

# rewrite a confusing item
cadenza-cli memory revise --op reescrever --target M-abc --texto "Clearer text."

# merge duplicates (two or more --target)
cadenza-cli memory revise --op mesclar --target M-a --target M-b --texto "Consolidated text."

# propose a new item
cadenza-cli memory revise --op nova --texto "Newly observed convention."

# flag a contradiction (informational; the human resolves it by editing)
cadenza-cli memory revise --op contradicao --target M-a --target M-b --nota "One says X, the other Y."
```

After emitting suggestions, stop. The human curates.
