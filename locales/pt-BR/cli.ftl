propose-accepted = Proposta aceita — nova task: { $task_id }
propose-rejected = Proposta rejeitada
propose-merged = Proposta mesclada na task { $task_id }
propose-timeout = Tempo esgotado aguardando decisão ({ $minutes } min)
propose-pending = Proposta pendente — abra o Cadenza para decidir

task-summary =
    { $count ->
        [one] { $count } task
       *[other] { $count } tasks
    } em '{ $estado }'

current-none = Nenhuma task em andamento
current-some = Task atual: { $task_id } — { $titulo }

log-appended = Log registrado
done-requested = Conclusão proposta — aguardando humano confirmar
