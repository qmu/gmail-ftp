//! Export-target mapping for Google-native docs (RFD-0001 §5 — "no raw bytes for Google docs").
//!
//! Docs/Sheets/Slides/Drawings have **no downloadable bytes**; a read must *export* to a
//! concrete office/text MIME. This module is the pure mapping from a Google-native source MIME
//! to its default [`ExportTarget`] (the export MIME + a file suffix), plus an explicit override
//! resolver for a path suffix (`report.gdoc!pdf`) or `?export=pdf`. Non-native MIMEs pass
//! through as raw download. No I/O.

use crate::schema::GOOGLE_NATIVE_PREFIX;

/// A concrete export target — the MIME a Google-native doc is exported to plus a file suffix.
/// Owned, vendor-free.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ExportTarget {
    /// The export MIME type (the `mimeType` query param of `files.export`).
    pub mime: String,
    /// The conventional file suffix for the exported artifact (e.g. `docx`).
    pub suffix: String,
}

impl ExportTarget {
    /// Construct an export target.
    #[must_use]
    pub fn new(mime: impl Into<String>, suffix: impl Into<String>) -> Self {
        Self {
            mime: mime.into(),
            suffix: suffix.into(),
        }
    }
}

/// The default export target for a Google-native source MIME, or `None` for a non-native MIME
/// (which downloads raw). The mapping (RFD §5): Doc → docx, Sheet → xlsx, Slides → pptx,
/// Drawing → pdf.
#[must_use]
pub fn default_export_target(source_mime: &str) -> Option<ExportTarget> {
    if !source_mime.starts_with(GOOGLE_NATIVE_PREFIX) {
        return None;
    }
    let target = match source_mime {
        "application/vnd.google-apps.document" => ExportTarget::new(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        "application/vnd.google-apps.spreadsheet" => ExportTarget::new(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
        "application/vnd.google-apps.presentation" => ExportTarget::new(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
        "application/vnd.google-apps.drawing" => ExportTarget::new("application/pdf", "pdf"),
        // Other native types (forms/sites/etc.) export to PDF as a deterministic fallback.
        _ => ExportTarget::new("application/pdf", "pdf"),
    };
    Some(target)
}

/// Resolve an explicit export override from a suffix token (`pdf`, `txt`, `csv`, `docx`, …) —
/// the value after `!` on a path or `?export=`. Maps the well-known short tokens to their MIME;
/// an unknown token is treated as an already-concrete MIME (passed through verbatim).
#[must_use]
pub fn override_export_target(token: &str) -> ExportTarget {
    let mime = match token {
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "csv" => "text/csv",
        "html" => "text/html",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        other => other,
    };
    ExportTarget::new(mime, token)
}
