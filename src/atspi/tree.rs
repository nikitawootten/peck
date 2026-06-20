use anyhow::{Context, Result};
use atspi::proxy::accessible::ObjectRefExt;
use atspi::{AccessibilityConnection, ObjectRefOwned, Role, State, StateSet};

use super::Element;

fn is_actionable(role: Role) -> bool {
    matches!(
        role,
        Role::Button
            | Role::ToggleButton
            | Role::PushButtonMenu
            | Role::CheckBox
            | Role::RadioButton
            | Role::CheckMenuItem
            | Role::RadioMenuItem
            | Role::MenuItem
            | Role::Menu
            | Role::Link
            | Role::PageTab
            | Role::ComboBox
            | Role::Slider
            | Role::SpinButton
            | Role::Entry
            | Role::PasswordText
    )
}

/// True if the element is currently interactable: on-screen and enabled.
fn is_interactable(states: &StateSet) -> bool {
    states.contains(State::Showing)
        && states.contains(State::Visible)
        // Some toolkits expose ENABLED, others SENSITIVE; accept either.
        && (states.contains(State::Enabled) || states.contains(State::Sensitive))
}

/// Find the active toplevel frame: walk applications under the registry root
/// and return the first toplevel that carries the `ACTIVE` state.
pub async fn active_frame(conn: &AccessibilityConnection) -> Result<Option<ObjectRefOwned>> {
    let zconn = conn.connection();
    let root = conn
        .root_accessible_on_registry()
        .await
        .context("failed to resolve AT-SPI registry root")?;

    let apps = root
        .get_children()
        .await
        .context("failed to list applications under registry root")?;

    for app_ref in apps {
        let app = match app_ref.as_accessible_proxy(zconn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreachable application");
                continue;
            }
        };

        let toplevels = app.get_children().await.unwrap_or_default();
        for top_ref in toplevels {
            let top = match top_ref.as_accessible_proxy(zconn).await {
                Ok(p) => p,
                Err(_) => continue,
            };
            match top.get_state().await {
                Ok(states) if states.contains(State::Active) => {
                    return Ok(Some(top_ref));
                }
                _ => {}
            }
        }
    }

    Ok(None)
}

/// Enumerate the actionable elements in the active application's subtree.
pub async fn actionable_elements(conn: &AccessibilityConnection) -> Result<Vec<Element>> {
    let frame = active_frame(conn)
        .await?
        .context("no active toplevel frame found (is a window focused, and a11y enabled?)")?;
    walk(conn, frame).await
}

/// Walk a subtree (rooted at `frame`) collecting actionable elements.
pub async fn walk(conn: &AccessibilityConnection, frame: ObjectRefOwned) -> Result<Vec<Element>> {
    let zconn = conn.connection();

    let mut out = Vec::new();
    let mut stack = vec![frame];

    while let Some(node) = stack.pop() {
        let proxy = match node.as_accessible_proxy(zconn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreachable node");
                continue;
            }
        };

        let states = match proxy.get_state().await {
            Ok(s) => s,
            Err(_) => continue,
        };

        // Prune non-showing subtrees: if a container is not showing, its
        // descendants are off-screen too, so don't descend.
        if !states.contains(State::Showing) {
            continue;
        }

        let role = proxy.get_role().await.unwrap_or(Role::Invalid);
        if is_actionable(role) && is_interactable(&states) {
            let name = proxy.name().await.unwrap_or_default();
            out.push(Element {
                object: node.clone(),
                role,
                name,
            });
        }

        for child in proxy
            .get_children()
            .await
            .unwrap_or_default()
            .into_iter()
            .rev()
        {
            stack.push(child);
        }
    }

    Ok(out)
}
