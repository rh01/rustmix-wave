//! Personal WeRead reader for this firmware.
//!
//! QR login, signed chapter fetches, content decoding, and progress upload follow
//! the web client behavior documented by eego-a4-weread (GPL-3.0) and
//! weread.koplugin (AGPL-3.0). Shelf, catalog, reading position, highlights, and
//! notes can also use the Apache-2.0 WeChatReading agent gateway. The algorithms
//! are reimplemented in this crate. Those source trees are not vendored.

pub mod bitmap;
pub mod body;
pub mod client;
pub mod crypto;
pub mod decode;
pub mod jsonutil;
pub mod limits;
pub mod nvs;
pub mod offline;
pub mod parse;
pub mod protocol;
pub mod qr;
pub mod session;
pub mod text;
pub mod ui;

#[cfg(target_os = "espidf")]
pub mod http;

pub use ui::WereadUi;
