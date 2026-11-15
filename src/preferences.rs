use crate::{Result, invalid, migration::atomic_json};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const LANGUAGES: [(&str, &str); 6] = [
    ("en", "English"),
    ("ru", "Русский"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("es", "Español"),
    ("it", "Italiano"),
];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Preferences {
    pub language: String,
    pub dark: bool,
    pub reduced_motion: bool,
    pub startup_updates: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: "en".into(),
            dark: false,
            reduced_motion: false,
            startup_updates: true,
        }
    }
}
impl Preferences {
    pub fn path() -> Result<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("XDG_CONFIG_HOME"))
            .ok_or_else(|| invalid("No per-user configuration directory is available."))?;
        Ok(PathBuf::from(base)
            .join("DiskMigrator")
            .join("preferences.json"))
    }
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let f = std::fs::File::open(path)?;
        if f.metadata()?.len() > 16 * 1024 {
            return Err(invalid("Preferences file is oversized."));
        }
        let mut p: Self = serde_json::from_reader(f)?;
        if !LANGUAGES.iter().any(|(c, _)| *c == p.language) {
            p.language = "en".into();
        }
        Ok(p)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(
            path.parent()
                .ok_or_else(|| invalid("Missing settings directory."))?,
        )?;
        atomic_json(path, &serde_json::to_vec_pretty(self)?)
    }
}
#[derive(Clone)]
pub struct Catalog {
    english: BTreeMap<String, String>,
    selected: BTreeMap<String, String>,
}
impl Catalog {
    pub fn new(language: &str) -> Self {
        let raw = match language {
            "ru" => include_str!("../locales/ru.json"),
            "fr" => include_str!("../locales/fr.json"),
            "de" => include_str!("../locales/de.json"),
            "es" => include_str!("../locales/es.json"),
            "it" => include_str!("../locales/it.json"),
            _ => include_str!("../locales/en.json"),
        };
        Self {
            english: serde_json::from_str(include_str!("../locales/en.json"))
                .expect("valid English catalog"),
            selected: serde_json::from_str(raw).expect("valid localization catalog"),
        }
    }
    pub fn text<'a>(&'a self, key: &'a str) -> &'a str {
        self.selected
            .get(key)
            .or_else(|| self.english.get(key))
            .map(String::as_str)
            .unwrap_or(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_roundtrip_and_defaults() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("prefs.json");
        let mut p = Preferences::load(&f).unwrap();
        assert!(p.startup_updates);
        p.language = "ru".into();
        p.dark = true;
        p.reduced_motion = true;
        p.startup_updates = false;
        p.save(&f).unwrap();
        assert_eq!(Preferences::load(&f).unwrap(), p);
    }
    #[test]
    fn all_catalogs_are_complete() {
        let en = Catalog::new("en");
        for (code, _) in LANGUAGES {
            let c = Catalog::new(code);
            assert_eq!(
                c.selected.keys().collect::<Vec<_>>(),
                en.english.keys().collect::<Vec<_>>(),
                "{code}"
            );
            assert!(c.selected.values().all(|s| !s.trim().is_empty()));
        }
    }
}
