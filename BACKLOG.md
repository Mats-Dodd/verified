# Backlog

Ideas, decisions and open questions from design discussions that haven't
landed in `PRINCIPLES.md`, the API draft (`docs/rfcs/0002-api.md`) or
the code. Nothing here is scheduled.
Each item is marked:

- **Leaning:** discussed, with a direction we like, not yet decided.
- **Decided:** agreed in discussion, waiting to be written into the docs or
  code.
- **Open:** a real question, with no direction yet.
- **Next:** decided, and the next thing to design or build.
- **Idea:** worth remembering, not yet discussed.

When an item lands, delete it from here. The commit that lands it carries
the reasoning.

## 1. What v is

- **An agent runtime, not a harness.** *Decided.* The model is an untrusted,
  nondeterministic program; its tool calls are system calls; your grants
  are its capabilities; the log is the journal. Already in `PRINCIPLES.md`.
- **A set of verified building blocks: an SDK first, a coding agent
  second.** *Leaning.*
  - The kernel is small and closed.
  - Everything else plugs in at seams with written contracts.
  - The local coding agent (daemon plus TUI) is one assembly of the
    blocks.
  - "Scripted" just means: there is an SDK. A program driving agents is a
    client of the runtime, like the TUI. No special scripting layer, and no
    "scripted decider".
- **Lineage:**
  - TigerBeetle: safety, bounds, reliable submission, the beat.
  - FoundationDB and s2: DST.
  - Erlang: mailboxes, links.
  - Temporal and Restate: the journal, idempotency keys, durable timers.
  - io_uring: never block; completions arrive on a queue.
  - Event sourcing: the log is the database.
  - OS kernels and capabilities: grants.
  - Elm, sans-io and Effect-TS: a pure core with effects as data.
  - AWS, CCF and Anvil: layered formal methods.

  **What makes v different:** the program is an LLM.
  1. It can't be replayed, so v journals its *decisions* rather than
     re-running code.
  2. It's untrusted, so an authority layer sits between it and the world.
  3. Failures are shown to the program (`Unknown`), not hidden by blind
     retries.
  4. A human is part of the topology.
- **Prior art to keep in view:**
  - **Pi Durable** (Earendil). The closest design: `replay: safe|unsafe`,
    the effect sandwich, `requestId`, steer/followUp, storage and env
    conformance suites, and it runs on Durable Objects. Its warts are
    sub-agent boilerplate and no approval layer.
  - **Claude Managed Agents.** Durable approvals as server-side events, but
    no stream cursor (list and dedupe instead).
  - **OpenAI Agents API.** A first-class `Idempotency-Key`, but no event
    replay.
  - **opencode.** An OpenAPI plus SSE boundary, declarative permission
    rules, and plugins that mutate in place (an anti-pattern).

## 2. Promises (`PRINCIPLES.md`)

- **"Env contract" becomes "Tool contract".** *Decided,* following the
  removal of `Env` (section 4).
- **The control/data-plane rule.** *Leaning.*
  - The control plane depends on the data plane only through measures
    (bytes), tool names and declared scopes, never through content.
  - The `Payload` generic is the boundary: `Record<Abstract>` is the
    control plane.
  - Review question for every change: does the core *read* this field (so
    it's control, and gets proved), or only carry it (so it's a payload
    slot)?
- **The one crossing: rules matching on arguments.** *Leaning.*
  - Tools declare `scope(args) -> Scope`, a small typed control-plane
    projection (paths, host, program name).
  - Rules match on `Scope`, never on raw `Args`.
  - α preserves `Scope`.
- **Build the two Planned parts of the ten promises.** *Decided.*
  - **C1, what runs is what you granted:** the fake `Env` compares every
    run's arguments with the `Approved` call in the log, on every run.
    Planted bug: a shell that swaps a call's arguments after approval.
  - **P3, the beat:** check headroom for the next step at every step
    boundary and end the turn there when there is none; don't start a turn
    with no room for its first step (today a turn starts into a full
    context and fails at once). Planted bug: a limit checked after the
    step it bounds.
- **Decided against, in the ten-promise round:**
  - **A red-button promise** ("a stop takes effect within a bound"). Not a
    promise; listed under Not promised.
  - **"No agent gains authority"** as its own clause. It follows from C1
    (grants come only from you), and isn't worth its own check.
  - **A limit ending the actor.** A limit ends the turn; the actor lives,
    and its mail waits (T3: at most one model call, never dropped).
- **The beat.** *Leaning,* now part of P3. TigerBeetle's lesson: run at
  your own pace and batch; don't react to each event.

## 3. Public API (`docs/rfcs/0002-api.md`)

