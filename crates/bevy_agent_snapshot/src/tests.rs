use super::*;
use anyhow::{Result, anyhow};
use bevy::ecs::world::EntityWorldMut;
use bevy::prelude::*;
use bevy_agent_core::{
    AgentAction, AgentActionQueue, AgentControlState, SimClock, SnapshotEntity, StableEntityId,
    StableIdAllocator,
};
use bevy_agent_core::{AgentControlPlugin, AgentFinalize, AgentTick};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TestComponent {
    value: i32,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct TestResource {
    value: String,
}

fn app_with_snapshot() -> App {
    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default())
        .add_plugins(AgentSnapshotPlugin)
        .insert_resource(TestResource {
            value: "initial".to_string(),
        });
    register_snapshot_components!(app, TestComponent).unwrap();
    register_snapshot_resources!(app, TestResource).unwrap();
    configure_tick_integration(&mut app);
    app.finish();
    app.cleanup();
    app
}

#[test]
fn registry_schema_hash_is_stable_regardless_of_registration_order() {
    let mut a = SnapshotRegistry::default();
    a.register_component::<StableEntityId>().unwrap();
    a.register_resource::<SimClock>().unwrap();

    let mut b = SnapshotRegistry::default();
    b.register_resource::<SimClock>().unwrap();
    b.register_component::<StableEntityId>().unwrap();

    assert_eq!(a.schema_hash(), b.schema_hash());
}

#[test]
fn snapshot_registration_macros_register_types() {
    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default())
        .add_plugins(AgentSnapshotPlugin);

    register_snapshot_components!(app, TestComponent).unwrap();
    register_snapshot_resources!(app, TestResource).unwrap();

    let registry = app.world().resource::<SnapshotRegistry>();
    assert!(
        registry
            .component_serializers
            .contains_key(<TestComponent as SnapshotType>::TYPE_ID)
    );
    assert!(
        registry
            .resource_serializers
            .contains_key(<TestResource as SnapshotType>::TYPE_ID)
    );
}

#[test]
fn create_snapshot_stores_label_and_updates_control_state() {
    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));

    let result = create_snapshot(app.world_mut(), Some("before".to_string())).unwrap();

    assert_eq!(result.tick, 0);
    assert_eq!(
        lookup_snapshot_by_label(app.world(), "before"),
        Some(result.snapshot_id)
    );
    assert_eq!(
        app.world()
            .resource::<AgentControlState>()
            .last_snapshot_created,
        Some(result.snapshot_id)
    );
}

#[test]
fn restore_snapshot_replaces_snapshot_entities_and_resources() {
    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));
    let snapshot = create_snapshot(app.world_mut(), Some("point".to_string())).unwrap();

    app.world_mut().resource_mut::<TestResource>().value = "changed".to_string();
    {
        let mut query = app.world_mut().query::<&mut TestComponent>();
        for mut component in query.iter_mut(app.world_mut()) {
            component.value = 99;
        }
    }
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(20),
        TestComponent { value: 20 },
    ));

    restore_snapshot(app.world_mut(), snapshot.snapshot_id).unwrap();

    assert_eq!(
        app.world().resource::<TestResource>().value,
        "initial".to_string()
    );
    let mut query = app.world_mut().query::<(&StableEntityId, &TestComponent)>();
    let mut rows = query
        .iter(app.world())
        .map(|(id, component)| (id.0, component.value))
        .collect::<Vec<_>>();
    rows.sort_unstable();
    assert_eq!(rows, vec![(10, 5)]);
}

#[test]
fn restore_snapshot_restores_registered_core_allocator() {
    let mut app = app_with_snapshot();

    let before = app.world().resource::<StableIdAllocator>().next;
    let snapshot = create_snapshot(app.world_mut(), None).unwrap();

    app.world_mut().resource_mut::<StableIdAllocator>().next = 4242;
    restore_snapshot(app.world_mut(), snapshot.snapshot_id).unwrap();

    assert_eq!(app.world().resource::<StableIdAllocator>().next, before);
}

#[test]
fn restore_snapshot_rejects_schema_mismatch_before_mutating_world() {
    let mut app = app_with_snapshot();
    let result = create_snapshot(app.world_mut(), None).unwrap();
    let snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(result.snapshot_id)
        .cloned()
        .unwrap();
    app.world_mut().resource_mut::<TestResource>().value = "changed".to_string();

    let mut incompatible = snapshot;
    incompatible.manifest.schema_hash = "incompatible-schema".to_string();

    let error = restore_snapshot_value(app.world_mut(), &incompatible).unwrap_err();

    assert!(error.to_string().contains("schema hash mismatch"));
    assert_eq!(app.world().resource::<TestResource>().value, "changed");
}

#[test]
fn snapshot_policy_prunes_old_checkpoints_and_labels() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));

    let old = create_snapshot(app.world_mut(), Some("old".to_string())).unwrap();
    // Manual pins and history protection are independent. Retention releases
    // the temporary first-capture protection for this manual snapshot.
    unpin_snapshot(app.world_mut(), old.snapshot_id).unwrap();
    app.world_mut().resource_mut::<SimClock>().tick = 1;
    let new = create_snapshot(app.world_mut(), Some("new".to_string())).unwrap();
    // Creation never prunes; the owner enforces retention after indexing.
    enforce_retention(app.world_mut(), &BTreeSet::new());

    let store = app.world().resource::<SnapshotStore>();
    assert!(!store.snapshots.contains_key(&old.snapshot_id));
    assert!(store.snapshots.contains_key(&new.snapshot_id));
    assert_eq!(store.labels.get("old"), None);
    assert_eq!(store.labels.get("new"), Some(&new.snapshot_id));
}

#[test]
fn automatic_roles_are_protected_separately_from_manual_pins() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));

    let initial = create_snapshot_with_role(app.world_mut(), None, SnapshotRole::Initial).unwrap();
    assert!(
        app.world()
            .resource::<SnapshotStore>()
            .protected
            .contains(&initial.snapshot_id)
    );
    let branch =
        create_snapshot_with_role(app.world_mut(), None, SnapshotRole::BranchFork).unwrap();
    assert!(
        app.world()
            .resource::<SnapshotStore>()
            .protected
            .contains(&branch.snapshot_id)
    );

    let empty_refs = BTreeSet::new();
    let store = app.world().resource::<SnapshotStore>();
    assert!(store.snapshots.contains_key(&initial.snapshot_id));
    assert!(store.snapshots.contains_key(&branch.snapshot_id));
    assert!(!can_evict(store, initial.snapshot_id, &empty_refs));
    assert!(!can_evict(store, branch.snapshot_id, &empty_refs));
}

