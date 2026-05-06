//! Subcommand implementations. Each command lives in its own module so the
//! flow for "what does `cargo burst build` actually do?" is locatable from
//! one file.

pub mod audit;
pub mod bench;
pub mod build;
pub mod check;
pub mod clippy;
pub mod down;
pub mod image;
pub mod install;
pub mod reap;
pub mod remote;
pub mod status;
pub mod test;
