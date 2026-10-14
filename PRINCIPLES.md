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
  run is a path of the Stateright model, and every model property holds on
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

## The five promises

Each answers one question about an effect, and none follows from the
others: drop one and a real failure becomes legal.

| Promise | Question | Without it, v may |
|---|---|---|
| **A. Authorized** | May it start? | act without you |
| **O. Once** | How many times? | act twice |
| **H. Honest** | How did it end? | lie about what it did |
| **L. Logged** | What does everyone see? | show a reader something that never happened |
| **B. Bounded** | Can it stall? | hang |

Below, each promise is split into clauses. A clause has a status:

- **Upheld**: it passes the admission rule. It is a promise.
- **Held**: a deterministic layer checks it, but no planted bug has made
  that check fail. It is not yet a promise.
- **Planned**: designed, not built. It is not a promise; it says what the
  check and the planted bug must be when it is built.

### A. Authorized

**An effect starts only under a grant from you, with the grant and the
intent durable before it starts. A cancel revokes the grant.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| A1 | Every effect (a tool run, a `send`, a `spawn`, a timer, a stop) starts only after a durable grant and a durable `Dispatched` (for a stop, its `StopSent`), in the current epoch. | Upheld (the core: every tool run, task run and stop it starts); Planned (the shell's deliveries into another actor's log) | Proved (`call_ok`, `licensed`, `decided`; `runs_ok` covers every `RunTool`, licensed by its call's `Dispatched` or by a running task that very call started (`Origin::Call { call }`), and every `Stop`, licensed by its `StopSent`). Trace-checked for tool runs. Every tool kind passes the one gate, a grant (A2) then a durable `Dispatched`. The shell runs only `read`, `edit` and `bash` yet: no `send`, `spawn`, timer or stop reaches another actor's log. | replay `running_tools_before_their_dispatch_is_durable_fails_the_replay`; Verus mutations recorded in the commits that admitted it (a `Stop` keyed other than by its `StopSent`; a task run as another call, `CallIx(0)`). Planned: a `send` delivered before its `Dispatched` is durable. |
| A2 | Every `Approved` record names your command or a rule in `.verified/v.toml`, with the version of the rules (the seq of their `RulesLoaded` record), that matches the call; the rules' content is in the log. Nothing else produces a grant, and who spawned an actor is never an input to one. | Upheld | Proved, both halves. Rules: `rules::grant` returns the first rule that fits; `spec_apply` refuses an `Approved` whose grant does not fit (`grant_fits`), so `decided` forces `decide` to grant only by a fitting rule; the matcher's signature is the rules, the actor's own name and the call's shape. Your command: `decide` ensures `honest`, so a `Grant::User` is recorded only on your `Approve` with `Verdict::Approve`, as its decision's first record, and `resume` records none (`nothing_given`). That it names the call your command names is tested, not proved. Model-checked ("every grant is the user's or a rule in force that fits"; "every user's grant answers the user's approval command"; α drops subjects, so the model checks patterns only as `Any`). Property-tested (`grant_is_the_first_fitting_rule`). The shell reads no rules file yet: it records the default rules (`read: allow`) at the first boot. | Stateright `approval_by_a_rule_that_does_not_match_is_caught`, `approval_by_the_user_without_a_command_is_caught`; Verus mutations recorded in the commits that admitted it (for your command: an unmatched call granted `Grant::User`; `Grant::User` on `Verdict::Deny`) |
| A3 | Once a cancel is durable, nothing under it starts: no step, retry, approval, dispatch or rerun until the turn ends. Once a stop is durable, nothing starts for that actor, ever, except the stops it sends its children: no turn, step, approval, dispatch, delivery or rerun. A stop always lands: the message limit bounds mail, not stops. | Upheld | Proved, for records and for effects. Records: `lemma_cancel_revokes`, `lemma_nothing_starts_after_a_cancel`, `lemma_stop_revokes`, `lemma_nothing_starts_after_a_stop` (`spec_apply` refuses each start under a cancel or a stop, and `decided` forces `decide` to respect it). Effects after a crash: `resumed` lets a call rerun only while its turn proceeds (`rerun` requires `proceeding`) and a stopped actor rerun only its stops (`task_runs`); `resume_tools` leaves no call dispatched under a cancel and `resume_tasks` no task of a stopped actor running; `resume` finishes a stop a crash tore. Tested: `a_stop_lands_at_the_message_limit`. The shell has no stop command yet, so only the core's tests deliver one. | Verus mutations recorded in the commits that admitted it (`StepStarted` without its cancel guard; `StepStarted` without its stop guard; a `Safe` or `Keyed` call left dispatched under a cancel on resume; a stopped actor's timer kept running on resume; a rerun without `proceeding`; a stopped actor's own task rerun). Test: `a_stop_lands_at_the_message_limit` (failed before its fix). |

### O. Once

**`Unsafe` effects start at most once, ever. `Keyed` effects take effect
exactly once.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| O1 | An `Unsafe` effect is dispatched at most once, ever, and acts at most once; after a crash v never starts it again. | Upheld | Proved (`resumed`). Model-checked. Trace-checked. | Stateright `rerunning_an_unsafe_call_after_a_crash_is_caught`; replay `running_a_tool_twice_in_one_dispatch_fails_the_replay` |
| O2 | A `Keyed` effect takes effect at most once, and exactly once across crashes unless a cancel or a stop revoked it first: resume starts it again with the same id while its turn proceeds, and its target accepts that id at most once; under a durable cancel or stop it resolves `Unknown` (it may have taken effect once) and is not sent again (A3). An acknowledged `send` is in its recipient's log exactly once. | Planned (the core's half is proved) | Proved for the core: resume runs a dispatched `Keyed` call again under its key (`ToolCall::key`, the seq of its `Dispatched`; `rerun` admits `Keyed` while the turn proceeds), and `Delivered` refuses an id already seen. Tested: `a_keyed_call_reruns_with_its_key`, `a_keyed_call_cancelled_before_a_crash_resolves_unknown`. To do: the shell's delivery between logs, and a model check of two actors with crashes on both sides. | Verus mutation recorded in the commit that admitted the core's half (`Delivered` without its seen guard). Planned: resume mints a fresh id (two deliveries); the ack sent before the recipient's append is durable (a lost envelope). |

