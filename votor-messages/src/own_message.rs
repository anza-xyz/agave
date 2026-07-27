//! Defines OwnMessage used to send votes and certificates within a node.

use crate::{certificate::Certificate, consensus_message::VoteMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A vote or a certificate generated and sent within a node.
pub enum OwnMessage {
    /// A msg of type vote
    Vote(VoteMessage),
    /// A cert
    Certificate(Certificate),
}
