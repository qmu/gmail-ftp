//! [`SlackEffect`] — the owned effect the driver realises a plan leaf as (RFD-0001 §6), and the
//! decode from a runtime [`EffectNode`] onto it. The applier ([`crate::applier`]) interprets one of
//! these against the Slack Web API under `COMMIT`.
//!
//! ## Verb → Web-API mapping (internal to the driver)
//! - `INSERT INTO .../messages`                  → [`SlackEffect::PostMessage`] (`chat.postMessage`)
//! - `INSERT INTO .../messages/<ts>/replies`     → [`SlackEffect::PostMessage`] with `thread_ts`
//! - `INSERT INTO .../dms/<user>/messages`       → [`SlackEffect::PostMessage`] (DM channel)
//! - `INSERT INTO .../messages/<ts>/reactions`   → [`SlackEffect::AddReaction`] (`reactions.add`)
//! - `REMOVE   .../messages/<ts>/reactions`      → [`SlackEffect::RemoveReaction`] (`reactions.remove`)
//! - `REMOVE   .../messages/<ts>`                → [`SlackEffect::DeleteMessage`] (`chat.delete`, irreversible)
//! - files `cp` → [`SlackEffect::UploadFile`] (`files.upload`); `rm` → [`SlackEffect::DeleteFile`] (`files.delete`, irreversible)
//! - `CALL slack.react/pin/unpin/update/delete`  → the matching [`SlackEffect`] CALL variants
//!
//! ## Idempotency (RFD §6)
//! `chat.postMessage` is **not** idempotent. The decoder attaches a `client_msg_id` idempotency key
//! (deterministic from the node id + channel + text, so a re-plan of the *same* INSERT carries the
//! same key) and the applier surfaces the at-least-once risk — an ambiguous post is **never**
//! auto-retried. `reactions.add` is naturally idempotent (already-reacted is swallowed by the
//! applier).
//!
//! ## Channel/user id resolution is the applier's job (not here)
//! The decoder keeps the **symbolic** `#channel` / `@user` exactly as the path named it. The
//! `#name`→`Cxxxx` lookup is I/O and is performed by the applier at commit — planning stays pure,
//! PREVIEW shows `#channel` (RFD §3 purity invariant).

use qfs_plan::{EffectKind, EffectNode};
use qfs_types::Value;

use crate::error::SlackError;
use crate::path::{SlackNode, SlackPath};

/// Row column carrying a message body (`INSERT INTO messages`, `slack.update`/`slack.post` text).
pub const TEXT_COL: &str = "text";
/// Row column carrying a reaction emoji name (`INSERT INTO reactions`, `slack.react`).
pub const EMOJI_COL: &str = "emoji";
/// Row column carrying an explicit channel for a `CALL` proc (overrides the path channel if set).
pub const CHANNEL_COL: &str = "channel";
/// Row column carrying an explicit message `ts` for a `CALL` proc.
pub const TS_COL: &str = "ts";
/// Row column carrying a file name for a `files.upload` (`cp`).
pub const NAME_COL: &str = "name";
/// Row column carrying file content (text) for a `files.upload` (`cp`).
pub const CONTENT_COL: &str = "content";

