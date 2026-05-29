board-column-inbox = Inbox
board-column-todo = A Fazer
board-column-doing = Fazendo
board-column-review = Aguardando Revisão
board-column-done = Feito
board-empty = (sem tasks)
ideia-empty = (sem ideias)
ideia-new-aria = Nova ideia
ideia-destrinchar = Destrinchar em tasks
ideia-modal-title-new = Nova ideia
ideia-modal-title-edit = Ideia
ideia-field-titulo = Título
ideia-field-project = Projeto
ideia-field-body = Descrição livre
ideia-project-required = Selecione um projeto.
confirm-delete-ideia = Excluir esta ideia? Esta ação não pode ser desfeita.
task-project-required = Selecione um projeto para esta task.

topbar-new-task = + Nova task
topbar-new-task-short = Nova task
topbar-settings = Configurações
topbar-new-task-aria = Criar nova task
topbar-settings-aria = Abrir configurações
topbar-theme-aria = Alternar tema (claro/escuro)
topbar-project-all = Todos os projetos
topbar-project-aria = Filtrar tasks pelo projeto ativo

action-save = Salvar
action-cancel = Cancelar
action-delete = Excluir
action-add = Adicionar
action-accept = Aceitar
action-reject = Rejeitar
action-merge = Mesclar com task atual
action-close = Fechar

confirm-delete-task = Excluir esta task? Esta ação não pode ser desfeita.

settings-title = Configurações
settings-section-language = Idioma
settings-section-projects = Projetos
settings-section-agent = Agente padrão
settings-language-pt = Português (pt-BR)
settings-language-en = English (en)
settings-projects-empty = Nenhum projeto cadastrado.
settings-projects-delete-last-error = Não é possível remover o único projeto existente.
settings-project-name = Nome
settings-project-path = Caminho
settings-project-path-browse = Selecionar pasta…
settings-agent-kind = Tipo
settings-agent-claude = Claude Code
settings-agent-codex = Codex
settings-agent-command = Comando (opcional, sobrescreve o PATH)
settings-agent-not-installed = (não instalado)
settings-agent-not-installed-tooltip = Não encontramos esse agente. Procuramos a CLI no PATH e a pasta de configuração na sua home. Instale a CLI ou rode-a ao menos uma vez antes de usar aqui.
settings-saved = Configurações salvas.
settings-save-error = Erro ao salvar: { $error }

