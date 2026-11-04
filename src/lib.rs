pub mod config;
pub mod core;
pub mod i18n;
pub mod logs;
pub mod version;

#[cfg(windows)]
pub mod about;
#[cfg(windows)]
pub mod app;
#[cfg(windows)]
pub mod platform;
#[cfg(windows)]
pub mod updates;
