//! Keeping the user's own screen out of the virtual monitor's way.
//!
//! Adding a virtual monitor changes the set of monitors, and Mutter reacts by
//! looking for a stored configuration for that new set. There is none, so it
//! **generates a default one**, which uses each monitor's *preferred* mode and
//! scale. Whatever the user had chosen for their laptop is thrown away.
//!
//! Measured on a 1920x1200 panel:
//!
//! | | laptop | virtual |
//! | --- | --- | --- |
//! | before | 1920x1200@60 +VRR, scale 1.0 | — |
//! | after `RecordVirtual` | 1920x1200@60, scale **1.25** | 1920x1080@60 |
//!
//! Scale 1.25 on that panel leaves 1536x960 of usable desktop: from the user's
//! chair, "casting made my screen smaller". Variable refresh rate was dropped
//! too.
//!
//! So this module takes the layout over instead of accepting Mutter's default:
//! it snapshots the arrangement before the virtual monitor appears, and puts it
//! back afterwards with the new screen appended to the right. The user's
//! monitor keeps the exact mode, scale and position it had.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

use nd_core::{NdError, Result};

/// How `ApplyMonitorsConfig` should treat the configuration.
///
/// `1` = temporary: it applies now and is not written to the user's stored
/// configuration. That is what we want — casting must not rewrite how someone's
/// desktop is laid out after the session ends.
const METHOD_TEMPORARY: u32 = 1;

/// How many times to retry restoring the layout.
const RESTORE_ATTEMPTS: u32 = 8;

/// The wait between restore attempts, while the monitor set settles.
const RESTORE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

#[zbus::proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
trait DisplayConfig {
    #[allow(clippy::type_complexity)]
    fn get_current_state(
        &self,
    ) -> zbus::Result<(
        u32,
        Vec<MonitorInfo>,
        Vec<LogicalMonitorState>,
        HashMap<String, OwnedValue>,
    )>;

    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;

    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: &[LogicalMonitorConfig],
        properties: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
}

/// A monitor as `GetCurrentState` reports it: identity, modes, properties.
type MonitorInfo = (
    (String, String, String, String),
    Vec<(
        String,
        i32,
        i32,
        f64,
        f64,
        Vec<f64>,
        HashMap<String, OwnedValue>,
    )>,
    HashMap<String, OwnedValue>,
);

/// A logical monitor as `GetCurrentState` reports it.
///
/// `(x, y, scale, transform, primary, [(connector, vendor, product, serial)])`
type LogicalMonitorState = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);

/// A logical monitor as `ApplyMonitorsConfig` expects it.
///
/// `(x, y, scale, transform, primary, [(connector, mode_id, properties)])`
type LogicalMonitorConfig = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, HashMap<String, Value<'static>>)>,
);

/// The desktop arrangement as it was before we touched anything.
#[derive(Debug, Clone)]
pub struct LayoutSnapshot {
    logical: Vec<LogicalMonitorConfig>,
    /// The rightmost edge, in logical pixels: where a new screen goes.
    right_edge: i32,
    layout_mode: u32,
    supports_layout_mode: bool,
    global_scale: Option<f64>,
    connectors: Vec<String>,
}

impl LayoutSnapshot {
    /// Reads the current arrangement.
    ///
    /// Returns `None` when `org.gnome.Mutter.DisplayConfig` is not there
    /// (another compositor, or Flatpak) — the caller then simply skips the
    /// whole layout dance.
    pub async fn capture(conn: &Connection) -> Option<Self> {
        let proxy = DisplayConfigProxy::new(conn).await.ok()?;
        let (_serial, monitors, logical, props) = proxy.get_current_state().await.ok()?;
        let layout_mode = props
            .get("layout-mode")
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(1);
        let supports_layout_mode = props
            .get("supports-changing-layout-mode")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false);
        let global_scale = props
            .get("global-scale-required")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
            .then(|| logical.first().map_or(1.0, |logical| logical.2));
        let all_connectors = monitors
            .iter()
            .map(|((name, ..), ..)| name.clone())
            .collect();

