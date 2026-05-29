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

# Prompt injetado no terminal quando o agente é iniciado a partir de uma
# task. O agente lê esta primeira mensagem como entrada do usuário —
# por isso é importante que mencione a skill `cadenza` (auto-descoberta
# por Claude Code via descrição) e o id da task.
agent-initial-prompt = Use a skill `cadenza` para coordenar com o Cadenza pelo cadenza-cli. Sua task é { $task_id } ({ $titulo }). Comece executando `cadenza-cli current --json`.
agent-initial-prompt-ideia = Use a skill `cadenza` para coordenar com o Cadenza pelo cadenza-cli. Destrincha a ideia { $ideia_id } em tasks acionáveis. Use `cadenza-cli read-ideia { $ideia_id }` para ler o conteúdo completo.
# Prompt injetado quando o agente é iniciado em modo PLANEJAMENTO: ele NÃO
# deve implementar nada, apenas entrevistar o humano e gravar o plano
# refinado via `cadenza-cli plan`. A task ainda está em `a_fazer`, então
# `current` não a retorna — o agente a lê com `list --json`.
agent-planning-prompt = Use a skill `cadenza` para coordenar com o Cadenza. Você está em modo PLANEJAMENTO da task { $task_id } ({ $titulo }) — NÃO escreva nem rode código ainda. Leia a task com `cadenza-cli list --json` e localize { $task_id }. Faça perguntas de esclarecimento, em lotes, até que a abordagem, o escopo e os critérios de aceite estejam claros. Quando combinarmos, salve o plano refinado enviando o markdown pela entrada padrão: `cadenza-cli plan { $task_id }` (omita `--body` para que o plano seja lido do stdin, evitando problemas de escape no shell). Não marque nada como concluído e não comece a implementação — eu inicio uma execução separada depois.
