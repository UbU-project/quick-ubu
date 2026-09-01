# Codex Prompt — C-1: core types, store, and deterministic precompute

Repo: `quick-ubu` (fresh — this ticket scaffolds the workspace and the `core` crate)
Global safety rule: No network egress. Do not fetch crates from anywhere the
sandbox forbids; if a listed dependency cannot resolve, stop and report rather
than substituting a different crate. Touch only files inside the repo. Do not
add features, types, or modules beyond those specified below. If the spec inlined
here is ambiguous, implement the most literal reading and note it in the final
report — do not invent behavior.
Commit policy: One commit per lettered task section, conventional-commit
messages (`feat(core): …`, `test(core): …`). No force-push. Leave the working
tree clean and `cargo test` green at the final commit.

## Objective

Stand up the `quick-ubu` Cargo workspace and implement the `core` crate: every
v1 data type, a concrete in-memory `Store` with referential-integrity
validation, and the three deterministic precompute functions (`topo_order`,
`resolve_preferences`, `affect_violations`) with full unit and property tests.
No I/O, no ollama, no planner, no UI, no networking. This is the pure,
compiler-and-test-arbitrated foundation everything else hangs off, and the
calibration ticket for the two-agent review loop.

## Prompt

### Preconditions

- Empty or non-existent `quick-ubu` directory. You are creating the workspace.
- Toolchain: stable Rust, edition 2021.
- Dependencies (exact): `chrono` (features = ["serde"]), `uuid` (features =
  ["v4","serde"]), `serde` (features = ["derive"]); dev-dependency `proptest`.
- All output must be deterministic: identical input Store ⇒ byte-identical
  function output across repeated runs.

### Decision context (inlined verbatim — this is the contract)

**Tiers & clearance.** Restrictiveness axis, SemiPublic lowest → TopSecret
highest. `visible_as_content` is the pure predicate; the *projection* of a Task
to a Handle and any egress filtering are OUT OF SCOPE (ticket M2).

```rust
pub type Id = uuid::Uuid;

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Tier { SemiPublic, UserShared, TopSecret }
impl Tier {
    pub fn level(self) -> u8 {
        match self { Tier::SemiPublic => 0, Tier::UserShared => 1, Tier::TopSecret => 2 }
    }
}
/// true  => full content visible to a target of this clearance (schedulable)
/// false => represented as a Handle only (fixed, opaque)
pub fn visible_as_content(task_tier: Tier, clearance: Tier) -> bool {
    task_tier.level() <= clearance.level()
}

pub struct TimeWindow { pub start: chrono::DateTime<chrono::Utc>,
                        pub end:   chrono::DateTime<chrono::Utc> }
```

**Objective, Task, and supporting enums** (derive `Debug, Clone, PartialEq,
Serialize, Deserialize`; add `Eq` only where all fields are `Eq`):

```rust
pub enum ObjectiveStatus { Active, Done, Dropped }
pub struct Objective {
    pub id: Id, pub tier: Tier, pub title: String, pub detail: Option<String>,
    pub target_date: Option<chrono::DateTime<chrono::Utc>>, pub status: ObjectiveStatus,
}

pub enum TaskStatus { Backlog, Scheduled, Active, Done, Deferred }
pub enum DeferPolicy {
    RescheduleAsap, DeferUntil(chrono::DateTime<chrono::Utc>), ReturnToBacklog,
}
pub enum Provenance { Manual, Crawled { source: String, ref_id: String } }
pub struct Commitment { pub person: String, pub note: Option<String> }

pub struct Task {
    pub id: Id, pub tier: Tier,
    pub title: String, pub detail: Option<String>,
    pub objective_ids: Vec<Id>,
    pub skills: Vec<String>,
    pub affect_cost: i32,                 // signed: >0 draining, <0 restorative
    pub est_duration: chrono::Duration,   // note: chrono::Duration is not Eq — use PartialEq
    pub due: Option<chrono::DateTime<chrono::Utc>>,
    pub earliest_start: Option<chrono::DateTime<chrono::Utc>>,
    pub blocked_by: Vec<Id>,              // precedence edges (dynamic)
    pub defer_policy: DeferPolicy,
    pub status: TaskStatus,
    pub provenance: Provenance,
    pub commitment: Option<Commitment>,
}
```