        let mut config = Vec::new();
        let mut right_edge = 0;
        for (x, y, scale, transform, primary, assigned, _) in logical {
            let mut connectors = Vec::new();
            for (connector, _, _, _) in &assigned {
                // The mode the monitor is on *right now*, not its preferred
                // one: reapplying the preferred mode is the very thing that
                // moved the user's screen to scale 1.25.
                if let Some(mode) = current_mode_of(&monitors, connector) {
                    let mut properties = HashMap::new();
                    if let Some((_, _, props)) =
                        monitors.iter().find(|((name, ..), ..)| name == connector)
                    {
                        if let Some(value) = props
                            .get("is-underscanning")
                            .and_then(|v| bool::try_from(v).ok())
                        {
                            properties.insert("underscanning".into(), Value::from(value));
                        }
                        if let Some(value) =
                            props.get("color-mode").and_then(|v| u32::try_from(v).ok())
                        {
                            properties.insert("color-mode".into(), Value::from(value));
                        }
                    }
                    connectors.push((connector.clone(), mode, properties));
                }
            }
            if connectors.is_empty() {
                continue;
            }
            if let Some(width) = logical_width(&monitors, &assigned, scale, transform, layout_mode)
            {
                right_edge = right_edge.max(x + width);
            }
            config.push((x, y, scale, transform, primary, connectors));
        }

        (!config.is_empty()).then_some(Self {
            logical: config,
            right_edge,
            layout_mode,
            supports_layout_mode,
            global_scale,
            connectors: all_connectors,
        })
    }

    fn properties(&self) -> HashMap<&str, Value<'_>> {
        if self.supports_layout_mode {
            HashMap::from([("layout-mode", Value::from(self.layout_mode))])
        } else {
            HashMap::new()
        }
    }

    /// Keep the current physical layout when the user rearranged it during capture.
    pub fn without(mut self, connector: &str) -> Option<Self> {
        self.connectors.retain(|name| name != connector);
        for logical in &mut self.logical {
            logical.5.retain(|(name, ..)| name != connector);
        }
        self.logical.retain(|logical| !logical.5.is_empty());
        if !self.logical.iter().any(|logical| logical.4) {
            if let Some(first) = self.logical.first_mut() {
                first.4 = true;
            }
        }
        (!self.logical.is_empty()).then_some(self)
    }

    /// Reapplies the saved arrangement, with `virtual_connector` appended to
    /// its right at the given mode.
    ///
    /// The user's monitors keep the exact mode, scale and position they had.
    /// The new screen goes in at scale 1.0: it exists to receive a video
    /// stream, and any other scale would only make the receiver upscale.
    pub async fn extend_with(
        &self,
        conn: &Connection,
        virtual_connector: &str,
        mode: &str,
    ) -> Result<()> {
        let proxy = DisplayConfigProxy::new(conn)
            .await
            .map_err(|e| NdError::Capture(e.to_string()))?;

        // The serial changed when the monitor set changed; a stale one is
        // rejected outright.
        let (serial, ..) = proxy
            .get_current_state()
            .await
            .map_err(|e| NdError::Capture(e.to_string()))?;

        let mut logical = self.logical.clone();
        logical.push((
            self.right_edge,
            0,
            self.global_scale.unwrap_or(1.0),
            0,
            false,
            vec![(
                virtual_connector.to_string(),
                mode.to_string(),
                HashMap::new(),
            )],
        ));

        proxy
            .apply_monitors_config(serial, METHOD_TEMPORARY, &logical, self.properties())
            .await
            .map_err(|e| NdError::Capture(format!("could not arrange the extra screen: {e}")))
    }

    /// Puts the arrangement back exactly as it was.
    ///
    /// Called when the session ends: Mutter regenerates a default layout when
    /// the virtual monitor goes away too, and that default is what changed the
    /// user's scale in the first place.
    ///
    /// Retried, and that is the whole point. `ApplyMonitorsConfig` rejects a
    /// configuration whose serial is not the current one, and the serial keeps
    /// changing while the virtual monitor is being torn down — the first
    /// attempt reliably came back with *"the requested configuration is based
    /// on stale information"*, the restore never happened, and the user was
    /// left with the compositor's default scale on their own screen.
    pub async fn restore(&self, conn: &Connection) -> Result<()> {
        let proxy = DisplayConfigProxy::new(conn)
            .await
            .map_err(|e| NdError::Capture(e.to_string()))?;

        let mut last = String::new();
        for attempt in 1..=RESTORE_ATTEMPTS {
            // Read the serial immediately before using it, every time.
            let (serial, monitors, ..) = proxy
                .get_current_state()
                .await
                .map_err(|e| NdError::Capture(e.to_string()))?;

            if monitors.len() != self.connectors.len()
                || monitors
                    .iter()
                    .any(|((name, ..), ..)| !self.connectors.contains(name))
            {
                // Stop returns before Mutter removes the virtual connector.
                // Let that transient topology settle before treating it as a
                // physical hotplug and preserving the compositor's layout.
                if attempt < RESTORE_ATTEMPTS {
                    tokio::time::sleep(RESTORE_RETRY_DELAY).await;
                    continue;
                }
                tracing::info!("monitor topology changed; preserving the compositor layout");
                return Ok(());
            }

            match proxy
                .apply_monitors_config(serial, METHOD_TEMPORARY, &self.logical, self.properties())
                .await
            {
                Ok(()) => return Ok(()),
                Err(err) => {
                    last = err.to_string();
                    tracing::debug!(attempt, %err, "layout restore rejected; retrying");
                    tokio::time::sleep(RESTORE_RETRY_DELAY).await;
                }
            }
        }

        Err(NdError::Capture(format!(
            "could not restore the layout after {RESTORE_ATTEMPTS} attempts: {last}"
        )))
    }
}

