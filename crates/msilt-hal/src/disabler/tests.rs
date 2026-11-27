use std::collections::{BTreeMap, VecDeque};

use super::*;

const BINDING: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Backup,
    Change(SystemChange),
    Wait(Duration),
}

struct Mock {
    states: BTreeMap<MsiObject, ObjectState>,
    backups: BTreeMap<std::path::PathBuf, Vec<u8>>,
    events: Vec<Event>,
    faults: VecDeque<(SystemChange, bool)>,
    ignore: Option<SystemChange>,
    backup_failure: bool,
    binding: String,
}

impl Default for Mock {
    fn default() -> Self {
        let mut states = BTreeMap::new();
        for &object in RESTORE_ORDER {
            let state = if object.is_service() {
                ObjectState::Service(ServiceState {
                    start: if object == Scm {
                        ServiceStart::Manual
                    } else {
                        ServiceStart::Automatic
                    },
                    delayed_auto_start: object == Foundation,
                    activity: if object == Scm {
                        ServiceActivity::Paused
                    } else {
                        ServiceActivity::Running
                    },
                })
            } else if object.is_task() {
                ObjectState::Task(TaskState {
                    enabled: true,
                    active: object == BrokerTask,
                })
            } else if object == TrueColorRun {
                ObjectState::Run(RunState {
                    name_64: Some(RegistryValue {
                        value_type: 2,
                        bytes: vec![65, 0, 32, 0, 0, 0],
                    }),
                    spaced_name_32: Some(RegistryValue {
                        value_type: 1,
                        bytes: vec![255, 0, 0],
                    }),
                    ..Default::default()
                })
            } else {
                ObjectState::StoreStartup(2)
            };
            states.insert(object, state);
        }
        Self {
            states,
            backups: BTreeMap::new(),
            events: Vec::new(),
            faults: VecDeque::new(),
            ignore: None,
            backup_failure: false,
            binding: BINDING.to_owned(),
        }
    }
}

impl SystemControlOps for Mock {
    type UpdateLock = ();

    fn lock_update(&mut self) -> Result<(), Diagnostic> {
        Ok(())
    }
    fn backup_binding(&mut self) -> Result<String, Diagnostic> {
        Ok(self.binding.clone())
    }
    fn read(&mut self, object: MsiObject) -> Result<ObjectState, Diagnostic> {
        Ok(self.states[&object].clone())
    }
    fn preflight(&mut self, _: &[MsiObject]) -> Result<(), Diagnostic> {
        Ok(())
    }

    fn execute(&mut self, change: SystemChange) -> Result<(), Diagnostic> {
        self.events.push(Event::Change(change.clone()));
        let mut fail_after = false;
        if self
            .faults
            .front()
            .is_some_and(|(expected, _)| *expected == change)
        {
            fail_after = self.faults.pop_front().unwrap().1;
            if !fail_after {
                return Err(Diagnostic::new("mock write", DiagnosticKind::Io));
            }
        }
        if self.ignore.as_ref() == Some(&change) {
            return Ok(());
        }
        match change {
            SystemChange::ServiceStart(object, start, delayed_auto_start) => {
                let ObjectState::Service(state) = self.states.get_mut(&object).unwrap() else {
                    panic!()
                };
                state.start = start;
                state.delayed_auto_start = delayed_auto_start;
            }
            SystemChange::ServiceActivity(object, activity) => {
                let ObjectState::Service(state) = self.states.get_mut(&object).unwrap() else {
                    panic!()
                };
                assert!(
                    activity == ServiceActivity::Stopped
                        || state.start != ServiceStart::Disabled
                        || state.activity != ServiceActivity::Stopped
                );
                state.activity = activity;
            }
            SystemChange::StopTask(object) => {
                let ObjectState::Task(state) = self.states.get_mut(&object).unwrap() else {
                    panic!()
                };
                state.active = false;
            }
            SystemChange::TaskEnabled(object, enabled) => {
                let ObjectState::Task(state) = self.states.get_mut(&object).unwrap() else {
                    panic!()
                };
                state.enabled = enabled;
            }
            SystemChange::Run { before, after } => {
                assert_eq!(
                    self.states.insert(TrueColorRun, ObjectState::Run(after)),
                    Some(ObjectState::Run(before))
                );
            }
            SystemChange::StoreStartup { before, after } => {
                assert_eq!(
                    self.states
                        .insert(StoreStartup, ObjectState::StoreStartup(after)),
                    Some(ObjectState::StoreStartup(before))
                );
            }
        }
        if fail_after {
            Err(Diagnostic::new("mock write", DiagnosticKind::Io))
        } else {
            Ok(())
        }
    }