#[test]
fn retention_limit_one_with_pinned_initial_keeps_new_manual() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));

    // Pinned initial is excluded from the keep_last_n evictable count.
    let initial = create_snapshot_with_role(
        app.world_mut(),
        Some("initial".to_string()),
        SnapshotRole::Initial,
    )
    .unwrap();
    let first_manual = create_snapshot(app.world_mut(), Some("m1".to_string())).unwrap();
    // Newly created id always survives: evict oldest evictable OTHER
    // than the new id.
    let second_manual = create_snapshot(app.world_mut(), Some("m2".to_string())).unwrap();
    // Creation never prunes; the owner enforces retention after indexing.
    enforce_retention(app.world_mut(), &BTreeSet::new());

    let store = app.world().resource::<SnapshotStore>();
    assert!(store.snapshots.contains_key(&initial.snapshot_id));
    assert!(
        store.snapshots.contains_key(&second_manual.snapshot_id),
        "newly created snapshot must survive enforcement"
    );
    assert!(
        !store.snapshots.contains_key(&first_manual.snapshot_id),
        "oldest evictable should be evicted once evictable count exceeds keep_last_n"
    );
    // Pinned initial does not count toward the limit: exactly one
    // evictable checkpoint remains.
    let empty_refs = BTreeSet::new();
    let evictable = store
        .checkpoints
        .iter()
        .filter(|id| can_evict(store, **id, &empty_refs))
        .count();
    assert_eq!(evictable, 1);
}

#[test]
fn referenced_snapshot_is_not_evicted_and_delete_is_rejected() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .keep_last_n_checkpoints = 1;
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));

    let first = create_snapshot(app.world_mut(), Some("m1".to_string())).unwrap();
    // Retention publishes current references independently of manual pins.
    unpin_snapshot(app.world_mut(), first.snapshot_id).unwrap();
    let mut referenced = BTreeSet::new();
    referenced.insert(first.snapshot_id);
    // Creation never prunes, so no policy bump is needed: `first`
    // survives creation even though it is referenced. The owner enforces
    // retention after indexing via `enforce_retention`.
    let second = create_snapshot(app.world_mut(), Some("m2".to_string())).unwrap();
    enforce_retention(app.world_mut(), &referenced);

    let store = app.world().resource::<SnapshotStore>();
    assert!(!can_evict(store, first.snapshot_id, &referenced));
    assert!(store.snapshots.contains_key(&first.snapshot_id));
    assert!(store.snapshots.contains_key(&second.snapshot_id));

    // Coordinated delete rejects pinned and referenced snapshots.
    pin_snapshot(app.world_mut(), second.snapshot_id).unwrap();
    let err =
        delete_snapshot_checked(app.world_mut(), second.snapshot_id, &referenced).unwrap_err();
    assert!(err.to_string().contains("pinned"));
    let err = delete_snapshot_checked(app.world_mut(), first.snapshot_id, &referenced).unwrap_err();
    assert!(err.to_string().contains("referenced"));
    // Unpinned + unreferenced delete succeeds.
    unpin_snapshot(app.world_mut(), second.snapshot_id).unwrap();
    delete_snapshot_checked(app.world_mut(), second.snapshot_id, &referenced).unwrap();
    assert!(
        !app.world()
            .resource::<SnapshotStore>()
            .snapshots
            .contains_key(&second.snapshot_id)
    );
}

#[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RefComponent {
    target: StableEntityId,
}

#[derive(Resource, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
struct OptionalResource {
    value: String,
}

fn app_with_refs() -> App {
    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default())
        .add_plugins(AgentSnapshotPlugin)
        .insert_resource(TestResource {
            value: "initial".to_string(),
        });
    register_snapshot_components!(app, TestComponent, RefComponent).unwrap();
    register_snapshot_resources!(app, TestResource, OptionalResource).unwrap();
    configure_tick_integration(&mut app);
    app.finish();
    app.cleanup();
    app
}

#[test]
fn stable_entity_ids_preserved_and_refs_survive_restore() {
    let mut app = app_with_refs();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(1),
        TestComponent { value: 5 },
    ));
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(2),
        RefComponent {
            target: StableEntityId(1),
        },
    ));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();

    // Mutate: retarget the reference and change values.
    {
        let mut query = app.world_mut().query::<&mut RefComponent>();
        for mut reference in query.iter_mut(app.world_mut()) {
            reference.target = StableEntityId(999);
        }
    }

    let mut seen = std::collections::HashMap::new();
    restore_snapshot_value_with_remap(
        app.world_mut(),
        &snapshot,
        Some(&|world, map| {
            // Fix-up pass runs after components are installed, so the
            // hook observes live component values.
            assert_eq!(map.len(), 2);
            assert!(map.contains_key(&StableEntityId(1)));
            let mut query = world.query::<(&StableEntityId, Option<&RefComponent>)>();
            let found = query.iter(world).any(|(_, reference)| {
                *reference.unwrap()
                    == RefComponent {
                        target: StableEntityId(1),
                    }
            });
            assert!(found, "remap hook must see installed components");
        }),
    )
    .unwrap();
    let mut query = app
        .world_mut()
        .query::<(&StableEntityId, Option<&RefComponent>)>();
    for (id, reference) in query.iter(app.world()) {
        seen.insert(id.0, reference.cloned());
    }
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[&2],
        Some(RefComponent {
            target: StableEntityId(1)
        })
    );
}

#[test]
fn malformed_resource_late_failure_leaves_state_unchanged() {
    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let mut snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();
    // Corrupt a resource payload so typed validation fails in prepare.
    for resource in &mut snapshot.resources {
        if resource.type_id == <TestResource as SnapshotType>::TYPE_ID {
            resource.value = serde_json::json!({ "value": 12345 });
        }
    }
    // Re-sign so the checksum precondition passes and the failure
    // surfaces at typed validation (the guarded late failure).
    snapshot.checksum = checksum_snapshot(&snapshot).unwrap();

    app.world_mut().resource_mut::<TestResource>().value = "live".to_string();
    let before_entities = capture_snapshot(app.world_mut(), None).unwrap();

    let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
    assert!(error.to_string().contains("validating resource"));

    // Original semantic state unchanged: resource + entities intact.
    assert_eq!(app.world().resource::<TestResource>().value, "live");
    let after = capture_snapshot(app.world_mut(), None).unwrap();
    assert_eq!(after.entities.len(), before_entities.entities.len());
    assert_eq!(
        checksum_snapshot(&after).unwrap().hash,
        checksum_snapshot(&before_entities).unwrap().hash
    );
}

