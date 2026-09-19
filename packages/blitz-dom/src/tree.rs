//! Versioned storage for the nodes of the DOM tree.

use std::ops::{Index, IndexMut};

use blitz_traits::node_id::NodeId;
use slotmap::{Key as _, KeyData, SlotMap};

use crate::Node;

slotmap::new_key_type! {
    /// The internal [`slotmap`] key for node storage. Only used at the
    /// storage boundary: all public APIs use [`NodeId`].
    struct NodeKey;
}

#[inline(always)]
fn to_key(id: NodeId) -> NodeKey {
    NodeKey::from(KeyData::from_ffi(id.as_u64()))
}

#[inline(always)]
fn to_id(key: NodeKey) -> NodeId {
    NodeId::from_u64(key.data().as_ffi())
}

/// The versioned map in which the nodes of the DOM tree are stored, backed by
/// a [`slotmap::SlotMap`].
///
/// Nodes are addressed by [`NodeId`], which carries the slot's version in
/// addition to its index: when a node is dropped and its slot reused, ids
/// referring to the dropped node no longer resolve ([`NodeTree::get`] returns
/// `None`, and indexing panics) instead of aliasing the new occupant.
pub struct NodeTree {
    map: SlotMap<NodeKey, Node>,
    /// The current version of each slot, by index (0 while the slot has never
    /// been occupied). Lets an id whose version bits were lost be recovered:
    /// see [`NodeTree::from_opaque`].
    versions: Vec<u32>,
}

impl NodeTree {
    pub(crate) fn new() -> Self {
        Self {
            map: SlotMap::with_key(),
            versions: Vec::new(),
        }
    }

    /// Recover a [`NodeId`] from the raw value Stylo hands back in an
    /// `OpaqueNode` (see `TNode::opaque`).
    ///
    /// `OpaqueNode` wraps a `usize`, so on 32-bit targets only the slot index
    /// survives the round trip and the version bits come back as zero. In that
    /// case the id of the slot's current occupant is returned; an id with its
    /// version intact is returned as is. `None` if the slot is empty.
    pub fn from_opaque(&self, raw: usize) -> Option<NodeId> {
        let raw = raw as u64;
        if raw >> 32 != 0 {
            return Some(NodeId::from_u64(raw));
        }
        let index = raw as u32;
        let version = *self.versions.get(index as usize)?;
        (version != 0).then(|| NodeId::from_u64(raw | (u64::from(version) << 32)))
    }

    /// The number of live nodes in the map.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Whether `id` resolves to a live node.
    pub fn contains_key(&self, id: NodeId) -> bool {
        self.map.contains_key(to_key(id))
    }

    /// Get a reference to the node with the given id, if it is still live.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.map.get(to_key(id))
    }

    /// Get a mutable reference to the node with the given id, if it is still live.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.map.get_mut(to_key(id))
    }

    /// Insert a node constructed with knowledge of its own id.
    pub(crate) fn insert_with_key(&mut self, f: impl FnOnce(NodeId) -> Node) -> NodeId {
        let id = to_id(self.map.insert_with_key(|key| f(to_id(key))));
        let index = id.as_u64() as u32 as usize;
        if self.versions.len() <= index {
            self.versions.resize(index + 1, 0);
        }
        self.versions[index] = (id.as_u64() >> 32) as u32;
        id
    }

    /// Remove the node with the given id, returning it if it was still live.
    pub(crate) fn remove(&mut self, id: NodeId) -> Option<Node> {
        let node = self.map.remove(to_key(id))?;
        if let Some(version) = self.versions.get_mut(id.as_u64() as u32 as usize) {
            *version = 0;
        }
        Some(node)
    }

    /// Iterate over all live `(NodeId, &Node)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.map.iter().map(|(key, node)| (to_id(key), node))
    }

    /// Iterate over all live `(NodeId, &mut Node)` pairs.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (NodeId, &mut Node)> {
        self.map.iter_mut().map(|(key, node)| (to_id(key), node))
    }
}

impl Index<NodeId> for NodeTree {
    type Output = Node;

    #[track_caller]
    #[inline]
    fn index(&self, id: NodeId) -> &Node {
        &self.map[to_key(id)]
    }
}

impl IndexMut<NodeId> for NodeTree {
    #[track_caller]
    #[inline]
    fn index_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.map[to_key(id)]
    }
}
