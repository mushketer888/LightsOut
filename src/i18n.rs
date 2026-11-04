//! Localization: one flat TOML file per language under `locales/`, embedded at build time.
//!
//! To add a language: copy `locales/en.toml` to `locales/<code>.toml`, translate it,
//! and add one entry to `LANGUAGES`. The tests fail if any key or placeholder is missing.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use crate::core::Block;

pub struct Language {
    pub code: &'static str,
    /// Shown in the language picker, in the language itself.
    pub name: &'static str,
    source: &'static str,
}

pub const LANGUAGES: &[Language] = &[
    Language {
        code: "en",
        name: "English",
        source: include_str!("../locales/en.toml"),
    },
    Language {
        code: "ru",
        name: "Русский",
        source: include_str!("../locales/ru.toml"),
    },
];

/// Used when the system language is not supported or a key is missing.
pub const FALLBACK: &str = "en";

type Strings = HashMap<String, String>;

fn flatten(prefix: &str, table: &toml::Table, out: &mut Strings) {
    for (key, value) in table {
        let full = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::String(s) => {
                out.insert(full, s.clone());
            }
            toml::Value::Table(t) => flatten(&full, t, out),
            _ => {}
        }
    }
}

fn parse(source: &str) -> Strings {
    let mut out = Strings::new();
    if let Ok(table) = toml::from_str::<toml::Table>(source) {
        flatten("", &table, &mut out);
    }
    out
}

fn language(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.code == code)
}

/// Maps the `language` setting (`auto` or a code) and the system locale
/// (like `ru-RU`) to a supported language code.
pub fn resolve(setting: &str, system_locale: &str) -> &'static str {
    let wanted = if setting.eq_ignore_ascii_case("auto") {
        system_locale
    } else {
        setting
    }
    .to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|l| {
            wanted == l.code
                || wanted.starts_with(&format!("{}-", l.code))
                || wanted.starts_with(&format!("{}_", l.code))
        })
        .map_or(FALLBACK, |l| l.code)
}

#[derive(Clone)]
pub struct Translator {
    code: &'static str,
    strings: Arc<Strings>,
    fallback: Arc<Strings>,
}

impl Translator {
    /// Unknown codes fall back to English.
    pub fn new(code: &str) -> Self {
        let lang = language(code).or_else(|| language(FALLBACK));
        let fallback = language(FALLBACK)
            .map(|l| parse(l.source))
            .unwrap_or_default();
        let strings = lang.map(|l| parse(l.source)).unwrap_or_default();
        Self {
            code: lang.map_or(FALLBACK, |l| l.code),
            strings: Arc::new(strings),
            fallback: Arc::new(fallback),
        }
    }