#[test]
fn absent_resource_restore_removes_extra_resource() {
    let mut app = app_with_refs();
    // OptionalResource absent at capture time.
    app.world_mut().spawn((SnapshotEntity, StableEntityId(1)));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();
    assert!(
        snapshot
            .absent_resources
            .contains(&<OptionalResource as SnapshotType>::TYPE_ID.to_string())
    );

    // World gains the optional resource after the snapshot.
    app.world_mut().insert_resource(OptionalResource {
        value: "extra".to_string(),
    });
    assert!(app.world().contains_resource::<OptionalResource>());

    restore_snapshot_value(app.world_mut(), &snapshot).unwrap();
    assert!(!app.world().contains_resource::<OptionalResource>());
}

#[test]
fn checksum_mismatch_leaves_state_unchanged() {
    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((
            SnapshotEntity,
            StableEntityId(10),
            TestComponent { value: 5 },
        ))
        .id();
    let mut snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    snapshot.checksum.hash ^= 1;
    app.world_mut().resource_mut::<TestResource>().value = "live".to_string();
    assert_prepare_rejects(
        app.world_mut(),
        &snapshot,
        entity,
        "checksum precondition failed",
    );
    assert_eq!(app.world().resource::<TestResource>().value, "live");
}

#[test]
fn checksum_detects_mutation_of_each_snapshot_field() {
    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let base_snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();
    let base = base_snapshot.checksum.hash;

    let mut mutated = base_snapshot.clone();
    mutated.clock.tick += 1;
    assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

    let mut mutated = base_snapshot.clone();
    mutated.resources[0].value = serde_json::json!({ "value": "other" });
    assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

    let mut mutated = base_snapshot.clone();
    mutated.entities[0].components[0].value = serde_json::json!({ "value": 6 });
    assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);

    let mut mutated = base_snapshot.clone();
    mutated.action_queue.push(bevy_agent_core::ScheduledAction {
        tick: 99,
        source: bevy_agent_core::ActionSource::Test,
        action: bevy_agent_core::AgentAction::Jump,
    });
    assert_ne!(checksum_snapshot(&mutated).unwrap().hash, base);
}

#[test]
fn maybe_take_snapshot_respects_interval() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .checkpoint_every_ticks = 2;

    app.world_mut().run_schedule(AgentTick);
    app.world_mut().run_schedule(AgentFinalize);
    assert!(
        app.world()
            .resource::<SnapshotStore>()
            .checkpoints
            .is_empty()
    );

    app.world_mut().run_schedule(AgentTick);
    app.world_mut().run_schedule(AgentFinalize);
    assert_eq!(app.world().resource::<SnapshotStore>().checkpoints.len(), 1);
}

#[test]
fn capture_snapshot_fails_for_snapshot_entity_without_stable_id() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .spawn((SnapshotEntity, TestComponent { value: 5 }));

    let error = capture_snapshot(app.world_mut(), None).unwrap_err();

    assert!(error.to_string().contains("missing StableEntityId"));
}

#[test]
fn restore_rejects_invalid_clock() {
    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let mut snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();
    // Poison the clock (NaN dt + absurd tick) and re-sign so the failure
    // surfaces at semantic clock validation, not the checksum gate.
    snapshot.clock.dt_seconds = f32::NAN;
    snapshot.clock.tick = u64::MAX;
    snapshot.checksum = checksum_snapshot(&snapshot).unwrap();

    let before = capture_snapshot(app.world_mut(), None).unwrap();
    let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
    assert!(
        error.to_string().contains("invalid snapshot clock"),
        "unexpected error: {error:?}"
    );
    // Prepare-phase failure: world untouched.
    let after = capture_snapshot(app.world_mut(), None).unwrap();
    assert_eq!(
        checksum_snapshot(&after).unwrap().hash,
        checksum_snapshot(&before).unwrap().hash
    );
}

#[test]
fn rollback_failure_surfaces_distinctly_and_marks_fault() {
    fn failing_restore(_entity: &mut EntityWorldMut<'_>, _value: &serde_json::Value) -> Result<()> {
        Err(anyhow!("injected restore failure"))
    }

    let mut app = app_with_snapshot();
    app.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(10),
        TestComponent { value: 5 },
    ));
    let created = create_snapshot(app.world_mut(), None).unwrap();
    let snapshot = app
        .world()
        .resource::<SnapshotStore>()
        .get(created.snapshot_id)
        .cloned()
        .unwrap();

    // Swap the TestComponent restore fn for one that always fails, so
    // both the initial apply and the rollback apply fail.
    {
        let mut registry = app.world_mut().resource_mut::<SnapshotRegistry>();
        let type_id = <TestComponent as SnapshotType>::TYPE_ID;
        if let Some(registration) = registry.component_serializers.get_mut(type_id) {
            registration.restore = failing_restore;
        }
    }

    let error = restore_snapshot_value(app.world_mut(), &snapshot).unwrap_err();
    let message = format!("{error:?}");
    assert!(
        message.contains("RollbackFailed"),
        "expected distinct RollbackFailed context, got: {message}"
    );
    let fault = app
        .world()
        .get_resource::<FaultState>()
        .unwrap_or_else(|| panic!("expected FaultState after rollback failure"));
    assert!(fault.message.contains("rollback failed"));
}

#[derive(Component, Resource, Clone, Serialize, Deserialize)]
struct DualRegistration;

#[test]
fn schema_hash_distinguishes_component_and_resource_registration() {
    let mut component_registry = SnapshotRegistry::default();
    component_registry
        .register_component::<DualRegistration>()
        .unwrap();
    let mut resource_registry = SnapshotRegistry::default();
    resource_registry
        .register_resource::<DualRegistration>()
        .unwrap();
    assert_ne!(
        component_registry.schema_hash(),
        resource_registry.schema_hash()
    );
}

#[test]
fn capture_rejects_duplicate_live_ids_without_storing_a_checkpoint() {
    let mut app = app_with_snapshot();
    let first = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    let second = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    assert!(
        create_snapshot(app.world_mut(), None)
            .unwrap_err()
            .to_string()
            .contains("duplicate StableEntityId")
    );
    assert!(app.world().resource::<SnapshotStore>().snapshots.is_empty());
    assert!(app.world().get_entity(first).is_ok());
    assert!(app.world().get_entity(second).is_ok());
}