- **Remove `Env`.** *Decided.*
  - A `Tool` is the only seam that touches the world.
  - There are two kinds: native Rust tools, and `McpTool` (an adapter
    making any MCP server's tools v tools).
  - A sandbox is just an MCP server offering `bash` and `edit`.
  - The local agent's `read`, `edit` and `bash` stay native in
    `verified-std` (*Leaning*), with a sandbox a one-line swap.
- **The MCP mapping.** *Leaning.*
  - The runtime calls a server only after a durable grant, and the server
    authenticates the runtime, not each call.
  - Class defaults to `Unsafe` and is configured per tool to `Safe` or
    `Keyed`.
  - MCP's `readOnlyHint`/`idempotentHint` are claims: shown as
    suggestions, never applied silently.
  - The idempotency key goes in `_meta`.
  - Progress notifications become `CallOutput`, and cancellation
    notifications are the cancel signal.
  - After a crash: `Unsafe` → `Unknown`, `Safe` → re-run, `Keyed` → call
    again with the same key.
- **`MaybeSend` bounds on every seam.** *Decided in principle.* WASM
  futures on Workers are `!Send`, so seams declared `Send + Sync` can't be
  implemented there. Do it before anything is built against `Send`.
- **`Tool::scope`.** *Leaning* (see section 2).
- **Tag each `Body` and `Event` variant with its plane.** *Idea.*
- **Wire representation is part of the contract.** *Leaning.*
  - Deliberate serde tags (`type`) and `snake_case` names.
  - Generate JSON Schema from the vocabulary (`schemars`) and snapshot it
    in CI next to `cargo public-api`.
- **Open: shutdown.** `Local::shutdown` is "a crash you can wait for":
  stop intake, signal once, wait a bounded time, release storage, write
  nothing. Confirm when implementing that it adds no second recovery path.
- **Open: should actors ever `follow` other actors?** The default is no:
  information enters a model only as envelopes (T3). It could become a
  `Safe` read tool later.
- **Structured message bodies** between agents. *Idea,* as a future `Body`
  variant (a deliberate breaking change).
- **Authenticated principals.** *Leaning.* The transport assigns a
  principal per connection, and the runtime stamps it like the sender.
  Closes the "unauthenticated approvals" hole without a vocabulary change.

## 4. Verification at the interfaces

The public interfaces are the verification boundaries. Assume-guarantee
across them. *Leaning,* all of it.

- **Traced plugin wrappers** (`Traced<S: Storage>`, `Traced<T: Tool>`, and
  so on) record every interface crossing. They replace the `cfg(sim)` hooks
  inside the shell, so test instrumentation leaves production code.
- **The model's alphabet becomes the seam crossings,** `Log::append` and
  its result, `Tool::run` and its report, `Model` parts, `submit`, events,
  instead of internal names (`Sync`, `Execute`). The model then specifies a
  correct runtime at its interfaces, and checks any implementation: the
  native shell, a Durable Object shell, anyone's.
- **Promise monitors over `Event`.** Each promise is also stated as a
  predicate over the public event stream ("every `CallStarted` is followed
  by exactly one `CallResolved`"), shipped as a monitor anyone can run over
  any stream, including production logs (PObserve-style). Needs a proof or
  proptest that the event projection preserves each property.
- **Conformance suites plus planted fakes** for every seam, in
  `verified-testkit`. Plugin authors run exactly what we run.
- **Compile-fail tests** (`trybuild`) for kernel-only types.
- **The limit:** interface verification is only as strong as the
  contracts, so each contract needs its planted fake.

## 5. Other languages

*Leaning.* Order: 1, 2, 3, and 4 only on demand.

1. **Freeze the wire's serde representation;** generate and snapshot the
   JSON Schema.
2. **Client SDKs (TypeScript, Python):**
   - generated types: discriminated unions with exhaustive checks;
   - a hand-written client of about 150 lines (`submit` with the mandatory
     retry loop: mint the id once, resend on `Unavailable`; `follow` with
     reconnect by offset);
   - language-neutral golden fixtures every client replays.
3. **Plugins in other languages are MCP.** The "remote tool protocol" is
   MCP plus `_meta.key`; there's nothing to invent. Storage plugins stay
   Rust-only.
4. **Embedding** (`napi-rs`/`PyO3`/`uniffi`). Hard part: host-language
   plugins through FFI callbacks. Only on demand.

## 6. Cloudflare Durable Objects

*Idea, explored.*

- **The mapping:**
  - actor = DO, with `idFromName(actor_id)`, and `V` its own DO;
  - storage = DO SQLite;
  - timers = alarms (one per object: multiplex the earliest deadline;
    at-least-once is fine);
  - delivery = stub calls;
  - `follow` = SSE or hibernatable WebSockets;
  - idle actors hibernate (virtual actors for free).
- **Output gates make "ack means durable" free.** A failed write restarts
  the object, which is fail-stop.
- **Input gates close only during storage operations,** not while awaiting
  `fetch`. So the rule is: **the critical section (decide → append → apply →
  start effects) awaits nothing but storage.** Effects are spawned, not
  awaited. Needs a test.
- **A thinner shell:** the platform does the serializing. The kernel
  compiles to WASM unchanged. Confirm `vstd`, `uuid` and `serde` actually
  build for `wasm32`; not yet checked.
- **Tools:** MCP over HTTP and native HTTP tools only. There's no
  filesystem and there are no processes in Workers.
- **Gotchas:**
  - deploys restart every DO, so every in-flight `Unsafe` call becomes
    `Unknown`;
  - limits: about 128 MB per isolate, CPU time per request, storage per
    object;
  - there are no cross-object transactions, and v never needs them.
- **Design constraint now:** keep the shell's job exactly decide → append →
  apply → start effects, with no tokio types in any seam.

## 7. Runtime and actor design, still open

- **Erlang-style restart policies** for supervision ("restart this child if
  it crashes"). *Idea.* Composes with `Keyed` spawn.
- **Restate-style durable promises,** for rendezvous between actors without
  blocking. *Idea.* Possibly cleaner than ad hoc `reply_to`.
- **Virtual actors as an explicit design point:** an actor with an empty
  mailbox costs nothing and is woken by a message. *Idea.*
- **Context compaction.** *Next.*
  - The model's context grows without bound for long-lived actors (the LSM
    analogy: recent turns as the mutable table, summaries as older levels).
  - v owns *when* and the guarantee of headroom; the summary's quality
    isn't promised.
  - Deserves its own design round.
- **Storage-engine work.** *Idea, later.*
  - checkpoints per bar (bounded recovery time);
  - log compaction and retention;
  - big data-plane values stored out of line (a blob by content hash, with
    `{hash, bytes}` in the control record);
  - headroom on the log once v owns its storage.
- **A VOPR-style liveness mode.** *Leaning.* Run the simulation, heal every
  fault, and assert the system goes quiet within a bound. It's the check P2
  and P3 need.
- **A full context parks the actor.** *Decided* (in `PRINCIPLES.md`, Not
  promised): its turns end at the first beat and its mail waits. Compaction
  unparks it.
- **The default rules.** *Open.* The default is exactly `read: allow`, so
  `wake`, `tasks`, `cancel`, `send` and `spawn` all ask unless
  `.verified/v.toml` allows them. The draft API proposes dropping defaults
  altogether. Your call.
- **The `Recovered` event carries nothing.** *Decided.* Revisit if
  supervising programs need more.

## 8. Testkit as a product

**Users simulate their own setups.** *Open.*
- `Runtime` is a trait a simulated runtime could implement: turmoil,
  scripted models, crash injection, and replay through the model checker.
- Users could then test their own tools, rules and agent topologies
  deterministically. Nobody else offers this.
- Not committed to.

## 9. Known holes and loose ends

**Holes**
- **Approvals are unauthenticated.** Fix via principals (section 3).
- **`.verified/` is deletable by an approved `bash`.** That now includes
  `.verified/v.toml`, the rules file, by the author's choice of location.
- **Auto-approved reads follow symlinks** out of the project.
- **Losing `V`'s log loses the actor index.** It's the root of recovery. A
  non-promise, to be listed.

**Fidelity gaps**
- **Orphans never act in the sim.** Real `bash` children outlive the
  daemon, but the fake never lets an orphaned run act after a crash.
- **The fake model never loops.** No seed reaches a limit (context, steps,
  calls, turns), so a wrong limits wiring would go unnoticed. One directed
  scenario covers the step bound.

- **No fake fills a disk.** Full-disk fail-stop is tested with a log that
  refuses the way the file log does; the file log cutting a partial write
  back off is exercised by no test.
- **The model can't stop a non-root actor.** Spawn honesty after a stop is
  covered only by a shell test.

**Checks**
- "`Safe` call ran, then interrupted" is probably an over-approximation,
  not a test gap. Reclassify it.
- The replay checks stream offsets, not content. Content is I6's job.

**Tests**
- `timeout_kills_and_output_is_bounded` (real IO) is flaky under load.

**Loose ends**
- The log line limit dropped from 8 MiB to 1.75 MiB for the stream byte
  bound. Revisit now that the full-log reserve is gone.
- The TUI's approval hint (`y allow · n deny · esc cancel turn`) doesn't
  say that typing steers.

## 10. Process notes

- **Agents reviewing agents worked, with gaps.** The adversarial review
  found 11 real bugs that Verus, Stateright, DST and proptest all missed.
  They lived where the layers don't look: resource limits, the release
  panic profile, client ergonomics, file mode.
- **Every review ran mutations to show a check bites.** Keep doing that;
  green checks that never failed misled more than once.
- **Steering a running workflow through the files its agents re-read**
  worked with no restart. Agents commit in the shared tree, so avoid
  committing there while a workflow runs.
