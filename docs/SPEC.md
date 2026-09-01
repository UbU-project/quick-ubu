# Quick UbU — v1 Schema & Operation Contracts

The buildable seed. Two objects that carry state (**Task**, **Handle**), the
ontology and preference structure that make re-planning better than naive
prompting, and two operations that move the system (**`re_plan`**, **`defer`**).
Everything else you specified rides on this without changing it.

Three organizing principles, stated once:

1. **Tier is one axis doing three jobs** — privacy, context-scoping, and
   compute-routing. There is no separate "privacy layer"; it's a label plus a
   filter at the egress point.
2. **The Handle is the universal redaction primitive.** Any task *above* a
   compute target's clearance is represented to that target by its Handle: a
   content-free occupied block the plan must schedule around but may not open.
   A top-secret Handle is byte-indistinguishable from any other Handle — content
   is hidden, existence is not (accepted v1 tradeoff).
3. **The operator is the serialization point, and authority splits by tense.**
   One body of attention cannot issue two conflicting commands at once, so there
   is no concurrency machinery, no CRDT, no causality DAG. The **operator owns
   the past** — what you did while offline has exactly one witness, you, so the
   desktop *records* your actuals and never re-litigates them. The **desktop owns
   the future** — it *resolves* commands and re-plans authoritatively over the
   resulting state. Mobile captures, views, logs actuals, and plans provisionally;
   its provisional *plan* is discarded on reconnect, but the *facts and commands*
   it logged are replayed, not discarded.

---

## 1. Core types

```rust
use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;
use std::collections::{BTreeSet, BTreeMap};

/// Opaque. Tier is NOT encoded in the id — the id appears on both a full Task
/// (desktop) and its Handle (mobile), and must reveal nothing by itself.
pub type Id = Uuid;

/// Restrictiveness axis. SemiPublic is the LOWEST (most shareable) level;
/// TopSecret the HIGHEST. "Details cannot go lower than the given level."
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tier { SemiPublic, UserShared, TopSecret }

impl Tier {
    /// SemiPublic=0, UserShared=1, TopSecret=2
    pub fn level(self) -> u8 {
        match self { Tier::SemiPublic => 0, Tier::UserShared => 1, Tier::TopSecret => 2 }
    }
}

pub struct TimeWindow { pub start: DateTime<Utc>, pub end: DateTime<Utc> }
```

### 1.1 The clearance rule (the spine)

A **compute target** has a clearance — the highest tier whose *content* it may
see. Content of a task is visible to a target iff the task's tier is at or below
that clearance; otherwise the task is presented as a Handle only.

```rust
pub enum ComputeTarget { DesktopOllama, HostedLlm }

impl ComputeTarget {
    pub fn clearance(&self) -> Tier {
        match self {
            ComputeTarget::DesktopOllama => Tier::TopSecret,   // sees everything
            ComputeTarget::HostedLlm     => Tier::SemiPublic,  // sees only semi-public
        }
    }
}

/// True  => task enters the context as full content (schedulable, rearrangeable).
/// False => task enters as a Handle (fixed occupied block; content never crosses).
pub fn visible_as_content(task_tier: Tier, clearance: Tier) -> bool {
    task_tier.level() <= clearance.level()
}
```

Consequences, all forced rather than chosen:

- Desktop ollama (TopSecret clearance) can rearrange every tier.
- A hosted LLM (SemiPublic clearance) sees only semi-public content; user-shared
  **and** top-secret appear as fixed Handles — which is exactly why offline
  mobile "cannot plan out even user-shared tasks."

---

## 2. Objective

Objectives are what you want completion estimates *for*. The ETA is never stored
here — it is transient Plan output (utility has no persisted preimage; neither
does an ETA).

```rust
pub enum ObjectiveStatus { Active, Done, Dropped }

pub struct Objective {
    pub id: Id,
    pub tier: Tier,
    pub title: String,
    pub detail: Option<String>,
    pub target_date: Option<DateTime<Utc>>, // soft target, optional
    pub status: ObjectiveStatus,
}
```

