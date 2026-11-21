use std::sync::atomic::{AtomicU8, Ordering};

/// Supported application UI & diagnostic languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppLang {
    Id,
    En,
}

static ACTIVE_LANG: AtomicU8 = AtomicU8::new(0); // 0 = Id, 1 = En

impl AppLang {
    /// Parses `"en"` / `"EN"` into `AppLang::En`; everything else defaults to `AppLang::Id`.
    pub fn from_code(code: &str) -> Self {
        if code.trim().eq_ignore_ascii_case("en") {
            Self::En
        } else {
            Self::Id
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::En => "en",
        }
    }

    #[inline]
    pub fn is_en(self) -> bool {
        matches!(self, Self::En)
    }
}

pub fn current_lang() -> AppLang {
    match ACTIVE_LANG.load(Ordering::Relaxed) {
        1 => AppLang::En,
        _ => AppLang::Id,
    }
}

pub fn set_lang(lang: AppLang) {
    ACTIVE_LANG.store(if lang.is_en() { 1 } else { 0 }, Ordering::Relaxed);
}

#[inline]
pub fn is_en() -> bool {
    current_lang().is_en()
}

/// Pure selector given an explicit `AppLang` (used both internally and by deterministic tests).
#[inline]
pub fn tr_for<'a>(lang: AppLang, id: &'a str, en: &'a str) -> &'a str {
    if lang.is_en() { en } else { id }
}

/// Returns the Indonesian or English variant matching the active global `AppLang`.
#[inline]
pub fn tr<'a>(id: &'a str, en: &'a str) -> &'a str {
    tr_for(current_lang(), id, en)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codes_and_translates_deterministically() {
        assert_eq!(AppLang::from_code("id"), AppLang::Id);
        assert_eq!(AppLang::from_code("en"), AppLang::En);
        assert_eq!(AppLang::from_code("  EN "), AppLang::En);
        assert_eq!(AppLang::from_code("jp"), AppLang::Id);
        assert_eq!(AppLang::from_code(""), AppLang::Id);

        assert_eq!(AppLang::Id.code(), "id");
        assert_eq!(AppLang::En.code(), "en");

        assert_eq!(tr_for(AppLang::Id, "Siap", "Ready"), "Siap");
        assert_eq!(tr_for(AppLang::En, "Siap", "Ready"), "Ready");
    }
}
