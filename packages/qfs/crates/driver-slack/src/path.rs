//! [`SlackPath`] — the parse of a qfs [`Path`](qfs_driver::Path) into the concrete Slack node it
//! names (RFD-0001 §5). Slack is a **multi-archetype** mount: an **Append/log** of messages,
//! threads, reactions and DMs; a **Blob/namespace** of files; and a **Relational** directory of
//! users — all under one `/slack/<ws>/...` tree, one [`SlackWsConfig`](crate::SlackWsConfig) per
//! workspace `<ws>`.
//!
//! ## Addressing
//! - `/slack/<ws>/<#channel>/messages` — the channel message log (Append; `SELECT(tail)` /
//!   `INSERT(post)`). `<#channel>` is a symbolic `#name` or a `Cxxxx` id (resolved at commit by
//!   the applier, never during planning — RFD §3 purity).
//! - `/slack/<ws>/<#channel>/messages/<ts>/replies` — a thread under a parent message `ts`.
//! - `/slack/<ws>/<#channel>/messages/<ts>/reactions` — the reactions on a message.
//! - `/slack/<ws>/dms/<user>/messages` — a direct-message log with `<user>`.
//! - `/slack/<ws>/files` — the workspace file namespace (Blob; `ls`/`cp`/`rm`).
//! - `/slack/<ws>/users` — the user directory (Relational; read-mostly).
//!
//! Pure parsing only — no I/O. Owned data only; no Slack/vendor type crosses.

use qfs_driver::Path;

use crate::error::SlackError;

/// The mount this driver answers for.
pub const MOUNT: &str = "/slack";

/// A reference to a channel: a symbolic `#name`, a bare `name`, or a `Cxxxx`/`Gxxxx` id. The
/// `#name`→id resolution is **I/O** and is performed by the applier at commit, never during
/// planning (PREVIEW shows the symbolic `#channel`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChannelRef {
    /// The raw channel token exactly as it appeared in the path (`#general`, `general`, `C0123`).
    pub raw: String,
}

impl ChannelRef {
    /// Wrap a raw channel token.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    /// Whether this token is already a Slack channel id (`Cxxxx`/`Gxxxx`/`Dxxxx`) rather than a
    /// symbolic `#name` needing a `conversations.list` lookup at commit.
    #[must_use]
    pub fn is_id(&self) -> bool {
        let b = self.raw.as_bytes();
        matches!(b.first(), Some(b'C' | b'G' | b'D'))
            && b.len() > 1
            && b[1..].iter().all(u8::is_ascii_alphanumeric)
    }

    /// The symbolic display form (`#general`) PREVIEW shows — the leading `#` is normalized in.
    #[must_use]
    pub fn symbolic(&self) -> String {
        if self.is_id() || self.raw.starts_with('#') {
            self.raw.clone()
        } else {
            format!("#{}", self.raw)
        }
    }
}

/// A reference to a user (`@name`, bare `name`, or a `Uxxxx` id) — the DM peer. Like [`ChannelRef`]
/// the `@name`→id resolution is I/O performed by the applier at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct UserRef {
    /// The raw user token exactly as it appeared in the path.
    pub raw: String,
}

impl UserRef {
    /// Wrap a raw user token.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }
}

/// The closed set of node *kinds* a `/slack/<ws>` mount exposes (RFD §5). Each carries a distinct
/// archetype (the multi-archetype property) and a typed [`Schema`](qfs_types::Schema). Declaration
/// order is the canonical order golden snapshots and capability gating use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NodeKind {
    /// `<#channel>/messages` — an Append/log of channel messages.
    Messages,
    /// `<#channel>/messages/<ts>/replies` — an Append/log of thread replies under a parent `ts`.
    Replies,
    /// `<#channel>/messages/<ts>/reactions` — an Append/log of reactions (`INSERT`/`REMOVE`).
    Reactions,
    /// `dms/<user>/messages` — an Append/log of direct messages.
    Dms,
    /// `files` — a Blob/namespace (`ls`/`cp`/`rm`).
    Files,
    /// `users` — a Relational/table directory (read-mostly).
    Users,
}

impl NodeKind {
    /// Every node kind in canonical order — the single source of truth for the kind/tie test.
    pub const ALL: [NodeKind; 6] = [
        NodeKind::Messages,
        NodeKind::Replies,
        NodeKind::Reactions,
        NodeKind::Dms,
        NodeKind::Files,
        NodeKind::Users,
    ];

    /// A short, stable label for this node kind (golden snapshots, structured errors).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            NodeKind::Messages => "messages",
            NodeKind::Replies => "replies",
            NodeKind::Reactions => "reactions",
            NodeKind::Dms => "dms",
            NodeKind::Files => "files",
            NodeKind::Users => "users",
        }
    }
}