fn assert_prepare_rejects(world: &mut World, snapshot: &Snapshot, entity: Entity, expected: &str) {
    let before = capture_snapshot(world, None).unwrap().checksum;
    let error = validate_snapshot_full(world, snapshot).unwrap_err();
    assert!(format!("{error:#}").contains(expected), "{error:#}");
    assert!(restore_snapshot_value(world, snapshot).is_err());
    // A late rollback would reallocate the entity. Keeping this id proves
    // that the complete prepare phase rejected the payload before mutation.
    assert!(world.get_entity(entity).is_ok());
    assert_eq!(capture_snapshot(world, None).unwrap().checksum, before);
    assert!(!world.contains_resource::<FaultState>());
}

#[test]
fn full_validation_rejects_ambiguous_or_inconsistent_signed_payloads_before_mutation() {
    let mut app = app_with_refs();
    let entity = app
        .world_mut()
        .spawn((
            SnapshotEntity,
            StableEntityId(7),
            TestComponent { value: 8 },
        ))
        .id();
    let baseline = capture_snapshot(app.world_mut(), None).unwrap();
    let mutate_and_reject = |world: &mut World, mutate: fn(&mut Snapshot), expected| {
        let mut snapshot = baseline.clone();
        mutate(&mut snapshot);
        snapshot.checksum = checksum_snapshot(&snapshot).unwrap();
        assert_prepare_rejects(world, &snapshot, entity, expected);
    };
    type MutationCase = (fn(&mut Snapshot), &'static str);
    let cases: &[MutationCase] = &[
        (
            |s| s.resources.push(s.resources[0].clone()),
            "duplicate resource",
        ),
        (
            |s| s.absent_resources.push(s.resources[0].type_id.clone()),
            "both present and absent",
        ),
        (
            |s| s.absent_resources.push(s.absent_resources[0].clone()),
            "duplicated",
        ),
        (
            |s| {
                s.resources
                    .retain(|r| r.type_id != <TestResource as SnapshotType>::TYPE_ID);
            },
            "omits registered resource",
        ),
        (
            |s| {
                let duplicate = s.entities[0].components[0].clone();
                s.entities[0].components.push(duplicate);
            },
            "duplicate component",
        ),
        (
            |s| {
                s.entities[0]
                    .components
                    .iter_mut()
                    .find(|c| c.type_id == <StableEntityId as SnapshotType>::TYPE_ID)
                    .unwrap()
                    .value = serde_json::json!(99);
            },
            "StableEntityId component disagrees",
        ),
        (
            |s| {
                s.entities[0]
                    .components
                    .retain(|c| c.type_id != <StableEntityId as SnapshotType>::TYPE_ID);
            },
            "omits registered StableEntityId",
        ),
        (|s| s.manifest.tick += 1, "manifest/clock tick mismatch"),
        (
            |s| s.replay_state.replay_cursor_tick += 1,
            "replay cursor/clock tick mismatch",
        ),
        (
            |s| {
                s.resources
                    .iter_mut()
                    .find(|r| r.type_id == <SimClock as SnapshotType>::TYPE_ID)
                    .unwrap()
                    .value["tick"] = serde_json::json!(1);
            },
            "SimClock resource disagrees",
        ),
        (
            |s| {
                let queue = serde_json::json!({"pending": {"1": [{
                    "tick": 1, "source": "Test", "action": {"type":"Jump"}
                }]}});
                s.resources
                    .iter_mut()
                    .find(|r| r.type_id == <AgentActionQueue as SnapshotType>::TYPE_ID)
                    .unwrap()
                    .value = serde_json::to_value(queue).unwrap();
            },
            "AgentActionQueue resource disagrees",
        ),
    ];
    for (mutate, expected) in cases {
        mutate_and_reject(app.world_mut(), *mutate, *expected);
    }
}

#[test]
fn signed_invalid_action_is_rejected_before_mutation() {
    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    let mut snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    snapshot
        .action_queue
        .push(bevy_agent_core::ScheduledAction {
            tick: 1,
            source: bevy_agent_core::ActionSource::Test,
            action: AgentAction::Move { x: 2.0, y: 0.0 },
        });
    snapshot
        .resources
        .iter_mut()
        .find(|r| r.type_id == <AgentActionQueue as SnapshotType>::TYPE_ID)
        .unwrap()
        .value = serde_json::json!({"pending": {"1": snapshot.action_queue.clone()}});
    snapshot.checksum = checksum_snapshot(&snapshot).unwrap();
    assert_prepare_rejects(
        app.world_mut(),
        &snapshot,
        entity,
        "validating captured AgentActionQueue",
    );
}

#[test]
fn snapshot_requires_the_current_contract() {
    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    let mut snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    assert_eq!(snapshot.manifest.schema_version, SNAPSHOT_SCHEMA_VERSION);
    let mut encoded = serde_json::to_value(&snapshot).unwrap();
    encoded["manifest"]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<Snapshot>(encoded).is_err());
    snapshot.manifest.schema_version = 2;
    assert_prepare_rejects(
        app.world_mut(),
        &snapshot,
        entity,
        "schema version mismatch",
    );
}

#[test]
fn snapshot_identity_is_required_and_cannot_use_nil_ids() {
    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    let snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    let mut nil = snapshot.clone();
    nil.manifest.snapshot_id.0 = Default::default();
    assert_prepare_rejects(app.world_mut(), &nil, entity, "snapshot ID cannot be nil");
    let mut nil = snapshot.clone();
    nil.manifest.created_from_timeline.0 = Default::default();
    assert_prepare_rejects(app.world_mut(), &nil, entity, "timeline ID cannot be nil");

    let mut encoded = serde_json::to_value(&snapshot).unwrap();
    let resource = encoded["resources"][0].as_object_mut().unwrap();
    let id = resource.remove("type_id").unwrap();
    resource.insert("type_name".into(), id);
    assert!(serde_json::from_value::<Snapshot>(encoded).is_err());
    let mut encoded = serde_json::to_value(&snapshot).unwrap();
    encoded["resources"][0]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert!(serde_json::from_value::<Snapshot>(encoded).is_err());
}

#[test]
fn missing_snapshot_prerequisites_return_errors() {
    let mut world = World::new();
    assert!(capture_snapshot(&mut world, None).is_err());
    world.insert_resource(SnapshotRegistry::default());
    assert!(
        capture_snapshot(&mut world, None)
            .unwrap_err()
            .to_string()
            .contains("SimClock")
    );
    world.insert_resource(SimClock::new(60));
    assert!(
        capture_snapshot(&mut world, None)
            .unwrap_err()
            .to_string()
            .contains("AgentActionCatalog")
    );
    world.insert_resource(bevy_agent_core::AgentActionCatalog::default());
    let snapshot = capture_snapshot(&mut world, None).unwrap();
    assert!(
        create_snapshot(&mut world, None)
            .unwrap_err()
            .to_string()
            .contains("SnapshotStore")
    );
    world.remove_resource::<SnapshotRegistry>();
    assert!(
        restore_snapshot_value(&mut world, &snapshot)
            .unwrap_err()
            .to_string()
            .contains("SnapshotRegistry")
    );
}

