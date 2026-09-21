//! ScreenCast Start adapter for properties not exposed by ashpd 0.13.
//!
//! Keep the documented `(u, a{sv})` response intact. In particular, converting
//! through ashpd::Stream first would discard portal v6's `pipewire-serial`.
//! https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use ashpd::desktop::{screencast::Screencast, Session};
use ashpd::WindowIdentifier;
use futures::StreamExt;
use nd_core::{NdError, Result};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

type Properties = HashMap<String, OwnedValue>;
type Streams = Vec<(u32, Properties)>;
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub(crate) struct StartedStream {
    pub node_id: u32,
    pub serial: Option<u64>,
    pub size: Option<(u32, u32)>,
    pub restore_token: Option<String>,
}

fn error(reason: impl std::fmt::Display) -> NdError {
    NdError::Capture(reason.to_string())
}

/// Subscribe before Start: a fast portal may emit Response before returning
/// the method reply. Use the same connection and session created by ashpd.
pub(crate) async fn start(
    proxy: &Screencast,
    session: &Session<Screencast>,
    parent: Option<&WindowIdentifier>,
) -> Result<StartedStream> {
    let connection = proxy.connection();
    let sender = connection
        .unique_name()
        .ok_or_else(|| error("portal connection has no unique name"))?;
    let token = format!(
        "bns_{}_{}",
        std::process::id(),
        NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
    );
    let path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sender.as_str().trim_start_matches(':').replace('.', "_"),
        token
    );
    let request = zbus::Proxy::new(
        connection,
        proxy.destination().clone(),
        path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .await
    .map_err(error)?;
    let mut responses = request.receive_signal("Response").await.map_err(error)?;
    let options = HashMap::from([("handle_token", Value::from(token.as_str()))]);
    let parent = parent.map(ToString::to_string).unwrap_or_default();
    let returned: OwnedObjectPath = proxy
        .call("Start", &(session, parent, options))
        .await
        .map_err(error)?;
    if returned.as_str() != path {
        return Err(error("portal returned an unexpected request handle"));
    }
    let response = responses
        .next()
        .await
        .ok_or_else(|| error("portal closed without a Start response"))?;
    let (status, properties): (u32, Properties) = response.body().deserialize().map_err(error)?;
    parse_response(status, properties)
}

fn parse_response(status: u32, mut properties: Properties) -> Result<StartedStream> {
    if status != 0 {
        return Err(error(if status == 1 {
            "screen selection was cancelled"
        } else {
            "the portal could not start screen capture"
        }));
    }
    let value = properties
        .remove("streams")
        .ok_or_else(|| error("portal response has no streams"))?;
    let mut streams: Streams = value.try_into().map_err(error)?;
    // SelectSources requested multiple=false; never silently select a different
    // screen when a backend violates that contract.
    if streams.len() != 1 {
        return Err(error("portal must return exactly one capture stream"));
    }
    let (node_id, mut stream) = streams.remove(0);
    let serial = stream
        .remove("pipewire-serial")
        .map(u64::try_from)
        .transpose()
        .map_err(error)?;
    if serial == Some(0) || (serial.is_none() && (node_id == 0 || node_id == u32::MAX)) {
        return Err(error("portal returned an invalid PipeWire target"));
    }
    let size = stream
        .remove("size")
        .map(|v| <(i32, i32)>::try_from(Value::from(v)))
        .transpose()
        .map_err(error)?;
    let size = match size {
        Some((w, h)) if w > 0 && h > 0 => Some((w as u32, h as u32)),
        Some(_) => return Err(error("portal returned invalid capture dimensions")),
        None => None,
    };
    let restore_token = properties
        .remove("restore_token")
        .map(String::try_from)
        .transpose()
        .map_err(error)?;
    if restore_token.as_ref().is_some_and(|s| s.len() > 4096) {
        return Err(error("portal restore token exceeds 4096 bytes"));
    }
    Ok(StartedStream {
        node_id,
        serial,
        size,
        restore_token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(serial: Option<OwnedValue>, size: (i32, i32), count: usize) -> Properties {
        let streams: Streams = (0..count)
            .map(|_| {
                let mut p = HashMap::new();
                p.insert("size".into(), Value::from(size).try_to_owned().unwrap());
                if let Some(s) = &serial {
                    p.insert("pipewire-serial".into(), s.try_clone().unwrap());
                }
                (42, p)
            })
            .collect();
        HashMap::from([(
            "streams".into(),
            Value::from(streams).try_to_owned().unwrap(),
        )])
    }

    #[test]
    fn preserves_full_u64_serial_without_node_id_conversion() {
        let serial = u64::from(u32::MAX) + 123;
        let s = parse_response(0, response(Some(serial.into()), (1920, 1080), 1)).unwrap();
        assert_eq!(s.node_id, 42);
        assert_eq!(s.serial, Some(serial));
        assert_eq!(s.size, Some((1920, 1080)));
    }
    #[test]
    fn older_portals_keep_the_explicit_node_id() {
        assert_eq!(
            parse_response(0, response(None, (1280, 720), 1))
                .unwrap()
                .serial,
            None
        );
    }
    #[test]
    fn malformed_serial_never_falls_back_to_a_reusable_id() {
        assert!(parse_response(0, response(Some(0u64.into()), (10, 10), 1)).is_err());
        assert!(parse_response(0, response(Some(42u32.into()), (10, 10), 1)).is_err());
    }
    #[test]
    fn rejects_ambiguous_streams_bad_sizes_and_cancelled_requests() {
        for count in [0, 2] {
            assert!(parse_response(0, response(None, (10, 10), count)).is_err());
        }
        assert!(parse_response(0, response(None, (-1, 10), 1)).is_err());
        assert!(parse_response(1, response(None, (10, 10), 1)).is_err());
    }
}
