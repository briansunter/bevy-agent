use super::*;

fn app_with_core() -> App {
    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default());
    configure_test_game(&mut app);
    app.finish();
    app.cleanup();
    app
}

fn configure_test_game(app: &mut App) {
    app.set_environment_metadata("core-tests", "1", None)
        .set_supported_actions([
            AgentActionKind::Noop,
            AgentActionKind::Move,
            AgentActionKind::Look,
            AgentActionKind::Jump,
            AgentActionKind::Crouch,
            AgentActionKind::Sprint,
            AgentActionKind::Interact,
            AgentActionKind::Attack,
            AgentActionKind::UseItem,
            AgentActionKind::Dodge,
        ])
        .set_supported_observation_modes([
            ObservationMode::Hybrid,
            ObservationMode::PlayerKnowledge,
            ObservationMode::FullDebugState,
            ObservationMode::DiffSinceLastTick,
            ObservationMode::PixelFrame,
        ])
        .insert_observation_extractor(|world, _| {
            Observation::default_for_tick(world.resource::<SimClock>().tick)
        })
        .insert_checksum_extractor(|world| default_checksum(world));
}

#[test]
fn stable_id_allocator_allocates_monotonic_ids() {
    let mut allocator = StableIdAllocator::default();

    assert_eq!(allocator.allocate(), StableEntityId(1));
    assert_eq!(allocator.allocate(), StableEntityId(2));
    assert_eq!(allocator.next, 3);
}

#[test]
fn sim_clock_advance_preserves_fixed_dt() {
    let mut clock = SimClock::new(20);

    clock.advance_one_tick();
    clock.advance_one_tick();

    assert_eq!(clock.tick, 2);
    assert_eq!(clock.dt_seconds, 0.05);
    assert!((clock.elapsed_seconds - 0.1).abs() < 1.0e-6);
}

#[test]
fn action_queue_schedules_and_clears_actions() {
    let mut queue = AgentActionQueue::default();

    let catalog = app_with_core()
        .world()
        .resource::<AgentActionCatalog>()
        .clone();
    queue
        .schedule(
            &catalog,
            &SimClock::default(),
            &AgentControlState::default(),
            &ExecutionContext::Live,
            7,
            ActionSource::Test,
            AgentAction::Jump,
        )
        .unwrap();
    assert_eq!(queue.len(), 1);

    queue.clear();
    assert!(queue.is_empty());
}

#[test]
fn custom_action_schemas_register_in_catalog() {
    let mut app = App::new();
    app.register_custom_action_schema(
        "Input",
        serde_json::json!({
            "type": "object",
            "required": ["type"],
            "properties": { "type": { "const": "Input" } }
        }),
    )
    .unwrap();

    let catalog = app.world().resource::<AgentActionCatalog>();
    assert!(catalog.custom_actions().contains_key("Input"));
    assert_eq!(
        catalog.custom_actions()["Input"].schema["properties"]["type"]["const"],
        "Input"
    );
}

#[test]
fn stable_json_hash_sorts_object_keys() {
    let a = serde_json::json!({ "b": 2, "a": [true, null] });
    let b = serde_json::json!({ "a": [true, null], "b": 2 });
    let c = serde_json::json!({ "a": [true, null], "b": 3 });

    assert_eq!(stable_hash_json(&a), stable_hash_json(&b));
    assert_ne!(stable_hash_json(&a), stable_hash_json(&c));
}

#[test]
fn core_tick_drains_only_current_actions_and_preserves_future_actions() {
    let mut app = app_with_core();
    schedule_action(app.world_mut(), 1, ActionSource::Agent, AgentAction::Jump).unwrap();
    schedule_action(
        app.world_mut(),
        3,
        ActionSource::Script,
        AgentAction::Interact,
    )
    .unwrap();

    run_agent_tick(app.world_mut()).unwrap();

    let input = app.world().resource::<CurrentInputFrame>();
    assert_eq!(input.tick, 1);
    assert_eq!(input.actions, vec![AgentAction::Jump]);
    assert_eq!(input.sources, vec![ActionSource::Agent]);
    assert_eq!(app.world().resource::<AgentActionQueue>().len(), 1);
    assert_eq!(
        app.world()
            .resource::<AgentControlState>()
            .last_action_count,
        1
    );
}

