//! Action dispatch via the AT-SPI `Action` interface.

use anyhow::{Context, Result};
use atspi::proxy::action::ActionProxy;
use atspi::zbus::names::BusName;
use atspi::{AccessibilityConnection, ObjectRefOwned, Role};

use super::Element;

const ACTIVATING_VERBS: &[&str] = &["click", "press", "activate", "jump", "open"];

/// Build an `Action` proxy for an object reference, mirroring `extents`'
/// `component_proxy`.
async fn action_proxy<'a>(
    conn: &'a AccessibilityConnection,
    object: &ObjectRefOwned,
) -> Result<ActionProxy<'a>> {
    let name: BusName = object
        .name()
        .context("object reference has no bus name")?
        .clone()
        .into();
    ActionProxy::builder(conn.connection())
        .destination(name)?
        .path(object.path())?
        .build()
        .await
        .context("failed to build Action proxy")
}

/// Try to activate `el` through the AT-SPI `Action` interface.
pub async fn try_action(conn: &AccessibilityConnection, el: &Element) -> Result<Option<String>> {
    // Text-entry-like roles: a real pointer click places the caret / focuses;
    // prefer that over any Action verb.
    if prefers_pointer(el.role) {
        return Ok(None);
    }

    let proxy = action_proxy(conn, &el.object).await?;

    // No Action interface (or it failed) → caller falls back to the pointer.
    let actions = match proxy.get_actions().await {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(name = %el.name, error = %e, "no Action interface; will fall back");
            return Ok(None);
        }
    };

    // Pick the highest-preference verb present, keeping the action's own name
    // for the outcome report.
    let chosen = ACTIVATING_VERBS.iter().find_map(|verb| {
        actions
            .iter()
            .position(|a| a.name.to_ascii_lowercase().contains(verb))
            .map(|index| (index, actions[index].name.clone()))
    });

    let Some((index, verb)) = chosen else {
        let names: Vec<&str> = actions.iter().map(|a| a.name.as_str()).collect();
        tracing::debug!(name = %el.name, ?names, "no activating verb; will fall back");
        return Ok(None);
    };

    let performed = proxy
        .do_action(index as i32)
        .await
        .context("Action.DoAction failed")?;

    Ok(performed.then_some(verb))
}

/// Roles for which a synthetic pointer click is preferable to an Action verb.
fn prefers_pointer(role: Role) -> bool {
    matches!(
        role,
        Role::Entry | Role::PasswordText | Role::ComboBox | Role::Slider | Role::SpinButton
    )
}
