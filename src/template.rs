//! Pure `{{KEY}}` template substitution.
//!
//! [`fill_template`] replaces placeholders whose key is in `replacements`, and
//! auto-injects `{{BASE_PATH}}` (the Zona directory with backslashes doubled,
//! `C:\Zona` -> `C:\\Zona`, as ModOrganizer `.ini` files expect). Everything
//! else is left exactly as written.

use std::collections::BTreeMap;

use crate::error::Result;

/// Reserved placeholder key, always auto-injected.
pub const BASE_PATH_KEY: &str = "BASE_PATH";

/// Substitutes every `{{KEY}}` placeholder in `template`.
///
/// Keys found in `replacements` are replaced; `{{BASE_PATH}}` is replaced by
/// [`doubled_backslashes`] of `base_path` (taking precedence over any
/// `BASE_PATH` entry in `replacements`); anything else is left literally.
/// Substitution is single-pass, so a replacement value is never re-scanned.
pub fn fill_template(
    template: &str,
    replacements: &BTreeMap<String, String>,
    base_path: &str,
) -> Result<String> {
    let base_path = doubled_backslashes(base_path);

    let lookup = |key: &str| -> Option<&str> {
        if key == BASE_PATH_KEY {
            Some(base_path.as_str())
        } else {
            replacements.get(key).map(String::as_str)
        }
    };

    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find("{{") {
        // Text before the opening braces is copied verbatim.
        out.push_str(&rest[..open]);

        let after_open = &rest[open + 2..];
        match after_open.find("}}") {
            Some(close) => {
                let key = &after_open[..close];
                match lookup(key) {
                    Some(value) => out.push_str(value),
                    // Unknown placeholder: keep the whole `{{KEY}}` literally.
                    None => out.push_str(&rest[open..open + 2 + close + 2]),
                }
                rest = &after_open[close + 2..];
            }
            None => {
                // Unterminated `{{` with no closing `}}`: the rest is plain text.
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }

    out.push_str(rest);
    Ok(out)
}

/// Doubles every backslash in `s`: `C:\Zona` -> `C:\\Zona`.
pub fn doubled_backslashes(s: &str) -> String {
    s.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replacements(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn substitutes_profile_name_and_leaves_separators() {
        let template = "selected_profile=@ByteArray({{PROFILE_NAME}})\n";
        let map = replacements(&[("PROFILE_NAME", "ZONA V1.38 SSS23.5 (High Graphics)")]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(
            filled,
            "selected_profile=@ByteArray(ZONA V1.38 SSS23.5 (High Graphics))\n"
        );
    }

    #[test]
    fn injects_backslash_doubled_base_path() {
        // `gamePath=@ByteArray({{BASE_PATH}}\\anomaly)` from ModOrganizer.ini.template.
        let template = r"gamePath=@ByteArray({{BASE_PATH}}\\anomaly)";
        let map = replacements(&[]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(filled, r"gamePath=@ByteArray(C:\\Zona\\anomaly)");
    }

    #[test]
    fn base_path_is_used_without_a_replacements_entry() {
        // Forward-slash form used elsewhere in the real template.
        let template = "{{BASE_PATH}}/anomaly/bin/AnomalyDX11AVX.exe";
        let filled = fill_template(template, &BTreeMap::new(), r"D:\Zona").unwrap();
        assert_eq!(filled, r"D:\\Zona/anomaly/bin/AnomalyDX11AVX.exe");
    }

    #[test]
    fn no_placeholder_template_passes_through_unchanged() {
        let template = "[General]\ngameName=STALKER Anomaly\nfirst_start=false\n";
        let map = replacements(&[("PROFILE_NAME", "ignored")]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(filled, template);
    }

    #[test]
    fn unknown_placeholder_is_left_literal() {
        let template = "a={{KNOWN}} b={{UNKNOWN}} c={{}} d={{BASE_PATH_WRONG}}";
        let map = replacements(&[("KNOWN", "x")]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(filled, "a=x b={{UNKNOWN}} c={{}} d={{BASE_PATH_WRONG}}");
    }

    #[test]
    fn unterminated_placeholder_is_left_literal() {
        let template = "start {{OPEN and more text";
        let filled = fill_template(template, &BTreeMap::new(), r"C:\Zona").unwrap();
        assert_eq!(filled, template);
    }

    #[test]
    fn replacement_values_are_not_rescanned() {
        // A replaced value containing a placeholder must be emitted verbatim,
        // not substituted again.
        let template = "{{A}}/{{B}}";
        let map = replacements(&[("A", "{{B}}"), ("B", "bee")]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(filled, "{{B}}/bee");
    }

    #[test]
    fn base_path_auto_injection_wins_over_a_replacements_entry() {
        let template = "{{BASE_PATH}}";
        let map = replacements(&[("BASE_PATH", "not-this")]);

        let filled = fill_template(template, &map, r"C:\Zona").unwrap();

        assert_eq!(filled, r"C:\\Zona");
    }

    #[test]
    fn doubled_backslashes_doubles_each_backslash() {
        assert_eq!(doubled_backslashes(r"C:\Zona"), r"C:\\Zona");
        assert_eq!(
            doubled_backslashes(r"C:\Zona\installer\downloads"),
            r"C:\\Zona\\installer\\downloads"
        );
        assert_eq!(doubled_backslashes("no backslashes"), "no backslashes");
        assert_eq!(doubled_backslashes(""), "");
    }
}
