//! The static namespace → [`Schema`] map powering `DESCRIBE` (RFD-0001 §5). Each of the eight
//! namespaces is an [`Archetype::ObjectGraphWorkflow`] node whose typed columns come from the
//! owned DTO in [`crate::dto`], so `DESCRIBE /github/{owner}/{repo}/<ns>` and type-checking agree
//! on the same canonical `qfs_types::Schema`.

use qfs_types::Schema;

use crate::dto::{
    BranchDto, CommentDto, FileMetaDto, IssueDto, PullDto, ReleaseDto, ReviewDto, RunDto,
};
use crate::path::Namespace;

/// The canonical typed [`Schema`] a namespace's rows conform to (the `DESCRIBE` columns). The
/// effective namespace (sub-collection if present, else top-level) selects the schema — so
/// `issues/123/comments` describes with the `comments` schema.
#[must_use]
pub fn schema_for(namespace: Namespace) -> Schema {
    match namespace {
        Namespace::Issues => IssueDto::schema(),
        Namespace::Pulls => PullDto::schema(),
        Namespace::Comments => CommentDto::schema(),
        Namespace::Reviews => ReviewDto::schema(),
        Namespace::Runs => RunDto::schema(),
        Namespace::Releases => ReleaseDto::schema(),
        Namespace::Files => FileMetaDto::schema(),
        Namespace::Branches => BranchDto::schema(),
    }
}