**Handle** (type only — no projection logic in C-1):

```rust
pub enum HandleStatus { Scheduled, Active, Deferred }
pub struct Handle {
    pub id: Id, pub window: Option<TimeWindow>, pub duration: chrono::Duration,
    pub status: HandleStatus, pub deferrable: bool,
}
```

**Preferences** (singleton bundles only in v1):

```rust
pub struct Bundle { pub id: Id, pub members: std::collections::BTreeSet<Id> }
pub enum Relation { Strict, Indifferent }   // closed; incomparable = ABSENCE of a row
pub struct Preference { pub left: Id, pub right: Id, pub relation: Relation }
//   left/right are Bundle ids; Strict means left ≻ right; Indifferent means left ~ right
```

**Session log** (facts recorded, commands resolved — see reconcile, not in C-1;
here only as stored types):

```rust
pub enum ActualStatus { Ongoing, Done }
pub enum FactKind {
    Actual { item_id: Id, status: ActualStatus,
             actual: Option<TimeWindow> },
}
pub enum CommandKind {
    Defer   { handle_id: Id },
    Capture { task: Task },
    EditDep { task_id: Id, blocked_by: Vec<Id> },
    EditDue { task_id: Id, due: Option<chrono::DateTime<chrono::Utc>> },
    EditPref{ pref: Preference, remove: bool },
}
pub enum LogEntryKind { Fact(FactKind), Command(CommandKind) }
pub struct LogEntry { pub id: Id, pub kind: LogEntryKind,
                      pub at: chrono::DateTime<chrono::Utc> }
```

**Errors:**

```rust
pub enum CoreError {
    UnknownTask { id: Id },
    DanglingDependency { task: Id, missing: Id },
    DanglingObjective  { task: Id, missing: Id },
    DanglingBundleMember { bundle: Id, missing: Id },
    DanglingPreferenceBundle { preference_index: usize, missing: Id },
    DependencyCycle { involved: Vec<Id> },   // tasks left unresolved by Kahn
    PreferenceCycle { involved: Vec<Id> },   // strict cycle across indifference classes
    NonSingletonBundle { bundle: Id },       // C-1 resolves singletons only
}
```

**Precompute semantics (implement exactly):**

1. `pub fn topo_order(store: &Store) -> Result<Vec<Id>, CoreError>`
   Kahn's algorithm over all tasks, where each id in `task.blocked_by` must
   precede `task`. Deterministic tie-break: among ready nodes, always emit the
   numerically-lowest `Id` next (keep the ready set in a `BTreeSet`). A
   `blocked_by` entry referencing a non-existent task ⇒ `DanglingDependency`. If
   not all tasks are emitted ⇒ `DependencyCycle { involved = the unemitted ids,
   sorted }`.

2. `pub fn resolve_preferences(store: &Store) -> Result<Vec<Vec<Id>>, CoreError>`
   Singleton-only. For any `Preference` whose `left`/`right` bundle is not a
   singleton (|members| ≠ 1) ⇒ `NonSingletonBundle`. Map each singleton bundle to
   its one task id. Then:
   - Union-find over tasks joined by `Indifferent` preferences ⇒ indifference
     classes. Canonical class representative = the numerically-lowest member Id.
   - Build a quotient graph: each `Strict` (left ≻ right) adds edge
     class(left) → class(right) meaning class(left) ranks *higher*.
   - Detect a cycle in the quotient ⇒ `PreferenceCycle { involved = the task ids
     in the offending classes, sorted }`.
   - Topologically order the quotient high→low, tie-broken by the class's
     representative Id ascending. Output `Vec<Vec<Id>>`: outer ordered high→low,
     each inner class sorted by Id ascending.
   - Only tasks that appear in ≥1 preference are ranked; unmentioned tasks are
     absent from the output (they are incomparable, per "absence = incomparable").
   Determinism is mandatory: same Store ⇒ identical nested vecs.