    fn wait(&mut self, duration: Duration) {
        self.events.push(Event::Wait(duration));
    }

    fn read_backup(&mut self, path: &Path) -> Result<Option<Vec<u8>>, Diagnostic> {
        Ok(self.backups.get(path).cloned())
    }

    fn create_backup(&mut self, path: &Path, bytes: &[u8]) -> Result<(), Diagnostic> {
        if self.backup_failure {
            return Err(Diagnostic::new("mock persist", DiagnosticKind::Io));
        }
        self.events.push(Event::Backup);
        assert!(
            self.backups
                .insert(path.to_owned(), bytes.to_vec())
                .is_none()
        );
        Ok(())
    }
}

fn all() -> DisablePlan {
    DisablePlan::new([
        DisableTier::Core,
        DisableTier::Lighting,
        DisableTier::TrueColor,
    ])
    .unwrap()
}

fn path() -> &'static Path {
    Path::new("snapshot.json")
}

fn stop(object: MsiObject) -> SystemChange {
    SystemChange::ServiceActivity(object, ServiceActivity::Stopped)
}

#[test]
fn disable_autostart_preserves_live_services_and_tasks_then_restores_config() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let before = disabler.ops.states.clone();
    disabler.disable_autostart(&all(), path()).unwrap();
    assert_eq!(disabler.ops.events.first(), Some(&Event::Backup));
    assert!(!disabler.ops.events.iter().any(|event| matches!(
        event,
        Event::Wait(_)
            | Event::Change(SystemChange::ServiceActivity(_, _) | SystemChange::StopTask(_))
    )));
    assert!(matches!(
        disabler.ops.states[&Center],
        ObjectState::Service(ServiceState {
            activity: ServiceActivity::Running,
            start: ServiceStart::Disabled,
            ..
        })
    ));
    assert_eq!(
        disabler.ops.states[&BrokerTask],
        ObjectState::Task(TaskState {
            enabled: false,
            active: true
        })
    );
    disabler.restore_from_backup(path()).unwrap();
    assert_eq!(disabler.ops.states, before);
}

#[test]
fn stop_now_preserves_start_types_and_other_tiers() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let original = disabler.ops.states.clone();
    disabler
        .stop_services(&DisablePlan::new([DisableTier::Core]).unwrap())
        .unwrap();
    assert_eq!(
        disabler.ops.events,
        [
            Event::Change(stop(Scm)),
            Event::Change(stop(Sendev)),
            Event::Wait(QUIET_PERIOD),
            Event::Change(stop(Center)),
            Event::Change(stop(Foundation))
        ]
    );
    assert_eq!(disabler.ops.states[&BrokerTask], original[&BrokerTask]);
    assert_eq!(disabler.ops.states[&MysticLight], original[&MysticLight]);
    assert!(matches!(
        disabler.ops.states[&Scm],
        ObjectState::Service(ServiceState {
            activity: ServiceActivity::Stopped,
            start: ServiceStart::Manual,
            ..
        })
    ));
}