#[derive(Component, Clone, Serialize, Deserialize)]
struct RawReference {
    target: Entity,
}

#[test]
fn failed_verification_rolls_back_and_remaps_backup_entity_references() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut app = app_with_snapshot();
    app.register_snapshot_component::<RawReference>().unwrap();
    let target = app
        .world_mut()
        .spawn((
            SnapshotEntity,
            StableEntityId(1),
            TestComponent { value: 8 },
        ))
        .id();
    app.world_mut()
        .spawn((SnapshotEntity, StableEntityId(2), RawReference { target }));
    let snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    app.world_mut()
        .entity_mut(target)
        .insert(TestComponent { value: 50 });
    app.world_mut().resource_mut::<TestResource>().value = "live".to_string();
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = calls.clone();
    let remap = move |world: &mut World,
                      map: &std::collections::HashMap<StableEntityId, Entity>| {
        let target = map[&StableEntityId(1)];
        let mut query = world.query::<&mut RawReference>();
        for mut reference in query.iter_mut(world) {
            reference.target = target;
        }
        if hook_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            world.resource_mut::<TestResource>().value = "injected mismatch".to_string();
        }
    };
    let error = restore_snapshot_value_with_remap_and_exclusions(
        app.world_mut(),
        &snapshot,
        Some(&remap),
        &[<RawReference as SnapshotType>::TYPE_ID],
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("verification failed; rolled back")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(app.world().resource::<TestResource>().value, "live");
    let mut query = app.world_mut().query::<&RawReference>();
    let reference = query.single(app.world()).unwrap();
    let restored_target = app.world().entity(reference.target);
    assert_eq!(
        restored_target.get::<StableEntityId>(),
        Some(&StableEntityId(1))
    );
    assert_eq!(
        restored_target.get::<TestComponent>(),
        Some(&TestComponent { value: 50 })
    );
    assert!(!app.world().contains_resource::<FaultState>());
}

#[test]
fn required_resources_cannot_be_missing_from_capture_or_restore() {
    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((SnapshotEntity, StableEntityId(1)))
        .id();
    let mut snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    let required = <bevy_agent_core::EpisodeState as SnapshotType>::TYPE_ID.to_string();
    snapshot
        .resources
        .retain(|resource| resource.type_id != required);
    snapshot.absent_resources.push(required);
    snapshot.checksum = checksum_snapshot(&snapshot).unwrap();
    assert_prepare_rejects(
        app.world_mut(),
        &snapshot,
        entity,
        "required snapshot resource",
    );
    app.world_mut()
        .remove_resource::<bevy_agent_core::EpisodeState>();
    assert!(
        capture_snapshot(app.world_mut(), None)
            .unwrap_err()
            .to_string()
            .contains("required snapshot resource")
    );
}

#[test]
fn required_resource_policy_is_part_of_the_schema() {
    let mut optional = SnapshotRegistry::default();
    optional.register_resource::<TestResource>().unwrap();
    let mut required = SnapshotRegistry::default();
    required
        .register_required_resource::<TestResource>()
        .unwrap();
    assert_ne!(optional.schema_hash(), required.schema_hash());
}

#[test]
fn periodic_checkpoint_includes_post_tick_mutation() {
    use bevy_agent_core::{AgentPostTick, run_agent_tick};
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .checkpoint_every_ticks = 1;
    app.add_systems(AgentPostTick, |mut resource: ResMut<TestResource>| {
        resource.value = "post tick".to_string();
    });
    run_agent_tick(app.world_mut()).unwrap();
    let store = app.world().resource::<SnapshotStore>();
    let snapshot = &store.snapshots[store.checkpoints.last().unwrap()];
    let captured = snapshot
        .resources
        .iter()
        .find(|resource| resource.type_id == <TestResource as SnapshotType>::TYPE_ID)
        .unwrap();
    assert_eq!(captured.value["value"], "post tick");
    assert_eq!(snapshot.clock.tick, 1);
}

#[test]
fn periodic_checkpoint_is_suppressed_during_reconstruction() {
    let mut app = app_with_snapshot();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .checkpoint_every_ticks = 1;
    app.world_mut()
        .insert_resource(bevy_agent_core::ExecutionContext::Reconstructing);
    bevy_agent_core::run_agent_tick(app.world_mut()).unwrap();
    assert!(app.world().resource::<SnapshotStore>().snapshots.is_empty());
}

#[test]
fn repeated_registration_preserves_required_resource_policy() {
    let mut registry = SnapshotRegistry::default();
    registry
        .register_required_resource::<TestResource>()
        .unwrap();
    let before = registry.schema_hash();
    registry.register_resource::<TestResource>().unwrap();
    assert!(registry.resource_serializers[<TestResource as SnapshotType>::TYPE_ID].required);
    assert_eq!(registry.schema_hash(), before);
    let mut app = app_with_snapshot();
    register_snapshot_resources!(app, bevy_agent_core::EpisodeState).unwrap();
    app.world_mut()
        .remove_resource::<bevy_agent_core::EpisodeState>();
    assert!(
        capture_snapshot(app.world_mut(), None)
            .unwrap_err()
            .to_string()
            .contains("required snapshot resource")
    );
}

macro_rules! test_identity {
    ($ty:ty,$id:literal) => {
        impl SnapshotType for $ty {
            const TYPE_ID: &'static str = $id;
            const SCHEMA_VERSION: u32 = 1;
        }
    };
}
test_identity!(TestComponent, "test.component");
test_identity!(TestResource, "test.resource");
test_identity!(OptionalResource, "test.optional_resource");
test_identity!(RefComponent, "test.reference");
test_identity!(DualRegistration, "test.dual");
test_identity!(RawReference, "test.raw_reference");

mod original_module {
    use super::*;
    #[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct Position {
        pub value: i32,
    }
    test_identity!(Position, "game.position");
}
mod moved_module {
    use super::*;
    #[derive(Component, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    pub struct RenamedPosition {
        pub value: i32,
    }
    test_identity!(RenamedPosition, "game.position");
}
mod next_schema {
    use super::*;
    #[derive(Component, Clone, Debug, Serialize, Deserialize)]
    pub struct Position {
        pub value: i32,
    }
    impl SnapshotType for Position {
        const TYPE_ID: &'static str = "game.position";
        const SCHEMA_VERSION: u32 = 2;
    }
}

