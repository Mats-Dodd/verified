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
  `send` (a message to an actor; a notification is a `send` from `v`), a
  `spawn`, a timer, a stop. Turns and steps are internal to the core; they
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
    before sending (`send`, `spawn`, a stop). It is started again, and takes
    effect once.
- **Grant.** Your permission for one effect: given live (an approval
  command) or in advance (a rule in a rules file you wrote, matched on the
  actor, the tool and its arguments).
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

**An effect starts only under a live grant from you, with the grant and the
intent durable before it starts. A cancel revokes the grant.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| A1 | A tool starts only after its call's `Approved` and `Dispatched` records are durable, in the current epoch. | Upheld | Proved (`call_ok`, `licensed`, `decided`). Trace-checked. | replay `running_tools_before_their_dispatch_is_durable_fails_the_replay` |
| A2 | Policy approves only `Safe` calls; an `Unsafe` call needs an approval command. | Upheld (until A3 replaces it) | Proved (`approver_fits`). Model-checked: `spec_apply` refuses a policy approval of an `Unsafe` call. | Stateright `approval_by_policy_of_an_edit_is_caught` |
| A3 | Every `Approved` record names your command or a rule, with the rules file's version, that matches the call; the rules file's content is in the log. Nothing else produces a grant, and who spawned an actor is never an input to one. | Planned | Prove: rule matching is a pure function in the core over data in the log. | A grant for a call no rule matches; a grant that consults the spawner. |
| A4 | Every effect, not only tools (`send`, `spawn`, timers, stops), starts only under a durable grant and a durable `Dispatched`. | Planned | Prove A1's lemmas for every effect kind. Trace-check. | A `send` delivered before its `Dispatched` is durable. |
| A5 | Once a cancel is durable, nothing under it starts: no dispatch, no new step for a cancelled turn, nothing for a stopped actor. | Planned | Prove in `decide`. Model-check with cancels at every point. | A step started after its turn's cancel. |

### O. Once

**`Unsafe` effects start at most once, ever. `Keyed` effects take effect
exactly once. `Safe` effects are started again until they resolve.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| O1 | An `Unsafe` call is dispatched at most once, ever, and acts at most once; after a crash v never starts it again. | Upheld | Proved (`resumed`). Model-checked. Trace-checked. | Stateright `rerunning_an_unsafe_call_after_a_crash_is_caught`; replay `running_a_tool_twice_in_one_dispatch_fails_the_replay` |
| O2 | A message starts at most one turn: a retried command gets `Exists` and records nothing. | Upheld | Proved (`spec_apply` refuses a `TurnStarted` for a seen message). | Mutation recorded in the commit that admitted it (`on_message` without its seen guard) |
| O3 | A `Keyed` effect takes effect exactly once across crashes: resume starts it again with the same id, and its target accepts that id at most once. An acknowledged `send` is in its recipient's log exactly once. | Planned | Prove the resume case and the target's dedup. Model-check two actors with crashes on both sides. | Resume mints a fresh id (two deliveries); the ack sent before the recipient's append is durable (a lost envelope). |
| O4 | A `Safe` effect dispatched before a crash is started again in the new epoch until it resolves. | Held | Model-checked (quiescence implies every call resolved). `resumed` proves only that no `Unsafe` call is started again. | A resume that drops a dispatched `Safe` call (it would hang: see B). |

### H. Honest

