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

update-available-title = Update available
update-available-body = A new version of Cadenza is ready. Restart now?

# Prompt injected into the terminal when the agent is started from a
# task. The agent reads this first message as user input, so it must
# mention the `cadenza` skill (auto-discovered by Claude Code via its
# description) and the task id.
agent-initial-prompt = Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Your task is { $task_id } ({ $titulo }). Start by running `cadenza-cli current --json`.
agent-initial-prompt-ideia = Use the `cadenza` skill to coordinate with Cadenza through cadenza-cli. Break the ideia { $ideia_id } down into actionable tasks. Use `cadenza-cli read-ideia { $ideia_id }` to read the full content.
