# Cadenza — Como usar

O Cadenza é uma **ferramenta de apoio opcional** para coordenar tarefas pelo
CLI `cadenza-cli`, que fala com o aplicativo Cadenza por um socket local. Ele
só é relevante quando você foi iniciado para uma task do Cadenza
(`$TASKAI_TASK_ID` setado) **ou** quando o humano pedir explicitamente para
usá-lo.

**Se `$TASKAI_TASK_ID` não estiver setado e o humano não tiver pedido o
Cadenza, ignore o resto deste documento e apenas trabalhe normalmente como
assistente** — não rode `cadenza-cli`, não reclame e não exija uma task. As
seções abaixo só valem quando existe um contexto de task ativo.

## Saiba qual é a sua task

Quando o app te inicia para uma task, ele injeta duas variáveis de
ambiente no seu shell:

- `$TASKAI_TASK_ID` — a task para a qual você foi iniciado (ex.: `T-42`).
- `$TASKAI_PROJECT_ID` — o projeto a que essa task pertence.

**Identifique sempre a sua task por `$TASKAI_TASK_ID`** — pode haver várias
tasks em `fazendo` ao mesmo tempo (uma por agente rodando), então `current`
é ambíguo e pode devolver o card de outro. Busque a *sua* task pelo id:

```bash
cadenza-cli get "$TASKAI_TASK_ID" --json
```

`get` devolve só essa task (ou sai com `30`, `task_not_found`, se o id não
existir). Só use `cadenza-cli current --json` como fallback quando
`$TASKAI_TASK_ID` não estiver setado (você foi rodado fora do terminal do app).

## Fluxo da task (quando há uma task ativa)

1. **Ao iniciar:** `cadenza-cli get "$TASKAI_TASK_ID" --json` — leia a sua
   task. Só trabalhe nela se o `estado` for `fazendo`.
2. **Durante o trabalho:** `cadenza-cli log "$TASKAI_TASK_ID" "<progresso>"`
   — reporte com frequência (no mínimo a cada decisão importante ou bloco
   de código alterado).
3. **Ao encontrar um problema derivado** (bug paralelo, refator que
   bloqueia, escopo novo): `cadenza-cli propose ...` — esse comando
   **bloqueia** e aguarda o humano decidir. Não invente solução por
   conta própria.
4. **Ao concluir:** reúna as evidências de validação e chame
   `cadenza-cli done` (veja "Concluir com evidências" abaixo) — você
   **nunca** move uma task para "feito" sozinho; isso pede ao humano.

## Planejando uma task (modo plano)

Quando você for iniciado em **modo plano**, NÃO implemente nada. A task
continua em `a_fazer` (então `current` não a retorna), mas
`$TASKAI_TASK_ID` continua setado — leia do mesmo jeito:

```bash
cadenza-cli get "$TASKAI_TASK_ID" --json
```

1. Leia a descrição breve da task nessa saída.
2. Entreviste o humano no terminal: faça perguntas de esclarecimento sobre
   escopo, casos de borda e critérios de aceite — um lote focado por vez.
3. Quando você e o humano combinarem, salve o plano refinado:

   ```bash
   cadenza-cli plan "$TASKAI_TASK_ID" --body "## Objetivo
   ...
   ## Passos
   1. ...
   ## Aceite
   - ..."
   ```

   Por padrão o plano é anexado como uma seção `## Plano`, preservando a
   descrição original. Use `--replace` para sobrescrever o body inteiro, ou
   omita `--body` para enviar o plano via stdin.
4. **Não** chame `done` e **não** comece a codar. O humano inicia uma
   execução separada que vai ler o plano que você salvou.

## Concluir com evidências

Ao terminar, não basta dizer "pronto" — você monta um **pacote de
evidências** que o Cadenza verifica de forma independente e mostra ao
revisor humano. O fluxo:

1. **Leia o contrato de qualidade do projeto** — as checagens que você
   deve rodar:

   ```bash
   cadenza-cli quality --json
   ```

   Retorna `{ contract_version, checks: [{id, name, cmd, required,
   required_if_changed}] }`. `--task` e `--project` são resolvidos do
   ambiente; passe-os só se precisar. Lista vazia = o projeto não tem
   contrato (siga em frente sem checagens).

2. **Rode cada checagem** exatamente como o `cmd` manda, capturando o
   **comando, o exit code e um trecho curto do log** (as últimas linhas
   relevantes — erros, resumo de testes).