#[test]
fn complete_inventory_is_read_only_and_allowlist_is_exact() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    assert_eq!(disabler.inspect_msi_software().unwrap().len(), 12);
    assert!(disabler.ops.events.is_empty());
    assert_eq!(MsiObject::from_name("msi_center_service").unwrap(), Center);
    assert_eq!(
        MsiObject::from_name("MSI TRUE COLOR").unwrap(),
        TrueColorRun
    );
    assert_eq!(
        MsiObject::from_name(r"\MSI Task Host - LEDKeeper2_Host").unwrap(),
        LedKeeperTask
    );
    for name in [
        "wmiacpi",
        "MSIAPCFG.DLL",
        "MofImagePath",
        "NahimicService",
        "Killer Network Service",
        "msiserver",
    ] {
        assert!(
            matches!(MsiObject::from_name(name), Err(DisablerError::Protected)),
            "{name}"
        );
    }
    for name in [
        "MSI",
        "WmiAcpiProxy",
        "MSI_Center_Service ",
        r"\Folder\OneDC_Updater",
    ] {
        assert!(
            matches!(MsiObject::from_name(name), Err(DisablerError::NotManaged)),
            "{name}"
        );
    }
}

#[test]
fn durable_backup_precedes_consumer_stop_then_quiet_before_brokers() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let before = disabler.inspect_msi_software().unwrap();
    let report = disabler.disable(&all(), path()).unwrap();
    let events = &disabler.ops.events;
    assert_eq!(events.first(), Some(&Event::Backup));
    let quiet = events
        .iter()
        .position(|e| *e == Event::Wait(Duration::from_millis(500)))
        .unwrap();
    for object in [LightKeeper, MysticLight, TrueColorService, Scm, Sendev] {
        assert!(events[..quiet].contains(&Event::Change(stop(object))));
    }
    assert!(events[..quiet].contains(&Event::Change(SystemChange::StopTask(BrokerTask))));
    for object in BROKERS {
        assert!(!events[..quiet].contains(&Event::Change(stop(*object))));
        assert!(events[quiet..].contains(&Event::Change(stop(*object))));
    }
    assert_eq!(report.changed.len(), 12);
    for row in report.observed {
        assert_eq!(
            row.state,
            disabled_state(
                row.object,
                &before
                    .iter()
                    .find(|old| old.object == row.object)
                    .unwrap()
                    .state
            )
            .unwrap()
        );
    }
    let snapshot = MsiBackupSnapshot::from_json(&disabler.ops.backups[path()]).unwrap();
    assert_eq!(snapshot.inventory(), before);
    assert_eq!(snapshot.plan(), &all());
    assert_eq!(
        MsiBackupSnapshot::from_json(&snapshot.to_json().unwrap()).unwrap(),
        snapshot
    );
}

#[test]
fn presets_modify_only_selected_and_present_objects() {
    let mut mock = Mock::default();
    mock.states.insert(LightKeeper, ObjectState::Missing);
    let original = mock.states.clone();
    let mut disabler = SoftwareDisabler::with_ops(mock);
    let plan = DisablePlan::new([DisableTier::Lighting]).unwrap();
    let report = disabler.disable(&plan, path()).unwrap();
    assert_eq!(report.changed, [LedKeeperTask, MysticLight]);
    assert_eq!(
        report
            .observed
            .iter()
            .find(|row| row.object == LightKeeper)
            .unwrap()
            .state,
        ObjectState::Missing
    );
    assert!(
        !disabler
            .ops
            .events
            .iter()
            .any(|e| matches!(e, Event::Wait(_)))
    );
    for (&object, state) in &disabler.ops.states {
        if object != MysticLight && object != LedKeeperTask {
            assert_eq!(state, &original[&object]);
        }
    }
    assert!(matches!(
        DisablePlan::new([]),
        Err(DisablerError::EmptyPlan)
    ));
}

