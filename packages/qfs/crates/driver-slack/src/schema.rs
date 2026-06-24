//! The static node-kind → ([`Archetype`], [`Schema`]) map powering `DESCRIBE` (RFD-0001 §5). This
//! is the **multi-archetype** heart of the Slack driver: messages/replies/reactions/dms are
//! [`Archetype::AppendLog`], files is [`Archetype::BlobNamespace`], and users is
//! [`Archetype::RelationalTable`] — each with a typed [`Schema`] from the owned DTO in
//! [`crate::dto`], so `DESCRIBE /slack/<ws>/...` and type-checking agree on the same canonical
//! `qfs_types::Schema`.

use qfs_driver::Archetype;
use qfs_types::Schema;

use crate::dto::{FileDto, MessageDto, ReactionDto, UserDto};
use crate::path::NodeKind;

/// The archetype a node kind maps onto (the per-node archetype, RFD §5).
#[must_use]
pub const fn archetype_for(kind: NodeKind) -> Archetype {
    match kind {
        NodeKind::Messages | NodeKind::Replies | NodeKind::Reactions | NodeKind::Dms => {
            Archetype::AppendLog
        }
        NodeKind::Files => Archetype::BlobNamespace,
        NodeKind::Users => Archetype::RelationalTable,
    }
}

/// The canonical typed [`Schema`] a node kind's rows conform to (the `DESCRIBE` columns).
#[must_use]
pub fn schema_for(kind: NodeKind) -> Schema {
    match kind {
        // messages / replies / dms are all message logs → the message schema.
        NodeKind::Messages | NodeKind::Replies | NodeKind::Dms => MessageDto::schema(),
        NodeKind::Reactions => ReactionDto::schema(),
        NodeKind::Files => FileDto::schema(),
        NodeKind::Users => UserDto::schema(),
    }
}
