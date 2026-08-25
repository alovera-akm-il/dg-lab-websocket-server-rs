//! Session presets / recipes: a named bundle of references to blocks
//! Features 1/2/3 (`templates`/`ramp`/`session`) already build -- Feature
//! 6 from `docs/dg-lab-panel-feature-requests.md`. Pure composition: a
//! recipe carries no device-control logic of its own, only a
//! [`SessionConfig`] and per-channel [`RampProfile`]/[`RecipePlaylist`]
//! references that `handler::post_recipe_start` resolves and starts in
//! sequence, reusing the exact functions `POST /api/playlist/{channel}/
//! load-template`, `POST /api/ramp`, and `POST /api/session/timer`
//! already call. Persisted like templates/button-map, since a recipe is
//! exactly the kind of "build once, reuse every session" config those
//! are for.
//!
//! **Two deliberate scope trims from the original request:**
//! - **`buttonMap` isn't a field here.** The request's example recipe
//!   references a button map *by name* (`"buttonMap": "default-warmup"`),
//!   but Feature 5 as built has exactly one active button map, not a
//!   named collection to choose between -- there's nothing for a name
//!   reference to resolve against. Adding that would mean building
//!   "templates, but for button maps" as a new sub-feature, out of
//!   scope for what was asked here.
//! - **Saving a recipe takes an explicit body, not a live-state
//!   snapshot.** The request frames `POST /api/session/recipes/{name}`
//!   as "save current session config as recipe," mirroring how `POST
//!   /api/templates/{name}` captures a channel's *actual* live queue.
//!   But a recipe's `playlistA`/`playlistB` reference a template *by
//!   name*, and a live playlist queue has no such name once loaded --
//!   there's nothing to snapshot back into a name reference unless the
//!   queue happens to still exactly match a saved template. Treating
//!   the recipe body as explicitly authored (matching how `POST
//!   /api/button-map` and `POST /api/session/timer` already work) is
//!   the closest style already used, so it stays that way here.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::persistence;
use super::ramp::RampProfile;
use super::session::SessionConfig;

const FILE: &str = "recipes.json";

pub fn load_all() -> HashMap<String, Recipe> {
    persistence::load_json(FILE)
}

pub fn save_all(recipes: &HashMap<String, Recipe>) {
    persistence::save_json(FILE, recipes);
}

/// A channel's playlist as part of a recipe -- a *reference* to a saved
/// template plus a load-time `shuffle` override, the exact same shape
/// `POST /api/playlist/{channel}/load-template`'s body already uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecipePlaylist {
    pub template: String,
    #[serde(default)]
    pub shuffle: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recipe {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer: Option<SessionConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "playlistA")]
    pub playlist_a: Option<RecipePlaylist>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "playlistB")]
    pub playlist_b: Option<RecipePlaylist>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "rampA")]
    pub ramp_a: Option<RampProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "rampB")]
    pub ramp_b: Option<RampProfile>,
}

impl Recipe {
    /// Validates this recipe's own numbers (timer/ramp configs) in
    /// isolation. Doesn't check that referenced templates actually
    /// exist -- that needs live `PanelState` access, done separately by
    /// `handler::post_recipe_start` right before starting anything, so
    /// a missing template is caught before any part of the recipe runs
    /// rather than partway through.
    pub fn validate_self(&self) -> Result<(), String> {
        if let Some(timer) = &self.timer {
            timer.validate().map_err(|e| format!("timer: {e}"))?;
        }
        if let Some(profile) = &self.ramp_a {
            profile.validate().map_err(|e| format!("rampA: {e}"))?;
        }
        if let Some(profile) = &self.ramp_b {
            profile.validate().map_err(|e| format!("rampB: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::ramp::RampProfile;

    fn valid_recipe() -> Recipe {
        Recipe {
            name: "Mara's Standard Warmup".to_string(),
            timer: Some(SessionConfig {
                duration_seconds: 1800,
                check_in_every_seconds: 900,
                phase_gates: vec![],
                auto_stop_playlists_at_end: true,
            }),
            playlist_a: Some(RecipePlaylist {
                template: "warmup-steady".to_string(),
                shuffle: false,
            }),
            playlist_b: None,
            ramp_a: Some(RampProfile::Linear {
                from: 10,
                to: 30,
                over_seconds: 600,
            }),
            ramp_b: Some(RampProfile::Hold {
                value: 15,
                duration_seconds: 1800,
            }),
        }
    }

    #[test]
    fn a_fully_populated_recipe_round_trips_through_json_matching_the_request_shape() {
        let recipe = valid_recipe();
        let json_str = serde_json::to_string(&recipe).unwrap();
        assert!(json_str.contains("\"playlistA\""));
        assert!(json_str.contains("\"rampA\""));
        assert!(
            !json_str.contains("playlistB"),
            "omitted Option fields shouldn't round-trip as null noise"
        );

        let reparsed: Recipe = serde_json::from_str(&json_str).unwrap();
        assert_eq!(reparsed.name, recipe.name);
        assert!(reparsed.playlist_b.is_none());
        assert!(matches!(reparsed.ramp_a, Some(RampProfile::Linear { .. })));
    }

    #[test]
    fn recipe_with_no_optional_blocks_is_valid_but_does_nothing() {
        let recipe = Recipe {
            name: "empty".to_string(),
            ..Default::default()
        };
        assert!(recipe.validate_self().is_ok());
    }

    #[test]
    fn validate_self_surfaces_an_invalid_ramp_target() {
        let mut recipe = valid_recipe();
        recipe.ramp_a = Some(RampProfile::Linear {
            from: 10,
            to: 30,
            over_seconds: 0, // invalid -- must be at least 1
        });
        assert!(recipe.validate_self().is_err());
    }

    #[test]
    fn validate_self_surfaces_an_invalid_timer_duration() {
        let mut recipe = valid_recipe();
        recipe.timer = Some(SessionConfig {
            duration_seconds: 0,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        });
        assert!(recipe.validate_self().is_err());
    }
}
