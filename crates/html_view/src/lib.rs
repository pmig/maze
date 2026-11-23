//! Maze's original HTML mode (layer L3, ADR-020): a message as its sender designed it,
//! rendered by an embedded Blitz engine (HTML and CSS, no JavaScript; ADR-033).
//!
//! - [`document`]: the page the engine gets, built from the sanitized HTML of the
//!   existing pipeline (PLAN §4.8) with the CSP and resources pointed at
//!   [`MESSAGE_ORIGIN`].
//! - [`policy`]: what may load (only that origin, answered from memory) and where a
//!   click goes (reported to the caller, which applies its link checks).
//! - [`OriginalView`]: the GPUI element. With the `blitz` feature it lays the message
//!   out with Blitz, paints it on the CPU and shows the frames as an image;
//!   without it ([`OriginalView::AVAILABLE`] is false) the message view keeps reader
//!   mode.
//!
//! `mail_ui` (also L3) depends on this crate; nothing else in L3 does.

pub mod document;
#[cfg(feature = "blitz")]
mod engine;
pub mod policy;
mod view;
#[cfg(all(test, feature = "blitz"))]
mod wpt;

pub use document::{DocumentOptions, MESSAGE_ORIGIN, OriginalDocument, PageColors, document_url};
pub use policy::{Located, Navigation, Resource, navigation_for, resource_for};
pub use view::{Fetch, OriginalView, OriginalViewEvent, Part, PartSource};