settings-section-storage = Armazenamento
settings-storage-hint = Onde as tasks ficam salvas. Trocar dispara migração automática e exige reiniciar o app.
settings-storage-files = Arquivos
settings-storage-files-hint = ~/.cadenza/tasks/*.md — compatível com task-ai (Node.js)
settings-storage-sqlite = SQLite
settings-storage-sqlite-hint = ~/.cadenza/cadenza.db — banco local, leitura/escrita mais rápida
settings-storage-postgres = PostgreSQL
settings-storage-postgres-hint = Em breve (Fase C) — Supabase/AWS/Azure, senha no keyring do SO
settings-storage-restart = Reinicie para aplicar a mudança de armazenamento.
settings-storage-restart-now = Reiniciar agora

settings-pg-host = Host
settings-pg-port = Porta
settings-pg-database = Banco de dados
settings-pg-user = Usuário
settings-pg-password = Senha
settings-pg-password-hint = Vai pro keyring do SO. Nunca é gravada em config.json.
settings-pg-ssl = Modo SSL
settings-pg-ssl-require = require (recomendado)
settings-pg-ssl-prefer = prefer
settings-pg-ssl-disable = disable
settings-pg-test = Testar conexão
settings-pg-save = Salvar e migrar
settings-pg-clear = Limpar senha
settings-pg-testing = Conectando…
settings-pg-test-ok = Conexão OK. Você pode salvar e migrar.
settings-pg-test-error = Falha na conexão: { $error }
settings-pg-saved = Configurações salvas. Reinicie para migrar os dados.
settings-pg-cleared = Senha removida do keyring.
settings-pg-fields-required = Preencha host, banco, usuário e senha.
settings-pg-stale = Os campos mudaram desde o teste. Teste a conexão de novo.

settings-section-skills = Skills do CLI
settings-skills-hint = Instala um snippet que ensina o agente (Claude Code, Codex) a usar o cadenza-cli. O snippet vai pro escopo escolhido (projeto atual ou global).
settings-skills-agents = Agentes
settings-skills-agent-claude = Claude Code
settings-skills-agent-codex = Codex
settings-skills-scope = Escopo
settings-skills-scope-project = Projeto atual
settings-skills-scope-global = Global (usuário)
settings-skills-force = Sobrescrever se já existir
settings-skills-install = Instalar
settings-skills-remove = Remover
settings-skills-refresh = Atualizar status
settings-skills-col-agent = Agente
settings-skills-col-scope = Escopo
settings-skills-col-status = Status
settings-skills-col-path = Caminho
settings-skills-status-installed = Instalado
settings-skills-status-installed-locale = Instalado [{ $locale }]
settings-skills-status-not-installed = Não instalado
settings-skills-summary-installed = { $count } instalado(s)
settings-skills-summary-removed = { $count } removido(s)
settings-skills-summary-skipped = { $count } ignorado(s)
settings-skills-no-agent = Selecione pelo menos um agente.
settings-skills-running = Executando…
settings-skills-error = Erro: { $error }
settings-skills-project-label = Projeto
settings-skills-project-empty = Nenhum projeto cadastrado — adicione um na seção Projetos acima.
settings-skills-project-required = Selecione um projeto antes de instalar/remover no escopo "projeto".
settings-section-models = Modelos
settings-models-hint = Descobre os modelos que cada agente oferece. A sondagem leva alguns segundos; o resultado fica salvo e é reusado ao iniciar um agente.
settings-models-refresh = Atualizar status
settings-models-col-agent = Agente
settings-models-col-count = Modelos
settings-models-col-current = Atual
settings-models-load = Carregar
settings-models-loaded = { $count } modelo(s)
settings-models-none = Não carregado
settings-models-loading = Carregando modelos…

task-modal-title-new = Nova task
task-modal-title-edit = Editar task
task-field-titulo = Título
task-field-project = Projeto
task-project-placeholder = — Selecionar projeto —
task-field-estado = Estado
task-field-body = Descrição (markdown)
task-error = Erro: { $error }

task-worktree-legend = Worktree / Ramo
task-field-branch = Ramo
task-field-worktree-path = Caminho do worktree
task-worktree-create = Criar worktree
task-worktree-switch = Trocar ramo
task-worktree-remove = Remover worktree
task-worktree-working = Executando git…
task-worktree-created = Worktree criado.
task-worktree-switched = Trocado para o ramo { $branch }.
task-worktree-removed = Worktree removido.
task-worktree-fields-required = Informe o ramo e o caminho do worktree.
task-worktree-defaults-error = Não foi possível ler o ramo atual: { $error }
task-worktree-error = Erro no git: { $error }
confirm-remove-worktree = Remover o worktree desta task? O git recusa se houver alterações não commitadas.

estado-a-fazer = A fazer
estado-fazendo = Fazendo
estado-aguardando-revisao = Aguardando revisão
estado-feito = Feito

triage-modal-title = Proposta de task derivada
triage-empty = (sem propostas pendentes)
triage-field-parent = Task de origem
triage-field-title = Título
triage-field-file = Arquivo
triage-field-repro = Como reproduzir
triage-field-what-failed = O que falhou
triage-field-action = Ação proposta
triage-field-created = Recebida em
triage-pending-badge = { $count ->
    [one] 1 proposta pendente
   *[other] { $count } propostas pendentes
}
triage-pending-tooltip = Abrir triagem
triage-decided = Decisão registrada.
triage-decided-error = Erro ao registrar decisão: { $error }
triage-load-error = Erro ao carregar proposta: { $error }

terminal-title = Terminal
terminal-empty = (nenhuma sessão ativa)
terminal-toggle-aria = Expandir ou recolher o terminal
terminal-close-aria = Encerrar sessão e fechar o terminal
terminal-resize-aria = Arrastar para redimensionar o terminal
terminal-attach-error = Erro ao anexar ao terminal: { $error }

task-modal-start = Iniciar
task-modal-start-aria = Iniciar agente para esta task
card-start-aria = Iniciar agente
card-start-resume-aria = Continuar conversa salva

start-agent-title = Iniciar agente
start-agent-kind-label = Plataforma
start-agent-model-label = Modelo
start-agent-model-loading = Carregando modelos…
start-agent-model-saved = salvo
start-agent-model-required = Escolha um modelo.
start-agent-model-text-hint = Nenhum modelo carregado — carregue em Configurações → Modelos, ou digite um id.
start-agent-resume-banner = Continuar conversa salva
start-agent-fresh = Iniciar nova
start-agent-fresh-confirm = Apagar conversa salva e iniciar uma nova?
start-agent-action-start = Iniciar
start-agent-action-resume = Continuar
start-agent-launching = Iniciando agente…

# Banner não-bloqueante exibido no topo da janela quando o updater
# detecta uma nova versão. Mesmas strings alimentam a notificação OS
# disparada por notify::show_info — `dump_namespace_strings("ui")` já
# cobre o notify porque o bundle Fluent funde todos os .ftl do locale.
update-available-title = Atualização disponível
update-available-body = Uma nova versão do Cadenza está pronta.
update-restart-now = Reiniciar agora
