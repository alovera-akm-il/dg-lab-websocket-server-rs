//! Named, reusable playlist definitions ("templates"): build a channel's
//! queue once, save it under a name, then load it into either channel
//! later without rebuilding it by hand. See
//! `docs/dg-lab-panel-feature-requests.md`'s "Named Playlist Templates"
//! section for the original request and design notes this implements.
//!
//! Stored server-side as one JSON file (`templates.json` under
//! `PANEL_DATA_DIR`, see [`super::persistence`]) so they survive a panel
//! restart -- unlike everything else in `PanelState`, which is
//! deliberately in-memory-only.
//!
//! A template's items deliberately don't carry ids: an id is queue-local
//! identity assigned fresh by `PlaylistQueue::add`/`load`, not portable
//! data, so loading the same template twice (or into both channels
//! independently) always gets its own fresh ids rather than colliding.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::persistence;
use super::playlist::{DurationSpec, EntryKind, PlaylistEntry};

const FILE: &str = "templates.json";

pub fn load_all() -> HashMap<String, Template> {
    persistence::load_json(FILE)
}

pub fn save_all(templates: &HashMap<String, Template>) {
    persistence::save_json(FILE, templates);
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum TemplateDuration {
    Fixed { seconds: u32 },
    Random { min: u32, max: u32 },
}

impl TemplateDuration {
    fn from_spec(spec: DurationSpec) -> Self {
        match spec {
            DurationSpec::Fixed(seconds) => TemplateDuration::Fixed { seconds },
            DurationSpec::Random { min, max } => TemplateDuration::Random { min, max },
        }
    }

    /// Same validation `handler::DurationBody::into_spec` applies to a
    /// freshly-POSTed queue item -- a template is just as capable of
    /// naming an invalid duration (especially if the file was hand-edited)
    /// as a live request body is, so it's checked the same way at load
    /// time rather than trusted just because it came from disk.
    fn into_spec(self) -> Result<DurationSpec, &'static str> {
        match self {
            TemplateDuration::Fixed { seconds } => {
                if seconds == 0 {
                    return Err("duration seconds must be at least 1");
                }
                Ok(DurationSpec::Fixed(seconds))
            }
            TemplateDuration::Random { min, max } => {
                if min == 0 || max == 0 {
                    return Err("duration seconds must be at least 1");
                }
                if min > max {
                    return Err("duration min must not exceed max");
                }
                Ok(DurationSpec::Random { min, max })
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TemplateItem {
    Pulse {
        waveform: String,
        duration: TemplateDuration,
    },
    Gap {
        duration: TemplateDuration,
    },
}

impl TemplateItem {
    fn from_entry(entry: &PlaylistEntry) -> Self {
        let duration = TemplateDuration::from_spec(entry.duration);
        match &entry.kind {
            EntryKind::Pulse { waveform } => TemplateItem::Pulse {
                waveform: waveform.clone(),
                duration,
            },
            EntryKind::Gap => TemplateItem::Gap { duration },
        }
    }

    fn into_parts(self) -> Result<(EntryKind, DurationSpec), &'static str> {
        match self {
            TemplateItem::Pulse { waveform, duration } => {
                if waveform.trim().is_empty() {
                    return Err("waveform must not be empty");
                }
                Ok((EntryKind::Pulse { waveform }, duration.into_spec()?))
            }
            TemplateItem::Gap { duration } => Ok((EntryKind::Gap, duration.into_spec()?)),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TemplateSettings {
    pub shuffle: bool,
    #[serde(rename = "loopPlayback")]
    pub loop_playback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub items: Vec<TemplateItem>,
    pub settings: TemplateSettings,
}

impl Template {
    /// Builds a template from a live channel's current queue contents --
    /// see `PanelState::playlist_entries_snapshot`/`template_save`.
    pub fn from_queue(
        name: String,
        entries: &[PlaylistEntry],
        shuffle: bool,
        loop_playback: bool,
    ) -> Self {
        Template {
            name,
            items: entries.iter().map(TemplateItem::from_entry).collect(),
            settings: TemplateSettings {
                shuffle,
                loop_playback,
            },
        }
    }

    /// Resolves every item into fresh `(EntryKind, DurationSpec)` pairs
    /// ready for `PlaylistQueue::load`. Fails (naming the offending
    /// item's index) if any item's stored data is invalid -- shouldn't
    /// happen for anything saved through `from_queue`, but a template
    /// file can be hand-edited, so this is checked rather than trusted.
    pub fn to_queue_items(&self) -> Result<Vec<(EntryKind, DurationSpec)>, String> {
        self.items
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, item)| item.into_parts().map_err(|e| format!("item {i}: {e}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(waveform: &str, duration: DurationSpec) -> PlaylistEntry {
        let mut q = super::super::playlist::PlaylistQueue::new();
        let id = q.add(
            EntryKind::Pulse {
                waveform: waveform.to_string(),
            },
            duration,
        );
        q.entries().iter().find(|e| e.id == id).unwrap().clone()
    }

    #[test]
    fn round_trips_entries_through_a_template_without_ids() {
        let entries = vec![
            entry("coyote-extrusion", DurationSpec::Fixed(20)),
            entry("coyote-climb", DurationSpec::Random { min: 8, max: 15 }),
        ];
        let template = Template::from_queue("Mara's Edge Test".to_string(), &entries, false, true);

        let json = serde_json::to_string(&template).unwrap();
        assert!(json.contains("\"mode\":\"fixed\""));
        assert!(
            !json.contains("\"id\""),
            "templates must not store entry ids"
        );

        let items = template.to_queue_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            (
                EntryKind::Pulse {
                    waveform: "coyote-extrusion".to_string()
                },
                DurationSpec::Fixed(20)
            )
        );
        assert_eq!(
            items[1],
            (
                EntryKind::Pulse {
                    waveform: "coyote-climb".to_string()
                },
                DurationSpec::Random { min: 8, max: 15 }
            )
        );
    }

    #[test]
    fn gap_entries_round_trip_too() {
        let mut q = super::super::playlist::PlaylistQueue::new();
        let id = q.add(EntryKind::Gap, DurationSpec::Fixed(5));
        let gap = q.entries().iter().find(|e| e.id == id).unwrap().clone();

        let template = Template::from_queue("Just a gap".to_string(), &[gap], false, false);
        let items = template.to_queue_items().unwrap();
        assert_eq!(items, vec![(EntryKind::Gap, DurationSpec::Fixed(5))]);
    }

    #[test]
    fn a_zero_second_duration_is_rejected_at_load_time() {
        let template = Template {
            name: "bad".to_string(),
            items: vec![TemplateItem::Gap {
                duration: TemplateDuration::Fixed { seconds: 0 },
            }],
            settings: TemplateSettings {
                shuffle: false,
                loop_playback: false,
            },
        };
        assert!(template.to_queue_items().is_err());
    }
}