    pub fn for_setting(setting: &str, system_locale: &str) -> Self {
        Self::new(resolve(setting, system_locale))
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The string for `key`; English if this language lacks it; the key itself as a last resort.
    pub fn t(&self, key: &str) -> String {
        self.strings
            .get(key)
            .or_else(|| self.fallback.get(key))
            .cloned()
            .unwrap_or_else(|| key.to_string())
    }

    /// Like `t`, replacing `{name}` placeholders.
    pub fn tr(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut text = self.t(key);
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    /// All strings under `prefix.` keyed by the remainder, for the settings page.
    pub fn section(&self, prefix: &str) -> BTreeMap<String, String> {
        let dotted = format!("{prefix}.");
        self.fallback
            .keys()
            .filter_map(|k| {
                k.strip_prefix(&dotted)
                    .map(|rest| (rest.to_string(), self.t(k)))
            })
            .collect()
    }

    pub fn describe_block(&self, block: &Block) -> String {
        match block {
            Block::Process(name) => self.tr("block.process", &[("name", name)]),
            Block::Fullscreen => self.t("block.fullscreen"),
            Block::Audio => self.t("block.audio"),
            Block::Cpu(percent) => self.tr("block.cpu", &[("percent", &percent.to_string())]),
            Block::Other(text) => text.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn keys(code: &str) -> BTreeSet<String> {
        parse(language(code).unwrap().source).into_keys().collect()
    }

    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let Some(len) = rest[start..].find('}') else {
                break;
            };
            found.insert(rest[start..start + len + 1].to_string());
            rest = &rest[start + len + 1..];
        }
        found
    }

    #[test]
    fn every_language_file_parses_and_is_not_empty() {
        for l in LANGUAGES {
            assert!(
                toml::from_str::<toml::Table>(l.source).is_ok(),
                "{} is invalid TOML",
                l.code
            );
            assert!(!parse(l.source).is_empty(), "{} is empty", l.code);
        }
    }

    #[test]
    fn all_languages_have_exactly_the_english_keys() {
        let en = keys(FALLBACK);
        for l in LANGUAGES {
            let have = keys(l.code);
            let missing: Vec<_> = en.difference(&have).collect();
            let extra: Vec<_> = have.difference(&en).collect();
            assert!(missing.is_empty(), "{}: missing keys {missing:?}", l.code);
            assert!(extra.is_empty(), "{}: unknown keys {extra:?}", l.code);
        }
    }

    #[test]
    fn no_empty_values() {
        for l in LANGUAGES {
            for (k, v) in parse(l.source) {
                assert!(!v.trim().is_empty(), "{}: empty value for {k}", l.code);
            }
        }
    }

    #[test]
    fn placeholders_match_across_languages() {
        let en = parse(language(FALLBACK).unwrap().source);
        for l in LANGUAGES {
            for (k, v) in parse(l.source) {
                assert_eq!(
                    placeholders(&v),
                    placeholders(&en[&k]),
                    "{}: placeholders of {k}",
                    l.code
                );
            }
        }
    }

    #[test]
    fn language_codes_are_unique_and_english_exists() {
        let codes: BTreeSet<_> = LANGUAGES.iter().map(|l| l.code).collect();
        assert_eq!(codes.len(), LANGUAGES.len());
        assert!(codes.contains(FALLBACK));
    }

    #[test]
    fn resolve_auto_follows_system_locale() {
        assert_eq!(resolve("auto", "ru-RU"), "ru");
        assert_eq!(resolve("AUTO", "ru_RU"), "ru");
        assert_eq!(resolve("auto", "en-US"), "en");
        assert_eq!(resolve("auto", "de-DE"), "en");
        assert_eq!(resolve("auto", ""), "en");
    }

    #[test]
    fn resolve_explicit_setting_wins_over_system() {
        assert_eq!(resolve("en", "ru-RU"), "en");
        assert_eq!(resolve("ru", "en-US"), "ru");
        assert_eq!(resolve("RU", "en-US"), "ru");
        // Unsupported code: English, not a crash.
        assert_eq!(resolve("klingon", "ru-RU"), "en");
    }

    #[test]
    fn translator_picks_language_and_falls_back() {
        assert_eq!(Translator::new("ru").code(), "ru");
        assert_eq!(Translator::new("xx").code(), "en");
        assert_eq!(Translator::new("en").t("menu.quit"), "Quit");
        assert_eq!(Translator::new("ru").t("menu.quit"), "Выход");
    }

    #[test]
    fn missing_key_in_language_uses_english_then_the_key() {
        let mut ru = Translator::new("ru");
        let mut strings = (*ru.strings).clone();
        strings.remove("menu.quit");
        ru.strings = Arc::new(strings);
        assert_eq!(ru.t("menu.quit"), "Quit");
        assert_eq!(ru.t("no.such.key"), "no.such.key");
    }

    #[test]
    fn placeholders_are_substituted() {
        let en = Translator::new("en");
        assert_eq!(
            en.tr("menu.about", &[("version", "1.2.3")]),
            "About (v1.2.3)"
        );
        let ru = Translator::new("ru");
        assert_eq!(
            ru.tr("toast.warning", &[("action", "сон"), ("secs", "60")]),
            "Нет активности: сон через 60 с. Пошевели мышкой, чтобы отменить."
        );
    }

    #[test]
    fn section_collects_settings_strings() {
        let s = Translator::new("ru").section("settings");
        assert_eq!(s["save"], "Сохранить");
        assert!(s.contains_key("processes_hint"));
        assert!(!s.contains_key("menu.quit"));
    }

    #[test]
    fn describe_block_covers_every_variant_in_every_language() {
        for l in LANGUAGES {
            let tr = Translator::new(l.code);
            for block in [
                Block::Process("ffmpeg.exe".into()),
                Block::Fullscreen,
                Block::Audio,
                Block::Cpu(42),
                Block::Other("custom".into()),
            ] {
                let text = tr.describe_block(&block);
                assert!(!text.is_empty());
                assert!(
                    !text.contains('{'),
                    "{}: unresolved placeholder in {text}",
                    l.code
                );
            }
        }
        assert_eq!(
            Translator::new("en").describe_block(&Block::Cpu(42)),
            "CPU 42%"
        );
    }
}
