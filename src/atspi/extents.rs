use anyhow::{Context, Result};
use atspi::proxy::component::ComponentProxy;
use atspi::zbus::names::BusName;
use atspi::{AccessibilityConnection, CoordType, ObjectRefOwned};

async fn component_proxy<'a>(
    conn: &'a AccessibilityConnection,
    object: &ObjectRefOwned,
) -> Result<ComponentProxy<'a>> {
    let name: BusName = object
        .name()
        .context("object reference has no bus name")?
        .clone()
        .into();
    ComponentProxy::builder(conn.connection())
        .destination(name)?
        .path(object.path())?
        .build()
        .await
        .context("failed to build Component proxy")
}

/// Fetch an element's window-relative logical extents `(x, y, w, h)`.
pub async fn window_extents(
    conn: &AccessibilityConnection,
    object: &ObjectRefOwned,
) -> Result<(i32, i32, i32, i32)> {
    let proxy = component_proxy(conn, object).await?;
    proxy
        .get_extents(CoordType::Window)
        .await
        .context("Component.GetExtents(Window) failed")
}