---

## 3. Task (full-detail, desktop-authoritative)

The Task holds content. Only the Handle-projected fields (§4) ever cross to a
lower-clearance target; the rest is content and stays behind the boundary.

```rust
pub enum TaskStatus { Backlog, Scheduled, Active, Done, Deferred }

pub struct Task {
    pub id: Id,
    pub tier: Tier,

    // --- content (hidden above clearance) ---
    pub title: String,
    pub detail: Option<String>,
    pub objective_ids: Vec<Id>,
    pub skills: Vec<String>,      // rusty skill => warm-up; free-form in v1
    pub affect_cost: i32,         // signed: >0 draining, <0 restorative

    // --- scheduling surface (some of this is projected to the Handle) ---
    pub est_duration: Duration,   // projected (a Handle must occupy real time)
    pub due: Option<DateTime<Utc>>,        // DYNAMIC — editable post-creation
    pub earliest_start: Option<DateTime<Utc>>,

    // --- THE core feature: dynamic dependency, not hardcoded at plan time ---
    pub blocked_by: Vec<Id>,      // precedence: these must be Done first; editable

    // --- behavior when deferred blindly via its Handle ---
    pub defer_policy: DeferPolicy,

    pub status: TaskStatus,

    // --- forward-compat hooks for the v2 crawler (present, ~unused in v1) ---
    pub provenance: Provenance,          // Manual now; Crawled{..} later
    pub commitment: Option<Commitment>,  // "promised to <person>" — the UbU soul
}

pub enum DeferPolicy {
    RescheduleAsap,               // next viable slot
    DeferUntil(DateTime<Utc>),    // not before t
    ReturnToBacklog,              // unschedule; re-enter the pool
}

pub enum Provenance { Manual, Crawled { source: String, ref_id: String } }

pub struct Commitment { pub person: String, pub note: Option<String> }
```

---

## 4. Handle (the redaction primitive)

The projection of any task above a target's clearance. Carries only enough to
occupy time and be commanded — never anything that distinguishes a top-secret
block from a mundane one.

```rust
pub enum HandleStatus { Scheduled, Active, Deferred } // no content-bearing states

pub struct Handle {
    pub id: Id,                    // SAME id as the Task; opaque; tier not encoded
    pub window: Option<TimeWindow>,// present once placed
    pub duration: Duration,
    pub status: HandleStatus,
    pub deferrable: bool,          // may the operator blind-defer this?
}
```

**Invariant:** no field of `Handle` is a function of `Task.tier` or `Task`
content. Two Handles with equal `window/duration/status/deferrable` are
indistinguishable regardless of the tiers of the tasks behind them.

---

## 5. Preferences

Your documented model, unchanged: immutable content-addressed **Bundle**,
**Preference** relating two bundles under a closed two-member enum,
incomparability represented by the *absence* of a row. Utility is never stored;
the resolved ranking is computed on demand (union-find over `Indifferent`, DAG /
topo over `Strict`) and handed to `re_plan` as ordered indifference-classes.

```rust
pub struct Bundle {
    pub id: Id,
    pub members: BTreeSet<Id>,     // unordered set of Task ids; immutable once made
}

pub enum Relation { Strict, Indifferent } // closed; NO "incomparable" member

pub struct Preference {
    pub left: Id,                  // Bundle id;  left ≻ right (Strict) | left ~ right
    pub right: Id,                 // Bundle id
    pub relation: Relation,
}
```

> **v1 note:** the schema admits full bundles, but v1 populates it almost
> entirely with singleton bundles (one task each). Genuine bundle-level
> preferences (complementarity / interference) and full resolution belong with
> the ontology test and v2, not the product seed. Incomparable pairs are simply
> pairs with no `Preference` row — never a value.

---

## 6. Plan (output of `re_plan` — disposable, regenerable)

The plan is not a precious artifact to defend; it is cheap output you regenerate
on every deviation. That disposability is what dissolves the
perfectionism-vs-novelty conflict.