/// The id of the mode a connector is currently using.
fn current_mode_of(monitors: &[MonitorInfo], connector: &str) -> Option<String> {
    let monitor = monitors.iter().find(|((c, ..), ..)| c == connector)?;
    monitor
        .1
        .iter()
        .find(|(_, _, _, _, _, _, props)| props.contains_key("is-current"))
        .map(|(id, ..)| id.clone())
}

/// A logical monitor's width in logical pixels (physical width ÷ scale).
fn logical_width(
    monitors: &[MonitorInfo],
    assigned: &[(String, String, String, String)],
    scale: f64,
    transform: u32,
    layout_mode: u32,
) -> Option<i32> {
    let (connector, ..) = assigned.first()?;
    let monitor = monitors.iter().find(|((c, ..), ..)| c == connector)?;
    let (_, width, height, ..) = monitor
        .1
        .iter()
        .find(|(_, _, _, _, _, _, props)| props.contains_key("is-current"))?;
    let extent = if transform % 2 == 1 { *height } else { *width };
    let scale = if layout_mode == 2 { 1.0 } else { scale };
    if !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    Some(((extent as f64) / scale).round() as i32)
}

/// Waits for the virtual monitor to show up and returns its connector name
/// (`Meta-0`, `Meta-1`, …).
///
/// Waiting is not optional. `RecordVirtual` cannot derive a size from an
/// existing monitor, so Mutter leaves it to PipeWire to negotiate one — and
/// **the monitor is only created once that negotiation finishes**, which
/// happens when the pipeline connects to the stream, well after `start()`
/// returns. Looking right away finds nothing, which is exactly what the first
/// version of this code did.
///
/// There is no API that reports the connector, so it is found by elimination:
/// the one that was not there before.
pub async fn wait_for_virtual_connector(
    conn: &Connection,
    before: &[String],
    timeout: std::time::Duration,
) -> Option<String> {
    let Ok(proxy) = DisplayConfigProxy::new(conn).await else {
        return None;
    };

    // Driven by `MonitorsChanged` rather than by polling, and the difference is
    // visible to the user. The moment the extra screen appears, Mutter has
    // already replaced the layout with a generated default — which uses the
    // panel's *preferred* scale, so the laptop's screen jumps to "everything
    // bigger" until our own layout goes back on. Polling every 250 ms left that
    // wrong layout on screen for up to a quarter of a second; reacting to the
    // signal closes the gap to as little as the compositor allows.
    let mut changed = proxy.receive_monitors_changed().await.ok();

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(found) = connectors(conn)
            .await
            .into_iter()
            .find(|connector| !before.contains(connector))
        {
            return Some(found);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }

        match changed.as_mut() {
            // Wake on the signal, with a ceiling so a missed signal cannot hang
            // this forever.
            Some(stream) => {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(500),
                    futures::StreamExt::next(stream),
                )
                .await;
            }
            None => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
        }
    }
}

