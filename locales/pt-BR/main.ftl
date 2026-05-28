app-name = Cadenza
tray-tooltip = Cadenza — agente de tarefas
tray-open = Abrir
tray-settings = Configurações…
tray-lang-pt = Idioma: Português
tray-lang-en = Idioma: English
tray-restart = Reiniciar
tray-revoke-token = Revogar token CLI
tray-copy-diag = Copiar diagnóstico
tray-quit = Sair

notification-proposal-title = Cadenza — nova proposta do agente
notification-proposal-body = { $task_title }: { $proposal_title }
notification-action-accept = Aceitar
notification-action-reject = Rejeitar
notification-action-open = Abrir janela

update-available-title = Atualização disponível
update-available-body = Uma nova versão do Cadenza está pronta. Reiniciar agora?

# Prompt injetado no terminal quando o agente é iniciado a partir de uma
# task. O agente lê esta primeira mensagem como entrada do usuário —
# por isso é importante que mencione a skill `cadenza` (auto-descoberta
# por Claude Code via descrição) e o id da task.
agent-initial-prompt = Use a skill `cadenza` para coordenar com o Cadenza pelo cadenza-cli. Sua task é { $task_id } ({ $titulo }). Comece executando `cadenza-cli current --json`.
agent-initial-prompt-ideia = Use a skill `cadenza` para coordenar com o Cadenza pelo cadenza-cli. Destrincha a ideia { $ideia_id } em tasks acionáveis. Use `cadenza-cli read-ideia { $ideia_id }` para ler o conteúdo completo.