#[test]
fn repeated_disable_and_restore_preserve_original_configuration() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let mut expected = disabler.ops.states.clone();
    expected.insert(
        BrokerTask,
        ObjectState::Task(TaskState {
            enabled: true,
            active: false,
        }),
    );
    expected.insert(
        LedKeeperTask,
        ObjectState::Task(TaskState {
            enabled: true,
            active: false,
        }),
    );
    disabler.disable(&all(), path()).unwrap();
    let bytes = disabler.ops.backups[path()].clone();
    disabler.ops.events.clear();
    assert!(disabler.disable(&all(), path()).unwrap().changed.is_empty());
    assert!(disabler.ops.events.is_empty());
    assert_eq!(disabler.ops.backups[path()], bytes);
    assert_eq!(
        disabler.restore_from_backup(path()).unwrap().changed.len(),
        12
    );
    assert_eq!(disabler.ops.states, expected);
    assert_eq!(
        disabler.ops.events.first(),
        Some(&Event::Change(SystemChange::ServiceStart(
            Foundation,
            ServiceStart::Automatic,
            true
        )))
    );
    disabler.ops.events.clear();
    assert!(
        disabler
            .restore_from_backup(path())
            .unwrap()
            .changed
            .is_empty()
    );
    assert!(disabler.ops.events.is_empty());
}

#[test]
fn backup_failure_prevents_any_mutation() {
    let mut disabler = SoftwareDisabler::with_ops(Mock {
        backup_failure: true,
        ..Mock::default()
    });
    let before = disabler.ops.states.clone();
    assert!(matches!(
        disabler.disable(&all(), path()),
        Err(DisablerError::System(_))
    ));
    assert!(disabler.ops.events.is_empty());
    assert_eq!(disabler.ops.states, before);
}

#[test]
fn partial_failure_including_last_mutation_rolls_back_in_reverse() {
    let mut mock = Mock::default();
    let mut expected = mock.states.clone();
    expected.insert(
        BrokerTask,
        ObjectState::Task(TaskState {
            enabled: true,
            active: false,
        }),
    );
    mock.faults.push_back((
        SystemChange::ServiceStart(Center, ServiceStart::Disabled, false),
        true,
    ));
    let mut disabler = SoftwareDisabler::with_ops(mock);
    assert!(
        matches!(disabler.disable(&all(), path()), Err(DisablerError::ApplyFailed {
        object: Center, rollback_failures, ..
    }) if rollback_failures.is_empty())
    );
    assert_eq!(disabler.ops.states, expected);
    let operations = &disabler.ops.events;
    let fail = operations
        .iter()
        .position(|e| {
            *e == Event::Change(SystemChange::ServiceStart(
                Center,
                ServiceStart::Disabled,
                false,
            ))
        })
        .unwrap();
    assert_eq!(
        operations[fail + 1],
        Event::Change(SystemChange::ServiceStart(
            Center,
            ServiceStart::Automatic,
            false
        ))
    );
    assert_eq!(
        operations.last(),
        Some(&Event::Change(SystemChange::TaskEnabled(UpdaterTask, true)))
    );
    assert!(disabler.ops.backups.contains_key(path()));
}

#[test]
fn rollback_failure_keeps_snapshot_and_can_be_retried() {
    let mut mock = Mock::default();
    mock.faults.extend([
        (
            SystemChange::ServiceStart(Center, ServiceStart::Disabled, false),
            true,
        ),
        (
            SystemChange::ServiceStart(Center, ServiceStart::Automatic, false),
            false,
        ),
    ]);
    let mut disabler = SoftwareDisabler::with_ops(mock);
    assert!(
        matches!(disabler.disable(&all(), path()), Err(DisablerError::ApplyFailed {
        object: Center, rollback_failures, ..
    }) if rollback_failures.len() == 1 && rollback_failures[0].object == Center)
    );
    assert_eq!(
        disabler.restore_from_backup(path()).unwrap().changed,
        [Center]
    );
}

#[test]
fn unchanged_readback_refuses_success_before_broker_stop() {
    let mock = Mock {
        ignore: Some(SystemChange::ServiceStart(
            LightKeeper,
            ServiceStart::Disabled,
            false,
        )),
        ..Mock::default()
    };
    let mut disabler = SoftwareDisabler::with_ops(mock);
    assert!(
        matches!(disabler.disable(&all(), path()), Err(DisablerError::ApplyFailed {
        object: LightKeeper, cause, rollback_failures,
    }) if cause.kind == DiagnosticKind::Rejected && rollback_failures.is_empty())
    );
    assert!(!disabler.ops.events.contains(&Event::Change(stop(Center))));
}

