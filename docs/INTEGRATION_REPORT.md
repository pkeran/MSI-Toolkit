# MSI-Toolkit 后端真实接线与 Release 真机实测报告

测量日期：2026-11-26（Asia/Shanghai）。以下路径相对于仓库根目录。

第 1–6 节与附录保留首次后端集成记录；空闲工作集优化后的结果见 [第 7 节](#7-空闲工作集优化与一键验收)。

## 1. 交付结论

- `msilt-daemon` 已真实接入本地 `msilt-hal::WindowsHal` 与实现 `AutomationEngine` 的 `msilt_automation::RuleEngine`；二进制默认采用真实只读 HAL。
- 全量后端测试 **162 项通过，0 失败、0 忽略、0 编译警告**；另在 Release 模式运行跨进程集成测试，**4 项全部通过**。
- Release 实例经命名管道实际读回本机 **MS-17S3、240 Hz、亮度 69%、电池 93%、当前 TrueColor_DCIP3_6300，以及 37 个已安装色彩配置 ID**。
- 最终 Release daemon 为 **1,169,408 字节**。PE 普通/延迟导入中，Direct2D、Direct3D、DXGI、Slint、Skia、WebView2、NVML、NVAPI 均为 **0**。
- 初始化完成并断开诊断客户端后，静置 **15,007.8539 ms**：**Working Set 20.33203125 MiB、Private 3.4375 MiB、16 线程、285 句柄、CPU 时间增量 0 ms**。
- 测量进程收到 `WM_CLOSE` 后以 **退出码 0** 结束；按进程 ID 与进程名再次查询，**残留 daemon 进程为 0**。

## 2. 接线与安全边界

### 2.1 工厂与开关

`crates/msilt-daemon/Cargo.toml` 新增两个本地依赖：

```toml
msilt-hal = { path = "../msilt-hal" }
msilt-automation = { path = "../msilt-automation" }
```

入口调用 `Daemon::run_native`，在确定交互用户身份后，由 `BackendOptions` 建立真实后端。HAL 实例在专属 HAL 工作线程内创建，保留现有 COM/串行执行约束。

| 参数 | 实际行为 |
| --- | --- |
| 默认 | 真实只读 Windows HAL；`include_nvml=false`；不打开 ACPI 通道 |
| `--readonly` | 再在执行器层强制屏蔽所有写能力，包括注入的可写测试后端 |
| `--acpi` | 显式开启经硬件身份审计的只读 ACPI；原生 EC 写门仍关闭 |
| `--allow-platform-control` | 显式允许非 EC 刷新率、亮度、色彩配置控制；与 `--readonly` 冲突 |
| `--noop-backend` | 纯离线 Noop HAL + Noop 自动化引擎 |
| `--no-tray` | 隐藏消息窗口与事件泵继续工作，关闭托盘/OSD 展示 |

HAL 只增加了分离“ACPI 只读”与“平台控制”的极简构造接口 `WindowsHal::for_platform_control(read_acpi)`，以及只读快照的已安装色彩配置 ID 投影。即使同时指定 `--acpi --allow-platform-control`，`ControlOptions.acpi` 仍为 `false`。

### 2.2 EC 与服务安全

- 原生工厂通过 `ec_map_loader::load_ms_17s3()` 解析嵌入自 `configs/ec_maps/ms-17s3.json` 的完整封套及 Core `guardMap`。当前封套为未验证、空写白名单，因此 `PhaseZeroPolicy` 返回 `UnverifiedMapping`，授予的 EC 写能力为零。
- daemon 保留唯一 EC 执行路径：`EcWritePolicy` → `WriteGuard::prepare` → 受控提交/回读/回滚。合成内存设备测试实证验证了 `0x50` 拒绝、缺失策略拒绝、提交前撤销、只读覆盖与成功回读；真机工厂不给出 EC 写授权。
- 默认与只读模式的 `writable=0`。真机 IPC 写请求分别得到精确的 `Unsupported`（刷新率/亮度）与 `WriteNotAllowed`（未验证 EC Cooler Boost），硬件读回保持不变。
- 工厂不接入 `SoftwareDisabler`，启动及遥测路径均不执行服务停用。最终 PE 导入中的 `ControlService`、`ChangeServiceConfig`、`CreateService`、`DeleteService` 命中数均为零。
- 默认启动与三个约 1 Hz 的遥测周期后，均使用 Toolhelp 枚举真实 daemon 已加载模块：**未出现 nvml.dll 或 nvapi 模块**。15 秒静置实例也再次检查了这两项。

本次真机运行均未开启 `--acpi`；EC 固件、CPU/GPU 温度与风扇传感值保持缺失状态，遥测 IPC 发出真实采样时间戳，不填造传感值。

### 2.3 交互用户自动化配置

通过经验证交互用户的令牌模拟，调用 `SHGetKnownFolderPath(FOLDERID_LocalAppData, token)`，读取其 `MSI-Toolkit\automation.toml`，而不从当前目录或服务账号环境变量推断配置路径。

- 配置目录拒绝 reparse point；文件以 `FILE_FLAG_OPEN_REPARSE_POINT` 打开并检查普通文件属性。
- 读取上限为 512 KiB + 1，使用真实 `AutomationRuleSet::parse` 的严格解析与限制。
- 缺失、非法 TOML、未知字段、非法 UTF-8、超大文件与非普通文件均回退为安全空规则集，启动继续。
- 本地 `daemon-diagnostics.log` 记录加载/回退结果，按同一交互用户令牌写入，并限制累计写入规模。
- 已有 `app.json` 的 `automation.enabled` 仍控制规则执行。额外的真实 RuleEngine → 协调器 → 租约 → 内存 HAL 测试验证了 AC 规则生效及条件消失后恢复基线。

本机 `automation.toml` 当前缺失；真实实例的诊断为 `missing; using safe empty rules`。

### 2.4 生产与诊断管道

生产默认 `\\.\pipe\MSIToolkit-IPC-v1` 保留提权交互用户、登录 SID、完整性级别、DACL/标签回验与受保护安装路径校验。CLI 源码与其默认传输验证均保持原样，17 项 CLI 测试通过。

当前会话为中完整性。为在本机完成真实只读联调，新增严格显式的诊断组合：

```powershell
msilt-daemon.exe --readonly --no-tray --pipe <独立名字>
```

此组合仅对非生产管道成立；生产管道名称以不区分大小写方式识别，防止大小写别名降级。诊断 ACL 限于当前登录 SID，握手仍核对用户、会话、登录 SID 与完整性级别。

只有进一步指定 `--test-client-pid <pid>`，并验证该同登录、同完整性进程仍存活，才允许它充当遥测 UI。保持进程句柄以防 PID 复用。跨进程测试客户端也校验服务端 PID 必须等于自己启动的 daemon。测试实证：

1. 固定 PID 客户端完成 `hello/getSnapshot/getConfig`、state/telemetry 订阅和取消。
2. 同登录但另一个进程得到 `AccessDenied`。
3. 未固定 PID 的诊断端点可以读取状态，但自称 UI 订阅 telemetry 得到 `Unsupported`。
4. 生产管道及其小写别名不能使用诊断 PID 参数；只读与平台控制参数冲突时启动被拒绝。

## 3. 测试结果

执行命令：

```powershell
$env:MSILT_EXPECT_MS17S3='1'
cargo test -p msilt-core -p msilt-hal -p msilt-probe -p msilt-automation -p msilt-cli -p msilt-daemon
cargo test --release -p msilt-daemon --test native_stack -- --nocapture
```

`MSILT_EXPECT_MS17S3=1` 额外强制断言主板为 MS-17S3、当前内屏 240 Hz、亮度/电池实际存在且色彩配置列表非空。

| 测试组 | 通过数 |
| --- | ---: |
| msilt-core 单元测试 | 32 |
| msilt-hal 单元测试 | 40 |
| msilt-probe 单元测试 | 2 |
| msilt-automation 单元测试 + 零分配测试 | 43 + 1 |
| msilt-cli 单元/原生命名管道测试 | 17 |
| msilt-daemon 单元/进程内原生命名管道测试 | 21 |
| msilt-daemon 跨进程测试 | 4 |
| Core/Automation doctest | 1 + 1 |
| **全量后端合计** | **162** |
| 另外重跑的 Release 跨进程测试 | 4 |

所有测试均零失败、零忽略；两次测试编译输出与最终 Release 构建输出中的 `warning` 数均为 0。改动 Rust 文件的 `rustfmt --check --edition 2024 --config skip_children=true` 退出码为 0。

## 4. Release 产物与导入审计

```powershell
cargo build --release -p msilt-daemon -p msilt-cli -p msilt-probe
```

沿用工作区既有 Release 设置：`opt-level="z"`、fat LTO、`codegen-units=1`、strip symbols、`panic="abort"`、`debug=0`、关闭 incremental。

| EXE | 精确字节 | MiB |
| --- | ---: | ---: |
| msilt-daemon.exe | 1,169,408 | 1.115234375 |
| msilt-cli.exe | 433,664 | 0.41357421875 |
| msilt-probe.exe | 605,696 | 0.57763671875 |

最终产物 SHA-256：

```text
msilt-daemon.exe f235095c47bacf7c10a42e8eceedcfcd9294b1e0f4c7ed4e07c925834e143140
msilt-cli.exe    50bf09d296f8f7d3748961b50dddbb2d8ca7953837b16741cbecbb8f33f6d3be
msilt-probe.exe  e3fc918d3036782cdd1b4caaaaeffd26c389495a7d067fb2c5eea8d7efb69159
```

使用 Visual Studio Build Tools 14.44.35207 的 `dumpbin /nologo /imports` 导出真实 PE 导入表，并由本机 `pefile` 检查普通与延迟导入目录。结果：**25 个普通导入描述符、0 个延迟导入描述符**；指定的全部禁用项均为零。`cargo tree -p msilt-daemon --edges normal` 同时确认依赖图无 UI、Slint、Skia、WebView2 或 Tokio 等图形/异步框架。

## 5. 真机 15 秒静置实测

初次测量使用 `crates/msilt-daemon/tools/Measure-Readonly.ps1`：以隐藏窗口启动独立的 Release 进程，核对管道服务端 PID，完成 CLI 身份的 hello、快照与配置读取后断开客户端，等待 1 秒，再读取累计 CPU 时间并静置 15 秒。结束时读取 .NET `Process` 原始计数，以 `WM_CLOSE` 结束该 PID 并核对残留。当前脚本的冷却时间与复测结果见第 7 节。

```powershell
& .\crates\msilt-daemon\tools\Measure-Readonly.ps1
```

| 项目 | 最终 Release 实测值 |
| --- | ---: |
| 测量时刻 | 2026-11-26T13:09:07.5740397+08:00 |
| daemon PID | 49012 |
| 静置实际时长 | 15,007.8539 ms |
| WorkingSet64 | 21,319,680 字节 / 20.33203125 MiB |
| PrivateMemorySize64 | 3,604,480 字节 / 3.4375 MiB |
| 线程数 | 16 |
| 句柄数 | 285 |
| 静置前累计 CPU 时间 | 109.375 ms |
| 静置后累计 CPU 时间 | 109.375 ms |
| **15 秒 CPU 时间增量** | **0 ms** |
| NVML / NVAPI 已加载 | false / false |
| 优雅退出码 / 残留进程 | 0 / 0 |

与任务给定的本机 MSI Center **790 MiB Private / 15 进程**基线对比：

| 后台 | Private | 进程数 |
| --- | ---: | ---: |
| MSI Center 基线 | 790 MiB | 15 |
| 本次已初始化、无订阅的只读 daemon | 3.4375 MiB | 1 |
| 减少量 | **786.5625 MiB（99.5648734%）** | **14** |

Private 对比使用同一内存口径；本次 Working Set 单独列示。1 Hz 订阅行为与动态模块检查由 Release 跨进程测试验证，静置资源测量时无客户端或周期遥测订阅。

## 6. 环境原始输出

```text
2026-11-26T13:10:28.2200862+08:00
Caption     : Microsoft Windows 11 家庭版 中文版
Version     : 10.0.26200
BuildNumber : 26200
Manufacturer : Micro-Star International Co., Ltd.
Product      : MS-17S3
Version      : REV:1.0
rustc 1.99.1 (58cf8f9c3 2026-10-13)
binary: rustc
commit-hash: 58cf8f9c35b355199727627d07ae34a4e2846d67
commit-date: 2026-10-13
host: x86_64-pc-windows-msvc
release: 1.99.1
LLVM version: 23.1.1
cargo 1.99.1 (5f94df478 2026-08-27)
```

## 附录 A：全量后端测试原始输出

<details>
<summary>展开完整测试输出</summary>

```text
   Compiling msilt-daemon v0.1.0 (crates\msilt-daemon)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.42s
     Running unittests src\lib.rs (target\debug\deps\msilt_automation-197679712f0a6c26.exe)

running 43 tests
test tests::battery_threshold_hysteresis_math ... ok
test tests::governor_forecast_is_side_effect_free ... ok
test tests::lease_stack_ranks_and_evicts_deterministically ... ok
test tests::debounce_primitive_is_asymmetric ... ok
test tests::daily_trigger_uses_the_configured_local_offset ... ok
test tests::action_budget_is_bounded_and_converges ... ok
test tests::daemon_refusal_latches_until_recovery ... ok
test tests::battery_hysteresis_holds_until_recovered ... ok
test tests::dry_run_reports_hits_and_blocks_without_mutating_state ... ok
test tests::failed_apply_drops_the_lease_and_cools_down ... ok
test tests::cooldown_and_flap_guard_are_self_clearing ... ok
test tests::foreground_process_exit_mode_survives_alt_tab ... ok
test tests::game_enter_acquires_and_exit_releases ... ok
test tests::capability_change_unlatches_unsupported_rules ... ok
test tests::hotkey_toggle_commits_immediately ... ok
test tests::idle_trigger_requires_the_threshold ... ok
test tests::interval_trigger_uses_one_deadline_at_a_time ... ok
test tests::manual_override_is_never_grabbed ... ok
test tests::misconfigured_power_rule_is_capped_and_frozen ... ok
test tests::empty_rule_set_never_schedules ... ok
test tests::governor_enforces_budget_gaps_and_freeze ... ok
test tests::debounce_boundaries_and_single_deadline ... ok
test tests::parses_the_documented_schema ... ok
test tests::lease_ttl_expires_and_retries_after_cooldown ... ok
test tests::power_storm_produces_zero_ec_writes ... ok
test tests::priority_and_specificity_arbitrate ... ok
test tests::rejects_bracket_bombs_before_parsing ... ok
test tests::rejects_bad_actions ... ok
test tests::rejects_oversized_and_non_utf8_configs ... ok
test tests::rejects_timer_periods_below_one_minute ... ok
test tests::rejects_deep_conditions_and_leaf_explosion ... ok
test tests::rejects_schema_mismatch_duplicates_and_bad_ids ... ok
test tests::rejects_unknown_and_missing_keys ... ok
test tests::rejects_out_of_range_scalars_and_limits ... ok
test tests::rejects_bad_conditions ... ok
test tests::weekday_and_daily_helpers ... ok
test tests::release_always_precedes_acquire ... ok
test tests::session_and_display_triggers_release_on_falsify ... ok
test tests::parses_the_shipped_example ... ok
test tests::world_matching_is_case_insensitive_and_bounded ... ok
test tests::time_window_handles_midnight_wrap ... ok
test tests::reload_releases_changed_rules_and_keeps_identical_ones ... ok
test tests::rejects_too_many_rules ... ok

test result: ok. 43 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

     Running tests\allocations.rs (target\debug\deps\allocations-19ae22a1e867963d.exe)

running 1 test
test steady_state_events_allocate_nothing ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src\main.rs (target\debug\deps\msilt_cli-771bd8aeb0a8272c.exe)

running 17 tests
test args::tests::parses_all_performance_modes ... ok
test args::tests::parses_all_release_targets ... ok
test args::tests::parses_control_values_and_boundaries ... ok
test args::tests::parses_read_commands_and_json_flag ... ok
test args::tests::rejects_invalid_arguments_without_contacting_the_daemon ... ok
test client::tests::frame_reader_checks_lengths_before_allocating_and_reports_truncation ... ok
test client::tests::rejected_hello_returns_the_daemon_error_without_sending_the_command ... ok
test output::tests::config_revision_and_unknown_values_are_distinct_from_zero ... ok
test output::tests::json_output_is_one_unframed_v1_response ... ok
test client::tests::release_serializes_a_channel_target_and_does_not_replay_a_rejected_write ... ok
test pipe::tests::untrusted_pipe_peer_is_rejected ... ok
test client::tests::lost_or_wrong_write_reply_is_indeterminate ... ok
test client::tests::control_uses_handshake_revision_and_returns_observed_snapshot ... ok
test client::tests::invalid_hello_prevents_the_command_from_being_sent ... ok
test client::tests::status_and_config_send_their_own_requests ... ok
test pipe::tests::process_image_requires_the_exact_canonical_path ... ok
test pipe::tests::overlapped_local_pipe_reads_writes_and_reaps_a_timed_out_read ... ok

test result: ok. 17 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.06s

     Running unittests src\lib.rs (target\debug\deps\msilt_core-2981c804607dd1d1.exe)

running 32 tests
test config::tests::defaults_and_persisted_config_round_trip ... ok
test ec::tests::final_verification_detects_collateral_changes_and_latches ... ok
test ec::tests::failed_rollback_blocks_the_next_transaction ... ok
test ec::tests::no_unmapped_values_or_fan_register_bypass ... ok
test config::tests::rejects_future_versions_ranges_and_unknown_fields ... ok
test ec::tests::read_only_map_parses_without_inventing_writable_addresses ... ok
test ec::tests::map_requires_a_valid_nonzero_hot_end_floor ... ok
test ec::tests::only_a_verified_layout_can_raise_the_ceiling_and_lock_the_anchor ... ok
test ec::tests::successful_fan_apply_is_safe_at_every_intermediate_curve ... ok
test ec::tests::snapshot_read_failure_never_enters_apply ... ok
test ec::tests::unknown_board_or_firmware_refuses_before_io ... ok
test ipc::tests::cli_cannot_enable_periodic_telemetry ... ok
test ec::tests::unsafe_snapshot_latches_without_writing ... ok
test ec::tests::no_rollback_or_more_io_on_indeterminate_timeout ... ok
test ec::tests::prepare_captures_snapshot_and_reverse_instructions_without_writing ... ok
test ipc::tests::rejects_malformed_and_unauthorized_message_shapes ... ok
test ipc::tests::queue_ttl_and_release_wire_shape ... ok
test ipc::tests::decodes_partial_and_coalesced_frames ... ok
test ec::tests::rolls_back_attempted_fan_writes_even_if_failed_write_had_side_effects ... ok
test ec::tests::readback_mismatch_restores_original_value ... ok
test ipc::tests::round_trips_error_and_push_messages ... ok
test ipc::tests::wire_shape_and_length_are_stable ... ok
test ec::tests::denylist_overrides_map_and_blocks_whole_fan_transaction ... ok
test model::tests::state_based_validation_uses_verified_choices ... ok
test model::tests::capability_flags_preserve_unknown_bits_on_wire ... ok
test model::tests::mode_wire_text_matches_environment_value ... ok
test ec::tests::rejects_curve_shape_unverified_knots_and_hot_floor_before_io ... ok
test ec::tests::map_rejects_unsafe_schema_and_ambiguous_bindings ... ok
test model::tests::no_values_are_fabricated_in_the_initial_snapshot ... ok
test model::tests::color_profile_id_is_not_a_path ... ok
test ipc::tests::oversized_output_leaves_no_partial_frame ... ok
test ipc::tests::rejects_invalid_lengths_before_waiting_for_payload ... ok

test result: ok. 32 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running unittests src\lib.rs (target\debug\deps\msilt_daemon-d70be30cb11bc533.exe)

running 21 tests
test lease::tests::bounded_stack_keeps_the_best_and_breaks_ties_by_latest_acquire ... ok
test lease::tests::manual_priority_and_automatic_stack_restore_the_first_baseline ... ok
test hal_executor::tests::ec_writes_require_policy_and_guard_and_publish_verified_readback ... ok
test hal_executor::tests::explicit_readonly_masks_even_an_injected_platform_writer ... ok
test tests::custom_endpoint_cannot_escape_the_local_pipe_namespace ... ok
test backends::tests::phase_zero_mapping_never_grants_a_write ... ok
test security::tests::native_access_check_confirms_the_medium_initiator_mask ... ok
test config_store::tests::automation_uses_real_rule_parser_and_engine ... ok
test config_store::tests::automation_directory_is_not_accepted_as_a_file ... ok
test config_store::tests::missing_invalid_and_oversized_rules_fall_back_and_record_a_local_diagnostic ... ok
test pipe_integration::config_corruption_is_preserved_and_failed_replace_is_atomic ... ok
test pipe_integration::real_rule_engine_drives_coordinator_leases_and_restores_mock_baseline ... ok
test pipe_integration::automation_lease_resumes_after_manual_release_then_restores_baseline ... ok
test desktop::tests::hidden_message_pump_dispatches_clock_changes_and_quits ... ok
test osd::tests::software_dib_contains_glyphs_and_premultiplied_alpha ... ok
test pipe_integration::rejects_anonymous_wrong_version_and_overlapping_requests ... ok
test pipe_integration::session_lock_during_preflight_prevents_write_until_new_inspection ... ok
test pipe_integration::queued_ttl_and_disconnect_never_execute_waiting_controls ... ok
test pipe_integration::late_sample_does_not_cross_a_subscription_generation ... ok
test pipe_integration::real_pipe_v1_round_trip_and_last_subscriber_stops_timer ... ok
test pipe_integration::enforces_connection_capacity_and_closes_a_stalled_half_frame ... ok

test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 5.29s

     Running unittests src\main.rs (target\debug\deps\msilt_daemon-ea6f07a1894d2aa2.exe)

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

     Running tests\native_stack.rs (target\debug\deps\native_stack-d3b3dee7c6355e7f.exe)

running 4 tests
test unauthorized_process_probe ... ok
msilt-daemon: --test-client-pid requires --readonly --no-tray and a custom --pipe
msilt-daemon: automation.toml: missing; using safe empty rules
msilt-daemon: --test-client-pid requires --readonly --no-tray and a custom --pipe
msilt-daemon: --readonly conflicts with --allow-platform-control
test diagnostic_flags_cannot_reclassify_the_production_endpoint ... ok
test unpinned_custom_endpoint_does_not_grant_ui_telemetry ... ok
test native_readonly_hal_and_rule_engine_cross_process ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.83s

     Running unittests src\lib.rs (target\debug\deps\msilt_hal-2e8040440e2d02d2.exe)

running 40 tests
test battery::tests::actual_capacity_and_signed_rate_use_the_correct_units ... ok
test battery::tests::relative_and_unknown_values_never_invent_watts_or_current ... ok
test display::tests::panel_ids_and_profile_ids_do_not_embed_paths ... ok
test disabler::tests::backup_failure_prevents_any_mutation ... ok
test disabler::tests::presets_modify_only_selected_and_present_objects ... ok
test disabler::tests::unchanged_readback_refuses_success_before_broker_stop ... ok
test disabler::tests::complete_inventory_is_read_only_and_allowlist_is_exact ... ok
test disabler::tests::durable_backup_precedes_consumer_stop_then_quiet_before_brokers ... ok
test display::tests::color_write_resolves_only_enumerated_ids ... ok
test disabler::tests::restore_rejects_unrelated_registry_edits_without_losing_other_objects ... ok
test disabler::tests::rollback_failure_keeps_snapshot_and_can_be_retried ... ok
test display::tests::display_api_restart_and_bad_modes_are_not_success ... ok
test display::tests::icc_requires_a_bounded_tag_payload ... ok
test disabler::tests::different_plan_cannot_replace_backup_and_policy_can_refuse_disable ... ok
test display::tests::refresh_request_keeps_geometry_and_uses_only_enumerated_hz ... ok
test display::tests::refresh_transaction_preflights_then_returns_readback_not_request ... ok
test hid_rgb::tests::all_24_distinct_colors_produce_ordered_pairs_and_abort_on_error ... ok
test disabler::tests::repeated_disable_and_restore_preserve_original_configuration ... ok
test disabler::tests::partial_failure_including_last_mutation_rolls_back_in_reverse ... ok
test hid_rgb::tests::detected_oem_conflicts_require_explicit_waiver_and_complete_inventory ... ok
test hid_rgb::tests::frame_covers_each_zone_once_and_groups_equal_colors ... ok
test hid_rgb::tests::hid_filter_and_unknown_conflict_are_fail_closed ... ok
test hid_rgb::tests::reports_match_select_and_static_wire_bytes ... ok
test disabler::tests::invalid_backup_and_other_user_are_refused_before_mutation ... ok
test mock::tests::acknowledged_mismatch_rolls_back_and_fresh_read_is_required ... ok
test mock::tests::all_mock_controls_and_read_errors_preserve_observed_state ... ok
test mock::tests::timeout_after_apply_is_indeterminate_and_never_rolls_back ... ok
test nvml::tests::text_and_driver_failure_classification_are_bounded ... ok
test windows_hal::tests::control_constructor_still_rejects_invalid_input_before_native_io ... ok
test windows_hal::tests::default_telemetry_and_mutation_do_not_activate_native_channels ... ok
test ec_map_loader::tests::parses_full_envelope_but_only_core_guard_is_executable ... ok
test windows_hal::tests::platform_control_cannot_open_the_ec_write_or_nvml_channel ... ok
test mock::tests::guard_blocks_builtin_denies_unknown_firmware_and_phase0_writes_before_io ... ok
test wmi_acpi::tests::exact_firmware_and_sensor_values ... ok
test wmi_acpi::tests::package_abi_checks_size_type_and_entire_status ... ok
test wmi_acpi::tests::brightness_validates_arguments_provider_result_and_exact_instance ... ok
test wmi_acpi::tests::set_data_has_a_token_gated_address_value_packet ... ok
test ec_map_loader::tests::rejects_malformed_identity_evidence_and_duplicate_fields ... ok
test ec_map_loader::tests::rejects_scope_escalation_and_research_prefix_promotion ... ok
test executor::tests::timed_out_read_keeps_lane_reserved_until_completion ... ok

test result: ok. 40 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.16s

     Running unittests src\main.rs (target\debug\deps\msilt_probe-41ef01015a257750.exe)

running 2 tests
test tests::optional_native_channels_require_explicit_flags ... ok
test tests::rejects_invalid_options_before_opening_hardware ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests msilt_automation

running 1 test
test crates\msilt-automation\src\lib.rs - (line 16) - compile ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

all doctests ran in 0.35s; merged doctests compilation took 0.30s
   Doc-tests msilt_core

running 1 test
test crates\msilt-core\src\ec.rs - ec::AuthorizedEcWrite (line 244) - compile fail ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s

   Doc-tests msilt_daemon

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests msilt_hal

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

</details>

## 附录 B：Release 跨进程、构建、体积与资源原始输出

<details>
<summary>展开 Release 跨进程测试完整输出</summary>

```text
   Compiling windows-link v0.2.1
   Compiling typenum v1.20.1
   Compiling itoa v1.0.19
   Compiling memchr v2.8.3
   Compiling winnow v1.0.5
   Compiling cpufeatures v0.2.17
   Compiling cfg-if v1.0.5
   Compiling serde_core v1.0.229
   Compiling zmij v1.0.23
   Compiling thiserror v2.0.21
   Compiling msilt-daemon v0.1.0 (crates\msilt-daemon)
   Compiling windows-result v0.4.1
   Compiling windows-strings v0.5.1
   Compiling windows-threading v0.2.1
   Compiling windows-core v0.62.2
   Compiling toml_parser v1.1.7+spec-1.1.0
   Compiling windows-numerics v0.3.1
   Compiling windows-collections v0.3.2
   Compiling windows-future v0.3.2
   Compiling generic-array v0.14.7
   Compiling crypto-common v0.1.7
   Compiling block-buffer v0.10.4
   Compiling windows v0.62.2
   Compiling digest v0.10.7
   Compiling sha2 v0.10.9
   Compiling serde_json v1.0.154
   Compiling serde v1.0.229
   Compiling toml_datetime v1.1.3+spec-1.1.0
   Compiling serde_spanned v1.1.3
   Compiling toml v1.2.3+spec-1.1.0
   Compiling msilt-core v0.1.0 (crates\msilt-core)
   Compiling msilt-automation v0.1.0 (crates\msilt-automation)
   Compiling msilt-hal v0.1.0 (crates\msilt-hal)
    Finished `release` profile [optimized] target(s) in 28.64s
     Running tests\native_stack.rs (target\release\deps\native_stack-5b0f9b0d5112693d.exe)

running 4 tests
test unauthorized_process_probe ... ok
msilt-daemon: --test-client-pid requires --readonly --no-tray and a custom --pipe
msilt-daemon: automation.toml: missing; using safe empty rules
msilt-daemon: --test-client-pid requires --readonly --no-tray and a custom --pipe
msilt-daemon: --readonly conflicts with --allow-platform-control
test diagnostic_flags_cannot_reclassify_the_production_endpoint ... ok
test unpinned_custom_endpoint_does_not_grant_ui_telemetry ... ok
PINNED_PEER=separate same-logon process rejected with AccessDenied
LIVE_READONLY_SNAPSHOT=HardwareState { identity: HardwareIdentity { board_id: Some("MS-17S3"), ec_firmware: None }, supported_performance_modes: [], performance_mode: None, cpu_fan: None, gpu_fan: None, cooler_boost: None, power_source: Some(Ac), battery_percent: Some(93), charge_limit_percent: None, supported_charge_limits: [], display_refresh_hz: Some(240), supported_refresh_rates: [60, 240], display_brightness_percent: Some(69), color_profile_id: Some("TrueColor_DCIP3_6300"), supported_color_profiles: ["RSWOP", "TrueColor_Adobe_6500", "TrueColor_DCIP3_6300", "TrueColor_DisplayP3_6500", "TrueColor_Native_6500", "TrueColor_sRGB_4400", "TrueColor_sRGB_6500", "icc_0857bf455658261d67881bcc13b9b631", "icc_089dc44a05f8b819de9923e28d27ac11", "icc_0ab8f2bbe3c452186ad423287ea144c4", "icc_1e8fd8f314e667240189434ccb994806", "icc_1ebe9c584ed0bd5b6dcf13a5d36acc12", "icc_2517cb76086613b5e7893a008616ec6e", "icc_26cf1d27a9378b3436b9e0651a2c0fa5", "icc_3496a0982f367bde15c1e46af9b2e0b3", "icc_3e8c83158291c04f18b8bfa69929860b", "icc_4528987896e89fc5dc4a465df1331c97", "icc_46573fa490d4352e4eebb17fc1734aba", "icc_4b675999b54592a46790ce22aff84d36", "icc_4cb7d34cd8522cc688cd380beeed4598", "icc_4e68c81ea95277ad35c9476fa259d62c", "icc_4f6091e23f74267158362ffb4610940e", "icc_525bd95518fb1e628e94cc73ca7d707c", "icc_57dcb49db9dee740a303306dfc7ee0e9", "icc_6df2d7249bfefc8d56bab919de0cff28", "icc_754da82588c86810ba5964b3953315c8", "icc_79607a2ee7e00e3bba8b7d315ba15baa", "icc_7d1839fd094361027f82b2f1ae117476", "icc_7e2c384f5380efd653381a94adeda098", "icc_9a15effcd0f2067406a13dbeea8fad7b", "icc_ac900dc0590c7c21ca0c6f5431ff9e94", "icc_b9746889bf45ce9250954d5b02c48ecb", "icc_c44ceb03001f4171115bc3980b4a6a0e", "icc_c523c510b737047161fb48c96b4b5de5", "icc_ca502b69b5bc342e63e99368ca67ad99", "icc_ef1244568b501f39ab3ea3376e4cd703", "icc_f85568e318211f7bdb5614cafb98620e"], keyboard_zones: None, gpu_mux: None }
TELEMETRY_TICKS=[1795669538821, 1795669539836, 1795669540838]; NVML/NVAPI absent at startup and after 3 ticks
READONLY_WRITES=Unsupported/Unsupported/WriteNotAllowed; hardware unchanged
DAEMON_EXIT=exit code: 0; graceful WM_CLOSE; no remaining child
test native_readonly_hal_and_rule_engine_cross_process ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 4.78s
```

</details>

首次执行指定 Release 构建命令的输出：

```text
   Compiling typenum v1.20.1
   Compiling serde_json v1.0.154
   Compiling winnow v1.0.5
   Compiling cpufeatures v0.2.17
   Compiling serde_core v1.0.229
   Compiling generic-array v0.14.7
   Compiling syn v2.0.119
   Compiling toml_parser v1.1.7+spec-1.1.0
   Compiling crypto-common v0.1.7
   Compiling block-buffer v0.10.4
   Compiling digest v0.10.7
   Compiling sha2 v0.10.9
   Compiling serde v1.0.229
   Compiling toml_datetime v1.1.3+spec-1.1.0
   Compiling serde_spanned v1.1.3
   Compiling toml v1.2.3+spec-1.1.0
   Compiling windows-interface v0.59.3
   Compiling windows-implement v0.60.2
   Compiling msilt-core v0.1.0 (crates\msilt-core)
   Compiling windows-core v0.62.2
   Compiling windows-collections v0.3.2
   Compiling windows-numerics v0.3.1
   Compiling windows-future v0.3.2
   Compiling windows v0.62.2
   Compiling msilt-automation v0.1.0 (crates\msilt-automation)
   Compiling msilt-cli v0.1.0 (crates\msilt-cli)
   Compiling msilt-hal v0.1.0 (crates\msilt-hal)
   Compiling msilt-daemon v0.1.0 (crates\msilt-daemon)
   Compiling msilt-probe v0.1.0 (crates\msilt-probe)
    Finished `release` profile [optimized] target(s) in 30.45s
```

完成全部代码检查与 Release 集成测试后，再执行同一构建命令确认最终产物：

```text
    Finished `release` profile [optimized] target(s) in 0.13s
```

最终文件体积、SHA-256、普通及延迟导入审计输出：

```text
RELEASE_FILES
{"name": "msilt-daemon.exe", "bytes": 1169408, "MiB": 1.115234375, "sha256": "f235095c47bacf7c10a42e8eceedcfcd9294b1e0f4c7ed4e07c925834e143140"}
{"name": "msilt-cli.exe", "bytes": 433664, "MiB": 0.41357421875, "sha256": "50bf09d296f8f7d3748961b50dddbb2d8ca7953837b16741cbecbb8f33f6d3be"}
{"name": "msilt-probe.exe", "bytes": 605696, "MiB": 0.57763671875, "sha256": "e3fc918d3036782cdd1b4caaaaeffd26c389495a7d067fb2c5eea8d7efb69159"}
PE_IMPORT_MODULES
KERNEL32.dll
VCRUNTIME140.dll
advapi32.dll
api-ms-win-core-synch-l1-2-0.dll
api-ms-win-crt-heap-l1-1-0.dll
api-ms-win-crt-locale-l1-1-0.dll
api-ms-win-crt-math-l1-1-0.dll
api-ms-win-crt-runtime-l1-1-0.dll
api-ms-win-crt-stdio-l1-1-0.dll
api-ms-win-crt-string-l1-1-0.dll
combase.dll
gdi32.dll
gdiplus.dll
hid.dll
kernel32.dll
mscms.dll
ntdll.dll
ole32.dll
oleaut32.dll
powrprof.dll
setupapi.dll
shell32.dll
user32.dll
userenv.dll
wtsapi32.dll
FORBIDDEN_IMPORTS=[]
ORDINARY_IMPORT_DESCRIPTORS=25
DELAY_IMPORT_DESCRIPTORS=0
```

15 秒静置脚本完整输出：

```text
Command: ".\target\release\msilt-daemon.exe" --no-tray --readonly --pipe MSIToolkit-bench-d79f35e2f83a403dbd684b52ab1685ff

Initialized       : ready
BoardId           : MS-17S3
RefreshHz         : 240
BrightnessPercent : 69
BatteryPercent    : 93
ColorProfile      : TrueColor_DCIP3_6300
ColorChoices      : 37
Writable          : 0
EcWriteStatus     : unverifiedMapping
ConfigRevision    : 0


{
  "MeasuredAt": "2026-11-26T13:09:07.5740397+08:00",
  "ProcessId": 49012,
  "IdleElapsedMs": 15007.8539,
  "WorkingSetBytes": 21319680,
  "WorkingSetMiB": 20.33203125,
  "PrivateBytes": 3604480,
  "PrivateMiB": 3.4375,
  "Threads": 16,
  "Handles": 285,
  "CpuStartMs": 109.375,
  "CpuEndMs": 109.375,
  "CpuDeltaMs": 0.0,
  "NvmlLoaded": false,
  "NvapiLoaded": false,
  "BaselinePrivateMiB": 790,
  "BaselineProcesses": 15,
  "DaemonProcesses": 1,
  "PrivateReductionPercent": 99.56487341772153
}
Shutdown: WM_CLOSE windows=2; exitCode=0; remainingOwnedProcesses=0
```

最终进程与格式复核输出：

```text
D2D: 0
D3D: 0
DXGI: 0
Slint: 0
Skia: 0
WebView2: 0
NVML: 0
NVAPI: 0
ControlService: 0
ChangeServiceConfig: 0
CreateService: 0
DeleteService: 0
RustfmtExitCode=0
RemainingMsiltDaemonProcesses=0
DiagnosticLogTail:
1795669502549 automation.toml: missing; using safe empty rules
1795669537567 automation.toml: missing; using safe empty rules
1795669731293 automation.toml: missing; using safe empty rules
```

## 附录 C：PE 完整导入表与依赖图

执行的导入导出及依赖审计命令：

```powershell
& 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\dumpbin.exe' /nologo /imports 'target\release\msilt-daemon.exe'
cargo tree -p msilt-daemon --edges normal
```

普通/延迟导入的断言与文件体积可用以下 Python 代码复测（本机已安装 `pefile`）：

```python
import hashlib, json, pathlib, pefile, re

root = pathlib.Path("target/release")
for name in ["msilt-daemon.exe", "msilt-cli.exe", "msilt-probe.exe"]:
    path = root / name
    print(name, path.stat().st_size, hashlib.sha256(path.read_bytes()).hexdigest())
pe = pefile.PE(str(root / "msilt-daemon.exe"))
imports = [entry.dll.decode() for directory in
           ["DIRECTORY_ENTRY_IMPORT", "DIRECTORY_ENTRY_DELAY_IMPORT"]
           for entry in getattr(pe, directory, [])]
banned = [name for name in imports if re.search(
    r"d2d|direct2d|d3d|direct3d|dxgi|slint|skia|webview|nvml|nvapi", name, re.I)]
print("FORBIDDEN_IMPORTS=" + json.dumps(banned))
assert not banned
```

<details>
<summary>展开最终 daemon PE 完整导入表</summary>

```text

Dump of file target\release\msilt-daemon.exe

File Type: EXECUTABLE IMAGE

  Section contains the following imports:

    kernel32.dll
             1400EC3D0 Import Address Table
             140115448 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 PostQueuedCompletionStatus
                           0 SetProcessInformation
                           0 SetThreadStackGuarantee
                           0 CreateDirectoryW
                           0 CompareStringOrdinal
                           0 OpenProcess
                           0 GetCurrentProcessId
                           0 DeleteFileW
                           0 CloseHandle
                           0 CreateThreadpoolTimer
                           0 SetThreadpoolTimer
                           0 WaitForThreadpoolTimerCallbacks
                           0 CloseThreadpoolTimer
                           0 ReleaseMutex
                           0 FindFirstFileExW
                           0 GetSystemDirectoryW
                           0 CreateIoCompletionPort
                           0 GetProcAddress
                           0 FreeLibrary
                           0 DisconnectNamedPipe
                           0 CancelIoEx
                           0 FindClose
                           0 CreateNamedPipeW
                           0 ConnectNamedPipe
                           0 LoadLibraryExA
                           0 CreateToolhelp32Snapshot
                           0 Process32FirstW
                           0 Process32NextW
                           0 SwitchToThread
                           0 GetQueuedCompletionStatus
                           0 MoveFileExW
                           0 GetCurrentProcess
                           0 SetPriorityClass
                           0 GetSystemPowerStatus
                           0 WriteFile
                           0 GetNamedPipeClientSessionId
                           0 GetLastError
                           0 WaitForSingleObject
                           0 QueryFullProcessImageNameW
                           0 FlushFileBuffers
                           0 CreateHardLinkW
                           0 GetSystemFirmwareTable
                           0 GetNamedPipeClientProcessId
                           0 FormatMessageW
                           0 HeapAlloc
                           0 HeapFree
                           0 SetDefaultDllDirectories
                           0 IsProcessorFeaturePresent
                           0 SetLastError
                           0 CreateMutexW
                           0 GetModuleHandleW
                           0 GetProcessHeap
                           0 DeviceIoControl
                           0 GetCurrentThread
                           0 GetSystemTimeAsFileTime
                           0 ReadFile
                           0 GetCommandLineW
                           0 SetFileInformationByHandle
                           0 FindNextFileW
                           0 CreateFileW
                           0 GetModuleFileNameW
                           0 GetSystemTimePreciseAsFileTime
                           0 GetFileInformationByHandle
                           0 SetFileTime
                           0 GetFullPathNameW
                           0 GetModuleHandleA
                           0 CreateThread
                           0 LoadLibraryExW
                           0 LocalFree
                           0 GetFileInformationByHandleEx
                           0 GetCurrentThreadId
                           0 GetCurrentDirectoryW
                           0 WaitForSingleObjectEx
                           0 LoadLibraryA
                           0 lstrlenW
                           0 CreateMutexA
                           0 WideCharToMultiByte
                           0 GetEnvironmentVariableW
                           0 MultiByteToWideChar
                           0 FlsAlloc
                           0 GetModuleHandleExW
                           0 FlsFree
                           0 IsThreadAFiber
                           0 FlsSetValue

    api-ms-win-core-synch-l1-2-0.dll
             1400EC1B0 Import Address Table
             140115228 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 WakeByAddressSingle
                           0 WakeByAddressAll
                           0 WaitOnAddress

    user32.dll
             1400EC810 Import Address Table
             140115888 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 QueryDisplayConfig
                           0 PostMessageW
                           0 ChangeDisplaySettingsExW
                           0 ChangeWindowMessageFilterEx
                           0 GetDisplayConfigBufferSizes
                           0 PostQuitMessage
                           0 GetWindowLongPtrW
                           0 SetWindowLongPtrW
                           0 UnregisterClassW
                           0 DisplayConfigGetDeviceInfo
                           0 DestroyIcon
                           0 DestroyWindow
                           0 EnumDisplaySettingsW
                           0 CreateWindowExW
                           0 DefWindowProcW
                           0 DestroyMenu
                           0 AppendMenuW
                           0 CreatePopupMenu
                           0 SetProcessDpiAwarenessContext
                           0 KillTimer
                           0 UnregisterPowerSettingNotification
                           0 RegisterWindowMessageW
                           0 UnregisterHotKey
                           0 GetCursorPos
                           0 ShowWindow
                           0 SetTimer
                           0 UpdateLayeredWindow
                           0 TrackPopupMenu
                           0 SetForegroundWindow
                           0 GetDpiForWindow
                           0 RegisterClassW
                           0 GetMonitorInfoW
                           0 MonitorFromWindow
                           0 GetForegroundWindow
                           0 DispatchMessageW
                           0 TranslateMessage
                           0 GetMessageW
                           0 RegisterPowerSettingNotification
                           0 RegisterHotKey
                           0 CreateIconFromResourceEx
                           0 EnumDisplayDevicesW

    advapi32.dll
             1400EC0B0 Import Address Table
             140115128 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 GetSecurityDescriptorControl
                           0 OpenThreadToken
                           0 ConvertStringSecurityDescriptorToSecurityDescriptorW
                           0 ImpersonateNamedPipeClient
                           0 RegGetValueW
                           0 CloseServiceHandle
                           0 QueryServiceStatus
                           0 OpenServiceW
                           0 OpenSCManagerW
                           0 CheckTokenMembership
                           0 CreateWellKnownSid
                           0 CreateProcessWithTokenW
                           0 IsValidSid
                           0 GetSidIdentifierAuthority
                           0 GetSidSubAuthorityCount
                           0 GetSidSubAuthority
                           0 IsWellKnownSid
                           0 RevertToSelf
                           0 ImpersonateLoggedOnUser
                           0 AccessCheck
                           0 OpenProcessToken
                           0 DuplicateToken
                           0 GetTokenInformation
                           0 GetSecurityDescriptorDacl
                           0 GetSecurityDescriptorSacl
                           0 ConvertSidToStringSidW
                           0 MapGenericMask
                           0 GetAce
                           0 GetSecurityInfo
                           0 GetSecurityDescriptorOwner
                           0 IsValidAcl

    shell32.dll
             1400EC7F8 Import Address Table
             140115870 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 SHGetKnownFolderPath
                           0 Shell_NotifyIconW

    combase.dll
             1400EC2D8 Import Address Table
             140115350 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 CoTaskMemFree

    wtsapi32.dll
             1400EC978 Import Address Table
             1401159F0 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 WTSFreeMemory
                           0 WTSQuerySessionInformationW
                           0 WTSRegisterSessionNotification
                           0 WTSUnRegisterSessionNotification

    userenv.dll
             1400EC960 Import Address Table
             1401159D8 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 DestroyEnvironmentBlock
                           0 CreateEnvironmentBlock

    gdi32.dll
             1400EC2E8 Import Address Table
             140115360 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 CreateCompatibleDC
                           0 DeleteDC
                           0 SelectObject
                           0 CreateDIBSection
                           0 DeleteObject

    gdiplus.dll
             1400EC318 Import Address Table
             140115390 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 GdipDeleteGraphics
                           0 GdipDisposeImage
                           0 GdiplusShutdown
                           0 GdipCreateFont
                           0 GdipSetTextRenderingHint
                           0 GdipDrawString
                           0 GdipDeleteBrush
                           0 GdipDeleteFont
                           0 GdipDeleteFontFamily
                           0 GdiplusStartup
                           0 GdipCreateSolidFill
                           0 GdipCreateBitmapFromScan0
                           0 GdipGetImageGraphicsContext
                           0 GdipCreateFontFamilyFromName

    powrprof.dll
             1400EC7B8 Import Address Table
             140115830 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 PowerUnregisterFromEffectivePowerModeNotifications
                           0 PowerRegisterForEffectivePowerModeNotifications

    ole32.dll
             1400EC718 Import Address Table
             140115790 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 CoSetProxyBlanket
                           0 CoCreateInstance
                           0 CoInitializeEx
                           0 CoUninitialize
                           0 CoInitializeSecurity

    oleaut32.dll
             1400EC748 Import Address Table
             1401157C0 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 SafeArrayGetElemsize
                           0 SafeArrayGetLBound
                           0 SafeArrayGetVartype
                           0 SafeArrayGetUBound
                           0 SafeArrayGetDim
                           0 SafeArrayCreateVector
                           0 SafeArrayAccessData
                           0 SysAllocStringLen
                           0 GetErrorInfo
                           0 SysStringLen
                           0 SysFreeString
                           0 SafeArrayUnaccessData
                           0 VariantClear

    hid.dll
             1400EC390 Import Address Table
             140115408 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 HidD_GetHidGuid
                           0 HidD_SetFeature
                           0 HidP_GetCaps
                           0 HidP_InitializeReportForID
                           0 HidD_GetPreparsedData
                           0 HidD_FreePreparsedData
                           0 HidD_GetAttributes

    setupapi.dll
             1400EC7D0 Import Address Table
             140115848 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 SetupDiEnumDeviceInterfaces
                           0 SetupDiDestroyDeviceInfoList
                           0 SetupDiGetClassDevsW
                           0 SetupDiGetDeviceInterfaceDetailW

    mscms.dll
             1400EC678 Import Address Table
             1401156F0 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           0 GetColorDirectoryW
                           0 WcsSetDefaultColorProfile
                           0 WcsGetDefaultColorProfile
                           0 WcsGetDefaultColorProfileSize
                           0 WcsAssociateColorProfileWithDevice
                           0 WcsDisassociateColorProfileFromDevice
                           0 WcsSetUsePerUserProfiles
                           0 WcsGetUsePerUserProfiles
                           0 ColorProfileGetDisplayList
                           0 ColorProfileGetDisplayUserScope
                           0 ColorProfileGetDisplayDefault

    ntdll.dll
             1400EC6D8 Import Address Table
             140115750 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                         30A RtlCaptureContext
                         516 RtlLookupFunctionEntry
                         2A7 NtWriteFile
                         668 RtlVirtualUnwind
                         209 NtQuerySystemInformation
                           0 RtlNtStatusToDosError
                         218 NtReadFile

    KERNEL32.dll
             1400EC000 Import Address Table
             140115078 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                         5B8 SetUnhandledExceptionFilter
                         5FB UnhandledExceptionFilter
                         3B0 IsDebuggerPresent
                         39A InitializeSListHead
                         383 HeapReAlloc
                         303 GetStdHandle
                         225 GetConsoleMode
                         229 GetConsoleOutputCP
                         65F WriteConsoleW
                         482 QueryPerformanceFrequency
                         481 QueryPerformanceCounter
                          14 AddVectoredExceptionHandler

    VCRUNTIME140.dll
             1400EC068 Import Address Table
             1401150E0 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           E __CxxFrameHandler3
                          1C __current_exception_context
                          3C memcpy
                           8 __C_specific_handler
                          3D memmove
                          3E memset
                          3B memcmp
                          1B __current_exception

    api-ms-win-crt-math-l1-1-0.dll
             1400EC1F8 Import Address Table
             140115270 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                         10A roundf
                          2F _hypotf
                           9 __setusermatherr

    api-ms-win-crt-string-l1-1-0.dll
             1400EC2C8 Import Address Table
             140115340 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                          A3 wcslen

    api-ms-win-crt-runtime-l1-1-0.dll
             1400EC218 Import Address Table
             140115290 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                          37 _initterm_e
                          36 _initterm
                          28 _get_initial_narrow_environment
                           4 __p___argc
                           5 __p___argv
                          16 _cexit
                          15 _c_exit
                          3D _register_thread_local_exe_atexit_callback
                          33 _initialize_narrow_environment
                          18 _configure_narrow_argv
                          42 _set_app_type
                          23 _exit
                          34 _initialize_onexit_table
                          3C _register_onexit_function
                          40 _seh_filter_exe
                          55 exit
                          67 terminate
                          1E _crt_atexit

    api-ms-win-crt-stdio-l1-1-0.dll
             1400EC2B0 Import Address Table
             140115328 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           1 __p__commode
                          54 _set_fmode

    api-ms-win-crt-locale-l1-1-0.dll
             1400EC1E8 Import Address Table
             140115260 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                           8 _configthreadlocale

    api-ms-win-crt-heap-l1-1-0.dll
             1400EC1D0 Import Address Table
             140115248 Import Name Table
                     0 time date stamp
                     0 Index of first forwarder reference

                          18 free
                          16 _set_new_mode

  Summary

        1000 .data
        7000 .pdata
       2C000 .rdata
        1000 .reloc
       EB000 .text
```

</details>

<details>
<summary>展开 daemon 完整正常依赖图</summary>

```text
msilt-daemon v0.1.0 (crates\msilt-daemon)
├── msilt-automation v0.1.0 (crates\msilt-automation)
│   ├── msilt-core v0.1.0 (crates\msilt-core)
│   │   ├── serde v1.0.229
│   │   │   ├── serde_core v1.0.229
│   │   │   └── serde_derive v1.0.229 (proc-macro)
│   │   │       ├── proc-macro2 v1.0.107
│   │   │       │   └── unicode-ident v1.0.27
│   │   │       ├── quote v1.0.47
│   │   │       │   └── proc-macro2 v1.0.107 (*)
│   │   │       └── syn v3.0.10
│   │   │           ├── proc-macro2 v1.0.107 (*)
│   │   │           ├── quote v1.0.47 (*)
│   │   │           └── unicode-ident v1.0.27
│   │   ├── serde_json v1.0.154
│   │   │   ├── itoa v1.0.19
│   │   │   ├── memchr v2.8.3
│   │   │   ├── serde_core v1.0.229
│   │   │   └── zmij v1.0.23
│   │   └── thiserror v2.0.21
│   │       └── thiserror-impl v2.0.21 (proc-macro)
│   │           ├── proc-macro2 v1.0.107 (*)
│   │           ├── quote v1.0.47 (*)
│   │           └── syn v3.0.10 (*)
│   ├── thiserror v2.0.21 (*)
│   └── toml v1.2.3+spec-1.1.0
│       ├── serde_core v1.0.229
│       ├── serde_spanned v1.1.3
│       │   └── serde_core v1.0.229
│       ├── toml_datetime v1.1.3+spec-1.1.0
│       │   └── serde_core v1.0.229
│       ├── toml_parser v1.1.7+spec-1.1.0
│       │   └── winnow v1.0.5
│       └── winnow v1.0.5
├── msilt-core v0.1.0 (crates\msilt-core) (*)
├── msilt-hal v0.1.0 (crates\msilt-hal)
│   ├── msilt-core v0.1.0 (crates\msilt-core) (*)
│   ├── serde v1.0.229 (*)
│   ├── serde_json v1.0.154 (*)
│   ├── sha2 v0.10.9
│   │   ├── cfg-if v1.0.5
│   │   ├── cpufeatures v0.2.17
│   │   └── digest v0.10.7
│   │       ├── block-buffer v0.10.4
│   │       │   └── generic-array v0.14.7
│   │       │       └── typenum v1.20.1
│   │       └── crypto-common v0.1.7
│   │           ├── generic-array v0.14.7 (*)
│   │           └── typenum v1.20.1
│   ├── thiserror v2.0.21 (*)
│   └── windows v0.62.2
│       ├── windows-collections v0.3.2
│       │   └── windows-core v0.62.2
│       │       ├── windows-implement v0.60.2 (proc-macro)
│       │       │   ├── proc-macro2 v1.0.107 (*)
│       │       │   ├── quote v1.0.47 (*)
│       │       │   └── syn v2.0.119
│       │       │       ├── proc-macro2 v1.0.107 (*)
│       │       │       ├── quote v1.0.47 (*)
│       │       │       └── unicode-ident v1.0.27
│       │       ├── windows-interface v0.59.3 (proc-macro)
│       │       │   ├── proc-macro2 v1.0.107 (*)
│       │       │   ├── quote v1.0.47 (*)
│       │       │   └── syn v2.0.119 (*)
│       │       ├── windows-link v0.2.1
│       │       ├── windows-result v0.4.1
│       │       │   └── windows-link v0.2.1
│       │       └── windows-strings v0.5.1
│       │           └── windows-link v0.2.1
│       ├── windows-core v0.62.2 (*)
│       ├── windows-future v0.3.2
│       │   ├── windows-core v0.62.2 (*)
│       │   ├── windows-link v0.2.1
│       │   └── windows-threading v0.2.1
│       │       └── windows-link v0.2.1
│       └── windows-numerics v0.3.1
│           ├── windows-core v0.62.2 (*)
│           └── windows-link v0.2.1
└── windows v0.62.2 (*)
```

</details>

## 7. 空闲工作集优化与一键验收

### 7.1 驻留来源与实现

在未修改的 Release 上，以 Windows 模块枚举、线程入口地址和 `QueryWorkingSetEx` 分阶段读取进程状态。首次 IPC `hello` 前有 41 个模块、7 个线程，Working Set 为 15,589,376 字节；首次硬件快照完成、客户端断开后为 55 个模块、16 个线程，15 秒后 Working Set 仍为 21,405,696 字节。其中已枚举映像驻留 19,124,224 字节，可共享映像页占 17,780,736 字节（16.95703125 MiB）。

新加载模块包括亮度 WMI 的 `fastprox.dll`、`wbemprox.dll`、`wbemsvc.dll`，以及 ICC/设备枚举的 `icm32.dll`、`cfgmgr32.dll`、`devobj.dll`；`combase.dll` 的可共享驻留页从 420 KiB 增至 1,768 KiB。按入口归属，静置的 16 个线程中 8 个进入 EXE（消息泵、协调器、HAL 工作线程、4 条 HAL 探测执行通道和亮度 WMI 通道），7 个进入同一 `ntdll.dll` 入口，1 个进入 `combase.dll`。默认路径只在第一次需要时启动 HAL 工作线程，不预热 NVML、MSI_ACPI 或 GDI+ 会话。

- 协调器在无管道客户端、无待处理管道关闭、无排队任务和无在途 HAL 工作时开始冷却，并排除 watchdog 隔离期。连续空闲 2 秒后、仍处于 EcoQoS 时，用现有 IOCP 单次计时器调用 `kernel32!SetProcessWorkingSetSizeEx(GetCurrentProcess(), SIZE_T(-1), SIZE_T(-1), 0)`。新活动取消待执行的修剪；直到下一次进入空闲，计时器不再为修剪安排唤醒。
- 保留串行 HAL 通道及 Win32 线程池，回收的是工作集驻留页。管道每连接的 4 KiB 接收块、收发堆缓冲在断开后释放，容量超过 8 KiB 的临时帧缓冲在帧结束即释放；显示拓扑数组随调用释放，OSD 软件位图在提交后释放。
- 隔离只读端点的配置加载诊断走 stderr，不写交互用户目录的 `daemon-diagnostics.log`。生产端点的日志行为保持不变。

### 7.2 同机前后对照

Windows 11 build 26200，MS-17S3。两次均由 `Measure-Readonly.ps1` 启动独立的 `--readonly --no-tray --pipe <随机名称>` 实例，完成 `hello/getSnapshot/getConfig` 后断开客户端，等待 3 秒覆盖单次修剪，再计算 15 秒无客户端区间的 CPU 增量。内存取样结束后才重新连接，避免把响应唤回的页计入静置 Working Set。

| 项目 | 原版 Release | 优化后 Release |
| --- | ---: | ---: |
| 测量时刻（+08:00） | 2026-11-26 13:42:59.2310850 | 2026-11-26 14:06:48.3034459 |
| 实际静置时长 | 15,009.4466 ms | 15,010.1286 ms |
| **Working Set** | **21,364,736 字节 / 20.375 MiB** | **335,872 字节 / 0.3203125 MiB** |
| **Private** | **3,608,576 字节 / 3.44140625 MiB** | **3,731,456 字节 / 3.55859375 MiB** |
| 线程 / 句柄 | 16 / 287 | 16 / 287 |
| 累计 CPU 时间（静置前 → 后） | 140.625 → 140.625 ms | 140.625 → 140.625 ms |
| **15 秒 CPU 增量** | **0 ms** | **0 ms** |
| 静置后重连 + `hello` | 1.8616 ms | 1.9471 ms |
| NVML / NVAPI 运行时模块 | 均未加载 | 均未加载 |
| 优雅退出码 / 残留进程 | 0 / 0 | 0 / 0 |

最终总验 Working Set 下降 **98.43%**。四次优化后静置样本的 Working Set 为 **0.3125–0.3203125 MiB**、Private 为 **3.40234375–3.55859375 MiB**、CPU 增量均为 **0 ms**。首次重连加 `hello` 为 **1.9471–20.9386 ms**；最终总验同连接的后续 `getSnapshot` 为 **0.8985 ms**。独立门禁实验中，已连接客户端保持超过 2 秒，Working Set 保持 21,311,488 字节；断开并冷却后降至 335,872 字节。

### 7.3 PE 与总验结果

`dumpbin /imports` 与 `pefile` 核对 daemon 两个 Release 产物的普通和延迟导入。以不区分大小写的 DLL 名比较，唯一新增的导入符号是 `kernel32.dll!SetProcessWorkingSetSizeEx`。

| daemon PE | 原版 Release | 优化后 Release |
| --- | ---: | ---: |
| EXE 大小 | 1,169,408 字节 | 1,170,432 字节 |
| 普通导入描述符 / 符号数 | 25 / 283 | 25 / 284 |
| 延迟导入描述符 / 符号数 | 0 / 0 | 0 / 0 |
| 禁用 GPU 渲染、GUI 框架及 NVML/NVAPI DLL 命中 | 0 | 0 |

```powershell
pwsh -NoProfile -File .\scripts\verify-all.ps1
```

脚本从非提权 PowerShell 运行，使用 MSVC `dumpbin`，执行锁定依赖的 workspace 测试与 Release 构建、EXE 体积与普通/延迟 PE 导入审计、关闭 ACPI/NVML 的 `msilt-probe --json`、只读 daemon 静置与重连。后端体积预算分别为 daemon 1.25 MiB、CLI 512 KiB、probe 1 MiB；UI 为 12,000,000 字节。静置门槛为 Working Set ≤12 MiB、Private ≤4 MiB、CPU 增量 0 ms。基准日志与 JSON 写入 Cargo target 目录。

- `cargo test --locked -p msilt-daemon`：**28 项通过**；`cargo build --release --locked -p msilt-daemon` 成功。额外重跑 Release 跨进程集成测试：**4 项通过**。
- `verify-all.ps1` 最终退出码 **0**。全 workspace **190 项通过、0 失败**（含 UI 25 项、后端 165 项）；daemon、CLI、probe、UI Release 分别为 **1,170,432 / 433,664 / 605,696 / 8,835,072 字节**，四个产物的禁用 PE 导入均为 **0**。
- 非提权探针退出码 **0**，读回 `MS-17S3`，ACPI/NVML 未启用，EC 写入关闭。静置三项资源指标达标，`WM_CLOSE` 退出码 **0**，测量实例残留 **0**。
