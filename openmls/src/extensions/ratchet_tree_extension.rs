use tls_codec::{TlsDeserialize, TlsSerialize, TlsSize};

use super::{Deserialize, Serialize};
use crate::treesync::node::Node;

/// # Ratchet Tree Extension.
///
/// The ratchet tree extension contains a list of (optional) [`Node`]s that
/// represent the public state of the tree in an MLS group.
#[derive(
    PartialEq,
    Eq,
    Clone,
    Debug,
    Default,
    Serialize,
    Deserialize,
    TlsSerialize,
    TlsDeserialize,
    TlsSize,
)]
pub struct RatchetTreeExtension {
    tree: Vec<Option<Node>>,
}

impl RatchetTreeExtension {
    /// Build a new extension from a vector of [`Node`]s.
    pub fn new(tree: Vec<Option<Node>>) -> Self {
        RatchetTreeExtension { tree }
    }

    /// Get a slice of the nodes in tis tree.
    pub(crate) fn as_slice(&self) -> &[Option<Node>] {
        self.tree.as_slice()
    }
}
