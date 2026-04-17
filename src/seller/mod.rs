//! Seller panel — recurring Discord auto-poster with auto-rendered item
//! screenshots.
//!
//! High-level flow:
//!   1. The web panel submits a [`StartRequest`] containing a user token,
//!      message, channel list, and up to 10 inventory / auction-house
//!      selections.
//!   2. [`SellerRunner::start`] validates the token + channels, resolves each
//!      selection through the live bot caches, renders one PNG per item via
//!      [`render::render_item_png`], then spawns a sender task per channel.
//!   3. Each sender task posts the PNGs + message to its channel on a fixed
//!      cooldown, continuing until [`SellerRunner::stop`] is called.
//!
//! Persistent user settings live in [`SellerConfig`] (JSON on disk — same
//! shape as the original standalone Python tool for easy migration).

pub mod config;
pub mod discord;
pub mod login;
pub mod pricing;
pub mod render;
pub mod runner;

pub use config::{
    mask_token, SelectedItem, SellerChannel, SellerConfig, DEFAULT_COOLDOWN_SECONDS,
    MAX_SELECTED_ITEMS, MIN_COOLDOWN_SECONDS,
};
pub use runner::{LogLevel, SellerLog, SellerRunner, SellerStatus, StartRequest};