**Every effect resolves exactly once, with an outcome that claims no more
than v knows.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| H1 | After a crash, a dispatched `Unsafe` call resolves `Unknown`, never as interrupted or succeeded. | Upheld | Model-checked. Trace-checked. | Stateright `resolving_an_unsafe_call_known_after_a_crash_is_caught`; replay `persisting_unknown_as_interrupted_fails_the_replay` |
| H2 | Each call has at most one result. | Upheld | Proved (`spec_apply` refuses a `ToolResult` for a resolved call). | Mutation recorded in the commit that admitted it (`apply` without its `Resolved` guard) |
| H3 | Each call has a result at quiescence (with H2, exactly one). | Held | Model-checked ("quiescent implies the turn ended"). Simulated (I5). Stateright plant `leaving_an_unsafe_call_dispatched_after_a_crash_is_caught`. | A planted shell bug: a run's report lost. |
| H4 | The outcome vocabulary is `NotStarted`, `Ok`, `Failed`, `Unknown`. `NotStarted` only when it provably never acted; a cancel or timeout while it ran is `Unknown`. Today `Interrupted` covers both "never started" and "stopped while running", and a timeout is `Failed`: both claim more than v knows. | Planned | Prove in the core that `NotStarted` follows only an undispatched call or a run that reported it never began. Trace-check. | A command cancelled mid-run reported `NotStarted`. |
| H5 | Every task ends exactly once, and its actor is notified exactly once (the notification is a `Keyed` `send` from `v`). | Planned | Prove the task records' grammar. Model-check with crashes between the end and the notification. | A notification sent twice after a crash; a task ended twice. |
| H6 | A known outcome is never overwritten: whichever of a completion and a cancel reaches the core first wins, and either order is honest. | Planned | Prove. Model-check every race. | A cancel that rewrites an `Ok` it arrived after. |
| H7 | An `edit` reported `Ok` wrote `new` in place of `old`. | Held | Simulated (I7). | A committed planted bug. |

### L. Logged

