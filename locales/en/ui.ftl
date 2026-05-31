board-column-inbox = Inbox
board-column-todo = To Do
board-column-doing = Doing
board-column-review = Awaiting Review
board-column-done = Done
board-empty = (no tasks)
ideia-empty = (no ideas)
ideia-new-aria = New idea
ideia-destrinchar = Break into tasks
ideia-modal-title-new = New idea
ideia-modal-title-edit = Idea
ideia-field-titulo = Title
ideia-field-project = Project
ideia-field-body = Free-form notes
ideia-project-required = Pick a project.
confirm-delete-ideia = Delete this idea? This cannot be undone.
task-project-required = Pick a project for this task.

topbar-new-task = + New task
topbar-new-task-short = New task
topbar-settings = Settings
topbar-new-task-aria = Create a new task
topbar-settings-aria = Open settings
topbar-theme-aria = Toggle theme (light/dark)
topbar-project-all = All projects
topbar-project-aria = Filter tasks by active project

action-save = Save
action-cancel = Cancel
action-delete = Delete
action-add = Add
action-accept = Accept
action-reject = Reject
action-merge = Merge with current task
action-close = Close

confirm-delete-task = Delete this task? This cannot be undone.

settings-title = Settings
settings-tab-geral = General
settings-tab-agentes = Agents
settings-tab-projeto = Project
settings-section-language = Language
settings-section-projects = Projects
settings-section-agent = Default agent
settings-language-pt = Português (pt-BR)
settings-language-en = English (en)
settings-projects-empty = No projects registered.
settings-projects-delete-last-error = Cannot remove the only existing project.
settings-project-name = Name
settings-project-path = Path
settings-project-path-browse = Select folder…
settings-project-new = New project
settings-project-remove = Remove project
settings-project-new-name = New project
settings-project-id = ID
settings-project-agent = Agent (override)
settings-project-agent-inherit = (inherit global)
settings-agent-kind = Kind
settings-agent-claude = Claude Code
settings-agent-codex = Codex
settings-agent-antigravity = Antigravity
settings-agent-opencode = OpenCode
settings-agent-command = Command (optional, overrides PATH)
settings-project-default-branch = Default branch
settings-agent-not-installed = (not installed)
settings-agent-not-installed-tooltip = Couldn't find this agent. We looked for the CLI binary on PATH and for its config folder under your home directory. Install the CLI or run it at least once before using it here.
settings-saved = Settings saved.
settings-save-error = Save failed: { $error }