/// One fully-decoded Slack effect — what the apply leg executes against the Web API. Owned DTOs; no
/// Slack/vendor type appears here. `Pin`/`DeleteMessage`/`DeleteFile` are irreversible (RFD §10/§6).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackEffect {
    /// `chat.postMessage` — post a message (or a threaded reply when `thread_ts` is set, or a DM
    /// when `channel` names a `@user`). **Not idempotent**; carries a `client_msg_id`.
    PostMessage {
        /// The symbolic channel/user the message targets (resolved at commit).
        channel: String,
        /// The message text.
        text: String,
        /// The parent thread `ts`, for a reply; `None` for a top-level post.
        thread_ts: Option<String>,
        /// The idempotency key surfaced in PREVIEW and attached to the post (at-least-once).
        client_msg_id: String,
        /// Whether the target was a DM (`/dms/<user>/messages`) — the applier opens a DM channel.
        is_dm: bool,
    },
    /// `reactions.add` — add an emoji reaction to a message. Naturally idempotent (already-reacted
    /// is swallowed).
    AddReaction {
        /// The symbolic channel.
        channel: String,
        /// The message `ts` to react to.
        ts: String,
        /// The emoji name (without colons).
        emoji: String,
    },
    /// `reactions.remove` — remove an emoji reaction.
    RemoveReaction {
        /// The symbolic channel.
        channel: String,
        /// The message `ts`.
        ts: String,
        /// The emoji name.
        emoji: String,
    },
    /// `chat.delete` — delete a message (**irreversible**).
    DeleteMessage {
        /// The symbolic channel.
        channel: String,
        /// The message `ts` to delete.
        ts: String,
    },
    /// `chat.update` — edit a message by `ts` (`CALL slack.update`; Snapshot @version).
    UpdateMessage {
        /// The symbolic channel.
        channel: String,
        /// The message `ts` to edit.
        ts: String,
        /// The new text.
        text: String,
    },
    /// `pins.add` — pin a message (**irreversible** in the audit sense; `CALL slack.pin`).
    Pin {
        /// The symbolic channel.
        channel: String,
        /// The message `ts` to pin.
        ts: String,
    },
    /// `pins.remove` — unpin a message (`CALL slack.unpin`).
    Unpin {
        /// The symbolic channel.
        channel: String,
        /// The message `ts` to unpin.
        ts: String,
    },
    /// `files.upload` — upload a file into the workspace (`cp` into `/slack/<ws>/files`, multipart).
    UploadFile {
        /// The file name.
        name: String,
        /// The file content (text bridge via codecs; the bytes path is E5/t15).
        content: String,
    },
    /// `files.delete` — delete a file (**irreversible**; `rm` in `/slack/<ws>/files`).
    DeleteFile {
        /// The file id (`Fxxxx`).
        id: String,
    },
}

impl SlackEffect {
    /// Decode a runtime [`EffectNode`] into the concrete Slack operation.
    ///
    /// # Errors
    /// [`SlackError`] if the `(kind, path)` pair is not one the driver services, or the row args
    /// carry no usable payload.
    pub fn from_node(node: &EffectNode) -> Result<Self, SlackError> {
        let path = SlackPath::parse_str(node.target.path.as_str())?;
        match &node.kind {
            EffectKind::Insert => Self::decode_insert(node, &path),
            EffectKind::Remove => Self::decode_remove(node, &path),
            EffectKind::Call(proc) => Self::decode_call(proc.as_str(), node, &path),
            other => Err(SlackError::MalformedEffect {
                verb: "EFFECT",
                path: node.target.path.as_str().to_string(),
                reason: format!("{} is not serviced by the Slack driver", other.label()),
            }),
        }
    }

    fn decode_insert(node: &EffectNode, path: &SlackPath) -> Result<Self, SlackError> {
        match &path.node {
            SlackNode::Messages { channel } => {
                let text = req_text(node, TEXT_COL, "INSERT", "a message needs `text`")?;
                let chan = channel.symbolic();
                Ok(SlackEffect::PostMessage {
                    client_msg_id: client_msg_id(node, &chan, &text),
                    channel: chan,
                    text,
                    thread_ts: None,
                    is_dm: false,
                })
            }
            SlackNode::Replies { channel, parent_ts } => {
                let text = req_text(node, TEXT_COL, "INSERT", "a reply needs `text`")?;
                let chan = channel.symbolic();
                Ok(SlackEffect::PostMessage {
                    client_msg_id: client_msg_id(node, &chan, &text),
                    channel: chan,
                    text,
                    thread_ts: Some(parent_ts.clone()),
                    is_dm: false,
                })
            }
            SlackNode::Dms { user } => {
                let text = req_text(node, TEXT_COL, "INSERT", "a DM needs `text`")?;
                let chan = user.raw.clone();
                Ok(SlackEffect::PostMessage {
                    client_msg_id: client_msg_id(node, &chan, &text),
                    channel: chan,
                    text,
                    thread_ts: None,
                    is_dm: true,
                })
            }
            SlackNode::Reactions { channel, ts } => Ok(SlackEffect::AddReaction {
                channel: channel.symbolic(),
                ts: ts.clone(),
                emoji: req_text(node, EMOJI_COL, "INSERT", "a reaction needs an `emoji`")?,
            }),
            // users / files INSERT are not supported (gated at parse time too).
            _ => Err(Self::cap_denied("INSERT", node)),
        }
    }

