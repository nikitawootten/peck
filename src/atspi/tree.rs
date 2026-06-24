use anyhow::{Context, Result};
use atspi::proxy::accessible::ObjectRefExt;
use atspi::{AccessibilityConnection, ObjectRefOwned, Role, State, StateSet};

use super::Element;

fn is_web_document(role: Role) -> bool {
    matches!(role, Role::DocumentWeb | Role::DocumentFrame)
}

fn is_actionable_role(role: Role) -> bool {
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

/// True if an element with this role and state set is a hint target: an
/// actionable role that is currently interactable.
pub fn is_actionable(role: Role, states: &StateSet) -> bool {
    is_actionable_role(role) && is_interactable(states)
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

/// Walk the subtree rooted at `frame`, yielding the on-screen elements that
/// `keep` selects.
pub async fn walk(
    conn: &AccessibilityConnection,
    frame: ObjectRefOwned,
    keep: impl Fn(Role, &StateSet) -> bool,
) -> Result<Vec<Element>> {
    let zconn = conn.connection();

    let mut out = Vec::new();
    // Each frame carries whether it is inside a web document (see prune below).
    let mut stack = vec![(frame, false)];

    while let Some((node, in_web_doc)) = stack.pop() {
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

        // Prune subtrees
        //
        // Normally, elements that are not SHOWING can be skipped, however in
        // the context of a web document, SHOWING seems to be unreliable so
        // fall back to pruning on VISIBLE instead.
        let hidden = if in_web_doc {
            !states.contains(State::Visible)
        } else {
            !states.contains(State::Showing)
        };
        if hidden {
            continue;
        }

        let role = proxy.get_role().await.unwrap_or(Role::Invalid);
        if keep(role, &states) {
            let name = proxy.name().await.unwrap_or_default();
            out.push(Element {
                object: node.clone(),
                role,
                name,
                states,
            });
        }

        let child_in_web_doc = in_web_doc || is_web_document(role);
        for child in proxy
            .get_children()
            .await
            .unwrap_or_default()
            .into_iter()
            .rev()
        {
            stack.push((child, child_in_web_doc));
        }
    }

    Ok(out)
}