/// The connector of the monitor GNOME considers primary.
///
/// Returns `None` only when the display configuration cannot be read at all.
pub async fn primary_connector(conn: &Connection) -> Option<String> {
    let proxy = DisplayConfigProxy::new(conn).await.ok()?;
    let (_serial, monitors, logical, _props) = proxy.get_current_state().await.ok()?;

    for (_x, _y, _scale, _transform, primary, assigned, _props) in &logical {
        if *primary {
            if let Some((connector, ..)) = assigned.first() {
                return Some(connector.clone());
            }
        }
    }
    // No monitor flagged primary (it happens on some setups): the first one is
    // a better answer than failing outright.
    monitors
        .first()
        .map(|((connector, ..), ..)| connector.clone())
}

/// The connectors currently attached.
pub async fn connectors(conn: &Connection) -> Vec<String> {
    let Ok(proxy) = DisplayConfigProxy::new(conn).await else {
        return Vec::new();
    };
    let Ok((_, monitors, ..)) = proxy.get_current_state().await else {
        return Vec::new();
    };
    monitors
        .iter()
        .map(|((connector, ..), ..)| connector.clone())
        .collect()
}

/// The preferred mode of a connector, and its size.
///
/// Used to put the extra screen at the resolution the receiver actually has,
/// rather than whatever PipeWire happened to negotiate.
pub async fn preferred_mode(conn: &Connection, connector: &str) -> Option<(String, i32, i32)> {
    let proxy = DisplayConfigProxy::new(conn).await.ok()?;
    let (_, monitors, ..) = proxy.get_current_state().await.ok()?;
    let monitor = monitors.iter().find(|((c, ..), ..)| c == connector)?;
    monitor
        .1
        .iter()
        .find(|(_, _, _, _, _, _, props)| props.contains_key("is-preferred"))
        .or_else(|| monitor.1.first())
        .map(|(id, w, h, ..)| (id.clone(), *w, *h))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portrait_width_and_physical_layout_are_preserved() {
        let monitors = vec![(
            (
                "DP-1".into(),
                "vendor".into(),
                "panel".into(),
                "serial".into(),
            ),
            vec![(
                "mode".into(),
                1920,
                1080,
                60.0,
                1.0,
                vec![1.0, 2.0],
                HashMap::from([("is-current".into(), OwnedValue::from(true))]),
            )],
            HashMap::new(),
        )];
        let assigned = vec![monitors[0].0.clone()];
        assert_eq!(logical_width(&monitors, &assigned, 2.0, 1, 1), Some(540));
        assert_eq!(logical_width(&monitors, &assigned, 2.0, 1, 2), Some(1080));
        assert_eq!(logical_width(&monitors, &assigned, 2.0, 0, 1), Some(960));
        assert_eq!(logical_width(&monitors, &assigned, 0.0, 0, 1), None);
    }
}
