# Principles

Use this file to decide what verified promises, what it doesn't, and why.
When a design question comes up ("should v handle X?"), the answer starts
here. If code, docs or a tool message claim more than this file does, the
claim is wrong.

## v controls dispatch

v decides what runs, when, with whose approval, at most once, and records
exactly one result for each call. That is the control plane, and it is all
v verifies.

Everything else is the world: files, processes, the OS, the network, the
model and its provider, and anything else running on the machine. v can do
four things to the world, and only these four:

1. **Ask permission** before acting.
2. **Record intent durably** before acting.
3. **Bound how long it waits.**
4. **Report honestly** what it saw, including `Unknown` when it cannot know.

Containment, transactions across calls, exactly-once side effects and making
sure a process is dead all mean managing the world. None of that belongs in
v. When it's needed, it goes behind the `Env` seam as a sandbox, which is
then the thing that controls that part of the world.

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

If a property fails any of these, it is not a promise. It is either a
non-promise (listed below) or a report (v says what happened and makes no
claim beyond that).

A green check that has never failed is not evidence.

## Assurance levels

The layers are not equally strong, so every invariant names its strongest
check.

- **Proved** (Verus): holds for every input.
- **Model-checked** (Stateright): holds in every interleaving within the
  model's bounds, against an abstract world.
- **Simulated** (DST): holds on every seed run, with the real daemon and TUI
  against faked seams. This is evidence, not proof.
- **Property-tested** (proptest): holds on sampled inputs.

Invariants about the core should be proved or model-checked. Shell and view
invariants may rest on simulation.

## Upheld

Every entry passes all four parts of the admission rule.

| # | Invariant | Strongest check | Planted bug |
|---|---|---|---|
| U1 | **Approval before dispatch.** A tool starts only after its call's `Approved` and `Dispatched` records are durable, in the current epoch. | Proved (`call_ok`, `licensed`, `decided`). Simulated (I1). | DST `running_tools_before_their_dispatch_is_durable_is_caught` |
| U2 | **Policy approves only `Safe` calls.** An `Unsafe` call needs an approval command; it is never approved automatically. | Proved (`approver_fits`). Model-checked. | Stateright `approval_by_policy_of_an_edit_is_caught` |
| U3 | **An `Unsafe` call is dispatched at most once, ever.** This holds across crashes, so v never re-runs it on its own. | Proved (`resumed`). Model-checked. Simulated (I3). | Stateright `rerunning_an_unsafe_call_after_a_crash_is_caught` |
| U4 | **After a crash, a dispatched `Unsafe` call resolves `Unknown`.** It is never reported as interrupted or as succeeded. | Model-checked. | Stateright `resolving_an_unsafe_call_known_after_a_crash_is_caught` |
| U5 | **State is the replay of the log.** `recover(log)` equals the fold of the log, and every durable prefix recovers. | Proved (`recover` ensures `replay(log) == Some(core)`). Model-checked. | Mutation recorded in the commit that added this file |
| U6 | **The view is an exact prefix of the durable log.** The client only folds committed records, with no gaps or duplicates across reconnects, and once it has caught up it equals the projection of the log. | Model-checked. Simulated (I6). | Stateright `serving_records_before_they_are_durable_is_caught` and `skipping_a_record_on_reconnect_is_caught` |

## Held, not yet admitted

A deterministic layer checks each of these today, but none has a committed
planted bug. Until one exists, they are not promises.

| Property | Current check | Missing |
|---|---|---|
| Each call has exactly one result: at most one always, and exactly one at quiescence. | Model-checked. Simulated (I2, I5). | A planted bug: a duplicate or missing `ToolResult`. |
| Anything acknowledged survives a crash and replays. | Property-tested (`everything_acknowledged_replays`, `damage_before_a_later_group_is_corrupt`). Simulated (I4). | A committed DST planted bug, such as an append that skips fsync. |
| A message starts at most one turn, so a retried command means the same thing. | `apply` refuses a duplicate message. Simulated (I8). | A planted bug. Is the core's refusal proved, or only checked at run time? |
| An `edit` reported `Ok` wrote `new` in place of `old`. | Simulated (I7). | A committed planted bug (the mutation was only run once). |

## Candidates

Each of these is a property we probably want, but nothing checks it yet.

- **What runs is exactly what was approved.** The arguments a tool runs with
  are byte-for-byte the arguments recorded and shown for approval. This
  holds by construction (the `Approved` token carries the call), but
  nothing checks it end to end.
- **The model gets a well-formed history.** Every `tool_use` sent to the
  provider is paired with exactly one `tool_result`, and none is orphaned.
- **Nothing waits without a bound.** Every in-flight effect completes, times
  out, or is abandoned within a configured limit. Today this follows from
  many separate timeouts; no single check covers it.
- **Results are honest for every tool.** The `edit` case is in the table
  above. `bash` and `read` are not checked.

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

**Time and cost**
- A turn always ends, but it may end `Failed` (provider outage, step limit,
  context limit). v promises no latency, and bounds cost only through its
  step and call limits.
- A message typed into the TUI is lost if the TUI dies before delivering it.
- The log format may change between versions without migration.

## Known holes

Each of these weakens something v appears to claim. Until it is fixed, read
the relevant invariant with this caveat.

- **Approval commands are unauthenticated.** The daemon listens on localhost
  TCP, and any local process can send an approval. U2 proves that policy
  never approves an `Unsafe` call. It does not prove that a person did.
- **Approved tools can destroy v's own state.** `.verified/` lives in the
  project, and an approved `bash` can delete or rewrite it. Recovery then
  refuses the log, which is honest, but the session is gone.
- **Auto-approved reads cross the project boundary.** `read` is `Safe` and
  follows symlinks, so a link can lead it to a secret that is then sent to
  the provider.

## The trusted base

These are assumed, not checked:

- **The verification tools:** Verus, Z3 and vstd, plus the two
  `external_body` functions in the core: `bump`, which crashes on overflow,
  and `CallId::same`.
- **Everything outside the core:** the shell, the TUI and their
  dependencies (tokio, hyper, axum, serde, ratatui) are simulated, not
  proved.
- **The simulation's fakes.** If a fake is kinder than the real seam it
  replaces, the simulation is optimistic. Fake fidelity is the weakest link
  in the chain.
- **The hardware and OS:** fsync tells the truth, the monotonic clock is
  monotonic, and signals are delivered on a best-effort basis.

## Mechanisms

These serve the invariants; they are not invariants themselves. They are
described in `AGENTS.md` and the code.

- Write the log before acting (U1, U3, U5).
- Crash-only: a broken invariant kills the process, and recovery is the
  only way back (U5).
- Epochs on effect ids, so stale completions change nothing (U1, U3).
- TigerBeetle-style ids at every hop: the sender mints the id, persists it,
  and reuses it on retry (the message candidate above).
- Group commit with bounds identical at append and replay (the "survives
  and replays" property above).

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
