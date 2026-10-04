//! Translations: `locales/<lang>.json` (flat keys, `%{name}` placeholders,
//! `_one`/`_other` plurals), loaded with `rust-i18n`. See `docs/I18N.md`.
//!
//! Every user-visible text goes through [`t!`] (or [`tn!`] when it depends
//! on a count). Both return a [`gpui::SharedString`], so the result can be
//! used directly as an element child, a button label or a `format!` argument.
//!
//! The language is the one chosen in Settings, otherwise the system language
//! if there is a translation for it, otherwise English. Adding a language is
//! only adding its JSON file: [`available`] lists what `rust-i18n` loaded.

/// Translated text for `key` in the current language.
///
/// Same syntax as `rust_i18n::t!` (`t!("key", name = value)`,
/// `t!("key", locale = "es")`), but returns a [`gpui::SharedString`].
macro_rules! t {
    ($($all:tt)*) => {
        ::gpui::SharedString::from(::rust_i18n::t!($($all)*))
    };
}

/// Translated text that depends on a count: picks `key_one`, `key_other`
/// (or the other CLDR categories the language has) and passes the count as
/// the `%{count}` placeholder.
///
/// ```ignore
/// tn!("hosts.count", hosts.len())
/// tn!("sftp.deleted", n, name = file_name)
/// ```
macro_rules! tn {
    ($key:literal, $count:expr $(, $name:ident = $value:expr)* $(,)?) => {{
        let count = $count;
        let key = $crate::i18n::plural_key($key, count);
        ::gpui::SharedString::from(::rust_i18n::t!(key.as_str(), count = count $(, $name = $value)*))
    }};
}

/// Language used when nothing else matches (it has every key).
pub const DEFAULT: &str = "en";

/// Available languages (BCP 47 codes), sorted.
pub fn available() -> Vec<&'static str> {
    static LOCALES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let all = LOCALES.get_or_init(|| {
        let mut v: Vec<String> = rust_i18n::available_locales!()
            .into_iter()
            .map(|l| l.into_owned())
            .collect();
        v.sort_unstable();
        v
    });
    all.iter().map(String::as_str).collect()
}

/// Name of a language in that language (`English`, `Español`).
pub fn language_name(locale: &str) -> String {
    t!("language.name", locale = locale).to_string()
}

/// The available language that matches `tag` exactly (ignoring case and
/// `_` vs `-`), or by its primary subtag (`es-ES` → `es`).
pub fn supported(tag: &str) -> Option<&'static str> {
    // POSIX locales may carry an encoding or modifier (`es_ES.UTF-8@euro`).
    let tag = tag
        .split(['.', '@'])
        .next()
        .unwrap_or_default()
        .trim()
        .replace('_', "-");
    if tag.is_empty() {
        return None;
    }
    let all = available();
    if let Some(l) = all.iter().find(|l| l.eq_ignore_ascii_case(&tag)) {
        return Some(l);
    }
    let primary = tag.split('-').next().unwrap_or_default();
    all.into_iter().find(|l| l.eq_ignore_ascii_case(primary))
}

/// First system language that has a translation.
pub fn system() -> Option<&'static str> {
    sys_locale::get_locales().find_map(|l| supported(&l))
}

/// Language to use for a saved choice (`None` = follow the system).
pub fn resolve(choice: Option<&str>) -> &'static str {
    choice
        .and_then(supported)
        .or_else(system)
        .unwrap_or(DEFAULT)
}

/// Switches the interface to the language for `choice` and returns it.
/// Views pick it up on their next render.
pub fn apply(choice: Option<&str>) -> &'static str {
    let locale = resolve(choice);
    rust_i18n::set_locale(locale);
    locale
}

/// Current interface language.
pub fn current() -> String {
    rust_i18n::locale().to_string()
}

