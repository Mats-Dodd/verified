# Principles

This file says what v promises, what it doesn't, and why. Every proof, test
and simulation in the repository exists to check something here. If code,
docs or a tool message claim more than this file, the claim is wrong.

## v is an agent runtime

v runs agents the way an operating system runs programs. The model is an
untrusted program, its tool calls are system calls, your grants are its
permissions, and the log is its journal. v verifies only what it controls:
what may start, how many times, how it ended, what everyone sees, and that
nothing stalls.

Everything else is the world: files, processes, the network, the model and
its provider. v does only four things to the world: it asks permission,
records intent before acting, bounds how long it waits, and reports what it
saw, including `Unknown` when it can't know.

## Vocabulary

- **Actor.** An agent: one log, one core, one mailbox. You are an actor at
  the address `user`, and the runtime is `v`. You talk to an agent the way
  agents talk to each other.
- **Effect.** Anything an actor causes beyond its own log: a tool run, a
  `send`, a `spawn`, a timer, a stop.
- **Task.** A background effect. Its call resolves once its start is
  durable; it runs on, and its end reaches the actor as a message from `v`.
- **Class.** What a crash does to an effect. `Safe` (`read`, a timer) is
  started again until it resolves. `Unsafe` (`edit`, `bash`) is never
  started again and resolves `Unknown`. `Keyed` (`send`, `spawn`, a stop)
  is started again under the same id, and its target deduplicates it.
- **Grant.** Your permission for one effect: an approval, or a rule in
  `.verified/v.toml`. With no rules file, the one rule is `read: allow`.
- **Outcome.** `NotStarted` (it provably never acted), `Ok` or `Failed`
  (it ran, and this is its report), `Unknown` (it may have acted).
- **The beat.** A step boundary: the one point where outside input
  reaches the model, a cancel takes hold, and limits are checked.

## The admission rule

A property is a promise only if:

1. **v controls it.** It says nothing about the world.
2. **It is stated precisely,** over records, effects, the stream or the
   simulation's ledger.
3. **A deterministic layer checks it:** Verus, Stateright, the turmoil
   simulation, or a property test.
4. **The check has been seen to fail.** A planted bug made it fail,
   committed as a test that must keep failing, or, for a proof, recorded as
   a mutation in the commit that added it.

A green check that has never failed is not evidence.

**Status.** *Upheld* passes all four. *Held* is checked but has no planted
bug yet. *Planned* is designed, not built; it names the check and the
planted bug it must land with. Only Upheld is a promise.

**Assurance**, strongest first: **proved** (Verus, every input);
**model-checked** (Stateright, every interleaving within bounds);
**trace-checked** (every simulation run replayed through the model, as
strong as what the seeds reach; `just coverage` lists the gaps);
**simulated** (turmoil, the real daemon against faked seams);
**property-tested** (sampled inputs). Core properties should be proved or
model-checked.

## The promises

| | Promise | Status |
|---|---|---|
| **Control** | *v acts only for you* | |
| C1 | **Granted.** Nothing starts without your grant, and what runs is exactly what you granted. | Upheld; "exactly what you granted" Planned |
| C2 | **Once.** A grant buys one execution: `Unsafe` at most once, `Keyed` exactly once. | Upheld |
| C3 | **Revocable.** A cancel revokes what was granted before it. A stop ends the actor. | Upheld |
| **Truth** | *what v says is true* | |
| T1 | **Logged.** Nothing starts before it's in the log, and nothing acknowledged ever leaves it. | Upheld |
| T2 | **Readers.** Every reader sees an exact prefix of the log. | Upheld |
| T3 | **Honest.** An effect gets at most one outcome, never overwritten, never claiming less than may have happened. | Upheld |
| T4 | **Messages.** Every accepted message reaches at most one model call, at a beat, and is never dropped. | Upheld |
| **Progress** | | |
| P1 | **Nothing hangs, nothing runs away.** Every turn ends, everything in flight resolves by its deadline, and without new input everything goes quiet. | Upheld; limits at the beat Planned |

### C1. Granted

Every effect starts only after a durable grant naming your approval or a
rule in force that matches the call's subject (its path, command, child or
recipient), with the rules' version and text in the log. Who spawned an
actor is never an input to a grant. The arguments that run are byte for
byte the ones recorded and shown to you.

- **Checks.** Proved: `grant_fits`, `honest`, `rules::grant`.
  Model-checked: "every grant is the user's or a rule in force that fits".
  Property-tested: `grant_is_the_first_fitting_rule`,
  `a_rule_matches_the_recorded_command_or_path`.
- **Planted bugs.** Stateright
  `approval_by_a_rule_that_does_not_match_is_caught`,
  `approval_by_the_user_without_a_command_is_caught`; property test
  `a_subject_cut_at_the_first_word_is_caught`; Verus mutations in their
  commits.
