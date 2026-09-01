//! Unit and property tests for the store and the three precompute functions.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use crate::precompute::{
    affect_violations, resolve_preferences, topo_order, AffectBudget, AffectViolation, WindowKey,
};
use crate::store::Store;
use crate::types::{
    visible_as_content, ActualStatus, Bundle, Commitment, CommandKind, CoreError, DeferPolicy,
    FactKind, Handle, HandleStatus, Id, LogEntry, LogEntryKind, Objective, ObjectiveStatus,
    Preference, Provenance, Relation, Task, TaskStatus, Tier, TimeWindow,
};

// ---------------------------------------------------------------- fixtures --

/// Ids are minted from `u128`s so that "numerically lowest" is legible in tests:
/// `Uuid` orders by big-endian bytes, i.e. by the underlying integer.
fn id(n: u128) -> Id {
    uuid::Uuid::from_u128(n)
}

fn at(secs: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(secs, 0).expect("valid timestamp")
}

fn task(n: u128) -> Task {
    Task {
        id: id(n),
        tier: Tier::UserShared,
        title: format!("task {n}"),
        detail: None,
        objective_ids: Vec::new(),
        skills: Vec::new(),
        affect_cost: 0,
        est_duration: chrono::Duration::minutes(30),
        due: None,
        earliest_start: None,
        blocked_by: Vec::new(),
        defer_policy: DeferPolicy::RescheduleAsap,
        status: TaskStatus::Backlog,
        provenance: Provenance::Manual,
        commitment: None,
    }
}

fn blocked(n: u128, blockers: &[u128]) -> Task {
    Task {
        blocked_by: blockers.iter().copied().map(id).collect(),
        ..task(n)
    }
}

fn costed(n: u128, affect_cost: i32) -> Task {
    Task {
        affect_cost,
        ..task(n)
    }
}

fn objective(n: u128) -> Objective {
    Objective {
        id: id(n),
        tier: Tier::SemiPublic,
        title: format!("objective {n}"),
        detail: None,
        target_date: None,
        status: ObjectiveStatus::Active,
    }
}

fn bundle(n: u128, members: &[u128]) -> Bundle {
    Bundle {
        id: id(n),
        members: members.iter().copied().map(id).collect(),
    }
}

fn pref(left: u128, right: u128, relation: Relation) -> Preference {
    Preference {
        left: id(left),
        right: id(right),
        relation,
    }
}

fn store_with_tasks(tasks: Vec<Task>) -> Store {
    let mut store = Store::new();
    for t in tasks {
        store.upsert_task(t);
    }
    store
}

/// Store holding tasks `1..=n`, each wrapped in singleton bundle `1000 + n`.
fn store_with_singleton_bundles(n: u128) -> Store {
    let mut store = Store::new();
    for i in 1..=n {
        store.upsert_task(task(i));
        store.upsert_bundle(bundle(1000 + i, &[i]));
    }
    store
}

// ------------------------------------------------------------ tier / types --

#[test]
fn tier_levels_ascend_with_restrictiveness() {
    assert_eq!(Tier::SemiPublic.level(), 0);
    assert_eq!(Tier::UserShared.level(), 1);
    assert_eq!(Tier::TopSecret.level(), 2);
}

#[test]
fn visible_as_content_requires_clearance_at_least_task_tier() {
    assert!(visible_as_content(Tier::SemiPublic, Tier::SemiPublic));
    assert!(visible_as_content(Tier::SemiPublic, Tier::TopSecret));
    assert!(visible_as_content(Tier::UserShared, Tier::UserShared));
    assert!(!visible_as_content(Tier::UserShared, Tier::SemiPublic));
    assert!(!visible_as_content(Tier::TopSecret, Tier::UserShared));
}