### H. Honest

**Every effect resolves exactly once, with an outcome that claims no more
than v knows.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| H1 | Every effect, call and task resolves exactly once, and a known outcome is never overwritten: whichever of a run's report and its deadline (`Expired`, v cancelling) reaches the core first resolves it, and the other is stale. | Upheld (calls and tasks: at most one end, never overwritten); Held (calls: a result for every call at quiescence); Planned (tasks: every task ends at quiescence) | Proved (`spec_apply` refuses a `ToolResult` for a resolved call and a `TaskEnded` for a task not running; `lemma_known_outcome_stays`, `lemma_a_task_ends_once`; `on_tool_done` and `on_expired` act only on a dispatched, unresolved call or a running task). Model-checked ("quiescent implies the turn ended"). Simulated (I5). The shell arms no deadline yet (B2), so only the core's tests and proptests deliver `Expired`. | Mutations recorded in the commits that admitted it (`apply` without its `Resolved` guard; `ToolResult` for a resolved call; `TaskEnded` without its running guard). For "a result at quiescence": Stateright `leaving_an_unsafe_call_dispatched_after_a_crash_is_caught` fails the model check, but the plant it needs is a shell's, a run's report lost. |
| H2 | An effect is `NotStarted` only when it provably never acted, and `Unknown` whenever it may have acted: a result v synthesizes is `NotStarted` only for a call never dispatched (or a timer that never fired) and `Unknown` for one that may have acted (after a crash, a dispatched `Unsafe` call or a running `Unsafe` task resolves `Unknown`, never `NotStarted` or succeeded), and a run reports `NotStarted` only if it never acted; a cancel or timeout while it ran is `Unknown`. A run's report is recorded exactly as it came. | Upheld | Proved (`result_fits` and `end_fits`: `spec_apply` refuses any other synthesized result or end, and `resolve` and `end_task` require it; `honest`: a result with the run's output is recorded only on that run's `ToolDone`, with its exit and output). Model-checked. Trace-checked (the model admits a `NotStarted` report only from a cancelled run that never acted). | Stateright `resolving_an_unsafe_call_known_after_a_crash_is_caught`; replay `persisting_unknown_as_not_started_fails_the_replay`, `reporting_a_command_stopped_mid_run_as_not_started_fails_the_replay`; Verus mutations recorded in the commits that admitted it (one for tasks: resume ends a running `Unsafe` task `NotStarted`; one for the relay: `on_tool_done` records `NotStarted` for the run's exit) |
| H3 | A task's actor is notified exactly once that it ended. | Upheld (at most once); Planned (at least once: every task ends) | Proved: the notification is the `TaskEnded` record itself, an envelope from `v` in the actor's own mailbox (`EnvelopeId::Task`), so it lands once per end (H1), and every envelope the actor holds appears once (`actor_ok`, `lemma_actor_keeps`). Property-tested (`actors_recover_and_carry_each_envelope_once`). To do: model-check that every task ends. | Verus mutation recorded in the commit that admitted it (`TaskEnded` notifying twice). Planned: a task never ended after a crash. |

