//! The PipeWire side of the Firefox engine (`firefox_engine.rs`): which sink Qobuz
//! plays to, and that sink's volume and rates. Everything here shells out to
//! PipeWire's own tools (pw-dump, pw-metadata, wpctl) and blocks: call it from
//! `spawn_blocking`.

use std::process::Command;

use serde_json::{Map, Value};

/// One snapshot of the PipeWire graph (`pw-dump`).
pub struct Graph(Vec<Value>);

fn prop(props: &Map<String, Value>, key: &str) -> Option<String> {
    props.get(key).map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

impl Graph {
    /// pw-dump prints one JSON array, and another for every change it saw while
    /// dumping: all of them are read, the latest version of an object winning (a
    /// removed one comes back with `"info": null` and so drops out).
    pub fn read() -> Graph {
        let out = Command::new("pw-dump").output().map(|o| o.stdout).unwrap_or_default();
        Graph::parse(&out)
    }

    fn parse(out: &[u8]) -> Graph {
        let mut objects: Vec<Value> = Vec::new();
        for doc in serde_json::Deserializer::from_slice(out).into_iter::<Vec<Value>>() {
            let Ok(doc) = doc else { break };
            for object in doc {
                let id = object.get("id").and_then(Value::as_u64);
                match objects.iter_mut().find(|o| id.is_some() && o.get("id").and_then(Value::as_u64) == id) {
                    Some(slot) => *slot = object,
                    None => objects.push(object),
                }
            }
        }
        Graph(objects)
    }

    fn nodes(&self) -> impl Iterator<Item = (u64, &Map<String, Value>, &Value)> {
        self.0
            .iter()
            .filter(|o| o.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Node"))
            .filter_map(|o| Some((o.get("id")?.as_u64()?, o.pointer("/info/props")?.as_object()?, o)))
    }

    fn sinks(&self) -> impl Iterator<Item = (u64, &Map<String, Value>, &Value)> {
        self.nodes().filter(|(_, p, _)| prop(p, "media.class").as_deref() == Some("Audio/Sink"))
    }

    pub fn sink_id(&self, name: &str) -> Option<u64> {
        self.sinks().find(|(_, p, _)| prop(p, "node.name").as_deref() == Some(name)).map(|(id, _, _)| id)
    }

    /// The output streams of the Firefox process `pid` (the audio of every page it
    /// runs comes out of its main process).
    pub fn streams_of(&self, pid: u32) -> Vec<u64> {
        let pid = pid.to_string();
        self.nodes()
            .filter(|(_, p, _)| {
                prop(p, "media.class").as_deref() == Some("Stream/Output/Audio")
                    && prop(p, "application.process.id").as_deref() == Some(pid.as_str())
            })
            .map(|(id, _, _)| id)
            .collect()
    }

    /// The sink a stream of `pid` is linked to: (node id, node name).
    pub fn linked_sink(&self, pid: u32) -> Option<(u64, String)> {
        let streams = self.streams_of(pid);
        let target = self.0.iter().find_map(|o| {
            if o.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Link") {
                return None;
            }
            let info = o.get("info")?;
            let out = info.get("output-node-id")?.as_u64()?;
            streams.contains(&out).then(|| info.get("input-node-id")?.as_u64()).flatten()
        })?;
        self.sinks()
            .find(|(id, _, _)| *id == target)
            .and_then(|(id, p, _)| Some((id, prop(p, "node.name")?)))
    }

    /// A sink's state ("running", "idle", "suspended") and the rate it runs at.
    pub fn sink_state(&self, name: &str) -> Option<(String, u32)> {
        let (_, _, node) = self.sinks().find(|(_, p, _)| prop(p, "node.name").as_deref() == Some(name))?;
        let state = node.pointer("/info/state").and_then(Value::as_str).unwrap_or("").to_string();
        let rate = node.pointer("/info/params/Format/0/rate").and_then(Value::as_u64).unwrap_or(0) as u32;
        Some((state, rate))
    }

    /// The PipeWire sink for an output device as ZBQ's audio settings name it: a
    /// PipeWire node name, an ALSA device ("hw:1,0", "plughw:1", "hw:CARD=Headset,DEV=0",
    /// "front:1", ...), or an ALSA path.
    pub fn resolve_sink(&self, device: &str) -> Option<String> {
        let name_of = |p: &Map<String, Value>| prop(p, "node.name");
        if let Some(name) = self.sinks().find(|(_, p, _)| name_of(p).as_deref() == Some(device)).and_then(|(_, p, _)| name_of(p)) {
            return Some(name);
        }
        if let Some(name) = self
            .sinks()
            .find(|(_, p, _)| prop(p, "api.alsa.path").as_deref() == Some(device))
            .and_then(|(_, p, _)| name_of(p))
        {
            return Some(name);
        }
        let (_, rest) = device.split_once(':')?;
        let mut card: Option<String> = None;
        let mut card_id: Option<String> = None;
        let mut dev: Option<String> = None;
        for (i, part) in rest.split(',').enumerate() {
            let part = part.trim();
            if let Some(v) = part.strip_prefix("CARD=") {
                if v.chars().all(|c| c.is_ascii_digit()) {
                    card = Some(v.to_string());
                } else {
                    card_id = Some(v.to_string());
                }
            } else if let Some(v) = part.strip_prefix("DEV=") {
                dev = Some(v.to_string());
            } else if i == 0 && part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                card = Some(part.to_string());
            } else if i == 1 && part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                dev = Some(part.to_string());
            }
        }
        if card.is_none() && card_id.is_none() {
            return None;
        }
        let matches = |p: &Map<String, Value>| {
            card.as_ref().map_or(true, |c| prop(p, "alsa.card").as_deref() == Some(c.as_str()))
                && card_id.as_ref().map_or(true, |c| prop(p, "alsa.id").as_deref() == Some(c.as_str()))
        };
        let with_dev = |p: &Map<String, Value>| dev.as_ref().map_or(true, |d| prop(p, "alsa.device").as_deref() == Some(d.as_str()));
        self.sinks()
            .find(|(_, p, _)| matches(p) && with_dev(p))
            .or_else(|| self.sinks().find(|(_, p, _)| matches(p)))
            .and_then(|(_, p, _)| name_of(p))
    }
}