#[test]
fn persisted_identity_survives_rust_module_and_type_renames() {
    let mut source = app_with_snapshot();
    source
        .register_snapshot_component::<original_module::Position>()
        .unwrap();
    source.world_mut().spawn((
        SnapshotEntity,
        StableEntityId(42),
        original_module::Position { value: 123 },
    ));
    let snapshot = capture_snapshot(source.world_mut(), None).unwrap();
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(serialized.contains("game.position"));
    assert!(!serialized.contains("original_module"));
    assert!(!serialized.contains("type_name"));

    let mut destination = app_with_snapshot();
    destination
        .register_snapshot_component::<moved_module::RenamedPosition>()
        .unwrap();
    assert_eq!(
        source.world().resource::<SnapshotRegistry>().schema_hash(),
        destination
            .world()
            .resource::<SnapshotRegistry>()
            .schema_hash()
    );
    restore_snapshot_value(destination.world_mut(), &snapshot).unwrap();
    let mut query = destination
        .world_mut()
        .query::<(&StableEntityId, &moved_module::RenamedPosition)>();
    let restored = query.single(destination.world()).unwrap();
    assert_eq!(restored.0.0, 42);
    assert_eq!(restored.1.value, 123);
}

#[test]
fn per_type_versions_change_registry_and_payload_checksums_and_reject_restore() {
    let mut original = SnapshotRegistry::default();
    original
        .register_component::<original_module::Position>()
        .unwrap();
    let mut revised = SnapshotRegistry::default();
    revised
        .register_component::<next_schema::Position>()
        .unwrap();
    assert_ne!(original.schema_hash(), revised.schema_hash());

    let mut app = app_with_snapshot();
    let entity = app
        .world_mut()
        .spawn((
            SnapshotEntity,
            StableEntityId(12),
            TestComponent { value: 7 },
        ))
        .id();
    let snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    let mut revised = snapshot.clone();
    revised.entities[0]
        .components
        .iter_mut()
        .find(|component| component.type_id == TestComponent::TYPE_ID)
        .unwrap()
        .schema_version += 1;
    revised.checksum = checksum_snapshot(&revised).unwrap();
    assert_ne!(snapshot.checksum, revised.checksum);
    assert_prepare_rejects(app.world_mut(), &revised, entity, "schema version mismatch");

    let mut revised = snapshot.clone();
    revised
        .resources
        .iter_mut()
        .find(|resource| resource.type_id == TestResource::TYPE_ID)
        .unwrap()
        .schema_version += 1;
    revised.checksum = checksum_snapshot(&revised).unwrap();
    assert_ne!(snapshot.checksum, revised.checksum);
    assert_prepare_rejects(app.world_mut(), &revised, entity, "schema version mismatch");
}

#[derive(Resource, Clone, Serialize, Deserialize)]
struct IdentityCollision;
test_identity!(IdentityCollision, "game.position");
#[derive(Component, Clone, Serialize, Deserialize)]
struct InvalidIdentity;
impl SnapshotType for InvalidIdentity {
    const TYPE_ID: &'static str = "invalid identity";
    const SCHEMA_VERSION: u32 = 1;
}
#[derive(Resource, Clone, Serialize, Deserialize)]
struct ZeroVersion;
impl SnapshotType for ZeroVersion {
    const TYPE_ID: &'static str = "test.zero_version";
    const SCHEMA_VERSION: u32 = 0;
}

#[test]
fn registration_collisions_and_invalid_contracts_fail_atomically() {
    let mut registry = SnapshotRegistry::default();
    registry
        .register_component::<original_module::Position>()
        .unwrap();
    let hash = registry.schema_hash();
    registry
        .register_component::<original_module::Position>()
        .unwrap();
    assert!(
        registry
            .register_component::<moved_module::RenamedPosition>()
            .is_err()
    );
    assert!(registry.register_resource::<IdentityCollision>().is_err());
    assert!(registry.register_component::<InvalidIdentity>().is_err());
    assert!(registry.register_resource::<ZeroVersion>().is_err());
    assert_eq!(registry.schema_hash(), hash);
    assert_eq!(registry.components().len(), 1);
    assert_eq!(registry.resources().len(), 0);

    let mut app = app_with_snapshot();
    let before = app.world().resource::<SnapshotRegistry>().schema_hash();
    assert!(
        register_snapshot_components!(
            app,
            original_module::Position,
            moved_module::RenamedPosition
        )
        .is_err()
    );
    assert_eq!(
        app.world().resource::<SnapshotRegistry>().schema_hash(),
        before
    );
    assert!(
        app.world()
            .resource::<SnapshotRegistry>()
            .component("game.position")
            .is_none()
    );
}

#[test]
fn imported_snapshot_batch_is_atomic_indexed_and_idempotent() {
    let mut source = app_with_snapshot();
    let mut first = capture_snapshot(source.world_mut(), Some("first".into())).unwrap();
    first.manifest.role = SnapshotRole::Initial;
    let mut second = capture_snapshot(source.world_mut(), Some("second".into())).unwrap();
    second.manifest.role = SnapshotRole::BranchFork;
    second.clock.tick = 1;
    second.clock.elapsed_seconds = second.clock.dt_seconds as f64;
    second.manifest.tick = 1;
    second.replay_state.replay_cursor_tick = 1;
    second
        .resources
        .iter_mut()
        .find(|resource| resource.type_id == SimClock::TYPE_ID)
        .unwrap()
        .value = serde_json::to_value(&second.clock).unwrap();
    second.checksum = checksum_snapshot(&second).unwrap();
    let mut target = app_with_snapshot();
    let mut invalid = second.clone();
    invalid.resources[0].schema_version += 1;
    invalid.checksum = checksum_snapshot(&invalid).unwrap();
    assert!(install_snapshots(target.world_mut(), vec![first.clone(), invalid]).is_err());
    assert!(target.world().resource::<SnapshotStore>().is_empty());
    assert!(install_snapshots(target.world_mut(), vec![first.clone(), first.clone()]).is_err());
    assert!(target.world().resource::<SnapshotStore>().is_empty());
    install_snapshots(target.world_mut(), vec![second.clone(), first.clone()]).unwrap();
    let store = target.world().resource::<SnapshotStore>();
    assert_eq!(
        store.checkpoints(),
        &[first.manifest.snapshot_id, second.manifest.snapshot_id]
    );
    assert!(store.protected().contains(&first.manifest.snapshot_id));
    assert!(store.protected().contains(&second.manifest.snapshot_id));
    assert_eq!(
        store.lookup_label("first"),
        Some(first.manifest.snapshot_id)
    );
    install_snapshots(target.world_mut(), vec![first.clone(), second]).unwrap();
    assert_eq!(target.world().resource::<SnapshotStore>().len(), 2);
    let mut conflicting = first;
    conflicting.manifest.label = Some("conflict".into());
    assert!(install_snapshots(target.world_mut(), vec![conflicting]).is_err());
    assert_eq!(target.world().resource::<SnapshotStore>().len(), 2);
    assert_eq!(
        target
            .world()
            .resource::<SnapshotStore>()
            .lookup_label("conflict"),
        None
    );
}

