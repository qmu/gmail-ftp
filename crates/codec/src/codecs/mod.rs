//! The builtin codecs (t15), one per module file (RFD §9 coding standard: one codec per
//! file under `codecs/`). Each is a zero-sized `Codec` impl mapping `bytes ↔ RowBatch`
//! through the shared [`crate::convert`] bridge, with vendor parser types confined to
//! the module (no leak across the `cfs-codec` boundary, RFD §9).
//!
//! [`builtin_codecs`] returns the canonical set `CodecRegistry::with_builtins()` loads.

mod csv;
mod json;
mod jsonl;
mod markdown;
mod toml;
mod yaml;

pub use csv::CsvCodec;
pub use json::JsonCodec;
pub use jsonl::JsonlCodec;
pub use markdown::MarkdownFrontmatterCodec;
pub use toml::TomlCodec;
pub use yaml::YamlCodec;

use std::sync::Arc;

use crate::Codec;

/// The six builtin codecs, as trait objects ready to `register` into a `CodecRegistry`
/// (`json`, `jsonl`, `yaml`, `toml`, `csv`, `md+frontmatter`). This is the single source
/// of truth `cfs-core`'s `CodecRegistry::with_builtins` consumes, so the builtin set is
/// defined in the codec crate (where the impls live) rather than duplicated in core.
#[must_use]
pub fn builtin_codecs() -> Vec<Arc<dyn Codec>> {
    vec![
        Arc::new(JsonCodec),
        Arc::new(JsonlCodec),
        Arc::new(YamlCodec),
        Arc::new(TomlCodec),
        Arc::new(CsvCodec::default()),
        Arc::new(MarkdownFrontmatterCodec),
    ]
}