#[test]
fn invalid_backup_and_other_user_are_refused_before_mutation() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    disabler.disable(&all(), path()).unwrap();
    disabler.ops.events.clear();
    let original = disabler.ops.backups[path()].clone();
    let mut invalid: serde_json::Value = serde_json::from_slice(&original).unwrap();
    invalid["inventory"][0]["object"] = serde_json::json!("WmiAcpi");
    disabler
        .ops
        .backups
        .insert(path().to_owned(), serde_json::to_vec(&invalid).unwrap());
    assert!(matches!(
        disabler.restore_from_backup(path()),
        Err(DisablerError::InvalidBackup(_))
    ));
    disabler.ops.backups.insert(path().to_owned(), original);
    disabler.ops.binding = "f".repeat(64);
    assert!(matches!(
        disabler.restore_from_backup(path()),
        Err(DisablerError::MachineOrUserMismatch)
    ));
    assert!(disabler.ops.events.is_empty());
}

#[test]
fn restore_rejects_unrelated_registry_edits_without_losing_other_objects() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let before = disabler.ops.states.clone();
    disabler.disable(&all(), path()).unwrap();
    let external = ObjectState::Run(RunState {
        name_64: Some(RegistryValue {
            value_type: 1,
            bytes: vec![1, 2, 3],
        }),
        ..Default::default()
    });
    disabler.ops.states.insert(TrueColorRun, external.clone());
    assert!(
        matches!(disabler.restore_from_backup(path()), Err(DisablerError::RestoreFailed(failures))
        if failures.len() == 1 && failures[0].object == TrueColorRun && failures[0].diagnostic.kind == DiagnosticKind::ConflictDetected)
    );
    assert_eq!(disabler.ops.states[&TrueColorRun], external);
    assert_eq!(disabler.ops.states[&StoreStartup], before[&StoreStartup]);
}

#[test]
fn different_plan_cannot_replace_backup_and_policy_can_refuse_disable() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    let plan = DisablePlan::new([DisableTier::Core]).unwrap();
    disabler.disable(&plan, path()).unwrap();
    disabler.ops.events.clear();
    assert!(matches!(
        disabler.disable(&all(), path()),
        Err(DisablerError::PlanMismatch)
    ));
    assert!(disabler.ops.events.is_empty());
    let mut mock = Mock::default();
    mock.states
        .insert(StoreStartup, ObjectState::StoreStartup(4));
    let mut disabler = SoftwareDisabler::with_ops(mock);
    assert!(
        matches!(disabler.disable(&all(), path()), Err(DisablerError::System(diagnostic))
        if diagnostic.kind == DiagnosticKind::ConflictDetected)
    );
    assert!(disabler.ops.events.is_empty());
}

#[test]
fn legacy_eleven_item_backup_upgrades_and_restores_cleanly() {
    let mut disabler = SoftwareDisabler::with_ops(Mock::default());
    disabler.disable(&all(), path()).unwrap();
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&disabler.ops.backups[path()]).unwrap();
    legacy["inventory"]
        .as_array_mut()
        .unwrap()
        .retain(|row| row["object"] != r"\MSI Task Host - LEDKeeper2_Host");
    assert_eq!(legacy["inventory"].as_array().unwrap().len(), 11);
    disabler
        .ops
        .backups
        .insert(path().to_owned(), serde_json::to_vec(&legacy).unwrap());
    // Re-running disable against a v1 11-item backup succeeds and disables newly tracked LedKeeperTask.
    disabler.ops.states.insert(
        LedKeeperTask,
        ObjectState::Task(TaskState {
            enabled: true,
            active: true,
        }),
    );
    let report = disabler.disable(&all(), path()).unwrap();
    assert_eq!(report.changed, [LedKeeperTask]);
    // Restoring from the v1 11-item backup succeeds cleanly without failing on missing LedKeeperTask.
    let restored = disabler.restore_from_backup(path()).unwrap();
    assert_eq!(restored.changed.len(), 11);
}
