//! Cadenza Fluent i18n + locale resolver.
//!
//! Shared between `src-tauri` and `cadenza-cli` per
//! DESIGN-desktop-v2.md § "Internacionalização".

pub mod bundle;
pub mod locale;

pub use bundle::I18n;
pub use fluent_bundle::FluentArgs;
pub use locale::{resolve, LocaleSources};

pub const DEFAULT_LOCALE: &str = "en";
pub const PRIMARY_LOCALE: &str = "pt-BR";
pub const SUPPORTED_LOCALES: &[&str] = &["pt-BR", "en"];