```rust
pub enum PlanAuthority { Authoritative, Provisional } // desktop | offline-mobile

pub struct ScheduleEntry { pub item: Id, pub window: TimeWindow, pub is_handle: bool }

pub struct Conflict { pub item: Id, pub reason: String } // surfaced for adjudication

pub struct Plan {
    pub id: Id,
    pub created_at: DateTime<Utc>,
    pub authority: PlanAuthority,
    pub clearance: Tier,                        // clearance it was computed under
    pub entries: Vec<ScheduleEntry>,
    pub objective_etas: BTreeMap<Id, Option<DateTime<Utc>>>, // transient completion ETAs
    pub conflicts: Vec<Conflict>,               // unplaceable-without-violation items
}
```

---

## 7. Session log (offline durability)

One append-only stream on mobile, replayed desktop-authoritatively on reconnect
in `at` order. Its entries fall into two classes, and the split *is* the
past/future authority principle made concrete:

- **Facts** — what the operator actually did offline. The desktop **records**
  them; it never re-litigates the past. This is what "respect the log" means.
- **Commands** — deferrals, captures, edits. The desktop **resolves** them,
  because they carry a forward consequence.

The rare overlap (an edit that also happens on the desk) resolves desktop-wins —
safe because you are the single serialization point and are not editing at the
desk while commanding from the phone.

```rust
pub enum ActualStatus { Ongoing, Done } // Ongoing = still in-flight at reconnect

pub enum FactKind {
    /// The operator did this offline. Desktop RECORDS it, never re-litigates.
    /// Works on an opaque Handle by id alone (blind completion, see §9).
    /// `actual` = real time spent, feeding per-minute history and affect learning.
    Actual { item_id: Id, status: ActualStatus, actual: Option<TimeWindow> },
}

pub enum CommandKind {
    Defer   { handle_id: Id },
    Capture { task: Task },
    EditDep { task_id: Id, blocked_by: Vec<Id> },
    EditDue { task_id: Id, due: Option<DateTime<Utc>> },
    EditPref{ pref: Preference, remove: bool },
}

pub enum LogEntryKind { Fact(FactKind), Command(CommandKind) }
pub struct LogEntry { pub id: Id, pub kind: LogEntryKind, pub at: DateTime<Utc> }
// SessionLog = append-only Vec<LogEntry>; replay dispatches on class:
//   Fact    -> record (past is not re-litigated)
//   Command -> resolve (forward consequence applied)
```

---

## 8. Operation contract — `re_plan`

Re-derive the schedule after a deviation. The engine is the LLM; the ontology is
the query. Deterministic structure is computed *outside* the model and passed in
as hard constraints; the model owns only the soft layer.

```
re_plan(state: &Store, deviation: Deviation, target: ComputeTarget) -> Plan
```

**Context assembly (the crux):**