#[test]
fn handle_and_log_types_are_constructible() {
    let window = TimeWindow {
        start: at(0),
        end: at(3600),
    };
    let handle = Handle {
        id: id(1),
        window: Some(window.clone()),
        duration: chrono::Duration::hours(1),
        status: HandleStatus::Scheduled,
        deferrable: true,
    };
    assert_eq!(handle.clone(), handle);

    let entries = vec![
        LogEntry {
            id: id(2),
            kind: LogEntryKind::Fact(FactKind::Actual {
                item_id: id(1),
                status: ActualStatus::Ongoing,
                actual: Some(window),
            }),
            at: at(10),
        },
        LogEntry {
            id: id(3),
            kind: LogEntryKind::Command(CommandKind::Capture { task: task(4) }),
            at: at(20),
        },
        LogEntry {
            id: id(5),
            kind: LogEntryKind::Command(CommandKind::EditDep {
                task_id: id(4),
                blocked_by: vec![id(1)],
            }),
            at: at(30),
        },
        LogEntry {
            id: id(6),
            kind: LogEntryKind::Command(CommandKind::EditDue {
                task_id: id(4),
                due: Some(at(99)),
            }),
            at: at(40),
        },
        LogEntry {
            id: id(7),
            kind: LogEntryKind::Command(CommandKind::EditPref {
                pref: pref(1001, 1002, Relation::Strict),
                remove: false,
            }),
            at: at(50),
        },
        LogEntry {
            id: id(8),
            kind: LogEntryKind::Command(CommandKind::Defer { handle_id: id(1) }),
            at: at(60),
        },
    ];
    assert_eq!(entries.clone(), entries);

    let commitment = Commitment {
        person: "ada".to_string(),
        note: Some("owed a reply".to_string()),
    };
    let crawled = Provenance::Crawled {
        source: "inbox".to_string(),
        ref_id: "42".to_string(),
    };
    let deferred = DeferPolicy::DeferUntil(at(7200));
    assert_eq!(commitment.clone(), commitment);
    assert_eq!(crawled.clone(), crawled);
    assert_eq!(deferred.clone(), deferred);
    assert_eq!(ObjectiveStatus::Done, ObjectiveStatus::Done);
    assert_eq!(TaskStatus::Deferred, TaskStatus::Deferred);
    assert_eq!(HandleStatus::Active, HandleStatus::Active);
    assert_eq!(ActualStatus::Done, ActualStatus::Done);
    assert_ne!(Relation::Strict, Relation::Indifferent);
}

// ------------------------------------------------------------------- store --

#[test]
fn crud_roundtrips_for_every_entity() {
    let mut store = Store::new();

    assert_eq!(store.upsert_objective(objective(1)), None);
    assert_eq!(store.get_objective(&id(1)), Some(&objective(1)));
    let renamed = Objective {
        title: "renamed".to_string(),
        ..objective(1)
    };
    assert_eq!(store.upsert_objective(renamed.clone()), Some(objective(1)));
    assert_eq!(store.list_objectives(), vec![&renamed]);
    assert_eq!(store.remove_objective(&id(1)), Some(renamed));
    assert_eq!(store.list_objectives(), Vec::<&Objective>::new());

    assert_eq!(store.upsert_task(task(2)), None);
    assert_eq!(store.get_task(&id(2)), Some(&task(2)));
    store.upsert_task(task(1));
    // `list_*` is BTreeMap-ordered: ascending by id, not insertion order.
    assert_eq!(store.list_tasks(), vec![&task(1), &task(2)]);
    assert_eq!(store.remove_task(&id(2)), Some(task(2)));
    assert_eq!(store.remove_task(&id(2)), None);

    assert_eq!(store.upsert_bundle(bundle(3, &[1])), None);
    assert_eq!(store.get_bundle(&id(3)), Some(&bundle(3, &[1])));
    assert_eq!(store.list_bundles(), vec![&bundle(3, &[1])]);
    assert_eq!(store.remove_bundle(&id(3)), Some(bundle(3, &[1])));
    assert_eq!(store.list_bundles(), Vec::<&Bundle>::new());

    let preference = pref(3, 4, Relation::Strict);
    store.add_preference(preference.clone());
    assert_eq!(store.preferences(), &[preference]);

    let entry = LogEntry {
        id: id(9),
        kind: LogEntryKind::Command(CommandKind::Defer { handle_id: id(1) }),
        at: at(1),
    };
    store.append_log(entry.clone());
    store.append_log(entry.clone());
    // The log is append-only: entries accumulate in insertion order.
    assert_eq!(store.log(), &[entry.clone(), entry]);
}

