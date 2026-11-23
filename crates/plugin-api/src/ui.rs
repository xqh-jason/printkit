//! UI extension points.
//!
//! [`UiPart`](crate::UiPart) remains the P0 compatibility shape; it keeps the
//! external `entry`/`titles` fields until S4 migrates single-window NavPage
//! pages. S1 only freezes the new types.
//!
//! Wire spelling: [`UiSlot`] is camelCase (`navPage`). [`UiContribution`] ids
//! must be unique within a slot; the window sorts by `order` ascending and
//! breaks ties by `id` ascending, so two plugins cannot depend on registration
//! order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A place the window can host a plugin contribution.
///
/// The enum is `#[non_exhaustive]`: a future 1.x may add a slot, and consumers
/// must keep a wildcard arm. A wire spelling change is an API 2 change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum UiSlot {
    /// A navigation page in a window.
    NavPage,
    /// A section inside settings.
    SettingsSection,
    /// A panel inside an existing page or panel.
    PanelSlot,
    /// An action shown in a toolbar.
    ToolbarAction,
}

impl UiSlot {
    /// Wire spelling used by [`UiContribution`] JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NavPage => "navPage",
            Self::SettingsSection => "settingsSection",
            Self::PanelSlot => "panelSlot",
            Self::ToolbarAction => "toolbarAction",
        }
    }
}

fn business_window() -> String {
    "business".to_owned()
}

/// Why a UI contribution is not valid in the S1 contract.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UiContractError {
    /// The contribution id is empty.
    #[error("a UI contribution needs a non-empty id")]
    Id,
    /// The title translation key is empty.
    #[error("a UI contribution needs a non-empty titleKey")]
    TitleKey,
    /// P0 only supports the `business` window.
    #[error("UI contribution window must be business, not {0}")]
    Window(String),
    /// A panel/toolbar contribution must name its parent page or panel.
    #[error("slot {0:?} needs a non-empty parent")]
    Parent(UiSlot),
    /// Two contributions share a slot/id pair.
    #[error("duplicate UI contribution {slot:?}/{id}")]
    DuplicateId {
        /// Slot both contributions use.
        slot: UiSlot,
        /// Repeated id.
        id: String,
    },
}

/// One contribution a plugin places in a slot.
///
/// Optional fields express the richer slots without changing the S4 single
/// NavPage shape:
/// - `parent`: page/panel that owns a [`UiSlot::PanelSlot`] or
///   [`UiSlot::ToolbarAction`].
/// - `entry`: external HTML entry relative to the plugin directory. Builtin
///   pages use a compiled component and leave it `None`.
/// - `icon_key` / `action_key`: icon translation key and action id.
/// - `titles`: localized titles; `title_key` stays the primary i18n key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UiContribution {
    /// Slot this contribution occupies.
    pub slot: UiSlot,
    /// Page or component id inside the slot; unique per slot.
    pub id: String,
    /// Window label; P0 contributes to `business` only.
    #[serde(default = "business_window")]
    pub window: String,
    /// Primary translation key for the visible title.
    pub title_key: String,
    /// Sort position inside the slot; lower values come first.
    #[serde(default)]
    pub order: i32,
    /// Parent page/panel for panel and toolbar slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// External page entry, relative to the plugin directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// Translation key for an icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_key: Option<String>,
    /// Action id the toolbar button invokes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_key: Option<String>,
    /// Localized titles for external pages, keyed by locale.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub titles: BTreeMap<String, String>,
}

impl UiContribution {
    /// Validate the P0/S4 invariants that the window relies on.
    pub fn validate(&self) -> Result<(), UiContractError> {
        if self.id.trim().is_empty() {
            return Err(UiContractError::Id);
        }
        if self.title_key.trim().is_empty() {
            return Err(UiContractError::TitleKey);
        }
        if self.window != "business" {
            return Err(UiContractError::Window(self.window.clone()));
        }
        if matches!(self.slot, UiSlot::PanelSlot | UiSlot::ToolbarAction)
            && self
                .parent
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
        {
            return Err(UiContractError::Parent(self.slot));
        }
        Ok(())
    }
}

/// Validate a whole contribution list: each item is valid and `(slot, id)` is
/// unique. The window then sorts by `order` ascending and breaks ties by `id`.
pub fn validate_ui_contributions(items: &[UiContribution]) -> Result<(), UiContractError> {
    let mut seen = BTreeSet::new();
    for item in items {
        item.validate()?;
        if !seen.insert((item.slot.as_str(), item.id.clone())) {
            return Err(UiContractError::DuplicateId {
                slot: item.slot,
                id: item.id.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contribution(slot: UiSlot) -> UiContribution {
        UiContribution {
            slot,
            id: "jobs".to_owned(),
            window: "business".to_owned(),
            title_key: "plugins.jobs.title".to_owned(),
            order: 1,
            parent: None,
            entry: None,
            icon_key: None,
            action_key: None,
            titles: BTreeMap::new(),
        }
    }

    #[test]
    fn a_nav_page_needs_no_parent() {
        assert_eq!(contribution(UiSlot::NavPage).validate(), Ok(()));
    }

    #[test]
    fn panel_and_toolbar_need_a_parent() {
        for slot in [UiSlot::PanelSlot, UiSlot::ToolbarAction] {
            assert!(matches!(
                contribution(slot).validate(),
                Err(UiContractError::Parent(_))
            ));
        }
    }

    #[test]
    fn the_window_is_business_only() {
        let mut item = contribution(UiSlot::NavPage);
        item.window = "dev".to_owned();
        assert!(matches!(item.validate(), Err(UiContractError::Window(_))));
    }
}