#[test]
fn pins_cannot_reference_missing_snapshots() {
    let mut world = World::new();
    let id = bevy_agent_core::SnapshotId::new();
    assert!(pin_snapshot(&mut world, id).is_err());
    assert!(unpin_snapshot(&mut world, id).is_err());
    world.insert_resource(SnapshotStore::default());
    assert!(pin_snapshot(&mut world, id).is_err());
    assert!(unpin_snapshot(&mut world, id).is_err());
    assert!(world.resource::<SnapshotStore>().pinned().is_empty());
}

#[test]
fn generated_capture_restore_retention_sequences_preserve_store_and_gameplay_invariants() {
    for seed in 0..12_u64 {
        let mut app = app_with_refs();
        app.world_mut()
            .resource_mut::<bevy_agent_core::AgentActionCatalog>()
            .set_supported_actions([bevy_agent_core::AgentActionKind::Noop]);
        let mut random = seed + 1;
        let mut next_entity = 1_u128;
        for step in 0..36 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            match random % 6 {
                0 => {
                    app.world_mut().spawn((
                        SnapshotEntity,
                        StableEntityId(next_entity),
                        TestComponent { value: step },
                    ));
                    next_entity += 1;
                }
                1 => {
                    app.world_mut().resource_mut::<TestResource>().value =
                        format!("seed={seed};step={step}");
                    let catalog = app
                        .world()
                        .resource::<bevy_agent_core::AgentActionCatalog>()
                        .clone();
                    app.world_mut()
                        .resource_mut::<AgentActionQueue>()
                        .replace_pending(
                            &catalog,
                            0,
                            [2, 1]
                                .into_iter()
                                .map(|tick| bevy_agent_core::ScheduledAction {
                                    tick,
                                    source: bevy_agent_core::ActionSource::Test,
                                    action: AgentAction::Noop,
                                })
                                .collect(),
                        )
                        .unwrap();
                    if step % 2 == 0 {
                        app.world_mut().insert_resource(OptionalResource {
                            value: format!("optional-{step}"),
                        });
                    } else {
                        app.world_mut().remove_resource::<OptionalResource>();
                    }
                }
                2 | 3 => {
                    create_snapshot(app.world_mut(), Some(format!("label-{}", step % 3))).unwrap();
                }
                4 => {
                    let candidate = app
                        .world()
                        .resource::<SnapshotStore>()
                        .checkpoints()
                        .last()
                        .copied();
                    if let Some(id) = candidate {
                        let expected = app
                            .world()
                            .resource::<SnapshotStore>()
                            .get(id)
                            .unwrap()
                            .clone();
                        restore_snapshot(app.world_mut(), id).unwrap();
                        assert_eq!(
                            capture_snapshot(app.world_mut(), None).unwrap().checksum,
                            expected.checksum
                        );
                        assert_eq!(
                            app.world().contains_resource::<OptionalResource>(),
                            !expected
                                .absent_resources
                                .iter()
                                .any(|id| id == OptionalResource::TYPE_ID)
                        );
                    }
                }
                _ => {
                    prune_checkpoints_with_refs(app.world_mut(), 2, &BTreeSet::new());
                }
            }
            let store = app.world().resource::<SnapshotStore>();
            assert_eq!(
                store
                    .checkpoints()
                    .iter()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .len(),
                store.len()
            );
            assert!(
                store
                    .checkpoints()
                    .iter()
                    .all(|id| store.get(*id).is_some())
            );
            assert!(store.pinned().iter().all(|id| store.get(*id).is_some()));
            for label in ["label-0", "label-1", "label-2"] {
                if let Some(id) = store.lookup_label(label) {
                    assert_eq!(
                        store.get(id).unwrap().manifest.label.as_deref(),
                        Some(label)
                    );
                }
            }
        }
    }
}

fn configure_tick_integration(app: &mut App) {
    use bevy_agent_core::AgentControlAppExt;
    app.set_environment_metadata("snapshot-test", "1", None)
        .set_supported_actions([bevy_agent_core::AgentActionKind::Noop])
        .set_supported_observation_modes([bevy_agent_core::ObservationMode::Hybrid])
        .insert_observation_extractor(|world, _| {
            bevy_agent_core::Observation::default_for_tick(world.resource::<SimClock>().tick)
        })
        .insert_checksum_extractor(|world| bevy_agent_core::EnvironmentChecksum {
            tick: world.resource::<SimClock>().tick,
            hash: 0,
        })
        .set_observation_schema(serde_json::json!({"type":"object"}))
        .unwrap();
}

#[derive(Resource, Clone, Deserialize)]
struct FallibleSerialization {
    value: i32,
}
impl Serialize for FallibleSerialization {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        if self.value == 1 {
            return Err(serde::ser::Error::custom("cannot capture partial state"));
        }
        #[derive(Serialize)]
        struct Data {
            value: i32,
        }
        Data { value: self.value }.serialize(serializer)
    }
}
test_identity!(FallibleSerialization, "test.fallible_serialization");

#[test]
fn transaction_backup_recovers_even_when_current_state_cannot_be_captured() {
    let mut app = app_with_snapshot();
    app.register_required_snapshot_resource::<FallibleSerialization>()
        .unwrap();
    app.insert_resource(FallibleSerialization { value: 3 });
    let backup = capture_snapshot(app.world_mut(), None).unwrap();
    app.world_mut()
        .resource_mut::<FallibleSerialization>()
        .value = 1;
    assert!(restore_snapshot_value(app.world_mut(), &backup).is_err());
    assert_eq!(app.world().resource::<FallibleSerialization>().value, 1);
    assert_eq!(
        restore_snapshot_backup(app.world_mut(), &backup).unwrap(),
        backup.checksum
    );
    assert_eq!(app.world().resource::<FallibleSerialization>().value, 3);
}