#[test]
fn reset_core_clears_episode_reward_input_and_preserves_tick_dt_and_rng_seed() {
    let mut app = app_with_core();
    {
        let world = app.world_mut();
        world.resource_mut::<SimClock>().tick = 99;
        world.resource_mut::<SimClock>().dt_seconds = 0.25;
        world.resource_mut::<RewardState>().current_reward = 10.0;
        world.resource_mut::<EpisodeState>().done = true;
        world
            .resource_mut::<CurrentInputFrame>()
            .actions
            .push(AgentAction::Jump);
        world.resource_mut::<DeterministicRng>().seed = 123;
        schedule_action(world, 100, ActionSource::Agent, AgentAction::Noop).unwrap();
    }

    app.world_mut().run_schedule(AgentReset);

    assert_eq!(app.world().resource::<SimClock>().tick, 0);
    assert_eq!(app.world().resource::<SimClock>().dt_seconds, 0.25);
    assert_eq!(app.world().resource::<RewardState>().current_reward, 0.0);
    assert!(!app.world().resource::<EpisodeState>().done);
    assert!(
        app.world()
            .resource::<CurrentInputFrame>()
            .actions
            .is_empty()
    );
    assert!(app.world().resource::<AgentActionQueue>().is_empty());
    assert_eq!(app.world().resource::<DeterministicRng>().seed, 123);
}

#[test]
fn configured_extractors_supply_observation_and_checksum() {
    let mut app = app_with_core();
    app.insert_observation_extractor(|world, mode| {
        assert!(matches!(mode, ObservationMode::FullDebugState));
        Observation::FullState(serde_json::json!({"tick":world.resource::<SimClock>().tick}))
    })
    .insert_checksum_extractor(|world| EnvironmentChecksum {
        tick: world.resource::<SimClock>().tick,
        hash: 42,
    });
    app.world_mut().resource_mut::<ObservationConfig>().mode = ObservationMode::FullDebugState;

    run_agent_tick(app.world_mut()).unwrap();

    let response = app
        .world()
        .resource::<LastStepResponse>()
        .0
        .as_ref()
        .unwrap();
    assert_eq!(
        response.observation,
        Observation::FullState(serde_json::json!({ "tick": 1 }))
    );
    assert_eq!(
        response.checksum,
        Some(EnvironmentChecksum { tick: 1, hash: 42 })
    );
}

#[test]
fn default_checksum_detects_mutation_of_each_authoritative_field() {
    use rand::RngCore;

    fn seeded_app() -> App {
        let mut app = app_with_core();
        app.world_mut().resource_mut::<SimClock>().tick = 7;
        app.world_mut().resource_mut::<SimClock>().dt_seconds = 0.05;
        app.world_mut().resource_mut::<SimClock>().elapsed_seconds = 0.35;
        app.world_mut().resource_mut::<RewardState>().current_reward = 1.5;
        app.world_mut()
            .resource_mut::<RewardState>()
            .cumulative_reward = 4.25;
        app.world_mut()
            .resource_mut::<CurrentInputFrame>()
            .actions
            .push(AgentAction::Jump);
        app.world_mut()
            .resource_mut::<CurrentInputFrame>()
            .sources
            .push(ActionSource::Agent);
        schedule_action(
            app.world_mut(),
            9,
            ActionSource::Script,
            AgentAction::Interact,
        )
        .unwrap();
        app
    }

    let base = default_checksum(seeded_app().world());

    let mut mutated = seeded_app();
    mutated.world_mut().resource_mut::<SimClock>().tick = 8;
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "tick mutation undetected"
    );

    let mut mutated = seeded_app();
    mutated
        .world_mut()
        .resource_mut::<RewardState>()
        .current_reward = 99.0;
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "reward mutation undetected"
    );

    let mut mutated = seeded_app();
    mutated.world_mut().resource_mut::<EpisodeState>().done = true;
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "episode.done mutation undetected"
    );

    let mut mutated = seeded_app();
    mutated
        .world_mut()
        .resource_mut::<CurrentInputFrame>()
        .actions
        .push(AgentAction::Crouch);
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "input action mutation undetected"
    );

    let mut mutated = seeded_app();
    schedule_action(
        mutated.world_mut(),
        10,
        ActionSource::Agent,
        AgentAction::Dodge,
    )
    .unwrap();
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "queued input mutation undetected"
    );

    let mut mutated = seeded_app();
    mutated
        .world_mut()
        .resource_mut::<DeterministicRng>()
        .rng
        .next_u32();
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "rng state mutation undetected"
    );

    // Action contents (not just queue length) must matter.
    let mut mutated = seeded_app();
    let world = mutated.world_mut();
    let mut pending = world
        .resource::<AgentActionQueue>()
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    pending[0].action = AgentAction::Dodge;
    let catalog = world.resource::<AgentActionCatalog>().clone();
    world
        .resource_mut::<AgentActionQueue>()
        .replace_pending(&catalog, 7, pending)
        .unwrap();
    assert_ne!(
        default_checksum(mutated.world()),
        base,
        "queued action contents mutation undetected"
    );
}

