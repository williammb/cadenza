app-name = Cadenza
tray-tooltip = Cadenza — agent task board
tray-open = Open
tray-settings = Settings…
tray-lang-pt = Language: Português
tray-lang-en = Language: English
tray-restart = Restart
tray-revoke-token = Revoke CLI token
tray-copy-diag = Copy diagnostics
tray-quit = Quit

notification-proposal-title = Cadenza — new agent proposal
notification-proposal-body = { $task_title }: { $proposal_title }
notification-action-accept = Accept
notification-action-reject = Reject
notification-action-open = Open window

# Prompt injected into the terminal when the agent is started from a
# task. The agent reads this first message as user input, so it must
# mention the `cadenza` skill (auto-discovered by Claude Code via its
# description) and the task id.
agent-initial-prompt = Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Your task is { $task_id } ({ $titulo }). Start by running `cadenza-cli current --json`.
agent-initial-prompt-ideia = Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Break the ideia { $ideia_id } down into actionable tasks. Use `cadenza-cli read-ideia { $ideia_id }` to read the full content.
# Prompt injected when the agent is started in PLAN mode: it must NOT
# implement anything, only interview the human and persist the refined
# plan via `cadenza-cli plan`. The task is still `a_fazer`, so `current`
# won't return it — the agent reads it with `list --json`.
agent-planning-prompt = Use the `cadenza` skill to coordinate with Cadenza. You are in PLANNING mode for task { $task_id } ({ $titulo }) — do NOT write or run any code yet. Read the task with `cadenza-cli list --json` and find { $task_id }. Ask me clarifying questions, in batches, until the approach, scope, and acceptance criteria are clear. When we agree, save the refined plan by piping the markdown into stdin: `cadenza-cli plan { $task_id }` (omit `--body` so the plan is read from stdin, avoiding shell quoting issues). Do not mark anything done and do not start the implementation — I will start a separate execution run.
