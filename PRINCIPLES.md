# Principles

Use this file to decide what verified promises, what it doesn't, and why.
When a design question comes up ("should v handle X?"), the answer starts
here. If code, docs or a tool message claim more than this file does, the
claim is wrong.

## v is an agent runtime

v runs agents the way a runtime runs programs. The model is an untrusted
program; its tool calls are system calls; your grants are its capabilities;
the log is the journal. v decides what may start, how many times, how it
ended, who sees what, and that nothing stalls. That is the control plane,
and it is all v verifies.

Everything else is the world: files, processes, the OS, the network, the
model and its provider, and anything else running on the machine. v can do
four things to the world, and only these four:

1. **Ask permission** before acting.
2. **Record intent durably** before acting.
3. **Bound how long it waits.**
4. **Report honestly** what it saw, including `Unknown` when it cannot know.

Containment, transactions across calls, exactly-once side effects in the
world and making sure a process is dead all mean managing the world. None of
that belongs in v. When it's needed, it goes behind the `Env` seam as a
sandbox, which is then the thing that controls that part of the world.

## The vocabulary

- **Actor.** An agent: one session, with one log, one core and one mailbox.
  You are an actor too, at the address `user`; the runtime's own address is
  `v`. You talk to an agent exactly as agents talk to each other.
- **Effect.** Anything an actor causes beyond its own log: a tool call, a
  `send` (a message to an actor), a `spawn`, a timer, a stop. A task's
  end reaches its own actor as a notification: an envelope from `v` in its
  mailbox, which is the `TaskEnded` record itself. Turns and steps are internal to the core; they
  are not effects.
- **Foreground and background.** A foreground effect is waited on by its
  step (`read`, `edit`). A background effect resolves its call as soon as
  its start is durable, and runs on as a **task** its actor watches without
  waiting; its end reaches the actor as a notification.
- **Class.** Every effect has one, which says what a crash does to it:
  - `Safe`: repeating it is harmless (`read`, a timer). It is started again
    until it resolves.
  - `Unsafe`: it may change the world (`edit`, `bash`). It is never started
    again; after a crash it resolves `Unknown`.
  - `Keyed`: its target deduplicates it by an id v minted and recorded
    before sending (`send`, `spawn`, a stop). It is started again unless a
    cancel or a stop revoked it, and takes effect at most once.
- **Grant.** Your permission for one effect: given live (an approval
  command) or in advance (a rule in the rules file you wrote, `v.toml`,
  matched on the actor's own name, the tool and its subject: the path or
  the command). With no rules file, the one rule is `read: allow`.
- **Envelope.** What lands in a mailbox: `{ id, from, to, reply_to?, body }`.
  `reply_to` is metadata for the model; no one is obliged to answer.
- **Outcome.** How an effect ended, saying no more than v knows:
  `NotStarted` (provably never acted: denied, cancelled before dispatch,
  over a limit), `Ok` and `Failed` (ran to completion, and this is what it
  reported), `Unknown` (it may have acted: a crash, a cancel or a timeout
  while it ran).
- **Cancel.** Revoking a grant. One primitive, cancel an effect by id:
  close the gate (it never starts), signal it once if it is running, and
  record honestly what comes back. Esc on a turn cancels that turn's open
  foreground calls and starts no more steps; it leaves the actor's tasks and
  children alone. Stopping an actor cancels everything it holds and sends
  `stop` to every actor it spawned.

## The admission rule

A property is something v **upholds** only if all four hold:

1. **v controls it.** It is about dispatch, the log, the client's view, or
   v's own process. It says nothing about the world.
2. **It is stated precisely**, as a predicate over records, effects, the
   stream, or the simulation's ledger.
3. **A deterministic layer checks it**: Verus, Stateright, the turmoil
   simulation (DST), or a property test.
4. **The check has been seen to fail.** A planted bug made it fail. For a
   test, the planted bug is committed as a test that must keep failing. For
   a proof, Verus cannot keep a failing proof in the tree, so the mutation
   is recorded in the commit that added the proof.

If a property fails any of these, it is not a promise. It is held or
planned (below), a non-promise, or a report (v says what happened and makes
no claim beyond that).

A green check that has never failed is not evidence.

## Assurance levels

The layers are not equally strong, so every invariant names its strongest
check.

- **Proved** (Verus): holds for every input.
- **Model-checked** (Stateright): holds in every interleaving within the
  model's bounds, against an abstract world.
- **Trace-checked** (DST traces replayed through the model): every seed
  run, over every actor's log, is a path of the Stateright model, and every model property holds on
  each state the run passes through. It checks the shell's IO discipline
  against the model's rules of the world (act only after a durable
  dispatch, at most once; a crash keeps a prefix of a pending group), not
  only the world's outcome. It is
  only as strong as what the seeds reach: `just coverage` lists what BFS
  reaches and no seed does.
- **Simulated** (DST): holds on every seed run, with the real daemon and TUI
  against faked seams. This is evidence, not proof.
- **Property-tested** (proptest): holds on sampled inputs.

Invariants about the core should be proved or model-checked. Shell and view
invariants may rest on simulation, and should be trace-checked where the
model states them.

## The three promises