### L. Logged

**The log is the truth. State is its replay, an acknowledgement means
durable, and every reader sees a consistent prefix of it, shaped for that
reader.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| L1 | An actor's state is the replay of its log: `recover(log)` equals the fold of the log, and every durable prefix recovers. `spec_apply` is the one home of every clause on the log's shape. | Upheld | Proved (`recover` ensures `replay(log) == Some(core)`). Model-checked. | Mutation recorded in the commit that added this file's first version |
| L2 | The client's view is an exact prefix of the durable log: only committed records, no gaps or duplicates across reconnects, and once caught up it equals the projection of the log. | Upheld | Model-checked. Simulated (I6). | Stateright `serving_records_before_they_are_durable_is_caught`, `skipping_a_record_on_reconnect_is_caught` |
| L3 | An acknowledgement means durable: anything acknowledged survives a crash and replays, and a 200 for a message means it is durably in the mailbox. A, O, H and L hold when storage refuses appends: v fails stop (it reports full, refuses new work, never acts without a durable record and never crash-loops). | Held (acknowledged means durable); Upheld (fail-stop) | Property-tested (`everything_acknowledged_replays`). Trace-checked (each acknowledged group is the model's next sync). Fail-stop: tested (`a_refused_append_fails_stop`: a refused `Dispatched` runs nothing, acknowledges nothing and appends nothing more; `a_full_log_stops_taking_records_and_loses_nothing`, `a_restart_on_a_full_log_fails_stop`) and simulated (`a_full_log_fails_stop_and_replays`: from a refusal to the next boot nothing is acknowledged and no run starts, and the run replays through the model). | A committed DST planted bug: an append that skips fsync. Fail-stop: `acting_after_a_refused_append_is_caught` (`Planted::ActAfterRefusal`: the shell starts a refused decision's effects). |
| L4 | Each durable envelope is consumed by exactly one model call. An envelope is consumed when a model call carrying it is dispatched (its `StepStarted`), not when it is written into the history: steering counts as consumption, and one injected and never seen (the turn was cancelled first) starts the next turn. A retried user command is accepted at most once: a repeat gets `Exists` and records nothing. | Upheld (at most one model call; a command accepted at most once); Planned (exactly one, at quiescence) | Proved (`spec_apply` refuses a second `Delivered` of an id, and a `StepStarted` that carries other than the batch at the head of the mailbox; every envelope carried or waiting appears once, `actor_ok`, kept by `lemma_actor_keeps`). Property-tested (`actors_recover_and_carry_each_envelope_once`). Tested (`an_unseen_message_starts_the_next_turn`). To do: model-check "carried exactly once at quiescence" with steering, cancels and crashes. It cannot hold yet: once `context_bytes` is reached (the count is cumulative), each delivery starts a turn that fails `Failed(Context)` before it carries anything, so the mail stays unread for good; B4 must decide what a full context does to the mailbox first. | Mutations recorded in the commits that admitted it (`on_message` without its seen guard; `Delivered` without its seen guard; a `StepStarted` that need not carry the head of the mailbox). Planned: consumption recorded at injection (a steered message lost to Esc). |
| L5 | Injections land only at step boundaries, after every result of the step, and the model's history is always well-formed: every `tool_use` has exactly one `tool_result`. | Upheld (the core: injections only at step boundaries); Planned (the history) | Proved (`lemma_carried_at_boundaries`: a `StepStarted` that carries anything starts a turn, or follows a step whose calls have all resolved). The shell's history renders what a step carried at its `StepStarted`, after the step before's results; nothing yet checks the history on every replayed state. | Verus mutation recorded in the commit that admitted it (`StepStarted` from `Tools` without its resolved guard). Planned: a message spliced between a `tool_use` and its result. |