#[test]
fn registration_macros_accept_a_plugin_style_app_reference() {
    fn register(app: &mut App) -> Result<()> {
        register_snapshot_components!(app, TestComponent)?;
        register_snapshot_resources!(app, TestResource)?;
        Ok(())
    }
    let mut app = App::new();
    register(&mut app).unwrap();
    assert!(
        app.world()
            .resource::<SnapshotRegistry>()
            .component(TestComponent::TYPE_ID)
            .is_some()
    );
}

#[test]
fn reinstalling_existing_snapshots_preserves_creation_order_after_rewind() {
    let mut app = app_with_snapshot();
    {
        let mut clock = app.world_mut().resource_mut::<SimClock>();
        clock.tick = 2;
        clock.elapsed_seconds = 2.0 * f64::from(clock.dt_seconds);
    }
    let later = create_snapshot(app.world_mut(), None).unwrap();
    {
        let mut clock = app.world_mut().resource_mut::<SimClock>();
        clock.tick = 0;
        clock.elapsed_seconds = 0.0;
    }
    let rewound = create_snapshot(app.world_mut(), None).unwrap();
    let snapshots = [rewound.snapshot_id, later.snapshot_id]
        .into_iter()
        .map(|id| {
            app.world()
                .resource::<SnapshotStore>()
                .get(id)
                .unwrap()
                .clone()
        })
        .collect();
    install_snapshots(app.world_mut(), snapshots).unwrap();
    assert_eq!(
        app.world().resource::<SnapshotStore>().checkpoints(),
        &[later.snapshot_id, rewound.snapshot_id]
    );
}

#[derive(Resource, Clone, Serialize, Deserialize)]
struct FloatPayload {
    optional: Option<f64>,
    nested: Vec<std::collections::BTreeMap<String, f32>>,
}
impl SnapshotType for FloatPayload {
    const TYPE_ID: &'static str = "test.float_payload";
    const SCHEMA_VERSION: u32 = 1;
}

#[test]
fn capture_rejects_nonfinite_floats_before_lossy_json_or_store_admission() {
    let mut app = app_with_snapshot();
    app.register_snapshot_resource::<FloatPayload>().unwrap();
    for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        app.world_mut().insert_resource(FloatPayload {
            optional: Some(number),
            nested: Vec::new(),
        });
        let error = create_snapshot(app.world_mut(), None).unwrap_err();
        assert!(format!("{error:#}").contains("non-finite"));
        assert!(app.world().resource::<SnapshotStore>().is_empty());
        assert!(
            app.world()
                .resource::<FloatPayload>()
                .optional
                .unwrap()
                .is_nan()
                || !number.is_nan()
        );
    }
    for number in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        app.world_mut().insert_resource(FloatPayload {
            optional: None,
            nested: vec![std::collections::BTreeMap::from([("float".into(), number)])],
        });
        assert!(capture_snapshot(app.world_mut(), None).is_err());
        assert!(app.world().resource::<SnapshotStore>().is_empty());
    }
    app.world_mut().insert_resource(FloatPayload {
        optional: Some(3.5),
        nested: Vec::new(),
    });
    let captured = capture_snapshot(app.world_mut(), None).unwrap();
    validate_snapshot_full(app.world(), &captured).unwrap();
    assert_eq!(
        captured
            .resources
            .iter()
            .find(|r| r.type_id == FloatPayload::TYPE_ID)
            .unwrap()
            .value["optional"],
        3.5
    );
}

#[test]
fn immutable_store_clones_share_payloads_and_byte_budget_rejects_atomically() {
    let mut app = app_with_snapshot();
    let first =
        create_snapshot_with_role(app.world_mut(), Some("first".into()), SnapshotRole::Initial)
            .unwrap();
    let store = app.world().resource::<SnapshotStore>();
    let clone = store.clone();
    assert!(std::sync::Arc::ptr_eq(
        &store.shared(first.snapshot_id).unwrap(),
        &clone.shared(first.snapshot_id).unwrap()
    ));
    let bytes = store.retained_bytes();
    app.world_mut()
        .resource_mut::<SnapshotPolicy>()
        .max_snapshot_bytes = bytes;
    let before = app
        .world()
        .resource::<AgentControlState>()
        .last_snapshot_created;
    assert!(
        create_snapshot(app.world_mut(), Some("rejected".into()))
            .unwrap_err()
            .to_string()
            .contains("budget")
    );
    assert_eq!(
        app.world().resource::<SnapshotStore>().retained_bytes(),
        bytes
    );
    assert_eq!(app.world().resource::<SnapshotStore>().len(), 1);
    assert_eq!(
        app.world()
            .resource::<AgentControlState>()
            .last_snapshot_created,
        before
    );
    assert_eq!(lookup_snapshot_by_label(app.world(), "rejected"), None);
}

#[test]
fn unrestorable_live_float_prevents_restore_before_entities_are_replaced() {
    let mut app = app_with_snapshot();
    app.register_snapshot_resource::<FloatPayload>().unwrap();
    app.world_mut().insert_resource(FloatPayload {
        optional: Some(1.0),
        nested: Vec::new(),
    });
    let entity = app
        .world_mut()
        .spawn((
            SnapshotEntity,
            StableEntityId(70),
            TestComponent { value: 9 },
        ))
        .id();
    let snapshot = capture_snapshot(app.world_mut(), None).unwrap();
    app.world_mut().resource_mut::<FloatPayload>().optional = Some(f64::NAN);
    assert!(restore_snapshot_value(app.world_mut(), &snapshot).is_err());
    assert!(app.world().get_entity(entity).is_ok());
    assert!(!app.world().contains_resource::<FaultState>());
}

#[derive(Resource, Clone, Serialize)]
struct LossyPayload(u8);
impl SnapshotType for LossyPayload {
    const TYPE_ID: &'static str = "test.lossy_payload";
    const SCHEMA_VERSION: u32 = 1;
}
impl<'de> Deserialize<'de> for LossyPayload {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let _ = u8::deserialize(d)?;
        Ok(Self(0))
    }
}
#[test]
fn capture_rejects_a_deserializer_that_loses_gameplay_state() {
    let mut app = app_with_snapshot();
    app.register_snapshot_resource::<LossyPayload>().unwrap();
    app.world_mut().insert_resource(LossyPayload(7));
    let error = create_snapshot(app.world_mut(), None).unwrap_err();
    assert!(format!("{error:#}").contains("round trip"), "{error:#}");
    assert_eq!(app.world().resource::<LossyPayload>().0, 7);
    assert!(app.world().resource::<SnapshotStore>().is_empty());
}