    fn decode_remove(node: &EffectNode, path: &SlackPath) -> Result<Self, SlackError> {
        match &path.node {
            SlackNode::Reactions { channel, ts } => Ok(SlackEffect::RemoveReaction {
                channel: channel.symbolic(),
                ts: ts.clone(),
                emoji: req_text(
                    node,
                    EMOJI_COL,
                    "REMOVE",
                    "removing a reaction needs an `emoji`",
                )?,
            }),
            // REMOVE of a message itself: /slack/<ws>/<#channel>/messages — the ts is in args.
            SlackNode::Messages { channel } => Ok(SlackEffect::DeleteMessage {
                channel: channel.symbolic(),
                ts: req_text(node, TS_COL, "REMOVE", "deleting a message needs its `ts`")?,
            }),
            SlackNode::Files => Ok(SlackEffect::DeleteFile {
                id: req_text(node, "id", "REMOVE", "deleting a file needs its `id`")?,
            }),
            _ => Err(Self::cap_denied("REMOVE", node)),
        }
    }

    fn decode_call(proc: &str, node: &EffectNode, path: &SlackPath) -> Result<Self, SlackError> {
        // The proc may be qualified (`slack.react`) or bare (`react`); accept the suffix.
        let name = proc.rsplit('.').next().unwrap_or(proc);
        // A CALL resolves its channel/ts from the args first, falling back to the path channel.
        let path_channel = path.node.channel().map(crate::path::ChannelRef::symbolic);
        let channel = opt_text(node, CHANNEL_COL)
            .or(path_channel)
            .ok_or_else(|| Self::malformed("CALL", node, "this procedure needs a `channel`"))?;
        match name {
            crate::procs::PROC_REACT => Ok(SlackEffect::AddReaction {
                channel,
                ts: req_text(node, TS_COL, "CALL", "react needs a `ts`")?,
                emoji: req_text(node, EMOJI_COL, "CALL", "react needs an `emoji`")?,
            }),
            crate::procs::PROC_PIN => Ok(SlackEffect::Pin {
                channel,
                ts: req_text(node, TS_COL, "CALL", "pin needs a `ts`")?,
            }),
            crate::procs::PROC_UNPIN => Ok(SlackEffect::Unpin {
                channel,
                ts: req_text(node, TS_COL, "CALL", "unpin needs a `ts`")?,
            }),
            crate::procs::PROC_UPDATE => Ok(SlackEffect::UpdateMessage {
                channel,
                ts: req_text(node, TS_COL, "CALL", "update needs a `ts`")?,
                text: req_text(node, TEXT_COL, "CALL", "update needs `text`")?,
            }),
            crate::procs::PROC_DELETE => Ok(SlackEffect::DeleteMessage {
                channel,
                ts: req_text(node, TS_COL, "CALL", "delete needs a `ts`")?,
            }),
            _ => Err(SlackError::UnknownProcedure(proc.to_string())),
        }
    }

    /// Whether this effect is irreversible (RFD §10/§6): a message/file delete + a pin.
    #[must_use]
    pub const fn is_irreversible(&self) -> bool {
        matches!(
            self,
            SlackEffect::DeleteMessage { .. }
                | SlackEffect::DeleteFile { .. }
                | SlackEffect::Pin { .. }
        )
    }

