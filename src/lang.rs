//! Interface language: Italian on Italian systems, English everywhere else.
//! `SALVAGE_LANG=it` or `SALVAGE_LANG=en` overrides the system setting.

use std::sync::OnceLock;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

#[cfg(windows)]
fn system_is_italian() -> bool {
    // Low 10 bits of a LANGID are the primary language; 0x10 is Italian.
    (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == 0x10
}

#[cfg(not(windows))]
fn system_is_italian() -> bool {
    std::env::var("LANG").is_ok_and(|l| l.starts_with("it"))
}

pub fn italian() -> bool {
    static ITALIAN: OnceLock<bool> = OnceLock::new();
    *ITALIAN.get_or_init(|| match std::env::var("SALVAGE_LANG").as_deref() {
        Ok("it") => true,
        Ok("en") => false,
        _ => system_is_italian(),
    })
}

/// Pick the fixed string for the current language.
pub fn tr(en: &'static str, it: &'static str) -> &'static str {
    if italian() {
        it
    } else {
        en
    }
}

/// `format!` in the current language: English literal first, Italian second.
macro_rules! t {
    ($en:literal, $it:literal $(, $arg:expr)* $(,)?) => {
        if $crate::lang::italian() {
            format!($it $(, $arg)*)
        } else {
            format!($en $(, $arg)*)
        }
    };
}

pub fn branches(n: usize) -> String {
    if italian() || n == 1 {
        format!("{n} branch")
    } else {
        format!("{n} branches")
    }
}

pub fn repos(n: usize) -> String {
    if italian() || n == 1 {
        format!("{n} repo")
    } else {
        format!("{n} repos")
    }
}
