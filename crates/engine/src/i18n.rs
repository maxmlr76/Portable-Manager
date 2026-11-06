//! The current UI language and the `tr!` macro. English is the source language;
//! engine messages reach the user, so they follow the language the app selected.
//! The command line never selects one and stays English.

use std::sync::atomic::{AtomicBool, Ordering};

static GERMAN: AtomicBool = AtomicBool::new(false);

pub fn set_german(on: bool) {
    GERMAN.store(on, Ordering::Relaxed);
}

pub fn is_german() -> bool {
    GERMAN.load(Ordering::Relaxed)
}

/// `tr!("English {x}", "Deutsch {x}", args…)`: format the text of the current language.
#[macro_export]
macro_rules! tr {
    ($en:literal, $de:literal $(, $($arg:tt)*)?) => {
        if $crate::i18n::is_german() {
            format!($de $(, $($arg)*)?)
        } else {
            format!($en $(, $($arg)*)?)
        }
    };
}