Each answers what you'd notice first if it broke.

| Promise | You care that | Without it, v may |
|---|---|---|
| **C. Control** | v acts only for you | act without you, or act twice |
| **T. Truth** | what v says is true, and nothing it accepted is lost | lie about what it did, or lose what you gave it |
| **P. Progress** | nothing hangs, and nothing runs away | hang, or chatter forever |

| # | Clause | Status |
|---|---|---|
| **C1** | Nothing starts without your grant, durable first, and what runs is exactly what you granted. | Upheld; "what runs is what you granted" Planned |
| **C2** | A grant buys one execution: `Unsafe` at most once, `Keyed` exactly once. | Upheld |
| **C3** | A cancel revokes: nothing granted before a durable cancel starts after it. A stop ends the actor. | Upheld |
| **T1** | An ack means durable; if storage refuses a write, v stops. | Upheld |
| **T2** | Every effect gets exactly one outcome, never claiming less than may have happened. | Upheld |
| **T3** | Every accepted message reaches at most one model call, between steps, and is never dropped. | Upheld |
| **T4** | Every reader sees an exact prefix of the log. | Upheld |
| **P1** | No actor waits on another. | Upheld |
| **P2** | Everything in flight resolves by its deadline. | Upheld |
| **P3** | Every turn lives within its limits: at each beat there's room for the next step or the turn ends, and a turn that would exceed a limit doesn't start. | Upheld for "goes quiet within its limits"; the beat Planned |

A clause has a status:

- **Upheld**: it passes the admission rule. It is a promise.
- **Held**: a deterministic layer checks it, but no planted bug has made
  that check fail. It is not yet a promise.
- **Planned**: designed, not built. It is not a promise; it says what the
  check and the planted bug must be when it is built.

Each clause below gives its one sentence, then what it means precisely,
its checks and its planted bugs. The precise text is the contract; the
sentence is how to remember it.

Under all three sits one design law: **an actor's state is the replay of
its log.** `recover(log)` equals the fold of the log, every durable prefix
recovers, and `spec_apply` is the one home of every rule on the log's
shape. Proved (`recover` ensures `replay(log) == Some(core)`) and
model-checked; its mutation is recorded in the commit that added this
file's first version. You never see it directly, but every proof below
rests on it.

### C. Control: v acts only for you

#### C1. Nothing starts without your grant, durable first, and what runs is exactly what you granted.

- **Precisely.** Every effect (a tool run, a `send`, a `spawn`, a timer, a
  stop) starts only after a durable grant and a durable `Dispatched` (for a
  stop, its `StopSent`), in the current epoch. Every `Approved` record
  names your command or a rule in `.verified/v.toml`, with the version of
  the rules (the seq of their `RulesLoaded` record), that matches the
  call; the rules' content is in the log. A rule matches the call's
  subject exactly as the log records it and the approval shows it: the
  path, the command, the child's name or the recipient. Nothing else
  produces a grant, and who spawned an actor is never an input to one.
  The arguments a run starts with are byte-for-byte the ones recorded and
  shown for approval.