3. `pub fn affect_violations(groups: &BTreeMap<WindowKey, Vec<Id>>, store: &Store,
        budget: &AffectBudget) -> Result<Vec<AffectViolation>, CoreError>`
   ```rust
   pub type WindowKey = String; // opaque placeholder until the planner supplies windows
   pub struct AffectBudget   { pub cap: i32 }        // max net affect load per window
   pub struct AffectViolation{ pub window: WindowKey, pub load: i32, pub cap: i32,
                               pub tasks: Vec<Id> }
   ```
   For each group, `load = Σ store.tasks[id].affect_cost`. Any id not in the
   store ⇒ `UnknownTask`. Emit an `AffectViolation` iff `load > budget.cap`
   (`tasks` sorted by Id). Return the violations sorted by `window` key ascending.

**Store** — concrete, in-memory, `BTreeMap`-backed:

```rust
pub struct Store {
    pub objectives:  BTreeMap<Id, Objective>,
    pub tasks:       BTreeMap<Id, Task>,
    pub bundles:     BTreeMap<Id, Bundle>,
    pub preferences: Vec<Preference>,
    pub log:         Vec<LogEntry>,   // append-only SessionLog
}
```
CRUD: `upsert_*`, `get_*`, `remove_*`, `list_*` for objective/task/bundle;
`add_preference`, `preferences()`; `append_log`, `log()`. Plus
`pub fn validate(&self) -> Result<(), Vec<CoreError>>` checking referential
integrity only: every `blocked_by`, `objective_ids`, bundle member, and
preference bundle id resolves. (Tier/projection invariants are M2, not here.)

### Tasks

- **A.** Scaffold the `quick-ubu` workspace: root `Cargo.toml` (`[workspace]`,
  members = ["core"]), `core` lib crate with the exact deps above, and place the
  full v1 spec at `docs/SPEC.md` (paste the schema the operator supplies; if
  absent, create the file with a one-line pointer and note it in the report).
- **B.** `core/src/types.rs`: all types + `CoreError` above, with the specified
  derives. Where a field's type is not `Eq` (e.g. `chrono::Duration`, floats),
  derive `PartialEq` only and say so.
- **C.** `core/src/store.rs`: `Store` + CRUD + `validate`.
- **D.** `core/src/precompute.rs`: `topo_order`.
- **E.** same module: `resolve_preferences` (+ a small union-find helper).
- **F.** same module: `affect_violations` and its `WindowKey/AffectBudget/
  AffectViolation` types.
- **G.** Tests: unit tests per function for the happy path and every error
  variant, plus `proptest` property tests:
  - topo: random constructed-acyclic graphs ⇒ output is a permutation of all
    task ids respecting every edge; graphs with an injected cycle ⇒
    `DependencyCycle`.
  - preferences: random strict-DAG + indifference edges ⇒ same-class membership
    for indifferent pairs, higher-class-before-lower for strict pairs, identical
    output across two runs; injected strict cycle ⇒ `PreferenceCycle`.
  - affect: random groups/costs ⇒ violation present iff Σ > cap.
  - determinism: each function called twice on one Store returns equal output.

### Out of scope (deliberately deferred — do NOT implement)

- The planner / `re_plan` and all types it owns (`Plan`, `ScheduleEntry`,
  `Conflict`, `PlanAuthority`, `ComputeTarget`) — ticket P-1.
- Handle *projection* and any clearance *filtering*/egress enforcement — M2.
- Persistence, SQLite, serialization to disk, any file or network I/O.
- CLI, UI, mobile, sync, reconcile.
- Incremental topological ordering (Pearce–Kelly) and Tarjan SCC — batch Kahn
  only for C-1. This omission is intentional.
- Non-singleton bundle resolution, the `pending_review`/`user_override` state
  machine — surface as errors only.

### Acceptance criteria

- `cargo build` succeeds with no errors; aim for no warnings.
- `cargo test` is green, including all property tests.
- Every type and function named above exists with the specified signature.
- All three precompute functions are deterministic (verified by test).
- No dependency beyond the four listed; no I/O; no networking; no out-of-scope
  types present.

## Required final response from Codex

1. The crate tree (`tree -I target`).
2. Verbatim tail of `cargo build` and `cargo test` (the pass/fail summary lines).
3. A checklist: each spec type and each of the three functions ⇒ implemented Y/N.
4. Every deviation or literal-reading assumption you made, with the file:line.
5. A final line: `C-1 GREEN` or `C-1 RED — <one-line reason>`.