#[test]
fn validate_accepts_a_referentially_whole_store() {
    let mut store = Store::new();
    store.upsert_objective(objective(10));
    store.upsert_task(task(1));
    store.upsert_task(Task {
        objective_ids: vec![id(10)],
        ..blocked(2, &[1])
    });
    store.upsert_bundle(bundle(100, &[1]));
    store.upsert_bundle(bundle(101, &[2]));
    store.add_preference(pref(100, 101, Relation::Strict));
    assert_eq!(store.validate(), Ok(()));
}

#[test]
fn validate_reports_every_dangling_reference() {
    let mut store = Store::new();
    store.upsert_task(Task {
        objective_ids: vec![id(77)],
        ..blocked(1, &[66])
    });
    store.upsert_bundle(bundle(100, &[55]));
    store.add_preference(pref(100, 101, Relation::Strict));

    assert_eq!(
        store.validate(),
        Err(vec![
            CoreError::DanglingDependency {
                task: id(1),
                missing: id(66)
            },
            CoreError::DanglingObjective {
                task: id(1),
                missing: id(77)
            },
            CoreError::DanglingBundleMember {
                bundle: id(100),
                missing: id(55)
            },
            CoreError::DanglingPreferenceBundle {
                preference_index: 0,
                missing: id(101)
            },
        ])
    );
}

// -------------------------------------------------------------- topo_order --

#[test]
fn topo_order_of_an_empty_store_is_empty() {
    assert_eq!(topo_order(&Store::new()), Ok(Vec::new()));
}

#[test]
fn topo_order_emits_the_lowest_ready_id_first() {
    // 1 is blocked by 3, so the ready set starts as {2, 3}.
    let store = store_with_tasks(vec![blocked(1, &[3]), task(2), task(3)]);
    assert_eq!(topo_order(&store), Ok(vec![id(2), id(3), id(1)]));
}

#[test]
fn topo_order_respects_a_chain_and_tolerates_duplicate_edges() {
    let store = store_with_tasks(vec![
        task(1),
        blocked(2, &[1, 1]),
        blocked(3, &[2]),
        blocked(4, &[1]),
    ]);
    assert_eq!(topo_order(&store), Ok(vec![id(1), id(2), id(3), id(4)]));
}

#[test]
fn topo_order_rejects_a_dangling_dependency() {
    let store = store_with_tasks(vec![blocked(1, &[99]), task(2)]);
    assert_eq!(
        topo_order(&store),
        Err(CoreError::DanglingDependency {
            task: id(1),
            missing: id(99)
        })
    );
}

#[test]
fn topo_order_reports_the_unemitted_tasks_of_a_cycle() {
    let store = store_with_tasks(vec![blocked(1, &[2]), blocked(2, &[1]), task(3)]);
    assert_eq!(
        topo_order(&store),
        Err(CoreError::DependencyCycle {
            involved: vec![id(1), id(2)]
        })
    );
}

#[test]
fn topo_order_treats_a_self_edge_as_a_cycle() {
    let store = store_with_tasks(vec![blocked(1, &[1])]);
    assert_eq!(
        topo_order(&store),
        Err(CoreError::DependencyCycle {
            involved: vec![id(1)]
        })
    );
}

// ------------------------------------------------------ resolve_preferences --

#[test]
fn resolve_preferences_without_rows_is_empty() {
    let store = store_with_singleton_bundles(3);
    assert_eq!(resolve_preferences(&store), Ok(Vec::<Vec<Id>>::new()));
}