#[test]
fn agent_decision_runs_once_before_each_simulation_tick() {
    #[derive(Resource, Default)]
    struct PolicyCalls(u64);

    fn policy(world: &mut World) {
        world.resource_mut::<PolicyCalls>().0 += 1;
        let tick = world.resource::<SimClock>().tick + 1;
        schedule_action(world, tick, ActionSource::Agent, AgentAction::Noop).unwrap();
    }

    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default())
        .init_resource::<PolicyCalls>()
        .add_systems(AgentDecision, policy);
    configure_test_game(&mut app);
    app.finish();
    app.cleanup();

    run_agent_tick(app.world_mut()).unwrap();
    run_agent_tick(app.world_mut()).unwrap();

    assert_eq!(app.world().resource::<PolicyCalls>().0, 2);
    assert_eq!(app.world().resource::<SimClock>().tick, 2);
    assert_eq!(
        app.world()
            .resource::<AgentControlState>()
            .last_action_count,
        1
    );
}

#[test]
fn sim_clock_rejects_zero_tick_rate() {
    assert!(SimClock::try_new(0).is_err());
    assert!(SimClock::try_new(60).is_ok());
}

#[test]
#[should_panic(expected = "tick rate must be > 0")]
fn sim_clock_new_panics_on_zero() {
    let _ = SimClock::new(0);
}

#[test]
fn sim_clock_validates_imported_values() {
    let valid = SimClock::new(60);
    assert!(valid.validate().is_ok());

    let mut bad_dt = valid.clone();
    bad_dt.dt_seconds = f32::INFINITY;
    assert!(bad_dt.validate().is_err());

    let mut bad_tick = valid.clone();
    bad_tick.tick = u64::MAX;
    assert!(bad_tick.validate().is_err());
    assert!(validate_clock_tick(u64::MAX).is_err());
    assert!(bad_tick.set_tick_checked(5).is_ok());
    assert_eq!(bad_tick.tick, 5);
}

#[test]
fn action_validation_rejects_unsupported_and_out_of_bounds() {
    let mut catalog = AgentActionCatalog::default();
    catalog.set_supported_actions([AgentActionKind::Move, AgentActionKind::Jump]);
    assert!(
        catalog
            .validate_action(&AgentAction::Move { x: 1.5, y: 0.0 })
            .is_err()
    );
    assert!(
        catalog
            .validate_action(&AgentAction::Move {
                x: f32::NAN,
                y: 0.0
            })
            .is_err()
    );
    assert!(catalog.validate_action(&AgentAction::Dodge).is_err());
    assert!(
        catalog
            .validate_action(&AgentAction::Move { x: 0.5, y: 0.0 })
            .is_ok()
    );
}