/// CLDR plural category of `n` in `locale` (only the rules of the languages
/// that need more than "one"/"other" for whole numbers are listed).
fn plural_category(locale: &str, n: u64) -> &'static str {
    let lang = locale.split('-').next().unwrap_or_default();
    match lang {
        // No plural forms.
        "ja" | "ko" | "zh" | "vi" | "th" | "id" | "ms" => "other",
        // 0 and 1 are singular.
        "fr" | "hi" | "fa" | "bn" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "pt" if locale != "pt-PT" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        "ru" | "uk" | "be" | "sr" | "hr" | "bs" => {
            let (m10, m100) = (n % 10, n % 100);
            if m10 == 1 && m100 != 11 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else {
                "many"
            }
        }
        "pl" => {
            let (m10, m100) = (n % 10, n % 100);
            if n == 1 {
                "one"
            } else if (2..=4).contains(&m10) && !(12..=14).contains(&m100) {
                "few"
            } else {
                "many"
            }
        }
        "cs" | "sk" => match n {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        _ => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// Key to use for `count` items: `key_<category>` if the current language
/// has it (`key_zero` is used for 0 when present), otherwise `key_other`.
pub fn plural_key<N: TryInto<u64>>(key: &str, count: N) -> String {
    let n = count.try_into().unwrap_or(u64::MAX);
    let locale = current();
    let has = |k: &str| crate::_rust_i18n_try_translate(&locale, k).is_some();
    if n == 0 {
        let zero = format!("{key}_zero");
        if has(&zero) {
            return zero;
        }
    }
    let exact = format!("{key}_{}", plural_category(&locale, n));
    if has(&exact) {
        exact
    } else {
        format!("{key}_other")
    }
}

/// Text of a server API error code (`error.<code>`, see `docs/API.md`), if
/// there is a translation for it.
pub fn api_error_text(code: &str) -> Option<String> {
    if code.is_empty() {
        return None;
    }
    let key = format!("error.{code}");
    crate::_rust_i18n_try_translate(&current(), &key).map(|s| s.into_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Map, Value};

    use super::*;

    fn locale_file(lang: &str) -> Map<String, Value> {
        let path = format!("{}/locales/{lang}.json", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn placeholders(s: &str) -> Vec<String> {
        let mut v: Vec<String> = s
            .split("%{")
            .skip(1)
            .filter_map(|p| p.split('}').next().map(str::to_string))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    #[test]
    fn matches_locales() {
        assert_eq!(supported("es"), Some("es"));
        assert_eq!(supported("ES-es"), Some("es"));
        assert_eq!(supported("es_ES.UTF-8"), Some("es"));
        assert_eq!(supported("en_GB"), Some("en"));
        assert_eq!(supported("xx"), None);
        assert_eq!(resolve(Some("es")), "es");
        assert_eq!(language_name("es"), "Español");
        assert_eq!(language_name("en"), "English");
        assert!(available().contains(&"en"));
    }

    #[test]
    fn plural_categories() {
        assert_eq!(plural_category("en", 1), "one");
        assert_eq!(plural_category("en", 0), "other");
        assert_eq!(plural_category("es", 2), "other");
        assert_eq!(plural_category("fr", 0), "one");
        assert_eq!(plural_category("ru", 22), "few");
        assert_eq!(plural_category("ru", 12), "many");
        assert_eq!(plural_category("ja", 1), "other");
    }

    /// Every language file is valid, has a name and keeps the placeholders
    /// of the English text. English and Spanish have exactly the same keys.
    #[test]
    fn locale_files_match_english() {
        let en = locale_file("en");
        for lang in available() {
            let other = locale_file(lang);
            assert!(
                other.get("language.name").and_then(Value::as_str).is_some(),
                "{lang}: missing language.name"
            );
            for (key, value) in &other {
                let text = value
                    .as_str()
                    .unwrap_or_else(|| panic!("{lang}: {key} is not a string"));
                let english = en
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{lang}: {key} is not in en.json"));
                // Plural forms may drop the count ("one file" / "un archivo").
                let is_plural = ["_zero", "_one", "_two", "_few", "_many", "_other"]
                    .iter()
                    .any(|s| key.ends_with(s));
                if is_plural {
                    let en_ph: BTreeSet<_> = placeholders(english).into_iter().collect();
                    let ph: BTreeSet<_> = placeholders(text).into_iter().collect();
                    assert!(
                        ph.iter().all(|p| p == "count" || en_ph.contains(p)),
                        "{lang}: unknown placeholder in {key}"
                    );
                } else {
                    assert_eq!(
                        placeholders(english),
                        placeholders(text),
                        "{lang}: placeholders of {key}"
                    );
                }
            }
        }
        let es = locale_file("es");
        let en_keys: BTreeSet<_> = en.keys().collect();
        let es_keys: BTreeSet<_> = es.keys().collect();
        assert_eq!(
            en_keys.difference(&es_keys).collect::<Vec<_>>(),
            Vec::<&&String>::new(),
            "keys missing in es.json"
        );
        assert_eq!(
            es_keys.difference(&en_keys).collect::<Vec<_>>(),
            Vec::<&&String>::new(),
            "keys missing in en.json"
        );
    }

    /// Keys used in the sources as `t!("...")` or `tn!("...")` (literal keys
    /// only; dynamic ones such as `error.<code>` are looked up at runtime).
    fn used_keys() -> Vec<(String, bool, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut keys = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (macro_name, plural) in [("t!(", false), ("tn!(", true)] {
                for (ix, _) in text.match_indices(macro_name) {
                    // `tn!(` is not `t!(`, nor is `print!(`, nor this scanner's
                    // own `"t!("`.
                    let prev = text[..ix].chars().next_back();
                    if prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"') {
                        continue;
                    }
                    // Examples in comments do not count.
                    let line_start = text[..ix].rfind('\n').map_or(0, |n| n + 1);
                    if text[line_start..ix].trim_start().starts_with("//") {
                        continue;
                    }
                    let args = text[ix + macro_name.len()..].trim_start();
                    let Some(args) = args.strip_prefix('"') else {
                        continue;
                    };
                    let Some(end) = args.find('"') else { continue };
                    keys.push((args[..end].to_string(), plural, file.display().to_string()));
                }
            }
        }
        keys
    }

    #[test]
    fn used_keys_exist_in_english() {
        let en = locale_file("en");
        let used = used_keys();
        assert!(used.len() > 100, "the source scan found too few keys");
        let mut missing = Vec::new();
        for (key, plural, file) in &used {
            let ok = if *plural {
                en.contains_key(&format!("{key}_one")) && en.contains_key(&format!("{key}_other"))
            } else {
                en.contains_key(key)
            };
            if !ok {
                missing.push(format!("{key} ({file})"));
            }
        }
        assert!(missing.is_empty(), "keys missing in en.json: {missing:#?}");
    }

    #[test]
    fn plural_keys() {
        // Tests never change the global locale, so this is English.
        assert_eq!(plural_key("common.items", 3usize), "common.items_other");
        assert_eq!(plural_key("common.items", 1u32), "common.items_one");
        assert_eq!(plural_key("common.items", 0i64), "common.items_other");
        assert_eq!(tn!("common.items", 2), "2 items");
        assert_eq!(
            t!("common.items_one", locale = "es", count = 1),
            "1 elemento"
        );
    }
}