- **Planned: exactly what you granted.** It holds by construction (the
  approval carries the call) but nothing checks the bytes. Check: the fake
  `Env` compares each run's arguments with the approved call. Planted bug:
  a shell that swaps the arguments after approval.

### C2. Once

An `Unsafe` effect is started at most once, ever. A `Keyed` effect takes
effect exactly once across crashes: it is resent under the same id while
its turn proceeds, and its target accepts that id once. An acknowledged
`send` is in its recipient's log exactly once, and a stop reaches its child.

- **Checks.** Proved: `resumed`, the `Delivered` seen guard. Model-checked:
  `deliveries_across_crashes`, `stops_across_deadlines_and_crashes`.
  Trace-checked. Simulated: I9, I12.
- **Planted bugs.** Stateright
  `rerunning_an_unsafe_call_after_a_crash_is_caught`, `Bug::FreshKeyOnResume`,
  `Bug::AckBeforeDurable`, `Bug::StopExpires`, `Bug::StopBeforeBirth`; replay
  `running_a_tool_twice_in_one_dispatch_fails_the_replay`; DST
  `Planted::FreshKeyOnResume`, `Planted::AckBeforeAppend`,
  `Planted::StopSkipsChildren`.

### C3. Revocable

Once a cancel is durable, nothing of its turn starts: no step, retry,
approval, dispatch or rerun. Your next message is new input under new
grants. Once a stop is durable, nothing starts for that actor ever again,
except the stops it sends its children.

- **Checks.** Proved: `lemma_cancel_revokes`,
  `lemma_nothing_starts_after_a_cancel`, `lemma_stop_revokes`,
  `lemma_nothing_starts_after_a_stop`, and `resumed` (a rerun only while
  its turn proceeds).
- **Planted bugs.** Verus mutations in their commits; test
  `a_stop_lands_at_the_message_limit`.

### T1. Logged