### B. Bounded

**No actor waits on another. Every effect resolves within its deadline.
Without new input, the system goes quiet within its limits.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| B1 | No turn waits on another actor or on a task: the only waits in a turn are its own foreground calls and approvals. So actors cannot deadlock. | Planned | Structural, in the core: a background call resolves `Ok` in the decision that dispatches it (`a_background_call_resolves_at_its_start_and_its_end_notifies_once`), and no core state waits on another actor. To do: model-check an actor cycle (A sends to B, B to A). | A blocking `ask`. |
| B2 | Every in-flight effect resolves by its deadline: it reports back, or v stops waiting and records `Unknown`. | Planned (today: many separate timeouts, no single check) | Model-check an `eventually` property with fairness. DST: every effect resolves within its computed bound. | A cancel that waits for a run that never reports. |
| B3 | Without new input the system goes quiet within its limits: chatter between actors is capped by global limits on messages, turns, tasks, actors and depth. | Planned | The core enforces its share (`messages_per_actor`, for mail only, `turns_per_actor`, `tasks_per_actor`, `depth`; tested). The `actors` cap is the registry's, still to come. To do: model-check an `eventually quiescent` property under the limits; DST with chattering actors. | A missing message limit. |
| B4 | At every beat (a step boundary) there is headroom for the next step in every resource v owns: context bytes, and mailbox, task and actor slots. Storage is not one of them (L3). | Planned | Model-check "within limits" at every step boundary. First decide what a full context does to the mailbox: today each delivery to an actor whose context is full starts a turn that fails at once and carries nothing (L4). | A limit checked after the step it bounds. |

Old numbers, for code comments that still use them. Before the restatement
around five promises: U1 is A1, U2 the retired grant-by-class clause, U3
O1, U4 H2, U5 L1, U6 L2, U7 L4 (a command accepted once), U8 H1. In the
first restatement: A1 and A4 are A1, A3 is A2, A5 is A3, A2 is retired
(superseded by A2), O3 is O2, O4 is a mechanism (a `Safe` rerun), H2, H3
and H6 are H1, with H5's "a task ends once"; H1 and H4 are H2; H5's
notification is H3; H7 is the Env contract (the trusted base); O2 is part of
L4; L6 is a mechanism (coalescing); B3 is not promised (a full disk); B4 is
B3.

## Candidates

Each of these is a property we probably want, but no clause above states it
yet.