/// A parsed Slack address — the concrete node a `/slack/<ws>/...` path resolves to. Owned,
/// vendor-free. The applier and the introspective methods branch on the [`NodeKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackNode {
    /// `<#channel>/messages`.
    Messages {
        /// The channel the log belongs to.
        channel: ChannelRef,
    },
    /// `<#channel>/messages/<ts>/replies`.
    Replies {
        /// The channel.
        channel: ChannelRef,
        /// The parent message `ts` the thread hangs from.
        parent_ts: String,
    },
    /// `<#channel>/messages/<ts>/reactions`.
    Reactions {
        /// The channel.
        channel: ChannelRef,
        /// The message `ts` the reactions attach to.
        ts: String,
    },
    /// `dms/<user>/messages`.
    Dms {
        /// The DM peer.
        user: UserRef,
    },
    /// `files`.
    Files,
    /// `users`.
    Users,
}

impl SlackNode {
    /// The [`NodeKind`] of this node — the capability/archetype/schema key.
    #[must_use]
    pub const fn kind(&self) -> NodeKind {
        match self {
            SlackNode::Messages { .. } => NodeKind::Messages,
            SlackNode::Replies { .. } => NodeKind::Replies,
            SlackNode::Reactions { .. } => NodeKind::Reactions,
            SlackNode::Dms { .. } => NodeKind::Dms,
            SlackNode::Files => NodeKind::Files,
            SlackNode::Users => NodeKind::Users,
        }
    }

    /// The channel this node addresses, if any (messages/replies/reactions). DMs/files/users
    /// carry no channel.
    #[must_use]
    pub const fn channel(&self) -> Option<&ChannelRef> {
        match self {
            SlackNode::Messages { channel }
            | SlackNode::Replies { channel, .. }
            | SlackNode::Reactions { channel, .. } => Some(channel),
            _ => None,
        }
    }
}

/// A parsed `/slack/<ws>/...` address: the workspace plus the concrete [`SlackNode`]. The
/// workspace selects the [`SlackWsConfig`](crate::SlackWsConfig); the node selects the
/// archetype/schema/capabilities and the Web-API call the verb maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SlackPath {
    /// The workspace segment (`<ws>`) — keys the per-workspace config.
    pub workspace: String,
    /// The concrete node addressed.
    pub node: SlackNode,
}

impl SlackPath {
    /// Parse a driver [`Path`] into a [`SlackPath`].
    ///
    /// # Errors
    /// [`SlackError::InvalidPath`] if the path is not a `/slack/<ws>/...` node this driver serves.
    pub fn parse(path: &Path) -> Result<Self, SlackError> {
        Self::parse_str(path.as_str())
    }

    /// Parse a raw path string into a [`SlackPath`] (the core parse).
    ///
    /// # Errors
    /// [`SlackError::InvalidPath`] on a malformed address.
    pub fn parse_str(raw: &str) -> Result<Self, SlackError> {
        let trimmed = raw.trim_end_matches('/');
        let Some(after) = trimmed.strip_prefix(&format!("{MOUNT}/")) else {
            return Err(SlackError::InvalidPath {
                path: raw.to_string(),
                reason: "path is not under the /slack mount",
            });
        };
        let seg: Vec<&str> = after.split('/').filter(|s| !s.is_empty()).collect();
        let invalid = |reason: &'static str| SlackError::InvalidPath {
            path: raw.to_string(),
            reason,
        };
        // The first segment is always the workspace.
        let Some((ws, rest)) = seg.split_first() else {
            return Err(invalid(
                "a Slack path must name a workspace: /slack/<ws>/...",
            ));
        };
        let workspace = (*ws).to_string();
        let node = match rest {
            // /slack/<ws>/users
            ["users"] => SlackNode::Users,
            // /slack/<ws>/files
            ["files"] => SlackNode::Files,
            // /slack/<ws>/dms/<user>/messages
            ["dms", user, "messages"] => SlackNode::Dms {
                user: UserRef::new(*user),
            },
            // /slack/<ws>/<#channel>/messages
            [channel, "messages"] => SlackNode::Messages {
                channel: ChannelRef::new(*channel),
            },
            // /slack/<ws>/<#channel>/messages/<ts>/replies
            [channel, "messages", ts, "replies"] => SlackNode::Replies {
                channel: ChannelRef::new(*channel),
                parent_ts: (*ts).to_string(),
            },
            // /slack/<ws>/<#channel>/messages/<ts>/reactions
            [channel, "messages", ts, "reactions"] => SlackNode::Reactions {
                channel: ChannelRef::new(*channel),
                ts: (*ts).to_string(),
            },
            [] => {
                return Err(invalid(
                    "the Slack workspace root is not a node; name a sub-path",
                ))
            }
            _ => return Err(invalid("not a recognized /slack/<ws>/... node")),
        };
        Ok(Self { workspace, node })
    }

    /// The node kind this address selects.
    #[must_use]
    pub const fn kind(&self) -> NodeKind {
        self.node.kind()
    }
}
