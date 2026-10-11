//! Display language (REQ-012, DES-014; same rules as nv): Japanese when the OS language is
//! Japanese, English otherwise; `NV_LANG=ja|en` overrides.

use std::sync::OnceLock;

static JAPANESE: OnceLock<bool> = OnceLock::new();

pub fn is_ja() -> bool {
    *JAPANESE.get_or_init(detect)
}

/// `tr!("日本語 {x}", "English {x}")`: pick the message for the display language.
#[macro_export]
macro_rules! tr {
    ($ja:literal, $en:literal $(,)?) => {
        if $crate::i18n::is_ja() { format!($ja) } else { format!($en) }
    };
    ($ja:literal, $en:literal, $($arg:tt)*) => {
        if $crate::i18n::is_ja() { format!($ja, $($arg)*) } else { format!($en, $($arg)*) }
    };
}

fn detect() -> bool {
    if let Some(v) = env_value("NV_LANG") {
        return is_ja_tag(&v);
    }
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = env_value(var) {
            // "C"/"POSIX" say nothing about the user's language.
            if v != "C" && v != "POSIX" && !v.starts_with("C.") {
                return is_ja_tag(&v);
            }
        }
    }
    os_language().is_some_and(|tag| is_ja_tag(&tag))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn is_ja_tag(tag: &str) -> bool {
    let t = tag.trim().trim_start_matches('"').to_ascii_lowercase();
    t == "ja" || t.starts_with("ja_") || t.starts_with("ja-") || t.starts_with("ja.")
}

#[cfg(target_os = "macos")]
fn os_language() -> Option<String> {
    // The first entry of the user's preferred languages, e.g. "ja-JP".
    let out = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLanguages"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_end_matches(','))
        .find(|l| !l.is_empty() && *l != "(" && *l != ")")
        .map(|l| l.trim_matches('"').to_string())
}

#[cfg(windows)]
fn os_language() -> Option<String> {
    // Primary language 0x11 is Japanese.
    let langid = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    Some(if langid & 0x3ff == 0x11 {
        "ja".into()
    } else {
        "en".into()
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
fn os_language() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_japanese_tags() {
        for t in ["ja", "ja_JP.UTF-8", "ja-JP", "JA_jp", "\"ja-JP\""] {
            assert!(is_ja_tag(t), "{t}");
        }
        for t in ["en", "en_US.UTF-8", "C", "jav", "fr-JA"] {
            assert!(!is_ja_tag(t), "{t}");
        }
    }
}