#[test]
fn observation_modes_are_declared_by_each_game() {
    let mut catalog = AgentObservationCatalog::default();
    assert!(catalog.validate().is_err());
    catalog.set_supported_modes([ObservationMode::PixelFrame]);
    assert!(catalog.validate_mode(&ObservationMode::PixelFrame).is_ok());
    assert!(catalog.validate_mode(&ObservationMode::Hybrid).is_err());
}

#[test]
fn control_mode_enforces_source_matrix() {
    assert!(ControlMode::Agent.accepts_source(&ActionSource::Agent));
    assert!(!ControlMode::Agent.accepts_source(&ActionSource::Human));
    assert!(ControlMode::Human.accepts_source(&ActionSource::Human));
    assert!(!ControlMode::Human.accepts_source(&ActionSource::Agent));
    assert!(ControlMode::Hybrid.accepts_source(&ActionSource::Human));
    assert!(ControlMode::Hybrid.accepts_source(&ActionSource::Agent));
    assert!(!ControlMode::Hybrid.accepts_source(&ActionSource::Replay));
    assert!(ControlMode::Replay.accepts_source(&ActionSource::Replay));
    assert!(!ControlMode::Paused.allows_stepping());
    assert!(!ControlMode::InspectOnly.allows_stepping());
}

#[test]
fn terminal_state_is_absorbing() {
    let live = EpisodeState::default();
    assert!(!live.is_terminal());
    assert!(live.should_accumulate_reward());
    let done = EpisodeState {
        done: true,
        truncated: false,
        reason: Some("goal_reached".to_string()),
    };
    assert!(done.is_terminal());
    assert!(!done.should_accumulate_reward());
    assert!(done.ensure_not_terminal().is_err());
}

#[test]
fn mode_changes_arbitrate_already_accepted_inputs_at_drain() {
    let mut app = app_with_core();
    schedule_action(app.world_mut(), 1, ActionSource::Agent, AgentAction::Jump).unwrap();
    app.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Human;
    run_agent_tick(app.world_mut()).unwrap();
    // An action accepted earlier is arbitrated against the mode at execution.
    assert!(
        app.world()
            .resource::<CurrentInputFrame>()
            .actions
            .is_empty()
    );
}

#[test]
fn integration_requires_explicit_contracts_and_extractors() {
    let mut app = App::new();
    app.add_plugins(AgentControlPlugin::default());
    assert!(matches!(
        validate_integration(app.world()),
        Err(AgentControlError::MissingResource(
            "AgentObservationExtractor"
        ))
    ));
    configure_test_game(&mut app);
    validate_integration(app.world()).unwrap();
    app.world_mut()
        .resource_mut::<AgentActionCatalog>()
        .set_supported_actions([]);
    assert!(matches!(
        validate_integration(app.world()),
        Err(AgentControlError::InvalidIntegration(_))
    ));
    app.set_supported_actions([AgentActionKind::Noop]);
    app.set_supported_observation_modes([]);
    assert!(validate_integration(app.world()).is_err());
    app.set_supported_observation_modes([ObservationMode::Hybrid]);
    app.set_environment_metadata(" ", "1", None);
    assert!(validate_integration(app.world()).is_err());
    app.set_environment_metadata("test", "1", None);
    app.world_mut()
        .resource_mut::<bevy::ecs::schedule::Schedules>()
        .remove(AgentDecision);
    assert!(matches!(
        validate_integration(app.world()),
        Err(AgentControlError::MissingSchedule("AgentDecision"))
    ));
}