3. **Monte um `evidence.json`** com o resultado, respeitando os limites:

   ```json
   {
     "contract_version": "sha256:…",
     "checks": [{"id": "clippy", "exit": 0, "log_excerpt": "…(últimas linhas)…"}],
     "groups": [{"label": "feat: pacote de review", "files": ["src-tauri/src/review.rs"]}],
     "open_questions": ["…dúvidas para o revisor…"]
   }
   ```

   - Use o `contract_version` exato que o `quality` devolveu.
   - `id` e `exit` de cada checagem são obrigatórios; `log_excerpt` é o
     trecho do log.
   - `groups` (opcional) mapeia arquivos → um rótulo de intenção, para o
     diff sair agrupado por intenção.
   - `open_questions` (opcional) sinaliza dúvidas ao revisor.
   - **Limites:** ≤ 64 checks, ≤ 64 groups, `log_excerpt` ≤ 8 KiB por
     checagem, arquivo inteiro ≤ 256 KiB. Estourar = exit 2 e nada muda.

4. **Peça a conclusão anexando as evidências:**

   ```bash
   cadenza-cli done "$TASKAI_TASK_ID" \
     --summary "endpoint validado; clippy e testes passando" \
     --evidence evidence.json
   ```

   A chave de idempotência é gerada automaticamente (ou passe
   `--idempotency-key <k>`); ela é ecoada no stderr, então re-rodar com a
   mesma chave é um no-op seguro. `--summary` é equivalente ao resumo
   posicional. Sem `--evidence`, o `done` ainda funciona (vira um pacote
   `no_validation`).

5. **`done` sempre tem sucesso** — ele não é um portão. O Cadenza deriva
   sozinho o diff, os riscos e o **estado de evidência** a partir do que
   você reportou e os mostra ao revisor humano, que é quem decide.

## Regras

- **Dentro de uma task ativa**, você só trabalha em tasks com
  `estado: fazendo`. Se `get "$TASKAI_TASK_ID"` mostrar outro estado (e você
  não estiver em modo plano), pare e pergunte ao humano.
- Se `$TASKAI_TASK_ID` não estiver setado e `cadenza-cli current --json`
  retornar `null` (ou o app não estiver rodando), **não há task ativa do
  Cadenza** — apenas ajude o humano normalmente. **Não** recuse e **não** exija
  uma task. Só use `cadenza-cli` quando o humano pedir ou quando existir
  contexto de task.
- Sempre use `--json` quando estiver parseando saída. Os valores
  `estado` são canônicos em português (`a_fazer`, `fazendo`,
  `aguardando_revisao`, `feito`) e **não** mudam com `--lang`.
- Após `propose`, observe o exit code:
  - `0` → aceita (saída inclui o novo `task_id`)
  - `20` → rejeitada — pare e reporte ao humano
  - `21` → timeout — pare, reporte que o humano não decidiu
- `get` sai com `30` (`task_not_found`) se o id não existir.
- Se receber exit code `10` ("app não está rodando") *quando o humano
  realmente quis usar o Cadenza*, peça para ele abrir o Cadenza. Caso
  contrário, app fechado é normal — siga em frente sem ele.
- Se receber exit code `11` ("token inválido"), peça ao humano para
  "Revogar token CLI" pelo menu da bandeja e tentar de novo.

## Exemplos rápidos

```bash
# Pegar a sua task em JSON (preferível a `current`)
cadenza-cli get "$TASKAI_TASK_ID" --json

# Reportar progresso
cadenza-cli log "$TASKAI_TASK_ID" "implementei o validador, próximo passo é o teste"

# Descobrir os IDs de projeto (para new-task / create-ideia)
cadenza-cli projects --json

# Propor task derivada (bloqueante)
cadenza-cli propose \
  --parent "$TASKAI_TASK_ID" \
  --title "Validar entrada em outro endpoint" \
  --repro "POST /api/foo com body inválido retorna 500 em vez de 400" \
  --file "src/handlers/foo.rs" \
  --what-failed "missing input validation" \
  --action "wrap with the same Validator pipeline used in the parent task"

# Ler o contrato de qualidade (checagens a rodar antes do done)
cadenza-cli quality --json

# Pedir conclusão com evidências (humano decide se vira "feito")
cadenza-cli done "$TASKAI_TASK_ID" \
  --summary "endpoint validado e coberto por dois testes novos" \
  --evidence evidence.json
```

## Destrinchar uma ideia (Inbox)

Se a variável de ambiente `CADENZA_IDEIA_ID` estiver setada quando você
começar, o humano quer que você quebre uma ideia da Inbox em tasks
concretas. O corpo da ideia está em `CADENZA_IDEIA_BODY` (também
disponível via `cadenza-cli read-ideia $CADENZA_IDEIA_ID`).

Para cada task que você derivar da ideia, rode:

```bash
cadenza-cli new-task --titulo "..." --body "..."
```