- **What runs is exactly what was approved.** The arguments a tool runs with
  are byte-for-byte the arguments recorded and shown for approval. This
  holds by construction (the `Approved` token carries the call; a task
  keeps its call's arguments), but nothing checks it end to end. What is
  proved is the index: a run, a task's included, names the call that was
  dispatched (A1).

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

**Time and cost**
- A turn always ends, but it may end `Failed` (provider outage, step limit,
  context limit). v promises no latency, and bounds cost only through its
  global limits.
- **Storage is the world.** A full disk, or the log's own file cap, halts
  progress: a turn running when the log fills need not end. What v
  promises there is fail-stop (L3), not liveness.
- A message typed into the TUI is lost if the TUI dies before delivering it.
- The log format may change between versions without migration.

## Known holes

Each of these weakens a promise above. Until it is fixed, read that promise
with this caveat.

- **Approval commands are unauthenticated** (weakens A). The daemon listens
  on localhost TCP, and any local process can send an approval. A2 proves
  that every grant names your command or your rule. It does not prove that
  the command came from you.
- **A rule naming an actor matches any actor that takes that name**
  (weakens A2, by design). Names are labels, and an agent that spawns a
  child chooses its name; you approved that `spawn` or wrote the rule that
  allows it.
- **The rules file is yours to get wrong** (weakens A, by design). A rule
  that allows `bash` allows it for every actor, including ones agents
  spawned. v proves a grant matches a rule, not that the rule was wise.
- **Approved tools can destroy v's own state** (weakens L). `.verified/`
  lives in the project, and an approved `bash` can delete or rewrite it.
  Recovery then refuses the log, which is honest, but the session is gone.
- **Auto-approved reads cross the project boundary** (weakens A's intent).
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
    after the crash, even after its call resolved `Unknown`. O1 and H2 hold
    there by model checking only; no run has seen it.
  - **The fake model never loops.** It ends a turn after one round of
    tools, so no run reaches the step limit; a real model may call tools
    until it does.
- **The model's world.** The replay checks a run against the model, so a
  model wrong about the world would pass a wrong shell. Where the model is
  more hostile than the world (late duplicate completions, replies from a
  dead daemon's tasks), only the model checks are stronger; `just coverage`
  lists these as over-approximations. A full log is outside BFS: the model
  never fills its log. Fail-stop (L3) is trace-checked only on the runs of
  `a_full_log_fails_stop_and_replays` (no seed fills a log): a decision the
  log refused never reaches the model, and a refused resume is applied by
  hand (`Observed::Refused`).
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

- Write the log before acting (A, O, L1).
- Crash-only: a broken invariant kills the process, and recovery is the
  only way back (L1).
- Epochs on effect ids, so stale completions change nothing (A1, O1).
- TigerBeetle-style ids at every hop: the sender mints the id, persists it,
  and reuses it on retry (O2, L4).
- Group commit with bounds identical at append and replay (the "survives
  and replays" property above).
- Trace checking: the shell reports, under `sim` only, each input entering
  the core and what it decided, and the barriers it passes; with the
  simulation's ledger these are a run's observations, replayed through the
  model after every DST run (A1, O1, H2).
- Model coverage: each step of the model, BFS's and every replayed run's,
  is keyed small (a call's class, how far it got and how it resolved; a
  crash against a run's act; situations such as a torn group). `just
  coverage A B` prints what BFS reaches and seeds `A..A+B` do not, each gap
  classified, and the directed scenarios that close test gaps: each a BFS
  witness written out as a scenario, with the one ordering no seed promises
  (two runs acting together, or an interrupt durable before a run
  starts) kept by a barrier.
- Cancel as revocation: one primitive (cancel an effect by id) that closes
  the gate, signals once and records what comes back (A3, H1, B2). Signal
  once is tested, not proved: Esc signals its turn's runs, a `cancel` its
  task (recorded on the task, `Task::cancelled`), and a repeated cancel, a
  stop or a deadline after them signals nothing more
  (`a_second_interrupt_signals_nothing`, `a_task_is_signalled_once`,
  `a_deadline_after_esc_signals_nothing_more`).
- Actors as sessions: one log per actor, envelopes delivered by appending
  to the recipient's log, and only `Keyed` effects crossing between logs
  (O2, L4).
- `Safe` effects are started again after a crash until they resolve: the
  class only says one may run more than once. Model-checked ("quiescent
  implies the turn ended"); `resumed` proves only that no `Unsafe` effect is
  started again.
- Coalescing: notifications that arrive together are carried by one model
  call, and a user message is carried alone, so it starts or steers
  exactly one turn.
- Deny-on-steer: while a user's message waits, no call of the step waits
  for you: each is denied ("the user steered") and the message lands at
  the step's boundary. That covers a message during the approval wait,
  one during the model call (the step's calls are denied as they are
  proposed), and a steer a crash tore after its `Delivered` (resume
  finishes it). Built in the core; until the shell steers, it answers a
  new message to a running turn `busy` without asking the core.
- The stop cascade: stopping an actor sends `stop` to every actor it
  spawned. That every descendant stops follows from A3 and O2.

## Deciding new work

- **Touching the world?** First decide what v reports when the operation
  fails. If the honest answer is "unknown", report `Unknown` and add no
  machinery.
- **Adding a promise?** Land the check and its planted bug in the same
  change, and add the entry here.
- **Removing a promise?** Edit this file in that change, deliberately.
- **Prefer a smaller promise you can prove** to a larger one you can only
  hope holds.
- **A promise without a planted bug** goes under "Held, not yet admitted"
  until it has one.
- **A new feature starts as Planned clauses here,** each naming its check
  and its planted bug, before its code is written.
