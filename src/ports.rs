//! The physical connectors on the graphics hardware.
//!
//! Sway only reports outputs that have a display attached, but an appliance
//! is configured against *sockets*: the operator positions "the projector on
//! DP-2" whether or not DP-2 currently has anything plugged into it, and the
//! configuration must survive the projector being unplugged, swapped, or not
//! yet delivered. The kernel enumerates every connector at driver probe,
//! independently of what is attached, so the full list is there to be read —
//! it is just not something the compositor will tell us.
//!
//! This is deliberately the only place that reaches past sway to the kernel.
//! It is read-only, it is advisory (a client uses it to offer choices), and
//! everything that *acts* still goes through sway.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A connector on the graphics hardware, attached or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Port {
    /// Connector name as sway would report it, e.g. `DP-2`.
    pub name: String,
    /// Whether a display is currently attached.
    pub connected: bool,
}

/// Where the kernel exposes DRM connectors.
const DRM_CLASS: &str = "/sys/class/drm";

/// Every connector the graphics hardware has, sorted by name.
///
/// Returns an empty list wherever this cannot be read — a non-Linux host, a
/// container without `/sys`, an exotic driver. Callers must treat the result
/// as "extra choices we can offer", never as the set of outputs that exist:
/// sway remains the authority on what is actually driving a display.
pub fn enumerate() -> Vec<Port> {
    enumerate_in(DRM_CLASS)
}

fn enumerate_in(root: &str) -> Vec<Port> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut ports: Vec<Port> = entries
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let name = connector_name(file_name.to_str()?)?;
            // `status` is "connected", "disconnected", or "unknown".
            let status = std::fs::read_to_string(entry.path().join("status")).ok()?;
            Some(Port {
                name,
                connected: status.trim() == "connected",
            })
        })
        .collect();
    ports.sort_by(|a, b| a.name.cmp(&b.name));
    ports.dedup();
    ports
}

/// The sway-visible connector name inside a DRM sysfs entry name.
///
/// Entries are `card<N>-<CONNECTOR>`, e.g. `card1-DP-2`; the card index is a
/// property of the GPU, not of the connector, and sway does not use it.
/// Anything else in the directory (`version`, `renderD128`, the card nodes
/// themselves) is not a connector, and `Unknown-*` entries are writeback or
/// virtual connectors that never become displays.
fn connector_name(entry: &str) -> Option<String> {
    let (card, connector) = entry.split_once('-')?;
    if !card.starts_with("card") || connector.is_empty() || connector.starts_with("Unknown") {
        return None;
    }
    Some(connector.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_names_drop_the_card_index() {
        assert_eq!(connector_name("card1-DP-2").as_deref(), Some("DP-2"));
        assert_eq!(
            connector_name("card0-HDMI-A-1").as_deref(),
            Some("HDMI-A-1")
        );
    }

    #[test]
    fn non_connector_entries_are_ignored() {
        for entry in ["card0", "renderD128", "version", "card1-Unknown-2"] {
            assert_eq!(connector_name(entry), None, "{entry}");
        }
    }

    #[test]
    fn enumerating_reads_status_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        for (entry, status) in [
            ("card1-DP-3", "connected\n"),
            ("card1-DP-1", "connected\n"),
            ("card1-DP-2", "disconnected\n"),
            ("card1-Unknown-2", "disconnected\n"),
        ] {
            let path = dir.path().join(entry);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("status"), status).unwrap();
        }
        // A card node with no status file must not become a phantom port.
        std::fs::create_dir(dir.path().join("card1")).unwrap();

        assert_eq!(
            enumerate_in(dir.path().to_str().unwrap()),
            vec![
                Port {
                    name: "DP-1".into(),
                    connected: true
                },
                Port {
                    name: "DP-2".into(),
                    connected: false
                },
                Port {
                    name: "DP-3".into(),
                    connected: true
                },
            ]
        );
    }

    #[test]
    fn a_missing_sysfs_is_not_an_error() {
        assert!(enumerate_in("/definitely/not/here").is_empty());
    }
}
