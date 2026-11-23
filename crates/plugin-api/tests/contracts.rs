//! S1 contract type round trips.
#![allow(clippy::unwrap_used)]

use printkit_plugin_api::{
    CapabilityKind, CapabilitySpec, HookCtx, HookPhase, UiContractError, UiContribution, UiSlot,
    validate_ui_contributions,
};

#[test]
fn ui_and_capability_contracts_round_trip() {
    let ui = UiContribution {
        slot: UiSlot::PanelSlot,
        id: "jobs".to_owned(),
        window: "business".to_owned(),
        title_key: "plugins.observability.jobs".to_owned(),
        order: 3,
        parent: Some("dashboard".to_owned()),
        entry: Some("ui/index.html".to_owned()),
        icon_key: Some("plugins.observability.icon".to_owned()),
        action_key: Some("openJobs".to_owned()),
        titles: std::collections::BTreeMap::from([("en".to_owned(), "Jobs".to_owned())]),
    };
    ui.validate().unwrap();
    let json = serde_json::to_string(&ui).unwrap();
    let parsed: UiContribution = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, ui);
    assert!(json.contains("\"slot\":\"panelSlot\""));

    let capability = CapabilitySpec {
        id: "pdf".to_owned(),
        kind: CapabilityKind::ExportFormat,
    };
    let json = serde_json::to_string(&capability).unwrap();
    let parsed: CapabilitySpec = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, capability);
    assert!(json.contains("\"kind\":\"export-format\""));
}

fn nav_page(id: &str) -> UiContribution {
    UiContribution {
        slot: UiSlot::NavPage,
        id: id.to_owned(),
        window: "business".to_owned(),
        title_key: format!("plugins.{id}.title"),
        order: 1,
        parent: None,
        entry: None,
        icon_key: None,
        action_key: None,
        titles: std::collections::BTreeMap::new(),
    }
}

#[test]
fn ui_contract_rejects_window_parent_and_duplicate_ids() {
    let mut wrong_window = nav_page("jobs");
    wrong_window.window = "dev".to_owned();
    assert!(matches!(
        wrong_window.validate(),
        Err(UiContractError::Window(_))
    ));

    let mut panel = nav_page("summary");
    panel.slot = UiSlot::PanelSlot;
    assert!(matches!(panel.validate(), Err(UiContractError::Parent(_))));

    let items = [nav_page("jobs"), nav_page("jobs")];
    assert!(matches!(
        validate_ui_contributions(&items),
        Err(UiContractError::DuplicateId { .. })
    ));
}

#[test]
fn hook_context_and_phase_are_stable() {
    let ctx = HookCtx {
        job_id: "job".to_owned(),
        trace_id: "trace".to_owned(),
        printer: Some("printer".to_owned()),
        pages: Some(2),
    };
    let json = serde_json::to_string(&ctx).unwrap();
    assert!(json.contains("\"jobId\":\"job\""));
    assert_eq!(HookPhase::BeforeRender, HookPhase::BeforeRender);
}