**The log is the truth. State is its replay, an acknowledgement means
durable, and every reader sees a consistent prefix of it, shaped for that
reader.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| L1 | An actor's state is the replay of its log: `recover(log)` equals the fold of the log, and every durable prefix recovers. `spec_apply` is the one home of every clause on the log's shape. | Upheld | Proved (`recover` ensures `replay(log) == Some(core)`). Model-checked. | Mutation recorded in the commit that added this file's first version |
| L2 | The client's view is an exact prefix of the durable log: only committed records, no gaps or duplicates across reconnects, and once caught up it equals the projection of the log. | Upheld | Model-checked. Simulated (I6). | Stateright `serving_records_before_they_are_durable_is_caught`, `skipping_a_record_on_reconnect_is_caught` |
| L3 | Anything acknowledged survives a crash and replays. A 200 for a message means it is durably in the mailbox. | Held | Property-tested (`everything_acknowledged_replays`). Trace-checked (each acknowledged group is the model's next sync). | A committed DST planted bug: an append that skips fsync. |
| L4 | Every durable envelope is consumed by exactly one model call, in send order per sender. An envelope is consumed when a model call carrying it is dispatched, not when it is written into the history: one injected and never seen (the turn was cancelled first) starts the next turn. | Planned | Prove the mailbox grammar. Model-check with steering, cancels and crashes. | Consumption recorded at injection (a steered message lost to Esc); two senders' order swapped. |
| L5 | The model's history is well-formed: every `tool_use` has exactly one `tool_result`, and steering messages and notifications are inserted only at step boundaries, after every result of the step. A message sent while the step waits for approvals denies them ("the user steered") and lands at that boundary. | Planned | Check the history builder on every replayed state. Model-check. | A message spliced between a `tool_use` and its result. |
| L6 | Notifications that arrive together are consumed by one turn; a user message is never coalesced, so it starts or steers exactly one turn. | Planned | Model-check. | A user message merged into a notification batch. |

### B. Bounded

**No actor waits on another. Every effect resolves within its deadline.
Without new input, the system goes quiet within its limits.**

| # | Clause | Status | Strongest check | Planted bug |
|---|---|---|---|---|
| B1 | No turn waits on another actor or on a task: the only waits in a turn are its own foreground calls and approvals. So actors cannot deadlock. | Planned | Structural: no effect resolves on another actor's turn. Model-check an actor cycle (A sends to B, B to A). | A blocking `ask`. |
| B2 | Every in-flight effect resolves by its deadline: it reports back, or v stops waiting and records `Unknown`. | Planned (today: many separate timeouts, no single check) | Model-check an `eventually` property with fairness. DST: every effect resolves within its computed bound. | A cancel that waits for a run that never reports. |
| B3 | Every turn ends, even on a full log: a reserve lets the running turn close. | Held | Simulated (`a_full_log_ends_the_running_turn_and_replays`); not model-checked (BFS never fills a log). | A full log that refuses the turn's closing records. |
| B4 | Without new user input the system reaches quiescence: chatter between actors is capped by global limits on messages, turns, actors and depth. Stopping an actor stops everything it spawned. | Planned | Model-check an `eventually quiescent` property under the limits. DST with chattering actors. | A stop that does not reach a child; a missing message limit. |

Old numbers, for code comments that still use them: U1 is A1, U2 is A2, U3
is O1, U4 is H1, U5 is L1, U6 is L2, U7 is O2, U8 is H2.

## Candidates

Each of these is a property we probably want, but no clause above states it
yet.

- **What runs is exactly what was approved.** The arguments a tool runs with
  are byte-for-byte the arguments recorded and shown for approval. This
  holds by construction (the `Approved` token carries the call), but
  nothing checks it end to end.
- **Results are honest for every tool.** The `edit` case is H7. `bash` and
  `read` are not checked.

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
- **No ordering across senders.** Envelopes from different actors may be
  consumed in any order.
- **A stop is not a kill.** Stopping an actor revokes its grants and asks
  its runs to stop; a child's in-flight `Unsafe` call may still land, and
  resolves `Unknown`.

**Time and cost**
- A turn always ends, but it may end `Failed` (provider outage, step limit,
  context limit). v promises no latency, and bounds cost only through its
  global limits.
- A message typed into the TUI is lost if the TUI dies before delivering it.
- The log format may change between versions without migration.

## Known holes

Each of these weakens a promise above. Until it is fixed, read that promise
with this caveat.

- **Approval commands are unauthenticated** (weakens A). The daemon listens
  on localhost TCP, and any local process can send an approval. A2 proves
  that policy never approves an `Unsafe` call; A3 will prove that every
  grant names your command or your rule. Neither proves that the command
  came from you.
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
    after the crash, even after its call resolved `Unknown`. O1 and H1 hold
    there by model checking only; no run has seen it.
  - **The fake model never loops.** It ends a turn after one round of
    tools, so no run reaches the step limit; a real model may call tools
    until it does.
- **The model's world.** The replay checks a run against the model, so a
  model wrong about the world would pass a wrong shell. Where the model is
  more hostile than the world (late duplicate completions, replies from a
  dead daemon's tasks), only the model checks are stronger; `just coverage`
  lists these as over-approximations. A full log is outside BFS: the model
  never fills its log, so ending a turn on a full log is not model-checked.
  It is trace-checked only on the runs of
  `a_full_log_ends_the_running_turn_and_replays` (no seed fills a log), whose
  replay applies the log's refusal and the resume it forces by hand
  (`Observed::Refused`, `Harness::resumed`).
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
  and reuses it on retry (O2, O3).
- Group commit with bounds identical at append and replay (the "survives
  and replays" property above).
- Trace checking: the shell reports, under `sim` only, each input entering
  the core and what it decided, and the barriers it passes; with the
  simulation's ledger these are a run's observations, replayed through the
  model after every DST run (A1, O1, H1).
- Model coverage: each step of the model, BFS's and every replayed run's,
  is keyed small (a call's class, how far it got and how it resolved; a
  crash against a run's act; situations such as a torn group). `just
  coverage A B` prints what BFS reaches and seeds `A..A+B` do not, each gap
  classified, and the directed scenarios that close test gaps: each a BFS
  witness written out as a scenario, with the one ordering no seed promises
  (two runs acting together, or an interrupt durable before a run
  starts) kept by a barrier.
- Cancel as revocation: one primitive (cancel an effect by id) that closes
  the gate, signals once and records what comes back (A5, H6, B2).
- Actors as sessions: one log per actor, envelopes delivered by appending
  to the recipient's log, and only `Keyed` effects crossing between logs
  (O3, L4).

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