#[test]
fn unsupported_observation_rejects_before_extraction_and_clears_stale_response() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let extractor_calls = calls.clone();
    let mut app = app_with_core();
    app.set_supported_observation_modes([ObservationMode::Hybrid])
        .insert_observation_extractor(move |world, _| {
            extractor_calls.fetch_add(1, Ordering::Relaxed);
            Observation::default_for_tick(world.resource::<SimClock>().tick)
        });
    collect_observation(app.world_mut()).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(matches!(
        collect_observation_with_mode(app.world_mut(), ObservationMode::PixelFrame),
        Err(AgentControlError::UnsupportedObservationMode(_))
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(app.world().resource::<LastStepResponse>().0.is_none());
    assert_eq!(
        app.world().resource::<ObservationConfig>().mode,
        ObservationMode::Hybrid
    );
}

#[test]
fn custom_catalog_enforces_full_json_schema_semantics() {
    let mut catalog = AgentActionCatalog::default();
    assert!(catalog.validate_action(&AgentAction::Noop).is_err());
    catalog.set_supported_actions([AgentActionKind::Custom]);
    assert!(catalog.validate().is_err());
    catalog
        .register_custom_action_schema(
            "target",
            serde_json::json!({
                "$defs": {"point": {
                    "type":"array", "minItems":2, "maxItems":2,
                    "items":{"type":"number", "minimum":0, "maximum":10}
                }},
                "type":"object", "required":["name", "point", "kind"],
                "properties": {
                    "name":{"type":"string", "pattern":"^[a-z]+$"},
                    "point":{"$ref":"#/$defs/point"},
                    "kind":{"enum":["normal", "charged"]},
                    "power":{"type":"integer", "minimum":1, "multipleOf":2}
                },
                "if":{"properties":{"kind":{"const":"charged"}}},
                "then":{"required":["power"]},
                "else":{"not":{"required":["power"]}},
                "additionalProperties":false
            }),
        )
        .unwrap();
    catalog.validate().unwrap();
    let action = |value| AgentAction::Custom { value };
    assert!(
        catalog
            .validate_action(&action(
                serde_json::json!({"name":"goal", "point":[0,10], "kind":"normal"})
            ))
            .is_ok()
    );
    assert!(
        catalog
            .validate_action(&action(
                serde_json::json!({"name":"goal", "point":[0,10], "kind":"charged", "power":4})
            ))
            .is_ok()
    );
    for value in [
        serde_json::json!({"name":"GOAL", "point":[0,10], "kind":"normal"}),
        serde_json::json!({"name":"goal", "point":[0,11], "kind":"normal"}),
        serde_json::json!({"name":"goal", "point":[0], "kind":"normal"}),
        serde_json::json!({"name":"goal", "point":[0,10], "kind":"charged"}),
        serde_json::json!({"name":"goal", "point":[0,10], "kind":"charged", "power":3}),
        serde_json::json!({"name":"goal", "point":[0,10], "kind":"normal", "power":4}),
        serde_json::json!({"name":"goal", "point":[0,10], "kind":"normal", "extra":true}),
    ] {
        assert!(
            catalog.validate_action(&action(value.clone())).is_err(),
            "accepted {value}"
        );
    }
    let restored: AgentActionCatalog =
        serde_json::from_value(serde_json::to_value(&catalog).unwrap()).unwrap();
    assert!(
        restored
            .validate_action(&action(
                serde_json::json!({"name":"goal", "point":[0,10], "kind":"normal"})
            ))
            .is_ok()
    );
    assert!(
        restored
            .validate_action(&action(serde_json::json!({})))
            .is_err()
    );
}

#[test]
fn overlapping_custom_schemas_accept_any_matching_schema() {
    let mut catalog = AgentActionCatalog::default();
    catalog.set_supported_actions([AgentActionKind::Custom]);
    catalog
        .register_custom_action_schema("integer", serde_json::json!({"type":"integer"}))
        .unwrap();
    catalog
        .register_custom_action_schema("number", serde_json::json!({"type":"number"}))
        .unwrap();
    assert!(
        catalog
            .validate_action(&AgentAction::Custom {
                value: serde_json::json!(2)
            })
            .is_ok()
    );
}

#[test]
fn invalid_or_external_schema_registration_is_transactional() {
    let mut catalog = AgentActionCatalog::default();
    catalog.set_supported_actions([AgentActionKind::Custom]);
    catalog
        .register_custom_action_schema("payload", serde_json::json!({"const":"valid"}))
        .unwrap();
    let before = serde_json::to_value(&catalog).unwrap();
    for schema in [
        serde_json::json!({"type":"unknown"}),
        serde_json::json!({"minimum":"bad"}),
        serde_json::json!({"$schema":"http://json-schema.org/draft-07/schema#"}),
        serde_json::json!({"$ref":"https://example.invalid/payload.json"}),
        serde_json::json!({"$ref":"file:///etc/passwd"}),
    ] {
        assert!(
            catalog
                .register_custom_action_schema("payload", schema.clone())
                .is_err(),
            "accepted {schema}"
        );
        assert_eq!(serde_json::to_value(&catalog).unwrap(), before);
    }
    assert!(
        catalog
            .validate_action(&AgentAction::Custom {
                value: serde_json::json!("valid")
            })
            .is_ok()
    );
    assert!(
        catalog
            .validate_action(&AgentAction::Custom {
                value: serde_json::json!("invalid")
            })
            .is_err()
    );
    let mut observations = AgentObservationCatalog::default();
    observations.set_supported_modes([ObservationMode::Hybrid]);
    observations
        .set_schema(serde_json::json!({"type":"object"}))
        .unwrap();
    assert!(
        observations
            .set_schema(serde_json::json!({"type":"wrong"}))
            .is_err()
    );
    assert_eq!(
        observations.schema().unwrap(),
        &serde_json::json!({"type":"object"})
    );
}

#[test]
fn observation_schema_validates_full_envelope_and_failure_is_visible() {
    let mut app = app_with_core();
    collect_observation(app.world_mut()).unwrap();
    app.set_observation_schema(serde_json::json!({
        "type":"object", "required":["kind", "tick", "value"],
        "properties":{"kind":{"const":"Domain"}, "tick":{"type":"integer"}, "value":{"type":"string"}},
        "additionalProperties":false
    })).unwrap();
    assert!(run_agent_tick(app.world_mut()).is_err());
    assert_eq!(app.world().resource::<SimClock>().tick, 1);
    assert!(app.world().resource::<LastStepResponse>().0.is_none());
    assert!(app.world().resource::<AgentTickFailure>().error().is_some());
    app.insert_observation_extractor(|world, _| Observation::Domain {
        tick: world.resource::<SimClock>().tick,
        value: serde_json::json!("ready"),
    });
    run_agent_tick(app.world_mut()).unwrap();
    assert!(app.world().resource::<AgentTickFailure>().error().is_none());
    assert_eq!(
        app.world()
            .resource::<LastStepResponse>()
            .0
            .as_ref()
            .unwrap()
            .tick,
        2
    );
}

#[test]
fn scheduled_reset_failure_clears_cached_response() {
    let mut app = app_with_core();
    collect_observation(app.world_mut()).unwrap();
    app.set_observation_schema(serde_json::json!(false))
        .unwrap();
    app.world_mut().run_schedule(AgentReset);
    assert!(app.world().resource::<LastStepResponse>().0.is_none());
    assert!(app.world().resource::<AgentTickFailure>().error().is_some());
    app.set_observation_schema(serde_json::json!(true)).unwrap();
    app.world_mut().run_schedule(AgentReset);
    assert!(app.world().resource::<AgentTickFailure>().error().is_none());
    assert_eq!(
        app.world()
            .resource::<LastStepResponse>()
            .0
            .as_ref()
            .unwrap()
            .tick,
        0
    );
}

#[test]
fn strict_action_deserialization_rejects_unknown_and_rounded_out_of_bounds() {
    for value in [
        serde_json::json!({"type":"Noop", "extra":true}),
        serde_json::json!({"type":"Move", "x":0, "y":0, "z":0}),
        serde_json::json!({"type":"Move", "x":1.00000001, "y":0}),
        serde_json::json!({"type":"Move", "x":0, "y":-1.00000001}),
        serde_json::json!({"type":"Look", "yaw_delta":f64::from(LOOK_YAW_DELTA_LIMIT_RADIANS)+1e-9, "pitch_delta":0}),
        serde_json::json!({"type":"Look", "yaw_delta":0, "pitch_delta":f64::from(LOOK_PITCH_DELTA_LIMIT_RADIANS)+1e-9}),
    ] {
        assert!(
            serde_json::from_value::<AgentAction>(value.clone()).is_err(),
            "accepted {value}"
        );
    }
    let value = serde_json::json!({"type":"Look", "yaw_delta":f64::from(LOOK_YAW_DELTA_LIMIT_RADIANS), "pitch_delta":f64::from(LOOK_PITCH_DELTA_LIMIT_RADIANS)});
    let action: AgentAction = serde_json::from_value(value).unwrap();
    let catalog = app_with_core()
        .world()
        .resource::<AgentActionCatalog>()
        .clone();
    catalog.validate_action(&action).unwrap();
    assert!(
        catalog
            .validate_action(&AgentAction::Look {
                yaw_delta: f32::NAN,
                pitch_delta: 0.0
            })
            .is_err()
    );
}

#[test]
fn derived_action_schema_matches_strict_runtime_limits() {
    let schema = serde_json::to_value(schemars::schema_for!(AgentAction)).unwrap();
    let validator = crate::schema::compile_schema(&schema).unwrap();
    for value in [
        serde_json::json!({"type":"Noop"}),
        serde_json::json!({"type":"Move", "x":-1, "y":1}),
        serde_json::json!({"type":"Look", "yaw_delta":f64::from(LOOK_YAW_DELTA_LIMIT_RADIANS), "pitch_delta":0}),
    ] {
        assert!(validator.is_valid(&value), "schema rejected {value}");
        serde_json::from_value::<AgentAction>(value).unwrap();
    }
    for value in [
        serde_json::json!({"type":"Noop", "extra":true}),
        serde_json::json!({"type":"Move", "x":1.00000001, "y":0}),
        serde_json::json!({"type":"Look", "yaw_delta":f64::from(LOOK_YAW_DELTA_LIMIT_RADIANS)+1e-9, "pitch_delta":0}),
    ] {
        assert!(!validator.is_valid(&value), "schema accepted {value}");
        assert!(serde_json::from_value::<AgentAction>(value).is_err());
    }
}

#[test]
fn rejected_scheduling_leaves_queue_unchanged() {
    let mut app = app_with_core();
    schedule_action(app.world_mut(), 3, ActionSource::Agent, AgentAction::Noop).unwrap();
    let before = serde_json::to_value(app.world().resource::<AgentActionQueue>()).unwrap();
    for (tick, source, action) in [
        (0, ActionSource::Agent, AgentAction::Noop),
        (u64::MAX, ActionSource::Agent, AgentAction::Noop),
        (1, ActionSource::Human, AgentAction::Noop),
        (1, ActionSource::Agent, AgentAction::Move { x: 2.0, y: 0.0 }),
        (
            1,
            ActionSource::Agent,
            AgentAction::Custom {
                value: serde_json::json!(1),
            },
        ),
    ] {
        assert!(schedule_action(app.world_mut(), tick, source, action).is_err());
        assert_eq!(
            serde_json::to_value(app.world().resource::<AgentActionQueue>()).unwrap(),
            before
        );
    }
    app.world_mut()
        .resource_mut::<ExecutionContext>()
        .clone_from(&ExecutionContext::Reconstructing);
    schedule_action(app.world_mut(), 1, ActionSource::Human, AgentAction::Noop).unwrap();
    run_agent_tick(app.world_mut()).unwrap();
    assert_eq!(
        app.world().resource::<CurrentInputFrame>().sources,
        vec![ActionSource::Human]
    );
}

#[test]
fn tick_indexed_queue_preserves_due_order_sources_duplicates_and_future() {
    let mut app = app_with_core();
    app.world_mut().resource_mut::<AgentControlState>().mode = ControlMode::Hybrid;
    for (tick, source, action) in [
        (3, ActionSource::Agent, AgentAction::Dodge),
        (1, ActionSource::Human, AgentAction::Jump),
        (1, ActionSource::Agent, AgentAction::Noop),
        (1, ActionSource::Human, AgentAction::Jump),
        (2, ActionSource::Script, AgentAction::Interact),
    ] {
        schedule_action(app.world_mut(), tick, source, action).unwrap();
    }
    let encoded = serde_json::to_value(app.world().resource::<AgentActionQueue>()).unwrap();
    let decoded: AgentActionQueue = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    assert_eq!(
        app.world()
            .resource::<AgentActionQueue>()
            .iter()
            .map(|action| action.tick)
            .collect::<Vec<_>>(),
        [1, 1, 1, 2, 3]
    );
    run_agent_tick(app.world_mut()).unwrap();
    let input = app.world().resource::<CurrentInputFrame>();
    assert_eq!(
        input.actions,
        [AgentAction::Jump, AgentAction::Noop, AgentAction::Jump]
    );
    assert_eq!(
        input.sources,
        [
            ActionSource::Human,
            ActionSource::Agent,
            ActionSource::Human
        ]
    );
    assert_eq!(app.world().resource::<AgentActionQueue>().len(), 2);
    run_agent_tick(app.world_mut()).unwrap();
    assert_eq!(
        app.world().resource::<CurrentInputFrame>().actions,
        [AgentAction::Interact]
    );
    run_agent_tick(app.world_mut()).unwrap();
    assert_eq!(
        app.world().resource::<CurrentInputFrame>().actions,
        [AgentAction::Dodge]
    );
    assert!(app.world().resource::<AgentActionQueue>().is_empty());
}

#[test]
fn queue_restore_and_merge_are_checked_and_preserve_multiplicity() {
    let catalog = app_with_core()
        .world()
        .resource::<AgentActionCatalog>()
        .clone();
    let jump = ScheduledAction {
        tick: 2,
        source: ActionSource::Agent,
        action: AgentAction::Jump,
    };
    let noop = ScheduledAction {
        tick: 2,
        source: ActionSource::Script,
        action: AgentAction::Noop,
    };
    let mut queue = AgentActionQueue::default();
    queue
        .replace_pending(&catalog, 0, vec![jump.clone(), noop.clone()])
        .unwrap();
    let before = serde_json::to_value(&queue).unwrap();
    assert!(
        queue
            .replace_pending(
                &catalog,
                0,
                vec![ScheduledAction {
                    tick: 0,
                    ..jump.clone()
                }]
            )
            .is_err()
    );
    assert_eq!(serde_json::to_value(&queue).unwrap(), before);
    merge_pending_actions(
        &mut queue,
        &catalog,
        0,
        vec![jump.clone(), noop.clone(), jump.clone()],
    )
    .unwrap();
    assert_eq!(
        queue.iter().cloned().collect::<Vec<_>>(),
        [jump.clone(), noop, jump]
    );
    for value in [
        serde_json::json!({"pending":{"2":[]}}),
        serde_json::json!({"pending":{"2":[{"tick":3,"source":"Agent","action":{"type":"Noop"}}]}}),
        serde_json::json!({"pending":{"18446744073709551615":[{"tick":18446744073709551615u64,"source":"Agent","action":{"type":"Noop"}}]}}),
    ] {
        assert!(serde_json::from_value::<AgentActionQueue>(value).is_err());
    }
}

#[test]
fn catalog_change_rejects_pending_action_before_tick_mutation() {
    let mut app = app_with_core();
    schedule_action(app.world_mut(), 1, ActionSource::Agent, AgentAction::Jump).unwrap();
    app.set_supported_actions([AgentActionKind::Noop]);
    assert!(run_agent_tick(app.world_mut()).is_err());
    assert_eq!(app.world().resource::<SimClock>().tick, 0);
    assert_eq!(app.world().resource::<AgentActionQueue>().len(), 1);
}

#[test]
fn terminal_and_tick_limit_reject_before_policy_runs() {
    #[derive(Resource, Default)]
    struct Calls(usize);
    let mut app = app_with_core();
    app.init_resource::<Calls>()
        .add_systems(AgentDecision, |mut calls: ResMut<Calls>| calls.0 += 1);
    app.world_mut().resource_mut::<EpisodeState>().done = true;
    assert!(run_agent_tick(app.world_mut()).is_err());
    app.world_mut().resource_mut::<EpisodeState>().done = false;
    app.world_mut().resource_mut::<SimClock>().tick = u64::from(u32::MAX);
    assert!(run_agent_tick(app.world_mut()).is_err());
    assert_eq!(app.world().resource::<Calls>().0, 0);
}
