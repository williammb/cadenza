# Cadenza — Como usar

Você tem acesso ao CLI `cadenza-cli` para gerenciar tarefas. Ele fala com
o aplicativo Cadenza pelo socket local; o app **precisa estar aberto**.

## Fluxo obrigatório

1. **Ao iniciar:** `cadenza-cli current --json` — leia a task atual.
2. **Durante o trabalho:** `cadenza-cli log <id> "<progresso>"` — reporte
   com frequência (no mínimo a cada decisão importante ou bloco de
   código alterado).
3. **Ao encontrar um problema derivado** (bug paralelo, refator que
   bloqueia, escopo novo): `cadenza-cli propose ...` — esse comando
   **bloqueia** e aguarda o humano decidir. Não invente solução por
   conta própria.
4. **Ao concluir:** `cadenza-cli done <id> "<resumo>"` — você **nunca**
   move uma task para "feito" sozinho; isso pede ao humano.

## Regras

- Você só trabalha em tasks com `estado: fazendo`. Se `cadenza-cli current`
  retornar `null`, pare e peça ao humano para começar uma task.
- Sempre use `--json` quando estiver parseando saída. Os valores
  `estado` são canônicos em português (`a_fazer`, `fazendo`,
  `aguardando_revisao`, `feito`) e **não** mudam com `--lang`.
- Após `propose`, observe o exit code:
  - `0` → aceita (saída inclui o novo `task_id`)
  - `20` → rejeitada — pare e reporte ao humano
  - `21` → timeout — pare, reporte que o humano não decidiu
- Se receber exit code `10` ("app não está rodando"), peça ao humano
  para abrir o Cadenza.
- Se receber exit code `11` ("token inválido"), peça ao humano para
  "Revogar token CLI" pelo menu da bandeja e tentar de novo.

## Exemplos rápidos

```bash
# Pegar a task atual em JSON
cadenza-cli current --json

# Reportar progresso
cadenza-cli log T-42 "implementei o validador, próximo passo é o teste"

# Propor task derivada (bloqueante)
cadenza-cli propose \
  --parent T-42 \
  --title "Validar entrada em outro endpoint" \
  --repro "POST /api/foo com body inválido retorna 500 em vez de 400" \
  --file "src/handlers/foo.rs" \
  --what-failed "missing input validation" \
  --action "wrap with the same Validator pipeline used in T-42"

# Pedir conclusão (humano decide se vira "feito")
cadenza-cli done T-42 "endpoint validado e coberto por dois testes novos"
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
`$CADENZA_PROJECT_ID` e `$CADENZA_IDEIA_ID`. Cada chamada imprime o
`task_id` recém-criado em stdout. Após a última task, a ideia de origem
é marcada automaticamente como `destrinchada`.

Mire em 3–8 tasks acionáveis por ideia: cada uma deve ser pequena o
suficiente para ser autocontida mas grande o suficiente para merecer um
card próprio. Não cole o corpo inteiro da ideia em uma única task — a
ideia é fatiar.
