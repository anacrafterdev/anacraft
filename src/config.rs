//! On-disk state, split by sensitivity.
//!
//! * `~/.config/anacraft/config.toml` — properties and their settings. Hand
//!   editable, safe to commit to a dotfile repo.
//! * `~/.anacraft/token.json` — OAuth tokens, written 0600.
//! * `~/.anacraft/license.json` — the subscription token and the last answer
//!   the lookup gave, written 0600: it names a Google account.
//!
//! Keeping them apart matters: people sync `~/.config` to public repos, and a
//! refresh token has no business travelling with it.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Where tokens and a user-supplied `client.json` live. Created on demand.
pub fn home() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not locate a home directory")?
        .join(".anacraft");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

/// `$XDG_CONFIG_HOME/anacraft` when set, else `~/.config/anacraft`.
pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(raw) if !raw.is_empty() => PathBuf::from(raw),
        _ => dirs::home_dir()
            .context("could not locate a home directory")?
            .join(".config"),
    };
    let dir = base.join("anacraft");
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Ok(dir)
}

/// One GA4 property and the settings that follow it around. Every setting is
/// optional: unset means "inherit the global default", which is what lets a
/// config hold a bare id and still work.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Property {
    /// Numeric GA4 property id, e.g. "397412345".
    pub id: String,
    /// Property name as GA reports it, cached to save an Admin API round trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short nickname shown in the TUI switcher in place of `name`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Palette for this property, so work and personal can look different.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Lookback window the dashboard opens with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    /// Seconds between report refreshes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<u64>,
    /// Seconds between realtime polls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_refresh: Option<u64>,
    /// What `craft watch` should treat as worth waking somebody for. Declared
    /// last because it is the only field that serialises as a TOML table, and
    /// a table written before a scalar swallows the scalar into it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watch: Option<Watch>,
}

/// Per-property thresholds for `craft watch`.
///
/// Every field is optional and the whole table may be absent, which is what
/// lets `craft watch` fire useful alerts on a config that has never heard of
/// it. Metric deviations are flattened into the same table rather than nested
/// under a `[threshold]` header, so tuning one metric is one line:
///
/// ```toml
/// [property.watch]
/// baseline_days = 28
/// users = 25
/// conversions = 40
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Watch {
    /// Days of history the baseline averages over. GA4 keeps 14 months of
    /// this data by default, and a longer window is steadier but slower to
    /// notice that a site's normal has genuinely moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_days: Option<u32>,
    /// A baseline below this never fires. On a site averaging four
    /// conversions a day, one quiet day is a 25% "drop" and means nothing —
    /// the floor is what keeps a small property from alerting every morning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_baseline: Option<f64>,
    /// Percentage deviation from the baseline that wakes somebody, keyed by
    /// metric — `users`, `sessions`, `views`, `conversions`, `bounce_rate`,
    /// `avg_session`, or the GA4 API name. Unknown keys are ignored rather
    /// than refused: a typo here should not stop the watch from running.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub thresholds: BTreeMap<String, f64>,
}

impl Watch {
    /// The threshold for a metric under either of the names it answers to.
    pub fn threshold(&self, key: &str, api: &str) -> Option<f64> {
        self.thresholds
            .get(key)
            .or_else(|| self.thresholds.get(api))
            .copied()
    }
}

impl Property {
    /// What to show in the switcher: nickname, else GA's name, else the id.
    pub fn display(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| format!("property {}", self.id))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Id of the property the dashboard opens on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// Palette used by any property that doesn't name its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Serialises as TOML `[[property]]` blocks.
    #[serde(default, rename = "property", skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<Property>,
    /// Set once an Anacraft subscription is active — the dashboard wears a star
    /// when it is. Written by `craft subscribe`, and refreshed on every
    /// dashboard and MCP launch from the record Stripe's webhook keeps in
    /// Supabase (see `license`). It is still only a cached answer in a config
    /// anybody can edit: a preference, not an entitlement.
    #[serde(default, skip_serializing_if = "is_false")]
    pub supporter: bool,
    /// Which of the three plans that subscription is on: `basic`, `pro` or
    /// `elite`. Kept beside `supporter` — the flag says the person has paid,
    /// this says for what — and written by the same lookup, so the two are
    /// never out of step for long. A subscription that predates plans has no
    /// tier, which reads as Basic ([`Config::tier`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<crate::license::Tier>,
}

