//! The Gmail **read composition** (t7): a `/mail/<label>` or `/mail/drafts` collection scan. Resolve
//! the path to a Gmail `q=` scope, list the matching message ids, and fetch each into the canonical
//! [`MailMessage`] rows. Pure-then-I/O over the mockable [`GmailClient`] — no vendor type crosses the
//! boundary, and the bearer never leaves the client. This is the read counterpart of the applier's
//! write leg; the binary's async `ReadDriver` adapter calls it (the same topology as the GitHub
//! driver's `read_rows`).

use qfs_types::{Predicate, Row, RowBatch, Value};

use crate::client::GmailClient;
use crate::error::GmailError;
use crate::path::MailPath;
use crate::query::build_query;
use crate::schema::{attachment_read_schema, label_listing_schema, MailMessage};

/// The fan-out ceiling for a collection scan when the engine still re-filters locally — over-fetch,
/// then the residual `WHERE`/`LIMIT` narrows. A pushed `LIMIT` tightens the fetch below this only
/// when the whole predicate pushed down (see [`read_rows`]).
const READ_CAP: u32 = 1_000;

/// Read a `/mail/<label>` or `/mail/drafts` collection into [`MailMessage`] rows, pushing the
/// `WHERE` into Gmail's `q=` search where it can (`from:`/`to:`/`subject:`/`after:`/`is:unread`/the
/// `label:` scope) and the `LIMIT` into the fetch cap where it is safe to.
///
/// The engine still re-applies the exact `WHERE`/`LIMIT` locally (over-fetch then filter, RFD §6),
/// so Gmail's looser field operators never return wrong rows; the `q=` is a backend pre-filter that
/// narrows the fetch. The pushed `LIMIT` is applied to the fetch cap **only** when nothing is left
/// as a local residual — with a residual, a tight cap would under-fetch (drop rows that survive the
/// local filter), so the fetch stays at [`READ_CAP`] and the engine applies the `LIMIT`.
///
/// # Errors
/// [`GmailError`] when the path is not a readable collection, or on an auth / transport / decode
/// failure from the client (secret-free, carrying the stable `code`).
pub fn read_rows(
    client: &dyn GmailClient,
    path: &str,
    predicate: Option<&Predicate>,
    limit: Option<u64>,
) -> Result<RowBatch, GmailError> {
    // Single-node reads short-circuit the search/limit machinery: the mailbox ROOT lists labels
    // (gmail-ftp `ls /`), and a single MESSAGE node downloads that one message's row (gmail-ftp
    // `get`) — both advertised by `caps_for` (Root: Ls/Select; Message: Select) and now wired.
    let parsed = MailPath::parse_str(path)?;
    match &parsed {
        MailPath::Root => {
            let rows = client
                .list_labels()?
                .into_iter()
                .map(|name| Row::new(vec![Value::Text(name)]))
                .collect();
            return Ok(RowBatch::new(label_listing_schema(), rows));
        }
        MailPath::Message { id } => {
            let row = client.get_message(id)?.to_row();
            return Ok(RowBatch::new(MailMessage::schema(), vec![row]));
        }
        MailPath::Attachment {
            message,
            attachment,
        } => {
            // gmail-ftp `get id:att:<msg>:<att>`: the bytes come from attachments.get, the
            // filename/mime/size from the owning message's part metadata (the API's attachment
            // endpoint returns only size + data). One combined row: filename, mime, size, content.
            let msg = client.get_message(message)?;
            let meta = msg
                .attachments
                .iter()
                .find(|a| &a.attachment_id == attachment)
                .ok_or(GmailError::InvalidPath {
                    path: path.to_string(),
                    reason: "no attachment with that id on the message",
                })?;
            let bytes = client.get_attachment(message, attachment)?;
            let row = Row::new(vec![
                Value::Text(meta.filename.clone()),
                Value::Text(meta.mime.clone()),
                Value::Int(meta.size),
                Value::Bytes(bytes),
            ]);
            return Ok(RowBatch::new(attachment_read_schema(), vec![row]));
        }
        _ => {}
    }
    let pushdown = match parsed {
        MailPath::Label { name } => build_query(Some(&name), predicate),
        MailPath::Drafts => {
            // The drafts scope is `in:draft`, not a `label:` term — build the predicate query, then
            // prepend the scope.
            let mut pd = build_query(None, predicate);
            pd.query = if pd.query.is_empty() {
                "in:draft".to_string()
            } else {
                format!("in:draft {}", pd.query)
            };
            pd
        }
        _ => {
            return Err(GmailError::InvalidPath {
                path: path.to_string(),
                reason: "read a /mail/<label> or /mail/drafts collection",
            })
        }
    };
    // Push the planner LIMIT into the fetch cap only when the whole predicate pushed down (no
    // residual); otherwise over-fetch up to READ_CAP and let the engine apply the LIMIT after its
    // local re-filter. Either way READ_CAP is the hard ceiling.
    let cap = match (limit, &pushdown.residual) {
        (Some(n), None) => u32::try_from(n).unwrap_or(READ_CAP).clamp(1, READ_CAP),
        _ => READ_CAP,
    };
    let page = client.search_message_ids(&pushdown.query, Some(cap))?;
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
        let batch = read_rows(&client, "/mail/INBOX", None, None).unwrap();
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
    fn reads_the_root_label_listing() {
        // gmail-ftp `ls /` parity: the mailbox root lists labels as `name` rows.
        let client = MockGmailClient::new().with_labels(vec![
            "INBOX".to_string(),
            "SENT".to_string(),
            "Work".to_string(),
        ]);
        let batch = read_rows(&client, "/mail", None, None).unwrap();
        assert_eq!(batch.schema.columns.len(), 1);
        assert_eq!(batch.schema.columns[0].name.as_str(), "name");
        let names: Vec<_> = batch
            .rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(s) => s.clone(),
                other => panic!("label name must be text, got {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["INBOX", "SENT", "Work"]);
        assert!(
            client
                .recorded()
                .iter()
                .any(|c| matches!(c, crate::client::RecordedCall::ListLabels)),
            "the root listing calls labels.list"
        );
    }

    #[test]
    fn reads_a_single_message_node_into_one_row() {
        // gmail-ftp `get` parity: a single message node downloads that message's row (headers +
        // snippet + attachments-as-nested-entries) instead of erroring `invalid_path`.
        let client = MockGmailClient::new().with_message(fixture_message("18f1a2b3", "Invoice 42"));
        let batch = read_rows(&client, "/mail/INBOX/18f1a2b3", None, None).unwrap();
        assert_eq!(batch.rows.len(), 1, "a message node is exactly one row");
        let idx = |name: &str| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == name)
                .unwrap_or_else(|| panic!("column {name}"))
        };
        assert!(matches!(&batch.rows[0].values[idx("id")], Value::Text(s) if s == "18f1a2b3"));
        assert!(
            matches!(&batch.rows[0].values[idx("subject")], Value::Text(s) if s == "Invoice 42")
        );
        // The same path also resolves via the `id:` selector (label-independent addressing).
        let by_id = read_rows(&client, "id:18f1a2b3", None, None).unwrap();
        assert_eq!(by_id.rows.len(), 1);
    }

    #[test]
    fn reads_an_attachment_node_into_a_content_row() {
        // gmail-ftp `get id:att:<msg>:<att>` parity: the attachment node yields one row with the
        // filename/mime/size (from the message part) and the decoded `content` bytes.
        let mut msg = fixture_message("18f1a2b3", "Invoice 42");
        msg.attachments = vec![crate::schema::AttachmentMeta {
            filename: "invoice.pdf".to_string(),
            mime: "application/pdf".to_string(),
            attachment_id: "att1".to_string(),
            size: 5,
        }];
        let client = MockGmailClient::new().with_message(msg).with_attachment(
            "18f1a2b3",
            "att1",
            b"hello".to_vec(),
        );
        let batch = read_rows(&client, "/mail/INBOX/18f1a2b3/att1", None, None).unwrap();
        assert_eq!(batch.rows.len(), 1);
        let idx = |name: &str| {
            batch
                .schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == name)
                .unwrap_or_else(|| panic!("column {name}"))
        };
        assert!(
            matches!(&batch.rows[0].values[idx("filename")], Value::Text(s) if s == "invoice.pdf")
        );
        assert!(
            matches!(&batch.rows[0].values[idx("mime")], Value::Text(s) if s == "application/pdf")
        );
        assert!(matches!(&batch.rows[0].values[idx("content")], Value::Bytes(b) if b == b"hello"));
        assert!(client
            .recorded()
            .iter()
            .any(|c| matches!(c, crate::client::RecordedCall::GetAttachment { .. })));
    }

    #[test]
    fn an_attachment_node_with_no_such_attachment_fails_closed() {
        // A message with no matching attachment id is a structured invalid_path, not a panic.
        let client = MockGmailClient::new().with_message(fixture_message("18f1a2b3", "Invoice 42"));
        let err = read_rows(&client, "/mail/INBOX/18f1a2b3/att1", None, None).unwrap_err();
        assert_eq!(err.code(), "invalid_path");
    }

    /// Extract the single recorded Gmail search call's `(query, max_results)`.
    fn recorded_search(client: &MockGmailClient) -> (String, Option<u32>) {
        client
            .recorded()
            .into_iter()
            .find_map(|c| match c {
                crate::client::RecordedCall::Search { query, max_results } => {
                    Some((query, max_results))
                }
                _ => None,
            })
            .expect("a search call was recorded")
    }

    #[test]
    fn pushes_the_where_into_the_gmail_search_query_and_keeps_a_lossy_residual_uncapped() {
        use qfs_types::{CmpOp, ColRef, Literal, Predicate};
        let client = MockGmailClient::new().with_search_page(MessageIdPage {
            ids: Vec::new(),
            next_page_token: None,
        });
        // `subject = 'Invoice'` is a LOSSY Gmail `subject:` pre-filter → kept as residual.
        let pred = Predicate::Cmp(
            ColRef::col("subject"),
            CmpOp::Eq,
            Literal::Text("Invoice".to_string()),
        );
        read_rows(&client, "/mail/INBOX", Some(&pred), Some(5)).unwrap();
        let (query, max_results) = recorded_search(&client);
        assert!(query.contains("label:INBOX"), "query: {query}");
        assert!(query.contains("subject:Invoice"), "query: {query}");
        // A residual remains, so the LIMIT is NOT pushed to the fetch — the engine applies it.
        assert_eq!(max_results, Some(READ_CAP));
    }

    #[test]
    fn pushes_the_limit_into_the_fetch_when_no_residual_remains() {
        let client = MockGmailClient::new().with_search_page(MessageIdPage {
            ids: Vec::new(),
            next_page_token: None,
        });
        // A bare label scan with no `WHERE` leaves no residual, so the LIMIT is safe to push.
        read_rows(&client, "/mail/INBOX", None, Some(5)).unwrap();
        let (query, max_results) = recorded_search(&client);
        assert!(query.contains("label:INBOX"), "query: {query}");
        assert_eq!(max_results, Some(5));
    }
}
