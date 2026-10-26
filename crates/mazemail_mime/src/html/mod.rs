//! The HTML pipeline (PLAN §4.8): sanitize, rewrite resources, build a CSP.
//!
//! ```text
//! message HTML ─ sanitize ─▶ SanitizedHtml (cacheable) ─ rewrite_resources(origin) ─▶ RewrittenHtml
//!                    └──────▶ ReaderDoc (reader mode, see crate::reader)
//! ```
//!
//! [`sanitize`] is independent of where the message is shown, so its output can be
//! cached. [`rewrite_resources`] then points `cid:` and remote URLs at one
//! [`ResourceOrigin`], and [`content_security_policy`] makes sure the browser loads
//! nothing else.

pub(crate) mod css;
pub(crate) mod dom;
mod nesting;
mod rewrite;
mod sanitize;
pub(crate) mod tracking;
pub(crate) mod urls;

use serde::{Deserialize, Serialize};

pub use rewrite::{
    InvalidResourceOrigin, ResourceOrigin, RewrittenHtml, content_security_policy,
    rewrite_for_embedding, rewrite_resources, standalone_document,
};
pub use sanitize::{SanitizeOptions, SanitizedHtml, sanitize};
pub use tracking::TrackerRules;

pub(crate) use sanitize::sanitize_to_dom;

use crate::reader::{ReaderDoc, dom_to_reader};

/// Sanitized HTML and its reader document, built from one parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedHtml {
    pub sanitized: SanitizedHtml,
    pub reader: ReaderDoc,
}

/// Sanitize message HTML and build the reader document in one go.
pub fn prepare(html: &str, options: &SanitizeOptions) -> PreparedHtml {
    let (dom, sanitized) = sanitize_to_dom(html, options);
    let reader = dom_to_reader(&dom);
    let root = dom.root();
    PreparedHtml {
        sanitized: SanitizedHtml {
            html: dom.serialize_children(root).trim().to_owned(),
            ..sanitized
        },
        reader,
    }
}