`--project` e `--from-ideia` são lidos automaticamente de
`$TASKAI_PROJECT_ID` e `$CADENZA_IDEIA_ID`. (Se `$TASKAI_PROJECT_ID` não
estiver setado, passe `--project` explicitamente — rode
`cadenza-cli projects` para achar o id.) Cada chamada imprime o
`task_id` recém-criado em stdout. Após a última task, a ideia de origem
é marcada automaticamente como `destrinchada`.

Mire em 3–8 tasks acionáveis por ideia: cada uma deve ser pequena o
suficiente para ser autocontida mas grande o suficiente para merecer um
card próprio. Não cole o corpo inteiro da ideia em uma única task — a
ideia é fatiar.

## Decompor uma issue do Jira

Se a variável de ambiente `CADENZA_JIRA_ISSUE_ID` estiver setada quando
você começar, o humano quer que você quebre uma issue do Jira em
subtasks. A identidade da issue vem no ambiente
(`CADENZA_JIRA_KEY`, `CADENZA_JIRA_SITE`, `CADENZA_JIRA_ISSUE_ID`) e o
`analysis_run_id` está em `CADENZA_ANALYSIS_RUN_ID`.

1. **Leia a issue injetada.** O resumo e a descrição (em Markdown) da
   issue já vêm no seu prompt inicial. Use-os como fonte de verdade do
   escopo.

2. **Entreviste em estilo "grill" (Ato 1).** Faça **uma pergunta por
   vez**, e para cada pergunta **recomende uma resposta**. Vá afunilando
   até travar a quebra em subtasks antes de submeter qualquer coisa. Não
   submeta uma lista de subtasks que você não conseguiria defender.

3. **Submeta via CLI com escopo de run.** Quando a quebra estiver
   travada, escreva as subtasks num arquivo JSON
   (`[{"title": "...", "body": "..."}, ...]`) e rode:

   ```bash
   cadenza-cli jira-materialize \
     --analysis-run-id "$CADENZA_ANALYSIS_RUN_ID" \
     --subtasks-file subtasks.json
   ```

   O secret de capacidade é lido automaticamente de
   `$CADENZA_RUN_SECRET` (ou de STDIN com `--secret-stdin`). **NUNCA
   passe o secret na linha de comando** — ele nunca deve aparecer em
   argv. O `jira_site` e o `jira_issue_id` são carimbados pelo servidor a
   partir do secret; você **não** os informa.

4. **Estados das subtasks.** As subtasks criadas nascem no fluxo normal
   do quadro e percorrem os estados canônicos: `a_fazer`, `fazendo`,
   `aguardando_revisao`, `feito`. Cada subtask deve ser pequena o
   suficiente para virar um card próprio.

## Memória do projeto

Cada projeto tem uma **memória oficial**: uma lista curada de fatos,
decisões e convenções que valem para aquele projeto. O **usuário é o
curador** — nada que você sugerir entra na memória sem ele aprovar.

- **No início de uma task**, a memória já vem injetada no seu prompt
  inicial. Para reler a qualquer momento:

  ```bash
  cadenza-cli memory list --json
  ```

- **Ao finalizar uma task** (antes do `done`), se você descobriu algo
  **genuinamente reaproveitável** para tasks futuras deste projeto — uma
  convenção, uma decisão de arquitetura, uma armadilha — proponha como
  aprendizado. Repetível e **opcional**; não force aprendizados triviais:

  ```bash
  cadenza-cli memory suggest "Os handlers de IPC ficam em ipc.rs; a lógica vai nos módulos."
  ```

  O aprendizado fica **pendente** até o humano promovê-lo no review da
  task. `--task` é resolvido de `$TASKAI_TASK_ID`; `--project` de
  `$TASKAI_PROJECT_ID`.

### Modo reavaliação da memória

Se a variável `CADENZA_MEMORY_REEVAL` estiver setada quando você começar,
o humano quer que você **reavalie a memória atual** do projeto. Leia-a com
`cadenza-cli memory list --json` e emita sugestões de revisão — **sem
alterar nada diretamente**. Cada sugestão fica pendente até o humano
aprovar na aba de Memória.

```bash
# remover item obsoleto
cadenza-cli memory revise --op remover --target M-abc

# reescrever item confuso
cadenza-cli memory revise --op reescrever --target M-abc --texto "Texto mais claro."

# mesclar duplicatas (dois ou mais --target)
cadenza-cli memory revise --op mesclar --target M-a --target M-b --texto "Texto consolidado."

# propor item novo
cadenza-cli memory revise --op nova --texto "Nova convenção observada."

# apontar contradição (informativa; humano resolve editando)
cadenza-cli memory revise --op contradicao --target M-a --target M-b --nota "Um diz X, o outro Y."
```

Depois de emitir as sugestões, pare. O humano faz a curadoria.