Every effect starts only after its grant and its `Dispatched` are durable,
in the current epoch. Anything acknowledged survives a crash and replays;
a 200 for a message means it is in the mailbox. When storage refuses a
write (the log's cap, a full disk), v fails stop: it refuses new work,
acts on nothing without a record, loses nothing it acknowledged, and
doesn't crash-loop.

- **Checks.** Proved: `runs_ok`, `licensed`. Trace-checked: every
  simulation run, over every actor's log. Property-tested:
  `everything_acknowledged_replays`. Simulated: I9, I16.
- **Planted bugs.** Replay
  `running_tools_before_their_dispatch_is_durable_fails_the_replay`; DST
  `Planted::DeliverBeforePersist`, `Planted::SkipFsync`,
  `Planted::ActAfterRefusal`; Stateright `Bug::DeliverBeforeDurable`.

### T2. Readers

A client sees only committed records, with no gaps or duplicates across
reconnects, and once caught up its view equals the projection of the log.

- **Checks.** Model-checked. Simulated: I6.
- **Planted bugs.** Stateright
  `serving_records_before_they_are_durable_is_caught`,
  `skipping_a_record_on_reconnect_is_caught`.

### T3. Honest

Every effect, call and task resolves at most once, and a known outcome is
never overwritten. `NotStarted` is never said of anything that may have
acted: after a crash a dispatched `Unsafe` call is `Unknown`, and a cancel
or timeout while it ran is `Unknown`. A run's report is recorded as it
came. Saying `Unknown` of something that never acted is imprecise, not
dangerous, and not promised against.

`NotStarted` exists because the model decides whether to retry from one
question, did it act: yes (`Ok`, `Failed`), maybe (`Unknown`), no
(`NotStarted`). C2 stops v from running an `Unsafe` call twice; T3 stops
the model from asking twice.

- **Checks.** Proved: `result_fits`, `end_fits`, `honest`,
  `lemma_known_outcome_stays`, `lemma_a_task_ends_once`. Model-checked.
  Trace-checked.
- **Planted bugs.** Stateright `Bug::EndTaskTwice`,
  `resolving_an_unsafe_call_known_after_a_crash_is_caught`; replay
  `persisting_unknown_as_not_started_fails_the_replay`,
  `reporting_a_command_stopped_mid_run_as_not_started_fails_the_replay`;
  Verus mutations in their commits.

### T4. Messages

A message is an envelope: yours, another actor's, or a task's end from `v`.
Each is carried by at most one model call, which is told it, and only at a
beat, after every result of the step. It is consumed when that call is
dispatched, so steering counts, and one injected but never seen starts the
next turn. One not yet carried stays in the mailbox. A retried command is
accepted once.

- **Checks.** Proved: `actor_ok`, `lemma_actor_keeps`,
  `lemma_carried_at_boundaries`. Model-checked: `steering_cancels_and_crashes`,
  `steering_across_steps`. Property-tested:
  `actors_recover_and_carry_each_envelope_once`. Simulated: I10, I14.
- **Planted bugs.** Stateright `Bug::CarryTwice`, `Bug::SkipCarry`,
  `Bug::SpliceSteering`; DST `Planted::ConsumeAtInjection`,
  `Planted::UntoldTaskEnds`; Verus mutations in their commits.

### P1. Nothing hangs, nothing runs away

Every turn ends, unless it waits on your approval. Every run has a
deadline (`read`, `edit`, `bash`, `send`, `spawn`) and resolves by it,
`Unknown` if it never reports; timers wait on purpose, and stops are sent
until they land. A limit ends a turn, never an actor. Caps on messages,
turns, tasks, actors and depth mean that without new input everything goes
quiet. Under a storage refusal, progress waits for a boot with room (T1).

- **Checks.** Model-checked: `hung_runs_resolve_by_their_deadline`,
  `chatter_goes_quiet_within_limits`, `an_actor_cycle_goes_quiet`.
  Simulated: I5, I11, I15, `chattering_actors_go_quiet_within_their_limits`.
- **Planted bugs.** Stateright `Bug::NoDeadline`, `Bug::NoActorLimit`,
  `Bug::BlockingSend`, `Bug::LeaveTaskRunning`; DST `Planted::NoDeadline`;
  shell `waiting_on_a_stalled_file_or_delivery_is_caught`.
- **Planned: limits at the beat.** At each beat there's room for the next
  step or the turn ends there, and a turn with no room doesn't start.
  Today a turn can start into a full context and fail at once. Check:
  "within limits" at every beat. Planted bug: a limit checked after the
  step it bounds.

## Design laws

Not promises, but the promises rest on them, and the code must keep them.

- **State is the replay of the log.** `recover` is proved to equal the
  fold of the log.
- **The core is deterministic and sans-io.** Nondeterminism enters only as
  recorded events, through the seams `clippy.toml` enforces.
- **Write before you act.**
- **No actor waits on another.** Only foreground calls and approvals wait;
  a background call resolves at its start.
- **Crash-only.** A broken invariant kills the process; recovery is the
  only way back.

The rest (coalescing, deny-on-steer, the stop cascade, `Safe` reruns, a
well-formed history for the provider) are mechanisms, described in
`AGENTS.md` and the code.

## Not promised

- **The model.** It may be wrong or unsafe. An approval covers the literal
  arguments, not their wisdom.
- **The world.** Nothing is contained. A stopped command's processes may
  live on. v sees only what tools report; the network, git and money are
  invisible to it.
- **Intents.** At most once applies to calls. After `Unknown`, the model
  may issue a new call that repeats the effect.
- **Transactions.** A turn is not one, and concurrent `Unsafe` calls are
  unordered.
- **Conversation.** v delivers messages, and promises nothing about
  answers, or ordering between senders.
- **That every message is read.** A stopped actor, one at a cap, or one
  with a full context keeps its mail unread. A full context parks an actor
  until compaction.
- **Latency.** No bound on how fast a stop lands or a turn ends.
- **Storage.** One machine, one disk, and fsync telling the truth. A full
  disk halts progress (T1).
- **Confidentiality.** What a tool reads may go to the model provider.
- **Old log formats.** The format may change without migration.

## Known holes

- **Approvals are unauthenticated.** Any local process can send one.
- **A rule names actors by name,** and an agent picks its child's name.
- **The rules file is yours to get wrong.** A rule that allows `bash`
  allows it everywhere.
- **A `prefix` rule compares bytes:** `cargo test` also grants
  `cargo test; rm -rf ~`.
- **A rules change reaches a running step at its next beat.**
- **Approved tools can destroy `.verified/`,** the rules file included.
- **Auto-approved reads follow symlinks** out of the project.

## Trusted base

Assumed, not checked: Verus, Z3 and vstd (and the core's two
`external_body` functions); everything outside the core, which is
simulated, not proved; the simulation's fakes, whose gaps `just coverage`
names; the model's picture of the world; tools reporting honestly (the
`Env` contract; `edit` is checked by I7); and fsync and the monotonic clock.

## Deciding new work

- **Touching the world?** Decide first what v reports when it fails. If
  the honest answer is "unknown", say `Unknown` and add no machinery.
- **Adding a promise?** Land its check and planted bug with it.
- **A new feature** starts as Planned text here, naming its check and
  planted bug, before its code.
- **Prefer a smaller promise you can prove** to a larger one you hope
  holds.

Old clause ids: C1 and C2 are unchanged and C3 is unchanged; the old T2
(outcomes) is T3, the old T3 (messages) is T4, the old T4 (readers) is T2;
P2 and P3 are P1, and the old P1 is the design law "no actor waits on
another".