1. Select in-scope tasks (the deviation's neighborhood, not the whole store).
2. For each task: `visible_as_content(task.tier, target.clearance())`
   - `true`  → include full content; task is **schedulable / rearrangeable**.
   - `false` → include its **Handle** only; block is **fixed**, plan must
     schedule around it and may not move or open it.
3. Pre-compute deterministically (NOT in the model):
   - precedence DAG from `blocked_by` → topological ordering;
   - resolved preference ranking → ordered indifference-classes (high→low);
   - hard resource / time-window caps; affect budget per window; slack target.
4. Prompt the target with: available windows, historical timing patterns, the
   fixed Handles, the schedulable tasks, the resolved ranking, the affect budget,
   the slack target, and the placement policy.

**Preconditions**

- Every `blocked_by` id resolves within the store.
- `target.clearance()` is honored: no above-clearance content is placed in the
  prompt (verified at the egress filter, not trusted to the model).

**Effects**

- Produces a `Plan`. Does **not** mutate tasks — the plan is derived state.
- `authority = Authoritative` when `target = DesktopOllama`; `Provisional` when a
  hosted LLM runs the semi-public slice on offline mobile.

**Invariants**

- Never rearranges a Handle; above-clearance items are fixed points.
- Never emits or persists a utility value; consumes only the resolved order.
- Unplaceable items go into `conflicts` for operator adjudication — never
  silently dropped, never force-fit past a hard constraint.
- The affect budget is a hard per-window cap: high-drain tasks are not stacked.

---

## 9. Operation contract — `defer` (offline-capable blind override)

The sharpest form of `user_override`: the operator commands a block they cannot
read, with no network and no running desktop. The boundary blocks *reading*
content; it never blocks *commanding* the block.

**Two distinct powers over an above-clearance block — never conflate them:**

1. **Commanding** the block (defer / drop / reschedule its position) is a
   *schedule-surface* act: it changes only *when/where* the opaque block sits,
   and mobile knows when/where (window, duration, status) for any block, even one
   it cannot read. Every command is therefore available blind, offline, and
   instantly. This is the operator's prerogative and it is **total**.
2. **Content-aware re-planning** of the block — the planner reasoning about the
   hidden task's affect cost, dependencies, and skills to *auto-select* a good
   slot — requires content mobile lacks offline, so it happens on desktop
   reconnect.

The accepted offline tradeoff ("mobile cannot plan out user-shared/top-secret
tasks offline") is (2) only; it never touches (1). And the planner treating
hidden blocks as **fixed points** is not a limit on the operator — it is the
guarantee that a hand-placed position is never silently overridden by automation.
*Fixed to the planner = controlled by the operator.* A blind command that
violates a hidden precedence only the desktop can see is **surfaced as a
conflict** on reconnect, never refused: the override wins, inconsistencies are
flagged.

**Reprioritization needs no separate command.** To advance a block, defer the
blocks ahead of it; the relative advancement is the *revealed preference* of that
deferral pattern. `defer` is thus a complete generating primitive for reordering
and the entire v1 command surface. (A defer acts on the current/next plan; it
does not rewrite the persistent `Preference` graph. Whether defers should also
*train* that ranking is a separable learning-loop question, out of v1.)

**Mobile (may be fully offline):**

```
defer(handle_id: Id) -> ()
  precondition: handle.deferrable == true
  effects:
    - mark the Handle Deferred locally; vacate its window
    - append LogEntry{ Command(Defer{handle_id}) } to the session log
    - re_plan(target = HostedLlm) over the semi-public slice  // Provisional Plan
```

**Blind completion is the same primitive.** If you *finished* or are *doing* an
opaque block offline, you log an actual against it by id alone — you record a
status on a block you cannot read, exactly as `defer` commands one:
`LogEntry{ Fact(Actual{ handle_id, Done|Ongoing, actual }) }`.

**Desktop (on reconnect) — record the past, then plan the future:**

```
reconcile(log: &SessionLog) -> Plan
  for entry in log, in `at` order:
    match entry.kind {
      // PAST — sole witness is the operator; desktop records, never re-litigates
      Fact(Actual{ item_id, status, actual }) =>
        - map item_id -> Task (content-aware, local; opaque Handles resolve too)
        - Task.status = Done | Active(ongoing)
        - store `actual` to history (per-minute timing + affect)
      // FORWARD-BEARING — desktop resolves the consequence
      Command(Defer{ id })   => map id -> Task; apply Task.defer_policy
      Command(Capture{ t })  => insert Task t
      Command(Edit*{ .. })   => apply the edit
    }
  re_plan(target = DesktopOllama)                            // Authoritative Plan
    // honors the resulting post-session state:
    - Done tasks     : retired from the pool, already fed to history above
    - Ongoing tasks  : treated in-flight — start is fixed, only remaining
                       duration is scheduled
    - Deferred tasks : re-placed per each Task.defer_policy — which is why the
                       next authoritative plan typically includes them again
    - fresh objective ETAs computed against the truthful post-session state
  re-project Handles; push to mobile; mobile discards its Provisional Plan
```

**Invariant (non-negotiable):** the desktop **must** honor both the deferral and
the logged actuals — it has no veto over the operator's override and no authority
to overwrite what the operator recorded doing. Desktop owns only the *resolution*
and the *forward* plan: what deferring a hidden task means for it, and how the
rest of the schedule re-flows. Mobile owns the surface and the record of the
past; desktop owns the content-level consequence and the future; neither can
perform the other's half.

---

## 10. Operation contracts — capture & mutate

Instant capture and dynamic edits are first-class because "dynamic
dependencies / preferences / due dates" is the whole reason this app exists.
Each mutation dirties the plan and kicks a (non-instant) `re_plan`.

```
capture(input) -> Id
  - create a Task (or a raw note promoted to a Task); minimal required fields
  - offline on mobile: append LogEntry{ Command(Capture{..}) }; enqueue re_plan
  - the capture keystroke/tap must be instant; recalculation need not be

log_actual(item_id, status, actual?) -> ()      // record the past; works offline
  - status ∈ { Ongoing, Done }; for an opaque Handle only the id is needed
  - append LogEntry{ Fact(Actual{ item_id, status, actual }) }
  - update the local view (mark ongoing/done); may kick a provisional re_plan
  - on reconnect the desktop records it (never re-litigated) before planning forward

mutate_dependency(task_id, blocked_by) -> ()   // edit precedence, then re_plan
mutate_due(task_id, due) -> ()                  // edit due date,  then re_plan
mutate_preference(pref, remove) -> ()           // add/remove a Preference row, then re_plan
```

Offline on mobile, all of the above enqueue and drive only the **semi-public**
provisional re-plan; anything touching user-shared or top-secret content
reconciles when the desktop is next reachable.

---

## 11. Decisions baked in (reject any in one word)

1. **`blocked_by: Vec<Id>` on the Task** is the dependency representation — not a
   separate edge table. Reject if edges must carry attributes (lag, kind).
2. **Preconditions/effects predicates are omitted from v1.** Explicit
   `blocked_by` *is* the dynamic dependency; the predicate layer that induces
   edges is a later enrichment.
3. **Bundle/Preference present, v1 populates singletons.** Full bundle-level
   resolution rides with the ontology test / v2.
4. **`DeferPolicy` is a three-member enum**, carried as a Task default, applied
   desktop-side on reconciliation.
5. **Types are storage-agnostic**; SQLite recommended on desktop for queryable
   deps, mobile holds projected state. Engine not pinned.

---

## 12. Deliberately omitted from v1 (known, bounded, not gaps)

- **Preconditions/effects world-state model** — later; `blocked_by` suffices now.
- **Existence-hiding** — v1 hides *what* a block is, not *that* it exists. The
  heavier redaction (enumerate only authorized blocks) is a later feature.
- **Zero-knowledge relay / remote realtime** — v1 is LAN-only; away from home,
  mobile runs on its last synced slice. Revisit a WireGuard-style blind relay in
  v2+ only if remote realtime proves to matter.
- **Email/IM ollama crawler** — v2. The north star; enriches the store via the
  same Task shape (its `provenance`/`commitment` hooks already exist here). Does
  not make the loop run, so it does not gate the seed.
- **Full bundle-preference resolution** — with the ontology test, not the seed.
- **Desktop-running / mobile-absent choreography** — declared an anti-pattern for
  v1. Only guard: desktop authority never blocks on an unreachable client; it
  proceeds and lets mobile reconcile on return. Presence detection and
  queuing-for-an-absent-mobile are v2+.

---

## Build order (smallest loop first)

1. Desktop store + `Task`/`Objective`/`blocked_by` + `re_plan` against ollama,
   text capture only. **This is the seed that helps this week.**
2. `defer` + Handle projection + the clearance filter (single-device first).
3. Mobile thin client: LAN sync of the shareable slice, capture, view,
   provisional semi-public re-plan, offline `defer` + queue + reconcile.
4. Whisper voice capture (pulled early — typing during a deviation is the exact
   friction you're fighting).
5. v2: crawler, then existence-hiding / relay if they earn their place.