/// Keeps `supporter = false` out of the written config, the way every other
/// unset field here stays out of it.
fn is_false(value: &bool) -> bool {
    !*value
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    /// The pre-0.4 location, kept only so `load` can migrate off it.
    fn legacy_path() -> Result<PathBuf> {
        Ok(home()?.join("config.json"))
    }

    pub fn load() -> Result<Config> {
        let path = Self::path()?;
        if path.exists() {
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            // A corrupt config should not brick the CLI; fall back to empty.
            return Ok(toml::from_str(&raw).unwrap_or_default());
        }

        // First run after upgrading: fold the old single-property JSON into the
        // new shape and write it out, so this only happens once.
        let migrated = Self::from_legacy()?;
        if !migrated.properties.is_empty() {
            migrated.save()?;
        }
        Ok(migrated)
    }

    /// Read `~/.anacraft/config.json` as a one-property config.
    fn from_legacy() -> Result<Config> {
        #[derive(Deserialize)]
        struct Legacy {
            property_id: Option<String>,
            property_name: Option<String>,
            theme: Option<String>,
        }

        let path = Self::legacy_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let raw = fs::read_to_string(&path)?;
        let Ok(old) = serde_json::from_str::<Legacy>(&raw) else {
            return Ok(Config::default());
        };

        let mut cfg = Config {
            active: old.property_id.clone(),
            theme: old.theme,
            properties: Vec::new(),
            // The pre-0.4 config predates subscriptions, so a migrated one
            // never carries the flag.
            supporter: false,
            tier: None,
        };
        if let Some(id) = old.property_id {
            cfg.properties.push(Property {
                id,
                name: old.property_name,
                ..Property::default()
            });
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let body = toml::to_string_pretty(self)?;
        let raw = format!(
            "# anacraft configuration\n\
             # `active` is the property every command reads. `craft use <id>` sets it,\n\
             # and so does quitting the dashboard on one \u{2014} Tab moves between them.\n\
             # Every key under [[property]] is optional and falls back to the global default.\n\n{body}"
        );
        fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&Property> {
        let id = normalize(id);
        self.properties.iter().find(|p| p.id == id)
    }

    /// The active property's entry, falling back to the first configured one so
    /// a hand-written config without `active` still opens.
    pub fn active_property(&self) -> Option<&Property> {
        self.active
            .as_deref()
            .and_then(|id| self.find(id))
            .or_else(|| self.properties.first())
    }

    /// Add or update an entry, preserving any settings already on it, and make
    /// it the one the dashboard opens on.
    pub fn upsert(&mut self, id: &str, name: Option<String>) -> &mut Property {
        let id = normalize(id);
        let index = match self.properties.iter().position(|p| p.id == id) {
            Some(i) => i,
            None => {
                self.properties.push(Property {
                    id: id.clone(),
                    ..Property::default()
                });
                self.properties.len() - 1
            }
        };
        if name.is_some() {
            self.properties[index].name = name;
        }
        self.active = Some(id);
        &mut self.properties[index]
    }

    /// Forget a property, and report whether it was there to forget.
    ///
    /// `active` cannot be left pointing at an entry that no longer exists, so
    /// it falls back to whatever remains — or to nothing, which puts the
    /// dashboard back on synthetic data rather than on an error.
    pub fn remove(&mut self, id: &str) -> bool {
        let id = normalize(id);
        let before = self.properties.len();
        self.properties.retain(|p| p.id != id);
        let removed = self.properties.len() != before;

        if self.active.as_deref() == Some(id.as_str()) {
            self.active = self.properties.first().map(|p| p.id.clone());
        }
        removed
    }

    /// Resolve the property to query: explicit `--property` wins, then
    /// `ANACRAFT_PROPERTY_ID`, then whatever is active on disk.
    pub fn resolve_property(&self, flag: Option<&str>) -> Result<String> {
        if let Some(p) = flag {
            return Ok(normalize(p));
        }
        if let Ok(p) = std::env::var("ANACRAFT_PROPERTY_ID") {
            if !p.trim().is_empty() {
                return Ok(normalize(&p));
            }
        }
        self.active_property()
            .map(|p| p.id.clone())
            .context("no property selected — run `craft props` to pick one")
    }

    /// A property's watch settings, if it named any.
    pub fn watch_for(&self, id: &str) -> Option<&Watch> {
        self.find(id).and_then(|p| p.watch.as_ref())
    }

    /// The plan this machine believes it is on, off the cached answer.
    ///
    /// `None` unless something has actually been paid for. An active flag with
    /// no tier named is Basic — the $2.99 plan is the one sold since the start,
    /// so that is the only honest reading of a config that predates plans.
    pub fn tier(&self) -> Option<crate::license::Tier> {
        if !self.supporter {
            return None;
        }
        self.tier.or(Some(crate::license::Tier::Basic))
    }

    /// Palette for a property: its own, else the global default.
    pub fn theme_for(&self, id: &str) -> Option<&str> {
        self.find(id)
            .and_then(|p| p.theme.as_deref())
            .or(self.theme.as_deref())
    }
}

/// Accept `123456`, `properties/123456`, or a pasted GA URL fragment.
pub fn normalize(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("properties/")
        .trim()
        .to_string()
}

/// Write a file readable only by the current user. On Windows we fall back to
/// a plain write, since ACL handling there is out of scope.
pub fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgetting_a_property_moves_active_off_it() {
        let mut cfg = Config {
            active: Some("111".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    ..Property::default()
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
            ..Config::default()
        };

        assert!(cfg.remove("properties/111"));
        assert_eq!(cfg.properties.len(), 1);
        // Left pointing at 111 the dashboard would open on a property that is
        // no longer configured.
        assert_eq!(cfg.active.as_deref(), Some("222"));

        // Forgetting the last one is not an error, and leaves nothing active —
        // which is how the dashboard falls back to synthetic data.
        assert!(cfg.remove("222"));
        assert!(cfg.properties.is_empty());
        assert_eq!(cfg.active, None);

        // Forgetting something that was never there says so.
        assert!(!cfg.remove("333"));
    }

    #[test]
    fn forgetting_a_property_leaves_a_different_active_alone() {
        let mut cfg = Config {
            active: Some("222".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    ..Property::default()
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
            ..Config::default()
        };
        assert!(cfg.remove("111"));
        assert_eq!(cfg.active.as_deref(), Some("222"));
    }

    #[test]
    fn normalize_strips_the_properties_prefix() {
        assert_eq!(normalize(" properties/397412345 "), "397412345");
        assert_eq!(normalize("397412345"), "397412345");
    }

    #[test]
    fn round_trips_through_toml() {
        let cfg = Config {
            active: Some("111".into()),
            theme: Some("osaka-jade".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    name: Some("anacraft.dev".into()),
                    label: Some("site".into()),
                    theme: Some("catppuccin".into()),
                    days: Some(14),
                    refresh: Some(60),
                    live_refresh: Some(5),
                    watch: None,
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
            ..Config::default()
        };
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert_eq!(back.active.as_deref(), Some("111"));
        assert_eq!(back.properties, cfg.properties);
    }

    #[test]
    fn a_bare_property_inherits_the_global_theme() {
        let cfg = Config {
            active: Some("222".into()),
            theme: Some("osaka-jade".into()),
            properties: vec![
                Property {
                    id: "111".into(),
                    theme: Some("catppuccin".into()),
                    ..Property::default()
                },
                Property {
                    id: "222".into(),
                    ..Property::default()
                },
            ],
            ..Config::default()
        };
        assert_eq!(cfg.theme_for("111"), Some("catppuccin"));
        assert_eq!(cfg.theme_for("222"), Some("osaka-jade"));
    }

    #[test]
    fn active_falls_back_to_the_first_entry() {
        // A hand-written config need not name an active property.
        let cfg = Config {
            active: None,
            theme: None,
            properties: vec![Property {
                id: "999".into(),
                ..Property::default()
            }],
            ..Config::default()
        };
        assert_eq!(cfg.resolve_property(None).unwrap(), "999");
    }

    #[test]
    fn upsert_keeps_existing_settings() {
        let mut cfg = Config::default();
        cfg.upsert("111", Some("First".into())).theme = Some("catppuccin".into());
        // Re-selecting the property must not wipe the theme set on it.
        cfg.upsert("properties/111", Some("Renamed".into()));

        assert_eq!(cfg.properties.len(), 1);
        assert_eq!(cfg.properties[0].theme.as_deref(), Some("catppuccin"));
        assert_eq!(cfg.properties[0].name.as_deref(), Some("Renamed"));
        assert_eq!(cfg.active.as_deref(), Some("111"));
    }

    #[test]
    fn display_prefers_the_label() {
        let mut p = Property {
            id: "111".into(),
            ..Property::default()
        };
        assert_eq!(p.display(), "property 111");
        p.name = Some("anacraft.dev".into());
        assert_eq!(p.display(), "anacraft.dev");
        p.label = Some("site".into());
        assert_eq!(p.display(), "site");
    }

    /// The flattened threshold table is the shape documented in the README, so
    /// it is worth a test that reads exactly what a user would type.
    #[test]
    fn watch_thresholds_sit_flat_in_the_property_table() {
        let raw = r#"
active = "552157097"

[[property]]
id = "552157097"
name = "anacraft"

[property.watch]
baseline_days = 28
min_baseline = 10
users = 25
conversions = 40
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let watch = cfg.watch_for("552157097").expect("watch table");

        assert_eq!(watch.baseline_days, Some(28));
        assert_eq!(watch.min_baseline, Some(10.0));
        assert_eq!(watch.threshold("users", "totalUsers"), Some(25.0));
        assert_eq!(watch.threshold("conversions", "keyEvents"), Some(40.0));
        // Not named, so the metric falls back to its default.
        assert_eq!(watch.threshold("sessions", "sessions"), None);
    }

    /// A threshold may be written under the GA4 API name instead.
    #[test]
    fn a_threshold_answers_to_the_api_name_too() {
        let raw = r#"
[[property]]
id = "1"

[property.watch]
bounceRate = 15
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let watch = cfg.watch_for("1").unwrap();
        assert_eq!(watch.threshold("bounce_rate", "bounceRate"), Some(15.0));
    }

    /// `save` writes what `load` reads. The risk this covers is TOML's rule
    /// that a table ends the scalars around it: if `watch` serialised before
    /// `Property`'s own keys, `name` and `theme` would land inside it.
    #[test]
    fn a_watch_table_survives_a_save_and_reload() {
        let mut thresholds = BTreeMap::new();
        thresholds.insert("users".to_string(), 25.0);

        let cfg = Config {
            active: Some("1".into()),
            properties: vec![Property {
                id: "1".into(),
                name: Some("anacraft".into()),
                theme: Some("github".into()),
                days: Some(14),
                watch: Some(Watch {
                    baseline_days: Some(28),
                    min_baseline: None,
                    thresholds,
                }),
                ..Property::default()
            }],
            ..Config::default()
        };

        let written = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&written).unwrap();

        assert_eq!(back.properties, cfg.properties, "\n{written}");
    }
}