settings-section-storage = Storage
settings-storage-hint = Where tasks are persisted. Switching auto-migrates and requires a restart.
settings-storage-files = Files
settings-storage-files-hint = ~/.cadenza/tasks/*.md — compatible with task-ai (Node.js)
settings-storage-sqlite = SQLite
settings-storage-sqlite-hint = ~/.cadenza/cadenza.db — local DB, faster reads/writes
settings-storage-postgres = PostgreSQL
settings-storage-postgres-hint = Coming soon (Phase C) — Supabase/AWS/Azure, password in OS keyring
settings-storage-restart = Restart to apply the storage change.
settings-storage-restart-now = Restart now

settings-pg-host = Host
settings-pg-port = Port
settings-pg-database = Database
settings-pg-user = User
settings-pg-password = Password
settings-pg-password-hint = Stored in the OS keyring. Never written to config.json.
settings-pg-ssl = SSL mode
settings-pg-ssl-require = require (recommended)
settings-pg-ssl-prefer = prefer
settings-pg-ssl-disable = disable
settings-pg-test = Test connection
settings-pg-save = Save and migrate
settings-pg-clear = Clear password
settings-pg-testing = Connecting…
settings-pg-test-ok = Connection OK. You can save and migrate.
settings-pg-test-error = Connection failed: { $error }
settings-pg-saved = Settings saved. Restart to migrate the data.
settings-pg-cleared = Password removed from the keyring.
settings-pg-fields-required = Fill host, database, user, and password.
settings-pg-stale = Fields changed since the test. Re-test the connection.

settings-section-skills = CLI skills
settings-skills-hint = Installs a snippet that teaches the agent (Claude Code, Codex, Antigravity, OpenCode) how to use cadenza-cli. The snippet is written to the selected scope (current project or global).
settings-skills-hint-global = Installs the snippet globally (in your home), applying to every project.
settings-skills-hint-project = Installs the snippet into the project selected above.
settings-skills-agents = Agents
settings-skills-agent-claude = Claude Code
settings-skills-agent-codex = Codex
settings-skills-agent-antigravity = Antigravity
settings-skills-agent-opencode = OpenCode
settings-skills-scope = Scope
settings-skills-scope-project = Current project
settings-skills-scope-global = Global (user)
settings-skills-force = Overwrite if it already exists
settings-skills-install = Install
settings-skills-update = Update
settings-skills-remove = Remove
settings-skills-refresh = Refresh status
settings-skills-col-agent = Agent
settings-skills-col-scope = Scope
settings-skills-col-status = Status
settings-skills-col-path = Path
settings-skills-status-installed = Installed
settings-skills-status-installed-locale = Installed [{ $locale }]
settings-skills-status-not-installed = Not installed
settings-skills-status-outdated = update available
settings-skills-summary-installed = { $count } installed
settings-skills-summary-removed = { $count } removed
settings-skills-summary-skipped = { $count } skipped
settings-skills-no-agent = Select at least one agent.
settings-skills-running = Running…
settings-skills-error = Error: { $error }
settings-skills-project-label = Project
settings-skills-project-empty = No projects configured — add one in the Projects section above.
settings-skills-project-required = Pick a project before installing/removing at the "project" scope.
settings-section-models = Models
settings-models-hint = Discovers the models each agent offers. The probe takes a few seconds; the result is saved and reused when starting an agent.
settings-models-refresh = Refresh status
settings-models-col-agent = Agent
settings-models-col-count = Models
settings-models-col-current = Current
settings-models-load = Load
settings-models-loaded = { $count } model(s)
settings-models-none = Not loaded
settings-models-loading = Loading models…

task-modal-title-new = New task
task-modal-title-edit = Edit task
task-field-titulo = Title
task-field-project = Project
task-project-placeholder = — Select project —
task-field-estado = State
task-blockers-legend = Blocked by
task-blockers-empty = No task available
task-field-body = Description (markdown)
task-error = Error: { $error }

# Image attachments (paste / drag-drop / file button + markdown preview)
attachment-edit = Edit
attachment-preview = Preview
attachment-button = Attach image
attachment-error-unsupported-format = Unsupported image format. Use PNG, JPEG, GIF or WebP.
attachment-error-too-large = Image exceeds the maximum size of 5 MB.
attachment-error-save-failed = Could not save the image.

task-worktree-legend = Worktree / Branch
task-worktree-use = Use worktree
task-field-origin-branch = Origin branch
task-field-branch = Destination branch
task-field-worktree-path = Worktree path
task-worktree-defaults-error = Could not read the current branch: { $error }
task-worktree-error = Git error: { $error }

estado-a-fazer = To do
estado-fazendo = Doing
estado-aguardando-revisao = Awaiting review
estado-feito = Done

triage-modal-title = Derived task proposal
triage-empty = (no pending proposals)
triage-field-parent = Parent task
triage-field-title = Title
triage-field-file = File
triage-field-repro = How to reproduce
triage-field-what-failed = What failed
triage-field-action = Proposed action
triage-field-created = Received at
triage-pending-badge = { $count ->
    [one] 1 pending proposal
   *[other] { $count } pending proposals
}
triage-pending-tooltip = Open triage
triage-decided = Decision recorded.
triage-decided-error = Failed to record decision: { $error }
triage-load-error = Failed to load proposal: { $error }

terminal-title = Terminal
terminal-empty = (no active session)
terminal-toggle-aria = Expand or collapse the terminal
terminal-close-aria = End session and close the terminal
terminal-resize-aria = Drag to resize the terminal
terminal-attach-error = Failed to attach to terminal: { $error }

task-modal-start = Start
task-modal-start-aria = Start an agent for this task
card-start-aria = Start agent
card-start-resume-aria = Resume saved conversation
card-plan-aria = Plan task before running
card-blocked-title = Blocked
card-unblocked-title = Unblocked
task-blocker-missing = not found

start-agent-title = Start agent
start-agent-branch-label = Branch
start-agent-worktree-label = Worktree
start-agent-worktree-none = Project repository (no worktree)
start-agent-kind-label = Platform
start-agent-model-label = Model
start-agent-model-loading = Loading models…
start-agent-model-default = (agent default)
start-agent-model-saved = saved
start-agent-model-required = Pick a model.
start-agent-model-text-hint = No models loaded — load them in Settings → Models, or type an id.
start-agent-resume-banner = Resume saved conversation
start-agent-fresh = Start new
start-agent-fresh-confirm = Discard the saved conversation and start a new one?
start-agent-action-start = Start
start-agent-action-resume = Resume
start-agent-action-plan = Plan
start-agent-title-plan = Plan task
start-agent-launching = Starting agent…

# Non-blocking banner at the top of the window when the updater finds
# a new release. The same strings feed the OS notification fired by
# notify::show_info — `dump_namespace_strings("ui")` already covers
# notify because the Fluent bundle merges every .ftl in the locale.
update-available-title = Update available
update-available-body = A new version of Cadenza is ready.
update-restart-now = Restart now
skill-update-available-title = Skill update available
skill-update-available-body = A newer Cadenza agent skill is available. Reinstall it from Settings → Skills.

# Settings → General: manual update-check button. The app checks on its
# own at boot and every 24h; this button checks on demand and, unlike
# the silent check, gives feedback even when already up to date.
settings-section-updates = Updates
settings-update-check = Check for updates
settings-update-checking = Checking…
settings-update-uptodate = You're up to date.
settings-update-available = New version available: v{ $version }
settings-update-error = Update check failed: { $error }
