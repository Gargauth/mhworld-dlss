use super::*;

const LUID: usize = 0x0000_0000_0001_2345;

fn game() -> GameInit {
    GameInit {
        app_id: MHW_APP_ID,
        data_path: 0x1435_1a270,
        device: 0x2000_0000,
        version: GAME_API_VERSION as usize,
        stack: [LUID, 0],
    }
}

#[test]
fn the_constants_are_the_observed_and_sdk_values() {
    assert_eq!(MHW_APP_ID, 100_182_111);
    assert_eq!(GAME_API_VERSION, 18);
    assert_eq!(API_VERSION, 21);
    let options = InitOptions::new(PathBuf::from("mhw-dlss"));
    assert_eq!(options.api_version, 21);
    assert_eq!(options.identity, Identity::GameAppId);
    assert_eq!(options.tail, InitTail::Zero);
    assert_eq!(options.timeout, None);
}

#[test]
fn the_driver_gets_the_legacy_init_with_the_chosen_tail() {
    let zero = plan(
        HostKind::Driver,
        &Identity::GameAppId,
        InitTail::Zero,
        &game(),
    );
    assert_eq!(zero.route, Route::Legacy { tail: [0, 0] });
    assert_eq!(zero.app_id, MHW_APP_ID);
    assert_eq!(zero.route.export(), None);
    let forwarded = plan(
        HostKind::Driver,
        &Identity::GameAppId,
        InitTail::Game,
        &game(),
    );
    assert_eq!(forwarded.route, Route::Legacy { tail: [LUID, 0] });
}

#[test]
fn a_foreign_host_gets_init_ext() {
    let foreign = plan(
        HostKind::Foreign,
        &Identity::AppId(42),
        InitTail::Game,
        &game(),
    );
    assert_eq!(foreign.route, Route::InitExt);
    assert_eq!(foreign.app_id, 42);
    assert_eq!(foreign.route.export(), Some(c"NVSDK_NGX_D3D12_Init_Ext"));
}

#[test]
fn a_project_id_goes_through_init_project_id_on_any_host() {
    let identity = Identity::ProjectId {
        project_id: "a0f57b54-1daf-4934-90ae-c4035c19df04".into(),
        engine_version: "1.0".into(),
    };
    for host in [HostKind::Driver, HostKind::Foreign] {
        let p = plan(host, &identity, InitTail::Zero, &game());
        assert_eq!(p.route, Route::ProjectId);
        assert_eq!(p.route.export(), Some(c"NVSDK_NGX_D3D12_Init_ProjectID"));
    }
}

#[test]
fn feature_common_info_has_the_sdk_layout() {
    // OptiScaler copies exactly 40 bytes out of it.
    assert_eq!(std::mem::size_of::<FeatureCommonInfo>(), 40);
    assert_eq!(std::mem::offset_of!(FeatureCommonInfo, paths), 0);
    assert_eq!(std::mem::offset_of!(PathListInfo, length), 8);
    assert_eq!(std::mem::offset_of!(FeatureCommonInfo, internal), 16);
    assert_eq!(std::mem::offset_of!(FeatureCommonInfo, logging), 24);
    assert_eq!(std::mem::offset_of!(LoggingInfo, minimum_level), 8);
    assert_eq!(std::mem::offset_of!(LoggingInfo, disable_other_sinks), 12);
    let info = FeatureCommonInfo::with_paths(std::ptr::null(), 1);
    assert_eq!(info.paths.length, 1);
    assert!(info.internal.is_null());
    assert_eq!(info.logging.callback, 0);
}

#[test]
fn availability_is_yes_only_for_available_one() {
    let yes = Availability {
        available: Some(1),
        init_result: Some(Status::SUCCESS),
        needs_updated_driver: Some(0),
        min_driver: (Some(470), Some(0)),
    };
    assert!(yes.usable());
    assert_eq!(yes.refusal("the driver's NGX"), None);

    // As measured with no usable library on NGX's search path.
    let not_found = Availability {
        available: Some(0),
        init_result: Some(Status::from_i32(0xBAD0_0004u32 as i32)),
        ..yes
    };
    let why = not_found.refusal("the driver's NGX").unwrap();
    assert!(why.starts_with("the driver's NGX reports"), "{why}");
    assert!(why.contains("SuperSampling.Available = 0"), "{why}");
    assert!(why.contains("0xbad00004 FeatureNotFound"), "{why}");

    let old_driver = Availability {
        available: Some(0),
        init_result: Some(Status(0xBAD0_000C)),
        needs_updated_driver: Some(1),
        min_driver: (Some(565), Some(90)),
    };
    let why = old_driver.refusal("OptiScaler.dll").unwrap();
    assert!(why.contains("OutOfDate"), "{why}");
    assert!(why.contains("at least 565.90"), "{why}");

    let silent = Availability::default();
    assert!(!silent.usable());
    assert!(silent.refusal("x").unwrap().contains("Available = unset"));
}

#[test]
fn both_features_are_reported_available_to_the_game() {
    let writes = availability_writes();
    let value = |name: &CStr| writes.iter().find(|w| w.name == name).map(|w| w.value);
    assert_eq!(value(names::SS_AVAILABLE), Some(Value::I32(1)));
    assert_eq!(value(names::ISP_AVAILABLE), Some(Value::I32(1)));
    assert_eq!(value(names::SS_FEATURE_INIT_RESULT), Some(Value::I32(1)));
    assert_eq!(value(names::ISP_FEATURE_INIT_RESULT), Some(Value::I32(1)));
    assert_eq!(value(names::SS_NEEDS_UPDATED_DRIVER), Some(Value::I32(0)));
    assert_eq!(value(names::ISP_NEEDS_UPDATED_DRIVER), Some(Value::I32(0)));
    assert_eq!(value(names::SS_MIN_DRIVER_MAJOR), Some(Value::U32(0)));
    assert_eq!(value(names::SS_MIN_DRIVER_MINOR), Some(Value::U32(0)));
}
