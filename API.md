# API

This file is the contract between v and everything built on it: programs
that drive agents (the SDK), plugins that extend the runtime (storage,
tools, models), and contributors, who must not widen the surface by
accident. `PRINCIPLES.md` says what v promises. This file says through
which types, and what each plugin must promise back. Together they are
assume-guarantee: v upholds its promises provided every plugin keeps its
contract.

If code exposes something this file does not, the code is wrong.

## Status

Each section carries a status, as in `PRINCIPLES.md`:

- **Stable:** built, and changed only deliberately.
- **Built:** exists and may still change.
- **Planned:** designed, not built. It is the spec for the reshaping pass
  that follows the runtime build.

Today almost everything here is **Planned**. The current code exposes an
earlier shape: typed HTTP routes per command, a server `Shell` handle, and
records on the wire as AI SDK chunks.

## Rules

1. **Vocabulary only.** Every public signature takes and returns types
   from `verified::vocab`, or the kernel-only types named below. There are
   no DTOs and no per-layer translations. The in-process API, the wire, the
   TUI, tests and agents' own effects all use the same types.
2. **Parse at the edges.** Untrusted data enters in exactly two places, the
   model's output and wire decoding, and is parsed into the vocabulary
   there. Nothing past an edge sees unparsed data.
3. **Erase into validated types.** Where types must be erased (a
   third-party tool's arguments and output in the log), they are erased
   into a vocabulary type that can only be created by validating it, and
   only the tool registry turns it back.
4. **Encodings are never contracts.** File framing, JSON on the wire and
   TOML for rules belong to an implementation, never to a signature.
5. **Behaviour is a trait, policy is data.** Code that touches the world
   plugs in at a seam. Decisions about authority (rules, agent specs) are
   data, recorded in the log.
6. **Extension is additive and explicit.** A new capability is a new
   `Body` or `Event` variant, a new world is a new `Tool`, new durability is
   a new `Storage`. Nothing hooks into the kernel.
7. **Exhaustive enums.** `Body`, `Event`, `Outcome` and the other public
   enums are matched exhaustively. Adding a variant is a breaking change,
   made deliberately. v breaks clients rather than letting them miss a
   case.
8. **Products depend on the handle, never on internals.** If the TUI needs
   something, it is an `Event` or a `Body`, or the vocabulary is missing
   it.

## Vocabulary (`verified::vocab`): Planned

### Identity and position

| Type | Meaning |
|---|---|
| `Id` | An idempotency key, minted by whoever sends (uuid7). Every input carries one. |
| `Address` | `User`, `V` (the runtime itself), or `Actor(ActorId)`. An `ActorId` is derived from the spawn's `Id`, so a spawner knows the address before the spawn is acknowledged, and a retry needs no lookup. |
| `Offset` | An opaque, ordered position in one actor's event stream. `Offset::START`. No arithmetic: every event carries its own offset, and you resume *after* one. |
| `TurnId`, `CallId`, `TaskId`, `Step` | The things events refer to. `Step` is an index within its turn. |
| `ToolName`, `ModelName`, `RuleId`, `RulesVersion` | Names, checked at construction. |

### Data

| Type | Meaning |
|---|---|
| `Text` | Bounded, measured text. Construction checks the bound. |
| `Args` | A tool's arguments, validated against the tool's schema. `.json()` views them, and `.decode::<T>()` gives back `T::Args`. |
| `Output`, `Details` | A tool's structured result for programs, and its result for UIs. Validated, with the same views. |
| `Summary` | What a call means, in one line, for people. |
| `Usage` | Token counts for one model step. |

### In: what you submit

```rust
pub struct Envelope { pub id: Id, pub to: Address, pub body: Body }

pub enum Body {
    // Data plane: enters the model at the next beat (a step boundary).
    Message { text: Text, reply_to: Option<Id> },
    // Control plane: the kernel acts on it at once.
    Answer  { call: CallId, verdict: Verdict },   // Allow { remember: Scope } | Deny
    Cancel  { target: Target },                   // Turn(TurnId) | Task(TaskId) | Call(CallId) | Actor
    Use     { model: ModelName },                 // switch this actor's model from its next step on
    Spawn   { spec: AgentSpec },                  // to: V
    Rules   { rules: Rules },                     // to: V; replaces all rules
}

pub struct AgentSpec { pub name: Text, pub prompt: Text, pub model: ModelName, pub tools: Vec<ToolName> }
```

- **An envelope names exactly one log,** the `to` actor's. `Spawn` and
  `Rules` go to `V`, whose log is the actor tree and the policy history.
- **The sender is never in the envelope.** The runtime stamps it: every
  SDK client is `User`, and an agent's own effects are stamped with that
  agent's address by the kernel. Authority cannot be self-declared. When
  approvals are authenticated (a known hole), the transport assigns a
  principal per connection and the runtime stamps it the same way.
- **An agent's `send` and `spawn` tools produce these same envelopes.**
  Agents address each other exactly as you address them.
- **Models are switchable, per actor.** A runtime holds any number of
  named models. `AgentSpec.model` is where an actor starts, and
  `Body::Use` switches it at the next beat (step boundary), so a turn can
  span models. An agent can spawn another on a different model. Each step
  records the model it used.
- `Rules` is data: `Rules::allow_all()`, `Rules::from_toml(&str)`, or a
  typed builder. `Verdict::Allow { remember }` records a rule, so a
  remembered approval is just another rule.

### Out: what you follow

```rust
pub enum Event {
    // mailbox (data plane)
    Received        { id: Id, from: Address, text: Text, reply_to: Option<Id> },

    // turns and steps
    TurnStarted     { turn: TurnId, consumed: Vec<Id> },
    Steered         { turn: TurnId, consumed: Vec<Id> },
    StepStarted     { turn: TurnId, step: Step, model: ModelName },
    Text            { turn: TurnId, delta: Text },
    Reasoning       { turn: TurnId, delta: Text },
    Retrying        { turn: TurnId, fault: Fault, after: Millis },
    StepEnded       { turn: TurnId, step: Step, usage: Usage },
    TurnEnded       { turn: TurnId, outcome: TurnOutcome },

    // calls
    CallProposed    { call: CallId, tool: ToolName, args: Args, summary: Summary },
    CallGranted     { call: CallId, by: Grant },
    CallStarted     { call: CallId },
    CallOutput      { call: CallId, chunk: Text },
    CallResolved    { call: CallId, outcome: Outcome },

    // tasks (background calls)
    TaskStarted     { task: TaskId, call: CallId },
    TaskEnded       { task: TaskId, outcome: Outcome },

    // control
    CancelRequested { id: Id, target: Target },
    ModelChanged    { id: Id, model: ModelName },

    // the runtime
    Recovered,

    // only in V's log
    Spawned         { address: Address, spec: AgentSpec, by: Address },
    Stopped         { address: Address },
    RulesChanged    { version: RulesVersion },
}

pub enum Outcome {
    NotStarted(Why),                                    // Denied | Cancelled | Limit(Limit) | Invalid(Reason)
    Ok(Report),
    Failed(Report),
    Unknown { why: Uncertain, partial: Option<Report> },// Crashed | Cancelled | TimedOut
}
pub struct Report { pub for_model: Text, pub structured: Option<Output>, pub details: Option<Details>, pub truncated: bool }
pub enum TurnOutcome { Answered { answer: Text }, Cancelled, Failed(Fault) }
pub enum Grant { Rule(RuleId), Answer(Id) }
```

The rules for events:

1. **Facts, in the past tense, and only committed ones.** Every event is
   projected from a durable record, so a follower's view is an exact prefix
   of the log (L2). Nothing speculative is shown.
2. **Impossible states cannot be built.** `NotStarted` has no report,
   because it never ran. `Unknown` may carry partial output, because it may
   have run. `CallStarted` is the exact point after which "may have acted"
   is true.
3. **Linked by ids,** so clients join events without relying on order.
4. **Control envelopes appear as what they caused.** An `Answer` shows up
   as `CallGranted { by: Answer(id) }` or `CallResolved { NotStarted(Denied) }`,
   a `Cancel` as `CancelRequested`. Only messages appear as `Received`.
5. **`Retrying` means: discard this step's text and reasoning.** The step
   starts over.
6. **`TurnEnded` carries the answer** (the final step's text), so an SDK
   program waits on one event.
7. **`CallOutput` is committed output,** batched like text and bounded by
   the same limits, so a long command shows progress honestly.
8. **A crash is visible but abstract.** `Recovered`, then
   `CallResolved { Unknown { why: Crashed } }` for whatever was in flight.

Events are a pure projection of the kernel's private records. The
projection lives in the domain crate and is property-tested to be total
and order-preserving.

## The handle (`verified::runtime`): Planned

```rust
pub trait Runtime: Send + Sync {
    async fn submit(&self, envelope: Envelope) -> Result<Accepted, Refusal>;
    fn follow(&self, actor: Address, after: Option<Offset>) -> Follow;  // Stream<Item = Result<(Offset, Event), Lag>>
}

pub struct Accepted { pub at: Offset }
pub enum Refusal { Rejected(Rejection), Unavailable }
```

Two verbs: you **submit** an envelope, and you **follow** an actor's
events. Every capability is one of the two.

**`submit`**
- Returns once the envelope is durable in the `to` actor's log. `at` is
  where it landed, so `follow(to, Some(at))` shows exactly what it caused.
- **One acceptance per id, ever,** and acceptance is stable: retrying an
  accepted id returns the same `at`.
- `Rejected` means nothing happened, deterministically (an unknown or
  stopped actor, a limit, no headroom, an invalid body). Rejections are not
  recorded, so a retry is evaluated again.
- `Unavailable` means you cannot know whether it landed. Resend the same
  id.
- `submit` never blocks waiting for room. No headroom is a rejection.

**`follow`**
- Per actor only. There is no merged stream, because v keeps no global
  order. The actor tree is `follow(Address::V, …)`.
- Every item carries its own `Offset`. Reconnect with `after` set to the
  last offset you handled.
- `Lag` means the follower fell behind a bound. Resume from your last
  offset. The runtime never drops or merges events silently.

**Implementations**
- `Local::open(Plugins).await` runs the kernel in your process. It reads
  `V`'s log to find every actor and recovers each one.
- `local.shutdown().await` is **a crash you can wait for**: it stops
  intake, signals every running effect once, waits a bounded time, and
  releases storage. It writes nothing. The next `open` recovers exactly as
  after a crash. It returns `Shutdown`, which lists what was still running.
- `Remote::connect(url).await` speaks the same trait over the wire.

Code written against `dyn Runtime` cannot tell which it has. The TUI, the
CLI, your programs and the simulation all hold one.

**Views**
- `ActorView` is the standard fold over one actor's events: status,
  transcript, calls, tasks. `ActorView::apply(&event)`.
- Write your own fold for anything else. There is no third verb.

## Composition: Planned

```rust
pub struct Plugins {
    pub storage: Arc<dyn Storage>,
    pub models:  Models,          // named: Models::new().with("main", Anthropic::from_env()?)
    pub tools:   Tools,           // Tools::new().with(Read).with(Edit); duplicate names refused
    pub rules:   Rules,           // explicit, never defaulted: Rules::allow_all() if you want auto
    pub clock:   Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
}

let runtime = Local::open(verified_std::defaults(project_root)?).await?;
```

Every field is required, so a missing plugin is a compile error. Authority
is explicit: there is no hidden default policy. `verified_std::defaults`
fills every field for a local coding agent.

## Seams (`verified::plugin`): Planned

Each seam has a contract, a conformance suite in `verified-testkit`, and
a planted fake the suite must catch. The last column says which
`PRINCIPLES.md` promises rest on the contract: a plugin that breaks it
voids those promises.

| Seam | Contract | Planted fake the suite catches | Promises that rest on it |
|---|---|---|---|
| `Storage` / `Log` | An acknowledged `append` is durable. `replay` returns exactly the acknowledged groups, in order, then possibly a prefix of one more. A refusal is fail-stop. | A log that acknowledges before it is durable. | L1, L3, and through them A, O, H |
| `Tool` | The declared `CLASS` is true. A `Keyed` tool passes `ctx.key()` to its target. The report claims no more than the tool knows. | A tool declared `Safe` that is not. | O1, O2, H2 |
| `Model` | None. It is untrusted. | (none) | (none) |
| `Clock` | Monotonic. | A clock that goes backwards. | B2 |
| `Entropy` | Unpredictable outside simulation. | (none) | (none) |

### Storage

```rust
pub trait Storage: Send + Sync {
    async fn open(&self, actor: &Address) -> Result<Box<dyn Log>, Unavailable>;
}
pub trait Log: Send {
    async fn append(&mut self, group: &Group) -> Result<(), Refused>;
    async fn replay(&mut self) -> Result<Replayed, Corrupt>;
}
pub struct Replayed { pub groups: Vec<Group>, pub torn: Option<Group> }
```

A `Group` is one decision's records. It is opaque: storage persists it
through serde, in any format, without seeing inside. It offers `Eq`,
`len()` and `is_prefix_of`, which is all the contract and its conformance
suite need. The runtime frames, checksums and orders. Storage only has to
get durability right.

### Tool

```rust
pub trait Tool: Send + Sync + 'static {
    const NAME: ToolName;
    const CLASS: Class;                       // Safe | Unsafe | Keyed
    type Args: Schema;
    type Output: Schema;
    fn description(&self) -> &Text;
    fn summarize(&self, args: &Self::Args) -> Summary { /* default: the arguments, pretty-printed */ }
    async fn run(&self, grant: Grant<Self::Args>, ctx: ToolCtx) -> Report<Self::Output>;
}
```

- **A tool sees its arguments only through `grant.args()`,** and a `Grant`
  can only be made by the kernel. So a tool cannot run without
  authorization, and it runs with exactly the arguments that were granted.
  That holds by type.
- `ToolCtx` offers `key()` (a stable idempotency key), `cancelled()`,
  `deadline()`, `output(chunk)` (committed as `CallOutput`) and `clock()`.
  Tools never use the real clock directly.
- `Report<O>` is `Ok`, `Failed` or `Unknown`, each with the three channels
  (for the model, structured, for the UI). A tool cannot report
  `NotStarted`. Only the kernel can, because only it can prove it.
- `summarize` is pure. It cannot touch the world before a grant.
- The standard tools (`read`, `edit`, `bash`) live in `verified-std` and
  act through an `Env`. That is their implementation detail: a sandbox is a
  different `Env` handed to them, not a public seam.

### Model

```rust
pub trait Model: Send + Sync {
    async fn call(&self, request: &Request, ctx: ModelCtx) -> PartStream;  // Stream<Item = Result<Part, Fault>>
}
pub enum Part {
    Text(Text),
    Reasoning { text: Text, seal: Option<Seal> },
    Call { id: CallId, tool: ToolName, args: RawArgs },
    Usage(Usage),
    End(EndReason),
}
```

- `Request` is read-only: `.history()` in v's own turn and message
  vocabulary, `.tools()`, `.max_tokens()`. Only the kernel builds one, so
  the history a model sees is always well-formed (L5). Each `Model` maps it
  to its provider's format.
- `RawArgs::new(json)` is the parse edge. The kernel validates the
  arguments against the tool's schema at once. If they don't fit, the call
  is recorded as invalid and resolves `NotStarted(Invalid)`.
- `Seal::new(token)` holds provider data that must be sent back untouched
  (reasoning signatures, for example). It is opaque to v, and it records
  which model minted it. Because an actor can switch models mid-history,
  `Request::history()` hands each model only the seals it minted. Another
  provider's seals are dropped, and their reasoning text is kept.
- Faults are typed (`Fault::{Overloaded, Network, Refused, …}`). The
  kernel decides retries.

### Clock and entropy

```rust
pub trait Clock: Send + Sync { fn now(&self) -> Instant; async fn sleep(&self, duration: Millis); }
pub trait Entropy: Send + Sync { fn fill(&self, buf: &mut [u8]); }
```

## Kernel-only types: Planned

These appear in public signatures, but nothing outside the kernel can
construct them. Each protects a promise. Each is pinned by a compile-fail
test (code that tries to construct it outside the kernel must not
compile), which is the admission rule applied to a promise the type system
keeps.

| Type | Protects |
|---|---|
| `Grant<A>` | A1: nothing runs without a grant, and it runs with exactly the granted arguments. |
| `Request` (and its history) | L5: the model's history is well-formed. |
| `ToolCtx`, `ModelCtx` | Idempotency keys, deadlines and committed output come only from the kernel. |
| `Args`, `Output`, `Details` | Rule 3: erased data is always validated. |
| The sender stamp | Authority is never self-declared. |

## Parse edges: Planned

1. **Model output.** `Part::Call { args: RawArgs }` is parsed against the
   tool's schema when the kernel receives it: valid becomes `Args`, invalid
   becomes `NotStarted(Invalid)`. `Seal`s pass through untouched.
2. **Wire decode.** The server decodes `Envelope`s and the client decodes
   `(Offset, Event)` into the same types, or refuses them.

## The wire (`verified::wire`): Planned

```
POST /submit                           Envelope → Accepted | Refusal
GET  /actors/{address}/events?after=   server-sent events of (Offset, Event)
```

- Two endpoints. A new capability is a new variant, not a new route.
- A protocol version is exchanged in the handshake and recorded in the
  first record of every log. A mismatch is refused. There is no migration
  (`PRINCIPLES.md`: the log format may change between versions).
- UI projections (for example, AI SDK chunks for a web client) are library
  functions over `Event`, not part of the protocol.

## Crates and the dependency rule: Planned

| Crate | Depends on | Holds |
|---|---|---|
| `verified-domain` | nothing | the vocabulary, the private records, the event projection |
| `verified-core` | domain | the kernel (Verus) |
| `verified-runtime` | core | `Runtime`, `Local`, the seams, `Plugins`, the shell |
| `verified-wire` | domain, runtime | `Remote` and the HTTP server: the vocabulary, serialized |
| `verified-std` | runtime | `FileLog`, the standard tools over `LocalEnv`, the Anthropic model, `defaults` |
| `verified-testkit` | runtime | a fake for each seam, the conformance suites, the planted fakes, the simulation, the Stateright model |
| `verified-tui`, `verified-cli` | wire, std | the product: a client of `dyn Runtime` |

Products depend on the handle. Nothing outside `verified-runtime` and
`verified-core` sees a record.

## Evolving the surface

A public item lands only with all of these:

| Requirement | Enforced by |
|---|---|
| The surface changes deliberately | a `cargo public-api` snapshot in CI; any change is a reviewed diff |
| Kernel-only types stay kernel-only | compile-fail tests (`trybuild`) |
| Seam contracts are testable | a conformance suite and a planted fake it catches |
| Examples stay true | every code block here is mirrored by a compiled example or doctest once built |

## Not in the API

- **Hooks into the kernel.** A veto is a rule, observation is `follow`,
  and input is never rewritten after it was granted.
- **A merged event stream.** v keeps no global order.
- **Records, epochs, groups' contents, sequence numbers, call indexes, the
  payload generic, barriers, shims.** Kernel and simulation internals.
- **Raw bytes or untyped JSON in a signature,** except at the two parse
  edges.
- **Agents following other actors' logs.** Information enters a model only
  as envelopes, consumed exactly once (L4). An agent may get a `Safe` read
  tool for this later; it can't be taken away once agents depend on it.
- **Serialized run state.** The log is the state, in storage you chose.
- **A graceful-shutdown record.** `shutdown` is a crash you can wait for.

## Future

- **Simulating your own setup.** `Runtime` is a trait a simulated runtime
  can implement (turmoil, scripted models, crash injection, replay through
  the model checker), so users could test their own tools, rules and agent
  topologies deterministically. Not committed to yet.
- **Structured message bodies** between agents, as a new `Body` variant.
- **Authenticated principals** on the wire, stamped like the sender.
- **Snapshots** for long logs, as storage-engine work.
