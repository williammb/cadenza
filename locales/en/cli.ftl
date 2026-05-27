propose-accepted = Proposal accepted — new task: { $task_id }
propose-rejected = Proposal rejected
propose-merged = Proposal merged into task { $task_id }
propose-timeout = Timed out waiting for decision ({ $minutes } min)
propose-pending = Proposal pending — open Cadenza to decide

task-summary =
    { $count ->
        [one] { $count } task
       *[other] { $count } tasks
    } in '{ $estado }'

current-none = No task currently in progress
current-some = Current task: { $task_id } — { $titulo }

log-appended = Log appended
done-requested = Completion requested — waiting for human confirmation