/// The system's default sink (`default.audio.sink`).
pub fn default_sink() -> Option<String> {
    let out = Command::new("pw-metadata").args(["-n", "default", "0", "default.audio.sink"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find("value:'")? + "value:'".len();
    let end = text[start..].find('\'')? + start;
    let value: Value = serde_json::from_str(&text[start..end]).ok()?;
    value.get("name").and_then(Value::as_str).map(str::to_string)
}

/// A sink's volume as the desktop's volume controls show it (1.0 = 100%).
pub fn volume(sink_id: u64) -> Option<f32> {
    let out = Command::new("wpctl").args(["get-volume", &sink_id.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).split_whitespace().nth(1)?.parse().ok()
}

pub fn set_volume(sink_id: u64, value: f32) -> bool {
    Command::new("wpctl")
        .args(["set-volume", &sink_id.to_string(), &format!("{:.4}", value.clamp(0.0, 1.0))])
        .status()
        .is_ok_and(|s| s.success())
}

/// What a sink can play, from the device's real rates (`None` when they could not be
/// detected).
#[derive(Clone)]
pub struct DeviceRates {
    /// The highest Qobuz quality it plays without resampling: 27 (up to 192 kHz), 7
    /// (up to 96 kHz) or 6 (CD). The same tiers as the device cap (`qbz_app::device_cap`).
    pub format: u32,
    pub max_hz: u32,
    pub rates: Vec<u32>,
}

pub fn device_rates(sink: &str) -> Option<DeviceRates> {
    let caps = qbz_audio::query_dac_capabilities(sink);
    if !caps.detected {
        return None;
    }
    let max = caps.sample_rates.iter().copied().max()?;
    Some(DeviceRates {
        format: if max >= 192_000 { 27 } else if max >= 96_000 { 7 } else { 6 },
        max_hz: max,
        rates: caps.sample_rates,
    })
}

/// Qobuz format ids in quality order (5 MP3, 6 CD, 7 Hi-Res, 27 Hi-Res up to 192).
pub fn format_rank(id: u32) -> u32 {
    match id {
        5 => 0,
        6 => 1,
        7 => 2,
        27 => 3,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::Graph;

    const SINK: &str = r#"{ "id": 55, "type": "PipeWire:Interface:Node",
        "info": { "state": "running", "params": { "Format": [ { "rate": 96000 } ] },
                  "props": { "media.class": "Audio/Sink", "node.name": "headset", "alsa.card": 1 } } }"#;
    const STREAM: &str = r#"{ "id": 90, "type": "PipeWire:Interface:Node",
        "info": { "props": { "media.class": "Stream/Output/Audio", "application.process.id": "4242" } } }"#;
    const LINK: &str = r#"{ "id": 91, "type": "PipeWire:Interface:Link",
        "info": { "output-node-id": 90, "input-node-id": 55 } }"#;

    #[test]
    fn one_document() {
        let g = Graph::parse(format!("[{SINK},{STREAM},{LINK}]").as_bytes());
        assert_eq!(g.sink_id("headset"), Some(55));
        assert_eq!(g.streams_of(4242), vec![90]);
        assert_eq!(g.linked_sink(4242), Some((55, "headset".to_string())));
        assert_eq!(g.sink_state("headset"), Some(("running".to_string(), 96000)));
    }

    /// pw-dump appends a document for each change it saw while dumping. A
    /// whole-output parse used to fail on it and read as an empty graph.
    #[test]
    fn later_documents_update_and_remove() {
        let out = format!("[{SINK},{STREAM},{LINK}]\n[{{ \"id\": 91, \"info\": null }}]\n");
        let g = Graph::parse(out.as_bytes());
        assert_eq!(g.sink_id("headset"), Some(55));
        assert_eq!(g.streams_of(4242), vec![90]);
        assert_eq!(g.linked_sink(4242), None, "the removed link is gone");
    }

    #[test]
    fn garbage_is_an_empty_graph() {
        assert_eq!(Graph::parse(b"").sink_id("headset"), None);
        assert_eq!(Graph::parse(b"not json").sink_id("headset"), None);
        // A truncated second document keeps what the first one said.
        let g = Graph::parse(format!("[{SINK}]\n[{{ \"id\": ").as_bytes());
        assert_eq!(g.sink_id("headset"), Some(55));
    }
}