    /// Whether this effect is a non-idempotent post the runtime must **never** auto-retry on an
    /// ambiguous timeout (at-least-once, RFD §6) — only `chat.postMessage`. Everything else is
    /// either naturally idempotent (`reactions.add`/`pins.add`) or carries a stable target id, so
    /// the at-least-once hazard is specific to the post.
    #[must_use]
    pub const fn is_at_least_once_post(&self) -> bool {
        matches!(self, SlackEffect::PostMessage { .. })
    }

    /// Whether the applier should swallow an "already done" application error (the naturally
    /// idempotent ops). This is **symmetric** across the add/remove pair (RFD §6 — the event story
    /// is at-least-once, so a redelivered or already-satisfied op must be a no-op success, not a
    /// terminal error):
    /// - `reactions.add` (`already_reacted`) / `reactions.remove` (`no_reaction`)
    /// - `pins.add` (`already_pinned`)        / `pins.remove`      (`not_pinned`)
    ///
    /// The already-done *codes* are recognized by [`crate::client::is_already_done`]; this selector
    /// must list every effect whose op is idempotent so the recognizer and the gate stay in sync.
    #[must_use]
    pub const fn swallows_already_done(&self) -> bool {
        matches!(
            self,
            SlackEffect::AddReaction { .. }
                | SlackEffect::RemoveReaction { .. }
                | SlackEffect::Pin { .. }
                | SlackEffect::Unpin { .. }
        )
    }

    /// The stable verb label (for the audit ledger / capability-denied errors).
    #[must_use]
    pub const fn verb_label(&self) -> &'static str {
        match self {
            SlackEffect::PostMessage { .. } | SlackEffect::AddReaction { .. } => "INSERT",
            SlackEffect::RemoveReaction { .. }
            | SlackEffect::DeleteMessage { .. }
            | SlackEffect::DeleteFile { .. } => "REMOVE",
            SlackEffect::UploadFile { .. } => "CP",
            SlackEffect::UpdateMessage { .. }
            | SlackEffect::Pin { .. }
            | SlackEffect::Unpin { .. } => "CALL",
        }
    }

    fn malformed(verb: &'static str, node: &EffectNode, reason: &str) -> SlackError {
        SlackError::MalformedEffect {
            verb,
            path: node.target.path.as_str().to_string(),
            reason: reason.to_string(),
        }
    }

    fn cap_denied(verb: &'static str, node: &EffectNode) -> SlackError {
        SlackError::CapabilityDenied {
            verb,
            path: node.target.path.as_str().to_string(),
        }
    }
}

/// Build the deterministic `client_msg_id` idempotency key for a post (RFD §6). Derived from the
/// node id + channel + text so re-planning the **same** INSERT yields the **same** key (a server
/// dedupe coordinate) while two distinct posts differ. Deterministic + pure — no clock, no RNG, so
/// PREVIEW shows the exact key COMMIT will send.
fn client_msg_id(node: &EffectNode, channel: &str, text: &str) -> String {
    let tag = crate::hmac::hmac_sha256(
        b"qfs-slack-client-msg-id",
        format!("{}:{channel}:{text}", node.id.0).as_bytes(),
    );
    // A compact, URL-safe-ish hex prefix (16 bytes is ample for a dedupe coordinate).
    format!("qfs-{}", &crate::hmac::hex_lower(&tag)[..32])
}

/// Read a non-empty `Text` value from the node's first row by column name.
fn opt_text(node: &EffectNode, name: &str) -> Option<String> {
    let idx = node
        .args
        .schema
        .columns
        .iter()
        .position(|c| c.name == name)?;
    match node.args.rows.first().and_then(|r| r.values.get(idx)) {
        Some(Value::Text(t)) if !t.is_empty() => Some(t.clone()),
        _ => None,
    }
}

/// Read a required `Text` column, erroring with `reason` if absent/empty.
fn req_text(
    node: &EffectNode,
    name: &str,
    verb: &'static str,
    reason: &str,
) -> Result<String, SlackError> {
    opt_text(node, name).ok_or_else(|| SlackEffect::malformed(verb, node, reason))
}