- **Status.** Upheld, except "what runs is exactly what you granted",
  which is Planned. It holds by construction (the `Approved` token carries
  the call; a task keeps its call's arguments), and the index is proved (a
  run, a task's included, names the call that was dispatched), but nothing
  checks the bytes end to end.
- **Checks: the durable grant.** Proved (`call_ok`, `licensed`, `decided`;
  `runs_ok` covers every `RunTool`, licensed by its call's `Dispatched` or
  by a running task that very call started (`Origin::Call { call }`), and
  every `Stop`, licensed by its `StopSent`). Trace-checked, tool runs and
  deliveries alike: every DST run is replayed through the model over every
  actor's log (`replay_actors`), and a delivery is its recipient's
  decision, which the model admits only after the license is durable. The
  shell starts a delivery only from a `RunTool` or `Stop` effect, so after
  its `Dispatched` or `StopSent` is durable. Simulated (I9: every envelope
  an actor minted names the record of its log that licensed it). Tested:
  `a_delivery_waits_for_its_dispatch_and_its_recipients_append`.
  Model-checked across logs ("every delivery follows its durable license",
  `deliveries_across_crashes`: two and three actors, a crash on either side
  of a delivery).
- **Checks: the grant is yours.** Proved, both halves. Rules:
  `rules::grant` returns the first rule that fits; `spec_apply` refuses an
  `Approved` whose grant does not fit (`grant_fits`), so `decided` forces
  `decide` to grant only by a fitting rule; the matcher's signature is the
  rules, the actor's own name and the call's shape. Your command: `decide`
  ensures `honest`, so a `Grant::User` is recorded only on your `Approve`
  with `Verdict::Approve`, as its decision's first record, and `resume`
  records none (`nothing_given`). That it names the call your command
  names is tested, not proved. Model-checked ("every grant is the user's or
  a rule in force that fits"; "every user's grant answers the user's
  approval command"; α drops subjects, so the model checks patterns only as
  `Any`). Property-tested (`grant_is_the_first_fitting_rule`). The subject,
  which the shell derives (`ToolArgs::shape`): property-tested through the
  matcher (`a_rule_matches_the_recorded_command_or_path`) and tested end to
  end (`an_exact_rule_grants_its_command_alone`). The shell reads
  `.verified/v.toml` through the `Env` seam at every boot, before every
  delivery to an actor and before each model response is settled, and
  records it (`RulesLoaded`, with the file's text) whenever its text
  differs from the text in force; no file means the default rules
  (`read: allow`), and a file that does not parse, or is too large to
  record, grants nothing. Tested
  (`the_rules_file_grants_and_is_read_before_each_message`;
  `a_removed_rule_stops_granting_to_a_child`; on a real disk,
  `a_rules_file_on_disk_grants_a_real_command`).
- **Planted bugs.** The durable grant: replay
  `running_tools_before_their_dispatch_is_durable_fails_the_replay`; Verus
  mutations recorded in the commits that admitted it (a `Stop` keyed other
  than by its `StopSent`; a task run as another call, `CallIx(0)`).
  Deliveries: DST
  `delivering_a_spawn_before_its_dispatch_is_durable_fails_the_replay`
  (`Planted::DeliverBeforePersist`); Stateright
  `delivering_before_the_license_is_durable_is_caught`
  (`Bug::DeliverBeforeDurable`); replay
  `a_path_of_two_actors_replays_and_a_delivery_before_its_license_does_not`;
  shell `delivering_a_send_before_its_dispatch_is_durable_is_caught`. The
  grant is yours: Stateright
  `approval_by_a_rule_that_does_not_match_is_caught`,
  `approval_by_the_user_without_a_command_is_caught`; Verus mutations
  recorded in the commits that admitted it (an unmatched call granted
  `Grant::User`; `Grant::User` on `Verdict::Deny`); property test
  `a_subject_cut_at_the_first_word_is_caught`.
- **Planned: what runs is what you granted.** Check: the fake `Env`
  compares every run's arguments with the `Approved` call in the log
  (simulated, on every run). Planted bug: a shell that swaps a call's
  arguments after approval.

#### C2. A grant buys one execution: `Unsafe` at most once, `Keyed` exactly once.

- **Precisely.** An `Unsafe` effect is dispatched and started at most
  once, ever; after a crash v never starts it again. (What the command
  then does in the world is the world's.) A `Keyed` effect takes effect at
  most once, and exactly once across crashes unless a cancel or a stop
  revoked it first: resume starts it again with the same id while its turn
  proceeds, and its target accepts that id at most once; under a durable
  cancel or stop it resolves `Unknown` (it may have taken effect once) and
  is not sent again (C3). An acknowledged `send` is in its recipient's log
  exactly once. A stop is `Keyed` too, and it reaches its child (see the
  stop cascade under Mechanisms).
- **Status.** Upheld.
- **Checks.** `Unsafe`: proved (`resumed`), model-checked, trace-checked.
  `Keyed`: proved for the core (resume runs a dispatched `Keyed` call again
  under its key, `ToolCall::key`, the seq of its `Dispatched`; `rerun`
  admits `Keyed` while the turn proceeds; `Delivered` refuses an id already
  seen). Model-checked (`deliveries_across_crashes`: "an acknowledged
  delivery is in its recipient's durable log once", "no call is delivered
  twice"; "every stop reaches its child" in
  `stops_across_deadlines_and_crashes` and
  `chatter_goes_quiet_within_limits`). Trace-checked: every seed's
  deliveries replay through the model, among them crashes aimed at a
  delivery. Simulated (I9: every `send` or `spawn` that resolved `Ok` is in
  its recipient's log exactly once at quiescence; I12: at quiescence every
  descendant of a stopped actor is stopped). Tested:
  `a_keyed_call_reruns_with_its_key`,
  `a_keyed_call_cancelled_before_a_crash_resolves_unknown`,
  `a_send_lands_exactly_once_across_a_crash`,
  `stopping_an_actor_stops_its_children`,
  `a_stop_the_crash_caught_in_flight_lands_after_the_restart` (failed
  before its fix).
- **Planted bugs.** `Unsafe`: Stateright
  `rerunning_an_unsafe_call_after_a_crash_is_caught`; replay
  `running_a_tool_twice_in_one_dispatch_fails_the_replay`. `Keyed`: Verus
  mutation recorded in the commit that admitted the core's half
  (`Delivered` without its seen guard). Stateright
  `minting_a_fresh_key_on_resume_is_caught` (`Bug::FreshKeyOnResume`),
  `acknowledging_before_the_append_is_durable_is_caught`
  (`Bug::AckBeforeDurable`), `giving_up_on_a_stop_at_its_deadline_is_caught`
  (`Bug::StopExpires`), `a_stop_before_its_childs_birth_is_caught`
  (`Bug::StopBeforeBirth`). DST `a_fresh_key_after_a_crash_delivers_twice`
  (`Planted::FreshKeyOnResume`), `acknowledging_before_the_append_fails_the_replay`
  (`Planted::AckBeforeAppend`), `a_stop_that_skips_a_child_fails`
  (`Planted::StopSkipsChildren`). Shell
  `a_fresh_key_after_a_crash_delivers_twice`,
  `acknowledging_before_the_append_loses_the_envelope`.

#### C3. A cancel revokes: nothing granted before a durable cancel starts after it. A stop ends the actor.

- **Precisely.** A cancel covers its turn: once it is durable, no step,
  retry, approval, dispatch or rerun of that turn starts. Your next message
  is new input and starts a new turn under new grants. Once a stop is
  durable, nothing starts for that actor, ever, except the stops it sends
  its children: no turn, step, approval, dispatch, delivery or rerun.
- **Status.** Upheld.
- **Checks.** Proved, for records and for effects. Records:
  `lemma_cancel_revokes`, `lemma_nothing_starts_after_a_cancel`,
  `lemma_stop_revokes`, `lemma_nothing_starts_after_a_stop` (`spec_apply`
  refuses each start under a cancel or a stop, and `decided` forces
  `decide` to respect it). Effects after a crash: `resumed` lets a call
  rerun only while its turn proceeds (`rerun` requires `proceeding`) and a
  stopped actor rerun only its stops (`task_runs`); `resume_tools` leaves
  no call dispatched under a cancel and `resume_tasks` no task of a stopped
  actor running; `resume` finishes a stop a crash tore. Tested:
  `a_stop_lands_at_the_message_limit`; in the shell, your stop
  (`ActorHandle::stop`) and the cascade's
  (`stopping_an_actor_stops_its_children`).
- **Planted bugs.** Verus mutations recorded in the commits that admitted
  it (`StepStarted` without its cancel guard; `StepStarted` without its
  stop guard; a `Safe` or `Keyed` call left dispatched under a cancel on
  resume; a stopped actor's timer kept running on resume; a rerun without
  `proceeding`; a stopped actor's own task rerun). Test:
  `a_stop_lands_at_the_message_limit` (failed before its fix).

### T. Truth: what v says is true, and nothing it accepted is lost

#### T1. An ack means durable; if storage refuses a write, v stops.

- **Precisely.** Anything acknowledged survives a crash and replays; a 200
  for a message means it is durably in the mailbox; an error never answers
  a message the log holds. When storage refuses an append (the log's own
  file cap, a full disk or a spent quota), v fails stop: it reports full,
  refuses new work, never acts without a durable record, never loses what
  it acknowledged and never crash-loops on it. Under a refusal the safety
  clauses hold (C1, C2's at most once, C3, T2's honesty, T3's at most one, T4); resolution
  and liveness (T2's exactly one, P2, P3) wait for a boot with room.
- **Status.** Upheld.
- **Checks.** Property-tested (`everything_acknowledged_replays`).
  Trace-checked (each acknowledged group is the model's next sync, and
  each boot's log is what a crash leaves of it). Fail-stop: simulated (I16
  on every run: from an actor's refused append until the next boot,
  nothing more of its log is acknowledged and none of its runs or
  deliveries starts; `a_full_log_fails_stop_and_replays`) and tested
  (`a_refused_append_fails_stop`, `a_full_disk_fails_stop`,
  `a_full_disk_refuses_and_other_errors_crash`,
  `a_full_log_stops_taking_records_and_loses_nothing`,
  `a_restart_on_a_full_log_fails_stop`,
  `a_message_to_an_unborn_actor_is_refused` and
  `a_message_past_the_turn_limit_is_refused_and_not_recorded`, each of the
  last two failed before its fix).
- **Planted bugs.** DST
  `acknowledging_an_append_without_its_fsync_fails_the_replay`
  (`Planted::SkipFsync`). Fail-stop: DST
  `acting_after_a_refused_append_fails_the_check`
  (`Planted::ActAfterRefusal`; I16), shell
  `acting_after_a_refused_append_is_caught`.

#### T2. Every effect gets exactly one outcome, never claiming less than may have happened.

- **Precisely.** Every effect, call and task resolves exactly once, and a
  known outcome is never overwritten: whichever of a run's report and its
  deadline (`Expired`, v cancelling) reaches the core first resolves it,
  and the other is stale. `NotStarted` is never said of an effect that may
  have acted: a result v synthesizes is `NotStarted` only for a call never
  dispatched (or a timer whose firing the core has not seen), and
  `Unknown` for one that may have acted (after a crash, a dispatched
  `Unsafe` call or a running `Unsafe` task resolves `Unknown`); a run
  reports `NotStarted` only if it never acted, and a cancel or timeout
  while it ran is `Unknown`. A spawn that made its child has acted. A run's
  report is recorded exactly as it came. (Saying `Unknown` of something
  that never acted is imprecise, not dangerous, and is not promised
  against.)
- **Why `NotStarted` exists.** The model decides whether retrying is safe
  from one question, did it act: yes (`Ok`, `Failed`), maybe (`Unknown`:
  look first), no (`NotStarted`: retry freely). C2 stops v from running an
  `Unsafe` call twice; T2 stops the model from asking twice because v told
  it the first never ran.
- **Status.** Upheld.
- **Checks.** Exactly once: proved (`spec_apply` refuses a `ToolResult`
  for a resolved call and a `TaskEnded` for a task not running;
  `lemma_known_outcome_stays`, `lemma_a_task_ends_once`; `on_tool_done` and
  `on_expired` act only on a dispatched, unresolved call or a running
  task). Model-checked ("quiescent implies the turn ended";
  `tasks_across_crashes`). Simulated (I5: at quiescence every call of every
  actor has a result and every turn ended; I11: every task ended).
  Honesty: proved (`result_fits` and `end_fits`: `spec_apply` refuses any
  other synthesized result or end; `honest`: a result with the run's output
  is recorded only on that run's `ToolDone`, with its exit and output).
  Model-checked. Trace-checked (the model admits a `NotStarted` report only
  from a cancelled run that never acted, and a spawn's only before it made
  its child). Tested:
  `a_spawn_whose_child_is_stopped_before_its_prompt_is_ok` (failed before
  its fix).
- **Planted bugs.** Exactly once: mutations recorded in the commits that
  admitted it (`apply` without its `Resolved` guard; `ToolResult` for a
  resolved call; `TaskEnded` without its running guard); Stateright
  `ending_a_task_twice_after_a_crash_is_caught` (`Bug::EndTaskTwice`),
  `leaving_a_task_running_after_a_crash_is_caught`
  (`Bug::LeaveTaskRunning`), `leaving_an_unsafe_call_dispatched_after_a_crash_is_caught`;
  DST `a_cancel_that_waits_for_a_silent_run_fails` (`Planted::NoDeadline`).
  Honesty: Stateright
  `resolving_an_unsafe_call_known_after_a_crash_is_caught`; replay
  `persisting_unknown_as_not_started_fails_the_replay`,
  `reporting_a_command_stopped_mid_run_as_not_started_fails_the_replay`;
  Verus mutations recorded in the commits that admitted it (resume ends a
  running `Unsafe` task `NotStarted`; `on_tool_done` records `NotStarted`
  for the run's exit).

#### T3. Every accepted message reaches at most one model call, between steps, and is never dropped.

- **Precisely.** A message is an envelope: yours, another actor's, or a
  task's end from `v` (the `TaskEnded` record itself, so a task's actor is
  notified exactly once that it ended). Each durable envelope is carried
  by at most one model call, and that call is told it. It is consumed when
  a model call carrying it is dispatched (its `StepStarted`), not when it
  is written into the history: steering counts as consumption, and one
  injected and never seen (the turn was cancelled first) starts the next
  turn. It lands only at a step boundary, after every result of the step.
  An envelope no model call has carried stays in the mailbox until one
  does; nothing removes it. A retried user command is accepted at most
  once: a repeat gets `Exists` and records nothing.
- **Status.** Upheld.
- **Checks.** Proved (`spec_apply` refuses a second `Delivered` of an id,
  and a `StepStarted` that carries other than the batch at the head of the
  mailbox; every envelope carried or waiting appears once, `actor_ok`, kept
  by `lemma_actor_keeps`; `lemma_carried_at_boundaries`: a `StepStarted`
  that carries anything starts a turn, or follows a step whose calls have
  all resolved). Property-tested
  (`actors_recover_and_carry_each_envelope_once`). Tested
  (`an_unseen_message_starts_the_next_turn`). Simulated (I10: every
  envelope an actor holds is carried by at most one model call, and at
  quiescence by exactly one unless its actor is stopped or at a limit;
  I14: every model call's request holds what its step carried).
  Model-checked with steering, cancels and crashes
  (`steering_cancels_and_crashes`, `steering_across_steps`: "every envelope
  is carried at most once", "at quiescence every envelope is carried",
  "mail is carried only at a step boundary").
- **Planted bugs.** Mutations recorded in the commits that admitted it
  (`on_message` without its seen guard; `Delivered` without its seen
  guard; a `StepStarted` that need not carry the head of the mailbox;
  `StepStarted` from `Tools` without its resolved guard; `TaskEnded`
  notifying twice). Stateright `carrying_an_envelope_twice_is_caught`
  (`Bug::CarryTwice`), `skipping_the_mail_at_a_step_is_caught`
  (`Bug::SkipCarry`), `splicing_a_message_into_a_step_is_caught`
  (`Bug::SpliceSteering`). DST
  `consumption_recorded_at_injection_loses_a_steer_to_esc`
  (`Planted::ConsumeAtInjection`: a steered message lost to Esc),
  `a_task_end_never_told_is_caught` (`Planted::UntoldTaskEnds`: I14).

#### T4. Every reader sees an exact prefix of the log.

- **Precisely.** The client's view is an exact prefix of the durable log:
  only committed records, no gaps or duplicates across reconnects, and
  once caught up it equals the projection of the log.
- **Status.** Upheld.
- **Checks.** Model-checked. Simulated (I6).
- **Planted bugs.** Stateright
  `serving_records_before_they_are_durable_is_caught`,
  `skipping_a_record_on_reconnect_is_caught`.

### P. Progress: nothing hangs, and nothing runs away

#### P1. No actor waits on another.

- **Precisely.** No turn waits on another actor or on a task: the only
  waits in a turn are its own foreground calls and approvals. So actors
  cannot deadlock.
- **Status.** Upheld.
- **Checks.** Structural, in the core: a background call resolves `Ok` in
  the decision that dispatches it
  (`a_background_call_resolves_at_its_start_and_its_end_notifies_once`),
  and no core state waits on another actor. Model-checked
  (`an_actor_cycle_goes_quiet`).
- **Planted bug.** Stateright `a_blocking_send_deadlocks_and_is_caught`
  (`Bug::BlockingSend`, a `send` that waits for its recipient's turn: an
  `ask`).

#### P2. Everything in flight resolves by its deadline.

- **Precisely.** An in-flight effect reports back, or v stops waiting and
  records `Unknown`. Every run has a deadline (`read`, `edit`, `bash`,
  `send`, `spawn`) but a timer's, which waits on purpose, a `tasks`
  listing's and a `cancel`'s, which v answers itself. A stop has none: it
  is sent until it lands (C2).
- **Status.** Upheld.
- **Checks.** The shell arms one deadline per run (`ServerLimits::deadline`,
  past the run's own timeout and drains; a delivery's is
  `deliver_deadline`) and reports `Expired`, which the core resolves
  `Unknown` (T2). Simulated (I15 on every run, from its own table of which
  runs need a deadline, apart from the shell's; seeds run commands that
  never report, `hang`, and reach `Expired`). Tested:
  `a_run_that_never_reports_resolves_unknown_at_its_deadline`,
  `reads_edits_and_deliveries_resolve_by_their_deadlines`, on a real shell
  `a_real_background_command_runs_on_and_times_out_honestly`.
  Model-checked (`hung_runs_resolve_by_their_deadline`).
- **Planted bugs.** DST `a_cancel_that_waits_for_a_silent_run_fails`
  (`Planted::NoDeadline`; I15); Stateright
  `waiting_on_a_run_that_never_reports_is_caught` (`Bug::NoDeadline`);
  shell `waiting_on_a_run_that_never_reports_is_caught`,
  `waiting_on_a_stalled_file_or_delivery_is_caught`.

#### P3. Every turn lives within its limits: at each beat there's room for the next step or the turn ends, and a turn that would exceed a limit doesn't start.

- **Precisely.** A limit ends a turn, never an actor. The beat is a step
  boundary. At each beat v checks headroom for the next step in every
  resource it owns (steps, context bytes, mailbox, task and actor slots);
  if there is none the turn ends there, `Failed` with the limit named, and
  any mail it hadn't carried waits for the next turn (T3). A turn that
  would exceed a limit before its first step doesn't start, and its mail
  waits. The chatter caps (messages, turns and tasks per actor; actors;
  depth) refuse new work, so without new input the system goes quiet.
  Storage is not a resource here (T1).
- **Status.** Upheld for "without new input the system goes quiet within
  its limits". Planned for the beat: today a turn can start into a full
  context and fail at once, and limits are checked where each is spent
  rather than at the beat.
- **Checks.** Quiet within limits: the core enforces its share
  (`messages_per_actor`, for mail only, `turns_per_actor`,
  `tasks_per_actor`, `depth`; tested); the `actors` cap is the registry's
  (`ServerLimits::actors`; `a_spawn_past_the_actor_limit_does_not_start`).
  Model-checked (`chatter_goes_quiet_within_limits`: "every actor stays
  within its limits", "the world settles"; `an_actor_cycle_goes_quiet`
  reaches the message limit). Simulated
  (`chattering_actors_go_quiet_within_their_limits`).
- **Planted bugs.** Stateright `a_missing_actor_limit_is_caught`
  (`Bug::NoActorLimit`).
- **Planned: the beat.** Check: model-check "within limits" at every step
  boundary, and that no turn starts that has no headroom for its first
  step. Planted bug: a limit checked after the step it bounds.

### Old numbers

For code comments, commits and reviews that still use them. The five
promises (A, O, H, L, B) became these three: A1 and A2 are C1; A3 is C3;
O1 and O2 are C2; H1 and H2 are T2; H3, L4 and L5's timing are T3 (L5's
well-formed history is a mechanism); L1 is the design law above; L2 is T4;
L3 is T1; B1 is P1; B2 is P2; B3 and B4 are P3. Older numberings are in
the history of this file.

## Not promised

v makes no claim about the following. Where it matters, v reports honestly
instead.

**The model**
- It may be wrong, unsafe or nondeterministic. An approval covers the
  literal arguments, not their wisdom.
- It does not see everything. Tool output is truncated at limits and marked
  as truncated, and earlier steps' reasoning is not carried forward.

**Side effects**
- **At most once applies to calls, not intents.** After `Unknown`, the model
  may issue a new call that repeats the effect. `Unknown` means "look before
  you redo".
- **A turn is not a transaction.** A crash can leave some of a turn's calls
  applied and others `Unknown`.
- **Concurrent `Unsafe` calls are unordered.** Two edits to one file can both
  report `Ok` while the file ends up holding only one of them. `Ok` means
  "this write happened", not "your change is in the file now".
- **Reads go stale immediately.** `edit` compare-and-swaps on its `old` text;
  nothing else guards against change.

**The world**
- **Nothing is contained.** Symlinks are followed. Processes a command
  started may outlive it, and a stopped or timed-out command's processes may
  keep running and finish later.
- **v does not track effects beyond the files it writes.** The network, git
  remotes, deploys and money are invisible to it.
- **Durability holds only on one machine and one disk, and only if fsync
  tells the truth.** There is no replication, and deleting the state
  directory deletes the sessions.
- **There is no confidentiality.** Whatever a tool reads can be sent to the
  model provider.

**Actors**
- **Delivery, not conversation.** v delivers every envelope exactly once and
  promises nothing about whether anyone answers. `reply_to` is a convention.
- **No ordering per sender, or across senders.** An actor consumes
  envelopes in the order its own log received them. An envelope may carry
  its sender's sequence number, so the model can see the order they were
  sent in; v does not serialize one sender's deliveries.
- **A stop is not a kill.** Stopping an actor revokes its grants and asks
  its runs to stop; a child's in-flight `Unsafe` call may still land, and
  resolves `Unknown`.

- **A message may never be read.** T3 promises at most one model call and
  that nothing is dropped, not that every message is read: a stopped
  actor, one at a lifetime cap, or one whose context is full keeps its
  mail unread.
- **A full context parks the actor.** Each turn ends at its first beat and
  its mail waits. Compaction (planned) is what unparks it.
- **A stop has no latency bound.** It takes effect at the next step
  boundary of the actor it reaches, and its runs resolve by their deadlines
  (P2); how soon is not promised.

**Time and cost**
- A turn always ends, but it may end `Failed` (provider outage, step limit,
  context limit). v promises no latency, and bounds cost only through its
  global limits.
- **Storage is the world.** A full disk, or the log's own file cap, halts
  progress: a turn running when the log fills need not end, and its runs
  and deliveries need not resolve until a boot with room. What v promises
  there is fail-stop (T1), not liveness. A daemon that cannot write its
  port file on a full disk does not start.
- A message typed into the TUI is lost if the TUI dies before delivering it.
- The log format may change between versions without migration.

## Known holes

Each of these weakens a promise above. Until it is fixed, read that promise
with this caveat.

- **Approval commands are unauthenticated** (weakens C1). The daemon listens
  on localhost TCP, and any local process can send an approval. C1 proves
  that every grant names your command or your rule. It does not prove that
  the command came from you.
- **A rule naming an actor matches any actor that takes that name**
  (weakens C1, by design). Names are labels, and an agent that spawns a
  child chooses its name; you approved that `spawn` or wrote the rule that
  allows it.
- **The rules file is yours to get wrong** (weakens C1, by design). A rule
  that allows `bash` allows it for every actor, including ones agents
  spawned. v proves a grant matches a rule, not that the rule was wise.
- **A `prefix` rule compares bytes** (weakens C1, by design). `bash` with
  `prefix = "cargo test"` also grants `cargo test; rm -rf ~`, and any
  command that starts with those bytes. v does not parse shell; use
  `exact` for a command you mean alone.
- **A rule change reaches a running step at its next boundary** (weakens
  C1's intent). The rules file is read before every delivery and before
  each model response is settled: a call already granted, or a run
  already started, runs under the rules it was granted by.
- **Approved tools can destroy v's own state** (weakens T1). `.verified/`
  lives in the project, and an approved `bash` can delete or rewrite it.
  Recovery then refuses the log, which is honest, but the session is gone.
- **Auto-approved reads cross the project boundary** (weakens C1's intent).
  `read` is allowed by the default rule and follows symlinks, so a link can
  lead it to a secret that is then sent to the provider.

## The trusted base

These are assumed, not checked:

- **The verification tools:** Verus, Z3 and vstd, plus the two
  `external_body` functions the core's proofs rely on: the core's `bump`,
  which crashes on overflow, and the domain's `CallId::same`.
- **Everything outside the core:** the shell, the TUI and their
  dependencies (tokio, hyper, axum, serde, ratatui) are simulated, not
  proved.
- **The simulation's fakes.** If a fake is kinder than the real seam it
  replaces, the simulation is optimistic. Fake fidelity is the weakest link
  in the chain. `just coverage` names the fidelity gaps it sees, where the
  model reaches what the fakes never do:
  - **Orphans never act.** A crashed daemon's tool tasks die with the
    turmoil host, but a real `bash` child outlives the daemon and may act
    after the crash, even after its call resolved `Unknown`. C2 and T2 hold
    there by model checking only; no run has seen it.
  - **The fake model loops only as its script says.** A script runs a
    turn of several steps (`A then B`, its placeholders filled from the
    results), and a directed scenario runs one past the step limit; a real
    model may loop on its own.
  - **The fake provider's rules are a copy.** I13 checks every request
    against what Anthropic refuses (`fake::malformed`); a provider with
    other rules would refuse what passes here.
- **The model's world.** The replay checks a run against the model, so a
  model wrong about the world would pass a wrong shell. Where the model is
  more hostile than the world (late duplicate completions, replies from a
  dead daemon's tasks), only the model checks are stronger; `just coverage`
  lists these as over-approximations. A full log is outside BFS: the model
  never fills its log. Fail-stop (T1) is trace-checked only on the runs
  that fill a log's own cap (`a_full_log_fails_stop_and_replays`; no seed
  does, and no fake fills a disk: a full disk is tested only by a log that
  refuses as the file log does once it has cut back a partial write): a
  decision the log refused never reaches the model, a refused resume is
  applied by hand (`Observed::Refused`), and a resume after it without a
  boot fails the replay. I16 judges fail-stop on every run, from the
  ledger.
- **The Env contract.** Tools act on the world through the `Env` seam, and
  their reports are the Env's to make honest; v records what they say. Each
  implementation is held to it by tests, not promised:
  - an `edit` reported `Ok` wrote `new` in place of `old` (simulated, I7, a
    check of `LocalEnv` and `SimEnv`; it has a committed planted bug);
  - `bash` and `read` report what they did; nothing checks it.
- **The hardware and OS:** fsync tells the truth, the monotonic clock is
  monotonic, and signals are delivered on a best-effort basis.

## Mechanisms

These serve the invariants; they are not invariants themselves. They are
described in `AGENTS.md` and the code.

- Write the log before acting (C1, C2, and the design law).
- Crash-only: a broken invariant kills the process, and recovery is the
  only way back (the design law).
- Epochs on effect ids, so stale completions change nothing (C1, C2).
- TigerBeetle-style ids at every hop: the sender mints the id, persists it,
  and reuses it on retry (C2, T3).
- Group commit with bounds identical at append and replay (the "survives
  and replays" property above).
- Trace checking: the shell reports, under `sim` only, each input entering
  the core and what it decided, and the barriers it passes; with the
  simulation's ledger these are a run's observations, replayed through the
  model after every DST run (C1, C2, T2).
- Model coverage: each step of the model, BFS's and every replayed run's,
  is keyed small (a call's class, how far it got and how it resolved; a
  crash against a run's act; situations such as a torn group). `just
  coverage A B` prints what BFS reaches and seeds `A..A+B` do not, each gap
  classified, and the directed scenarios that close test gaps: each a BFS
  witness written out as a scenario, with the one ordering no seed promises
  (two runs acting together, or an interrupt durable before a run
  starts) kept by a barrier.
- Cancel as revocation: one primitive (cancel an effect by id) that closes
  the gate, signals once and records what comes back (C3, T2, P2). Signal
  once is tested, not proved: Esc signals its turn's runs, a `cancel` its
  task (recorded on the task, `Task::cancelled`), and a repeated cancel, a
  stop or a deadline after them signals nothing more
  (`a_second_interrupt_signals_nothing`, `a_task_is_signalled_once`,
  `a_deadline_after_esc_signals_nothing_more`).
- Actors as sessions: one log per actor, envelopes delivered by appending
  to the recipient's log, and only `Keyed` effects crossing between logs
  (C2, T3). The registry never awaits a session, a session's loop never
  awaits a delivery, and each delivery awaits only the registry and its
  recipient's append (a stop first awaits its parent's spawn of that
  child, itself such a delivery), so no actor waits on another (P1).
- `Safe` effects are started again after a crash until they resolve: the
  class only says one may run more than once. Model-checked ("quiescent
  implies the turn ended"); `resumed` proves only that no `Unsafe` effect is
  started again.
- A well-formed history: every `tool_use` has exactly one `tool_result`,
  and what a step carried is rendered at its `StepStarted`, after the step
  before's results, so the model is never shown a broken conversation
  (T3's "between steps" is what makes it possible). Simulated (I13: the
  fake provider checks every request it receives, as Anthropic would, with
  `fake::malformed`); planted bug DST
  `splicing_a_steer_into_its_step_is_caught` (`Planted::SpliceSteering`).
- Coalescing: notifications that arrive together are carried by one model
  call, and a user message is carried alone, so it starts or steers
  exactly one turn.
- Deny-on-steer: while a user's message waits, no call of the step waits
  for you: each is denied ("the user steered") and the message lands at
  the step's boundary. That covers a message during the approval wait,
  one during the model call (the step's calls are denied as they are
  proposed), and a steer a crash tore after its `Delivered` (resume
  finishes it). Built in the core, and the shell steers: a message to a
  busy actor is delivered like any other and answered once its
  `Delivered` is durable (tested over HTTP:
  `a_message_to_a_busy_actor_steers_its_running_turn`). Seeds steer while
  a turn streams, while a tool runs and instead of answering a prompt.
- The stop cascade: stopping an actor sends `stop` to every actor it
  spawned. That every descendant stops follows from C3 and C2 (a stop
  reaches its child). Built: the shell delivers each `StopSent` as a
  `Keyed` stop from `v`, with no deadline, once its parent's spawn of that
  child in flight has landed (`stopping_an_actor_stops_its_children`);
  you stop an actor with `POST /actors/{id}/stop` (Ctrl-K in the TUI),
  and seeds do (I12).

## Deciding new work

- **Touching the world?** First decide what v reports when the operation
  fails. If the honest answer is "unknown", report `Unknown` and add no
  machinery.
- **Adding a promise?** Land the check and its planted bug in the same
  change, and add the entry here.
- **Removing a promise?** Edit this file in that change, deliberately.
- **Prefer a smaller promise you can prove** to a larger one you can only
  hope holds.
- **A promise without a planted bug** is Held until it has one.
- **A new feature starts as Planned clauses here,** each naming its check
  and its planted bug, before its code is written.
