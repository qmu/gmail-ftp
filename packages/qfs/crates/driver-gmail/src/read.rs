//! The Gmail **read composition** (t7): a `/mail/<label>` or `/mail/drafts` collection scan. Resolve
//! the path to a Gmail `q=` scope, list the matching message ids, and fetch each into the canonical
//! [`MailMessage`] rows. Pure-then-I/O over the mockable [`GmailClient`] — no vendor type crosses the
//! boundary, and the bearer never leaves the client. This is the read counterpart of the applier's
//! write leg; the binary's async `ReadDriver` adapter calls it (the same topology as the GitHub
//! driver's `read_rows`).

use qfs_types::{Predicate, RowBatch};

use crate::client::GmailClient;
use crate::error::GmailError;
use crate::path::MailPath;
use crate::schema::MailMessage;

/// The fan-out cap for a label scan — the engine residual applies the real `WHERE`/`LIMIT`.
const READ_CAP: u32 = 1_000;

/// Read a `/mail/<label>` or `/mail/drafts` collection into [`MailMessage`] rows. `predicate` is
/// reserved for a future Gmail `q=` pushdown (`from:`/`subject:`/`is:unread`); today the `WHERE`
/// stays a local residual the engine re-filters, so only the label/drafts scope is pushed.
///
/// # Errors
/// [`GmailError`] when the path is not a readable collection, or on an auth / transport / decode
/// failure from the client (secret-free, carrying the stable `code`).
pub fn read_rows(
    client: &dyn GmailClient,
    path: &str,
    _predicate: Option<&Predicate>,
) -> Result<RowBatch, GmailError> {
    let query = match MailPath::parse_str(path)? {
        MailPath::Label { name } => format!("label:{name}"),
        MailPath::Drafts => "in:draft".to_string(),
        _ => {
            return Err(GmailError::InvalidPath {
                path: path.to_string(),
                reason: "read a /mail/<label> or /mail/drafts collection",
            })
        }
    };
    let page = client.search_message_ids(&query, Some(READ_CAP))?;
    let mut rows = Vec::with_capacity(page.ids.len());
    for id in &page.ids {
        rows.push(client.get_message(id)?.to_row());
    }
    Ok(RowBatch::new(MailMessage::schema(), rows))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::client::{MessageIdPage, MockGmailClient};
    use qfs_types::Value;

    fn fixture_message(id: &str, subject: &str) -> MailMessage {
        MailMessage {
            id: id.to_string(),
            thread_id: "t1".to_string(),
            label_ids: vec!["INBOX".to_string()],
            date: 1_700_000_000,
            from: "alice@example.com".to_string(),
            subject: subject.to_string(),
            snippet: "preview".to_string(),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn reads_a_label_collection_into_message_rows() {
        let client = MockGmailClient::new()
            .with_search_page(MessageIdPage {
                ids: vec!["m1".to_string()],
                next_page_token: None,
            })
            .with_message(fixture_message("m1", "Invoice 42"));
        let batch = read_rows(&client, "/mail/INBOX", None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        let subj = batch
            .schema
            .columns
            .iter()
            .position(|c| c.name.as_str() == "subject")
            .expect("subject column");
        assert!(matches!(&batch.rows[0].values[subj], Value::Text(s) if s == "Invoice 42"));
    }

    #[test]
    fn a_message_node_is_not_a_collection_read() {
        let client = MockGmailClient::new();
        let err = read_rows(&client, "/mail/INBOX/18f1a2b3", None).unwrap_err();
        assert_eq!(err.code(), "invalid_path");
    }
}
