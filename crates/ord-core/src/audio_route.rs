//! Pure per-application audio routing decisions.
//!
//! This is the testable core of per-app audio (gpu-screen-recorder's `app:` /
//! `app-inverse:` selectors, OBS's pipewire-audio-capture-app model): given the
//! live PipeWire node graph and a track's configured [`AudioSource`]s, decide
//! which application output streams to link into the track's capture sink and
//! which device monitors to capture directly.
//!
//! No PipeWire calls happen here — the live capture module feeds this the node
//! list and acts on the returned plan — so the matching logic is unit-tested
//! without a sound server.

use ord_common::config::AudioSource;

/// A live PipeWire audio node relevant to routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioNode {
    /// PipeWire global id.
    pub id: u32,
    /// Application name (`application.name` / `node.name`), if any.
    pub app_name: Option<String>,
    /// Whether this node is an application playback stream that can be linked
    /// into a capture sink (Stream/Output/Audio). Sinks/sources are not.
    pub is_output_stream: bool,
}

/// A device/monitor source a track captures directly (not via app linking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorSel {
    /// The default sink monitor (desktop/game audio).
    DefaultOutput,
    /// The default source (microphone).
    DefaultInput,
    /// A named device.
    Device(String),
}

/// The resolved capture plan for one track.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackPlan {
    /// Device monitors to capture directly.
    pub monitors: Vec<MonitorSel>,
    /// Application stream node ids to link into the track's virtual sink.
    pub app_node_ids: Vec<u32>,
}

/// Case-insensitive substring match of an app stream against `needle`.
fn app_matches(needle: &str, node: &AudioNode) -> bool {
    if !node.is_output_stream {
        return false;
    }
    match &node.app_name {
        Some(name) => name
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase()),
        None => false,
    }
}

/// Resolve a track's sources against the live node list. `App`/`AppInverse`
/// select application streams to link; `DefaultOutput`/`DefaultInput`/`Device`
/// become direct monitor captures. App ids are de-duplicated and sorted.
pub fn plan_track(sources: &[AudioSource], nodes: &[AudioNode]) -> TrackPlan {
    let mut monitors = Vec::new();
    let mut app_ids = Vec::new();
    for src in sources {
        match src {
            AudioSource::DefaultOutput => monitors.push(MonitorSel::DefaultOutput),
            AudioSource::DefaultInput => monitors.push(MonitorSel::DefaultInput),
            AudioSource::Device(name) => monitors.push(MonitorSel::Device(name.clone())),
            AudioSource::App(name) => {
                for n in nodes {
                    if app_matches(name, n) {
                        app_ids.push(n.id);
                    }
                }
            }
            AudioSource::AppInverse(name) => {
                for n in nodes {
                    if n.is_output_stream && !app_matches(name, n) {
                        app_ids.push(n.id);
                    }
                }
            }
        }
    }
    app_ids.sort_unstable();
    app_ids.dedup();
    TrackPlan {
        monitors,
        app_node_ids: app_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: u32, app: Option<&str>, stream: bool) -> AudioNode {
        AudioNode {
            id,
            app_name: app.map(|s| s.to_string()),
            is_output_stream: stream,
        }
    }

    fn nodes() -> Vec<AudioNode> {
        vec![
            node(10, Some("Firefox"), true),
            node(11, Some("Discord"), true),
            node(12, Some("obs"), true),
            node(13, Some("AlsaSink"), false), // a sink, not a stream
        ]
    }

    #[test]
    fn app_selects_matching_streams_case_insensitive() {
        let plan = plan_track(&[AudioSource::App("firefox".into())], &nodes());
        assert_eq!(plan.app_node_ids, vec![10]);
        assert!(plan.monitors.is_empty());
    }

    #[test]
    fn app_inverse_selects_all_other_streams() {
        let plan = plan_track(&[AudioSource::AppInverse("discord".into())], &nodes());
        // Everything that is an output stream except Discord (11); the sink (13)
        // is excluded because it isn't a stream.
        assert_eq!(plan.app_node_ids, vec![10, 12]);
    }

    #[test]
    fn monitors_resolved_for_default_and_device() {
        let plan = plan_track(
            &[
                AudioSource::DefaultOutput,
                AudioSource::DefaultInput,
                AudioSource::Device("alsa_output.pci".into()),
            ],
            &nodes(),
        );
        assert_eq!(
            plan.monitors,
            vec![
                MonitorSel::DefaultOutput,
                MonitorSel::DefaultInput,
                MonitorSel::Device("alsa_output.pci".into()),
            ]
        );
        assert!(plan.app_node_ids.is_empty());
    }

    #[test]
    fn mixed_track_resolves_both_and_dedups() {
        // Two selectors that both match Firefox (10) must dedup to one id, and a
        // monitor source is kept alongside the app links.
        let plan = plan_track(
            &[
                AudioSource::DefaultOutput,
                AudioSource::App("fire".into()),
                AudioSource::App("Firefox".into()),
            ],
            &nodes(),
        );
        assert_eq!(plan.monitors, vec![MonitorSel::DefaultOutput]);
        assert_eq!(plan.app_node_ids, vec![10]);
    }

    #[test]
    fn no_match_yields_empty_app_ids() {
        let plan = plan_track(&[AudioSource::App("spotify".into())], &nodes());
        assert!(plan.app_node_ids.is_empty());
    }
}