#[test]
fn resolve_preferences_orders_a_strict_chain_high_to_low() {
    let mut store = store_with_singleton_bundles(3);
    store.add_preference(pref(1002, 1003, Relation::Strict));
    store.add_preference(pref(1001, 1002, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Ok(vec![vec![id(1)], vec![id(2)], vec![id(3)]])
    );
}

#[test]
fn resolve_preferences_fuses_indifferent_tasks_into_one_class() {
    let mut store = store_with_singleton_bundles(4);
    store.add_preference(pref(1001, 1003, Relation::Indifferent));
    store.add_preference(pref(1001, 1002, Relation::Strict));
    // Task 4 appears in no preference, so it is incomparable and unranked.
    assert_eq!(
        resolve_preferences(&store),
        Ok(vec![vec![id(1), id(3)], vec![id(2)]])
    );
}

#[test]
fn resolve_preferences_breaks_ties_by_class_representative() {
    let mut store = store_with_singleton_bundles(4);
    // Two unrelated strict edges: 3 ≻ 1 and 4 ≻ 2. Both winners start ready, so
    // the lower representative (3) is emitted first; that releases 1, which is
    // then the lowest ready class and outranks the still-ready 4.
    store.add_preference(pref(1004, 1002, Relation::Strict));
    store.add_preference(pref(1003, 1001, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Ok(vec![vec![id(3)], vec![id(1)], vec![id(4)], vec![id(2)]])
    );
}

#[test]
fn resolve_preferences_rejects_a_dangling_bundle() {
    let mut store = store_with_singleton_bundles(1);
    store.add_preference(pref(1001, 1001, Relation::Indifferent));
    store.add_preference(pref(1001, 9999, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Err(CoreError::DanglingPreferenceBundle {
            preference_index: 1,
            missing: id(9999)
        })
    );
}

#[test]
fn resolve_preferences_rejects_non_singleton_bundles() {
    let mut store = store_with_singleton_bundles(2);
    store.upsert_bundle(bundle(2000, &[1, 2]));
    store.upsert_bundle(bundle(2001, &[]));

    store.add_preference(pref(2000, 1001, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Err(CoreError::NonSingletonBundle { bundle: id(2000) })
    );

    store.preferences.clear();
    store.add_preference(pref(1001, 2001, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Err(CoreError::NonSingletonBundle { bundle: id(2001) })
    );
}

#[test]
fn resolve_preferences_reports_a_strict_cycle() {
    let mut store = store_with_singleton_bundles(3);
    store.add_preference(pref(1001, 1002, Relation::Strict));
    store.add_preference(pref(1002, 1001, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Err(CoreError::PreferenceCycle {
            involved: vec![id(1), id(2)]
        })
    );
}

#[test]
fn resolve_preferences_reports_a_strict_edge_inside_one_class() {
    // Indifference fuses 1 and 2; the strict edge then becomes a self-loop on
    // that class, which is a cycle in the quotient.
    let mut store = store_with_singleton_bundles(2);
    store.add_preference(pref(1001, 1002, Relation::Indifferent));
    store.add_preference(pref(1001, 1002, Relation::Strict));
    assert_eq!(
        resolve_preferences(&store),
        Err(CoreError::PreferenceCycle {
            involved: vec![id(1), id(2)]
        })
    );
}

// -------------------------------------------------------- affect_violations --

fn groups(pairs: &[(&str, &[u128])]) -> BTreeMap<WindowKey, Vec<Id>> {
    pairs
        .iter()
        .map(|(key, ids)| ((*key).to_string(), ids.iter().copied().map(id).collect()))
        .collect()
}

#[test]
fn affect_violations_of_no_groups_is_empty() {
    let store = store_with_tasks(vec![costed(1, 100)]);
    assert_eq!(
        affect_violations(&BTreeMap::new(), &store, &AffectBudget { cap: 0 }),
        Ok(Vec::new())
    );
}

#[test]
fn affect_violations_fire_only_strictly_above_cap() {
    let store = store_with_tasks(vec![costed(1, 3), costed(2, 4)]);
    // load == cap is not a violation.
    assert_eq!(
        affect_violations(
            &groups(&[("w", &[1, 2])]),
            &store,
            &AffectBudget { cap: 7 }
        ),
        Ok(Vec::new())
    );
    assert_eq!(
        affect_violations(
            &groups(&[("w", &[2, 1])]),
            &store,
            &AffectBudget { cap: 6 }
        ),
        Ok(vec![AffectViolation {
            window: "w".to_string(),
            load: 7,
            cap: 6,
            tasks: vec![id(1), id(2)],
        }])
    );
}

#[test]
fn affect_violations_net_restorative_tasks_against_draining_ones() {
    let store = store_with_tasks(vec![costed(1, 10), costed(2, -6)]);
    assert_eq!(
        affect_violations(
            &groups(&[("w", &[1, 2])]),
            &store,
            &AffectBudget { cap: 5 }
        ),
        Ok(Vec::new())
    );
}

#[test]
fn affect_violations_come_back_sorted_by_window_key() {
    let store = store_with_tasks(vec![costed(1, 9), costed(2, 9)]);
    let result = affect_violations(
        &groups(&[("b", &[2]), ("a", &[1]), ("c", &[1, 2])]),
        &store,
        &AffectBudget { cap: 1 },
    )
    .expect("all ids known");
    let windows: Vec<&str> = result.iter().map(|v| v.window.as_str()).collect();
    assert_eq!(windows, vec!["a", "b", "c"]);
    assert_eq!(result[2].load, 18);
}

#[test]
fn affect_violations_reject_an_unknown_task() {
    let store = store_with_tasks(vec![costed(1, 1)]);
    assert_eq!(
        affect_violations(
            &groups(&[("w", &[1, 99])]),
            &store,
            &AffectBudget { cap: 0 }
        ),
        Err(CoreError::UnknownTask { id: id(99) })
    );
}

// ------------------------------------------------------------- properties --

/// Lower-triangular edges: task `i` may only be blocked by tasks `< i`, so the
/// generated graph is acyclic by construction.
fn acyclic_graph() -> impl Strategy<Value = Vec<Vec<usize>>> {
    (1usize..9).prop_flat_map(|n| {
        proptest::collection::vec(proptest::collection::vec(0usize..64, 0..4), n).prop_map(
            move |raw| {
                raw.into_iter()
                    .enumerate()
                    .map(|(i, edges)| {
                        if i == 0 {
                            Vec::new()
                        } else {
                            edges.into_iter().map(|e| e % i).collect()
                        }
                    })
                    .collect::<Vec<Vec<usize>>>()
            },
        )
    })
}

fn store_from_graph(graph: &[Vec<usize>]) -> Store {
    store_with_tasks(
        graph
            .iter()
            .enumerate()
            .map(|(i, edges)| {
                let blockers: Vec<u128> = edges.iter().map(|e| *e as u128 + 1).collect();
                blocked(i as u128 + 1, &blockers)
            })
            .collect(),
    )
}

proptest! {
    #[test]
    fn prop_topo_order_is_a_permutation_respecting_every_edge(graph in acyclic_graph()) {
        let store = store_from_graph(&graph);
        let order = topo_order(&store).expect("constructed acyclic");

        prop_assert_eq!(order.len(), graph.len());
        let unique: BTreeSet<Id> = order.iter().copied().collect();
        prop_assert_eq!(unique, store.tasks.keys().copied().collect::<BTreeSet<Id>>());

        let position: BTreeMap<Id, usize> =
            order.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        for (i, edges) in graph.iter().enumerate() {
            for e in edges {
                prop_assert!(position[&id(*e as u128 + 1)] < position[&id(i as u128 + 1)]);
            }
        }
    }

    #[test]
    fn prop_topo_order_detects_an_injected_cycle(
        graph in acyclic_graph(),
        a in 0usize..8,
        b in 0usize..8,
    ) {
        let n = graph.len();
        prop_assume!(n >= 2);
        let (a, b) = (a % n, b % n);
        prop_assume!(a != b);

        let mut store = store_from_graph(&graph);
        // Mutual blocking is a guaranteed two-node cycle.
        store.tasks.get_mut(&id(a as u128 + 1)).unwrap().blocked_by.push(id(b as u128 + 1));
        store.tasks.get_mut(&id(b as u128 + 1)).unwrap().blocked_by.push(id(a as u128 + 1));

        match topo_order(&store) {
            Err(CoreError::DependencyCycle { involved }) => {
                prop_assert!(involved.contains(&id(a as u128 + 1)));
                prop_assert!(involved.contains(&id(b as u128 + 1)));
                let mut sorted = involved.clone();
                sorted.sort();
                prop_assert_eq!(involved, sorted);
            }
            other => prop_assert!(false, "expected DependencyCycle, got {:?}", other),
        }
    }

    #[test]
    fn prop_topo_order_is_deterministic(graph in acyclic_graph()) {
        let store = store_from_graph(&graph);
        prop_assert_eq!(topo_order(&store), topo_order(&store));
    }
}

/// Tasks `1..=n` labelled by rank band, plus indifference pairs drawn from one
/// band and strict pairs drawn from a lower band to a higher one — a quotient
/// DAG by construction.
fn preference_graph() -> impl Strategy<Value = (usize, Vec<usize>, Vec<(usize, usize)>, Vec<(usize, usize)>)>
{
    (2usize..7).prop_flat_map(|n| {
        (
            Just(n),
            proptest::collection::vec(0usize..3, n),
            proptest::collection::vec((0..n, 0..n), 0..6),
            proptest::collection::vec((0..n, 0..n), 0..6),
        )
    })
}

proptest! {
    #[test]
    fn prop_resolve_preferences_honours_indifference_and_strictness(
        (n, labels, indifferent, strict) in preference_graph()
    ) {
        let mut store = store_with_singleton_bundles(n as u128);
        // Keep only edges consistent with the labelling: indifference inside a
        // band, strictness from a stronger band to a weaker one.
        let indifferent: Vec<(usize, usize)> = indifferent
            .into_iter()
            .filter(|(i, j)| i != j && labels[*i] == labels[*j])
            .collect();
        let strict: Vec<(usize, usize)> = strict
            .into_iter()
            .filter(|(i, j)| labels[*i] < labels[*j])
            .collect();
        for (i, j) in &indifferent {
            store.add_preference(pref(1001 + *i as u128, 1001 + *j as u128, Relation::Indifferent));
        }
        for (i, j) in &strict {
            store.add_preference(pref(1001 + *i as u128, 1001 + *j as u128, Relation::Strict));
        }

        let ranked = resolve_preferences(&store).expect("constructed acyclic quotient");
        prop_assert_eq!(&ranked, &resolve_preferences(&store).expect("second run"));

        let class_of = |task: Id| -> Option<usize> {
            ranked.iter().position(|class| class.contains(&task))
        };
        // Each class is sorted, and no task is ranked twice.
        let mut seen: BTreeSet<Id> = BTreeSet::new();
        for class in &ranked {
            let mut sorted = class.clone();
            sorted.sort();
            prop_assert_eq!(class, &sorted);
            for t in class {
                prop_assert!(seen.insert(*t));
            }
        }
        for (i, j) in &indifferent {
            let (a, b) = (id(*i as u128 + 1), id(*j as u128 + 1));
            prop_assert_eq!(class_of(a), class_of(b));
            prop_assert!(class_of(a).is_some());
        }
        for (i, j) in &strict {
            let (high, low) = (id(*i as u128 + 1), id(*j as u128 + 1));
            prop_assert!(class_of(high).unwrap() < class_of(low).unwrap());
        }
    }

    #[test]
    fn prop_resolve_preferences_detects_an_injected_strict_cycle(
        n in 2usize..7,
        a in 0usize..7,
        b in 0usize..7,
    ) {
        let (a, b) = (a % n, b % n);
        prop_assume!(a != b);
        let mut store = store_with_singleton_bundles(n as u128);
        store.add_preference(pref(1001 + a as u128, 1001 + b as u128, Relation::Strict));
        store.add_preference(pref(1001 + b as u128, 1001 + a as u128, Relation::Strict));

        match resolve_preferences(&store) {
            Err(CoreError::PreferenceCycle { involved }) => {
                prop_assert!(involved.contains(&id(a as u128 + 1)));
                prop_assert!(involved.contains(&id(b as u128 + 1)));
                let mut sorted = involved.clone();
                sorted.sort();
                prop_assert_eq!(involved, sorted);
            }
            other => prop_assert!(false, "expected PreferenceCycle, got {:?}", other),
        }
    }
}

proptest! {
    #[test]
    fn prop_affect_violations_fire_exactly_when_the_sum_exceeds_cap(
        costs in proptest::collection::vec(-1000i32..1000, 1..8),
        memberships in proptest::collection::vec((0usize..3, 0usize..8), 0..12),
        cap in -500i32..500,
    ) {
        let store = store_with_tasks(
            costs.iter().enumerate().map(|(i, c)| costed(i as u128 + 1, *c)).collect(),
        );
        let mut grouped: BTreeMap<WindowKey, Vec<Id>> = BTreeMap::new();
        for (window, task_index) in &memberships {
            let task_index = task_index % costs.len();
            grouped
                .entry(format!("w{window}"))
                .or_default()
                .push(id(task_index as u128 + 1));
        }

        let budget = AffectBudget { cap };
        let violations = affect_violations(&grouped, &store, &budget).expect("all ids known");
        prop_assert_eq!(&violations, &affect_violations(&grouped, &store, &budget).expect("second run"));

        for (window, ids) in &grouped {
            let load: i32 = ids.iter().map(|t| store.tasks[t].affect_cost).sum();
            let reported = violations.iter().find(|v| &v.window == window);
            if load > cap {
                let reported = reported.expect("over-cap window must be reported");
                prop_assert_eq!(reported.load, load);
                prop_assert_eq!(reported.cap, cap);
                let mut sorted = ids.clone();
                sorted.sort();
                prop_assert_eq!(&reported.tasks, &sorted);
            } else {
                prop_assert!(reported.is_none());
            }
        }
        // Windows come back in ascending key order.
        let windows: Vec<&WindowKey> = violations.iter().map(|v| &v.window).collect();
        let mut sorted = windows.clone();
        sorted.sort();
        prop_assert_eq!(windows, sorted);
    }
}

#[test]
fn every_precompute_function_is_deterministic_on_one_store() {
    let mut store = store_with_singleton_bundles(4);
    store.upsert_task(blocked(3, &[1, 2]));
    store.upsert_task(blocked(4, &[3]));
    store.upsert_task(costed(2, 5));
    store.add_preference(pref(1002, 1004, Relation::Strict));
    store.add_preference(pref(1001, 1003, Relation::Indifferent));
    store.add_preference(pref(1003, 1002, Relation::Strict));
    let grouped = groups(&[("w1", &[1, 2, 3]), ("w0", &[4])]);
    let budget = AffectBudget { cap: 1 };

    assert_eq!(topo_order(&store), topo_order(&store));
    assert_eq!(resolve_preferences(&store), resolve_preferences(&store));
    assert_eq!(
        affect_violations(&grouped, &store, &budget),
        affect_violations(&grouped, &store, &budget)
    );

    // …and the values are the expected ones, not merely stable.
    assert_eq!(
        topo_order(&store),
        Ok(vec![id(1), id(2), id(3), id(4)])
    );
    assert_eq!(
        resolve_preferences(&store),
        Ok(vec![vec![id(1), id(3)], vec![id(2)], vec![id(4)]])
    );
    assert_eq!(
        affect_violations(&grouped, &store, &budget),
        Ok(vec![AffectViolation {
            window: "w1".to_string(),
            load: 5,
            cap: 1,
            tasks: vec![id(1), id(2), id(3)],
        }])
    );
}
