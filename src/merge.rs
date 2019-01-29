// Copyright 2018 Mozilla

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::{HashMap, HashSet, VecDeque},
          mem};

use error::{ErrorKind, Result};
use guid::Guid;
use tree::{Content, MergeState, MergedNode, Node, Tree};

/// Structure change types, used to indicate if a node on one side is moved
/// or deleted on the other.
#[derive(Eq, PartialEq)]
pub(crate) enum StructureChange {
    /// Node not deleted, or doesn't exist, on the other side.
    Unchanged,
    /// Node moved on the other side.
    Moved,
    /// Node deleted on the other side.
    Deleted,
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
pub struct StructureCounts {
    /// Remote non-folder change wins over local deletion.
    pub remote_revives: u64,
    /// Local folder deletion wins over remote change.
    pub local_deletes: u64,
    /// Local non-folder change wins over remote deletion.
    pub local_revives: u64,
    /// Remote folder deletion wins over local change.
    pub remote_deletes: u64,
    /// Deduped local items.
    pub dupes: u64,
}

/// Holds (matching remote dupes for local GUIDs, matching local dupes for
/// remote GUIDs).
type MatchingDupes<'t> = (HashMap<Guid, Node<'t>>, HashMap<Guid, Node<'t>>);

/// Represents an accepted local or remote deletion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Deletion {
    pub guid: Guid,
    pub local_level: i64,
    pub should_upload_tombstone: bool,
}

/// Indicates which side to take in case of a merge conflict.
enum ConflictResolution {
    Local,
    Remote,
    Unchanged,
}

/// A merge driver provides methods to customize merging behavior.
pub trait Driver {
    /// Generates a new GUID for the given invalid GUID. This is used to fix up
    /// items with GUIDs that Places can't store (bug 1380606, bug 1313026).
    ///
    /// Implementations of `Driver` can either use the `rand` and `base64`
    /// crates to generate a new, random GUID (9 bytes, Base64url-encoded
    /// without padding), or use an existing method like Desktop's
    /// `nsINavHistoryService::MakeGuid`. Dogear doesn't generate new GUIDs
    /// automatically to avoid depending on those crates.
    ///
    /// An implementation can also return:
    ///
    /// - `Ok(invalid_guid.clone())` to pass through all invalid GUIDs, as the
    ///   tests do.
    /// - An error to forbid them entirely, as `DefaultDriver` does.
    fn generate_new_guid(&self, invalid_guid: &Guid) -> Result<Guid>;
}

/// A default implementation of the merge driver.
pub struct DefaultDriver;

impl Driver for DefaultDriver {
    /// The default implementation returns an error and fails the merge if any
    /// items have invalid GUIDs.
    fn generate_new_guid(&self, invalid_guid: &Guid) -> Result<Guid> {
        Err(ErrorKind::GenerateGuid(invalid_guid.clone()).into())
    }
}

/// A two-way merger that produces a complete merged tree from a complete local
/// tree and a complete remote tree with changes since the last sync.
///
/// This is ported almost directly from iOS. On iOS, the `ThreeWayMerger` takes
/// a complete "mirror" tree with the server state after the last sync, and two
/// incomplete trees with local and remote changes to the mirror: "local" and
/// "mirror", respectively. Overlaying buffer onto mirror yields the current
/// server tree; overlaying local onto mirror yields the complete local tree.
///
/// Dogear doesn't store the shared parent for changed items, so we can only
/// do two-way merges. Our local tree is the union of iOS's mirror and local,
/// and our remote tree is the union of iOS's mirror and buffer.
///
/// Unlike iOS, Dogear doesn't distinguish between structure and value changes.
/// The `needs_merge` flag notes *that* a bookmark changed, but not *how*. This
/// means we might detect conflicts, and revert changes on one side, for cases
/// that iOS can merge cleanly.
///
/// Fortunately, most of our users don't organize their bookmarks into deeply
/// nested hierarchies, or make conflicting changes on multiple devices
/// simultaneously. A simpler two-way tree merge strikes a good balance between
/// correctness and complexity.
pub struct Merger<'t, D = DefaultDriver> {
    driver: D,
    local_tree: &'t Tree,
    new_local_contents: Option<&'t HashMap<Guid, Content>>,
    remote_tree: &'t Tree,
    new_remote_contents: Option<&'t HashMap<Guid, Content>>,
    matching_dupes_by_local_parent_guid: HashMap<Guid, MatchingDupes<'t>>,
    merged_guids: HashSet<Guid>,
    delete_locally: HashSet<Guid>,
    delete_remotely: HashSet<Guid>,
    structure_counts: StructureCounts,
}

impl<'t> Merger<'t, DefaultDriver> {
    pub fn new(local_tree: &'t Tree, remote_tree: &'t Tree) -> Merger<'t> {
        Merger { driver: DefaultDriver,
                 local_tree,
                 new_local_contents: None,
                 remote_tree,
                 new_remote_contents: None,
                 matching_dupes_by_local_parent_guid: HashMap::new(),
                 merged_guids: HashSet::new(),
                 delete_locally: HashSet::new(),
                 delete_remotely: HashSet::new(),
                 structure_counts: StructureCounts::default(), }
    }

    pub fn with_contents(local_tree: &'t Tree,
                         new_local_contents: &'t HashMap<Guid, Content>,
                         remote_tree: &'t Tree,
                         new_remote_contents: &'t HashMap<Guid, Content>)
                         -> Merger<'t>
    {
        Merger::with_driver(DefaultDriver, local_tree, new_local_contents, remote_tree,
                            new_remote_contents)
    }
}

impl <'t, D: Driver> Merger<'t, D> {
    pub fn with_driver(driver: D, local_tree: &'t Tree,
                       new_local_contents: &'t HashMap<Guid, Content>,
                       remote_tree: &'t Tree,
                       new_remote_contents: &'t HashMap<Guid, Content>)
                       -> Merger<'t, D>
    {
        Merger { driver,
                 local_tree,
                 new_local_contents: Some(new_local_contents),
                 remote_tree,
                 new_remote_contents: Some(new_remote_contents),
                 matching_dupes_by_local_parent_guid: HashMap::new(),
                 merged_guids: HashSet::new(),
                 delete_locally: HashSet::new(),
                 delete_remotely: HashSet::new(),
                 structure_counts: StructureCounts::default(), }
    }

    pub fn merge(&mut self) -> Result<MergedNode<'t>> {
        let merged_root_node = {
            let local_root_node = self.local_tree.root();
            let remote_root_node = self.remote_tree.root();
            self.two_way_merge(local_root_node, remote_root_node)?
        };

        // Any remaining deletions on one side should be deleted on the other side.
        // This happens when the remote tree has tombstones for items that don't
        // exist locally, or the local tree has tombstones for items that
        // aren't on the server.
        for guid in self.local_tree.deletions() {
            if !self.mentions(guid) {
                self.delete_remotely.insert(guid.clone());
            }
        }
        for guid in self.remote_tree.deletions() {
            if !self.mentions(guid) {
                self.delete_locally.insert(guid.clone());
            }
        }

        Ok(merged_root_node)
    }

    #[inline]
    pub fn telemetry(&self) -> &StructureCounts {
        &self.structure_counts
    }

    #[inline]
    pub fn subsumes(&self, tree: &Tree) -> bool {
        tree.guids().all(|guid| self.mentions(guid))
    }

    #[inline]
    pub fn deletions<'m>(&'m self) -> impl Iterator<Item = Deletion> + 'm {
        self.local_deletions().chain(self.remote_deletions())
    }

    fn local_deletions<'m>(&'m self) -> impl Iterator<Item = Deletion> + 'm {
        self.delete_locally.iter().filter_map(move |guid| {
            if self.delete_remotely.contains(guid) {
                None
            } else {
                let local_level = self.local_tree
                                      .node_for_guid(guid)
                                      .map(|node| node.level())
                                      .unwrap_or(-1);
                // Items that should be deleted locally already have tombstones
                // on the server, so we don't need to upload tombstones for
                // these deletions.
                Some(Deletion { guid: guid.clone(),
                                local_level,
                                should_upload_tombstone: false, })
            }
        })
    }

    fn remote_deletions<'m>(&'m self) -> impl Iterator<Item = Deletion> + 'm {
        self.delete_remotely.iter().map(move |guid| {
            let local_level = self.local_tree
                                  .node_for_guid(guid)
                                  .map(|node| node.level())
                                  .unwrap_or(-1);
            Deletion { guid: guid.to_owned(),
                       local_level,
                       should_upload_tombstone: true, }
        })
    }

    #[inline]
    fn mentions(&self, guid: &Guid) -> bool {
        self.merged_guids.contains(guid) ||
        self.delete_locally.contains(guid) ||
        self.delete_remotely.contains(guid)
    }

    fn merge_local_node(&mut self, local_node: Node<'t>) -> Result<MergedNode<'t>> {
        trace!("Item {} only exists locally", local_node);

        self.merged_guids.insert(local_node.guid.clone());

        let merged_guid = if local_node.guid.valid() {
            local_node.guid.clone()
        } else {
            let new_guid = self.driver.generate_new_guid(&local_node.guid)?;
            if new_guid != local_node.guid {
                self.merged_guids.insert(new_guid.clone());
            }
            new_guid
        };

        let mut merged_node = MergedNode::new(merged_guid,
                                              MergeState::Local { local_node, remote_node: None });
        if local_node.is_folder() {
            // The local folder doesn't exist remotely, but its children might, so
            // we still need to recursively walk and merge them. This method will
            // change the merge state from local to new if any children were moved
            // or deleted.
            for local_child_node in local_node.children() {
                self.merge_local_child_into_merged_node(&mut merged_node,
                                                        local_node,
                                                        None,
                                                        local_child_node)?;
            }
        }

        Ok(merged_node)
    }

    fn merge_remote_node(&mut self, remote_node: Node<'t>) -> Result<MergedNode<'t>> {
        trace!("Item {} only exists remotely", remote_node);

        self.merged_guids.insert(remote_node.guid.clone());

        let merged_guid = if remote_node.guid.valid() {
            remote_node.guid.clone()
        } else {
            let new_guid = self.driver.generate_new_guid(&remote_node.guid)?;
            if new_guid != remote_node.guid {
                self.merged_guids.insert(new_guid.clone());
                // Upload tombstones for changed remote GUIDs.
                self.delete_remotely.insert(remote_node.guid.clone());
            }
            new_guid
        };

        let mut merged_node = MergedNode::new(merged_guid,
                                              MergeState::Remote { local_node: None, remote_node });
        if remote_node.is_folder() {
            // As above, a remote folder's children might still exist locally, so we
            // need to merge them and update the merge state from remote to new if
            // any children were moved or deleted.
            for remote_child_node in remote_node.children() {
                self.merge_remote_child_into_merged_node(&mut merged_node,
                                                         None,
                                                         remote_node,
                                                         remote_child_node)?;
            }
        }

        Ok(merged_node)
    }

    /// Merges two nodes that exist locally and remotely.
    fn two_way_merge(&mut self,
                     local_node: Node<'t>,
                     remote_node: Node<'t>)
                     -> Result<MergedNode<'t>>
    {
        trace!("Item exists locally as {} and remotely as {}",
               local_node,
               remote_node);

        if !local_node.has_compatible_kind(&remote_node) {
            // TODO(lina): Remove and replace items with mismatched kinds in
            // `check_for_{}_structure_change_of_{}_node`.
            error!("Merging local {} and remote {} with different kinds",
                   local_node, remote_node);
            return Err(ErrorKind::MismatchedItemKind(local_node.kind, remote_node.kind).into());
        }

        self.merged_guids.insert(remote_node.guid.clone());

        if local_node.guid != remote_node.guid {
            // We deduped a NEW local item to a remote item.
            self.merged_guids.insert(local_node.guid.clone());
        }

        let (item, children) = self.resolve_value_conflict(local_node, remote_node);

        let mut merged_node = MergedNode::new(remote_node.guid.clone(), match item {
            ConflictResolution::Local => {
                MergeState::Local { local_node, remote_node: Some(remote_node) }
            },
            ConflictResolution::Remote => {
                MergeState::Remote { local_node: Some(local_node), remote_node }
            },
            ConflictResolution::Unchanged => {
                MergeState::Unchanged { local_node, remote_node }
            },
        });

        match children {
            ConflictResolution::Local => {
                for local_child_node in local_node.children() {
                    self.merge_local_child_into_merged_node(&mut merged_node,
                                                            local_node,
                                                            Some(remote_node),
                                                            local_child_node)?;
                }
                for remote_child_node in remote_node.children() {
                    self.merge_remote_child_into_merged_node(&mut merged_node,
                                                             Some(local_node),
                                                             remote_node,
                                                             remote_child_node)?;
                }
            },

            ConflictResolution::Remote | ConflictResolution::Unchanged => {
                for remote_child_node in remote_node.children() {
                    self.merge_remote_child_into_merged_node(&mut merged_node,
                                                             Some(local_node),
                                                             remote_node,
                                                             remote_child_node)?;
                }
                for local_child_node in local_node.children() {
                    self.merge_local_child_into_merged_node(&mut merged_node,
                                                            local_node,
                                                            Some(remote_node),
                                                            local_child_node)?;
                }
            },
        }

        Ok(merged_node)
    }

    /// Merges a remote child node into a merged folder node. This handles the
    /// following cases:
    ///
    /// - The remote child is locally deleted. We recursively move all of its
    ///   descendants that don't exist locally to the merged folder.
    /// - The remote child doesn't exist locally, but has a content match in the
    ///   corresponding local folder. We dedupe the local child to the remote
    ///   child.
    /// - The remote child exists locally, but in a different folder. We compare
    ///   merge flags and timestamps to decide where to keep the child.
    /// - The remote child exists locally, and in the same folder. We merge the
    ///   local and remote children.
    ///
    /// This is the inverse of `merge_local_child_into_merged_node`.
    ///
    /// Returns `true` if the merged structure state changed because the remote
    /// child was locally moved or deleted; `false` otherwise.
    fn merge_remote_child_into_merged_node(&mut self,
                                           merged_node: &mut MergedNode<'t>,
                                           local_parent_node: Option<Node<'t>>,
                                           remote_parent_node: Node<'t>,
                                           remote_child_node: Node<'t>)
                                           -> Result<()>
    {
        if self.merged_guids.contains(&remote_child_node.guid) {
            trace!("Remote child {} already seen in another folder and merged",
                   remote_child_node);
            return Ok(());
        }

        trace!("Merging remote child {} of {} into {}",
               remote_child_node,
               remote_parent_node,
               merged_node);

        // Check if the remote child is locally deleted. and move all
        // descendants that aren't also remotely deleted to the merged node.
        // This handles the case where a user deletes a folder on this device,
        // and adds a bookmark to the same folder on another device. We want to
        // keep the folder deleted, but we also don't want to lose the new
        // bookmark, so we move the bookmark to the deleted folder's parent.
        if self.check_for_local_structure_change_of_remote_node(merged_node,
                                                                remote_parent_node,
                                                                remote_child_node)? ==
           StructureChange::Deleted
        {
            // Flag the merged parent for reupload, since we deleted the
            // remote child.
            merged_node.merge_state = merged_node.merge_state.with_new_structure();
            return Ok(());
        }

        // The remote child isn't locally deleted. Does it exist in the local tree?
        if let Some(local_child_node) = self.local_tree.node_for_guid(&remote_child_node.guid) {
            // The remote child exists in the local tree. Did it move?
            let local_parent_node =
                local_child_node.parent()
                                .expect("Can't merge existing remote child without local parent");

            trace!("Remote child {} exists locally in {} and remotely in {}",
                   remote_child_node,
                   local_parent_node,
                   remote_parent_node);

            if self.remote_tree.is_deleted(&local_parent_node.guid) {
                trace!("Unconditionally taking remote move for {} to {} because local parent {} is \
                        deleted remotely",
                       remote_child_node,
                       remote_parent_node,
                       local_parent_node);

                let mut merged_child_node = self.two_way_merge(local_child_node,
                                                               remote_child_node)?;
                if remote_child_node.diverged() || merged_node.remote_guid_changed() {
                    // If the remote structure diverged, or the merged parent
                    // GUID changed, flag the remote child for reupload so that
                    // its `parentid` is correct.
                    merged_child_node.merge_state =
                        merged_child_node.merge_state.with_new_structure();
                }
                merged_node.merged_children.push(merged_child_node);
                return Ok(());
            }

            match self.resolve_structure_conflict(local_parent_node,
                                                  local_child_node,
                                                  remote_parent_node,
                                                  remote_child_node)
            {
                ConflictResolution::Local => {
                    // The local move is newer, so we ignore the remote move.
                    // We'll merge the remote child later, when we walk its new
                    // local parent.
                    trace!("Remote child {} moved locally to {} and remotely to {}; \
                            keeping child in newer local parent and position",
                           remote_child_node,
                           local_parent_node,
                           remote_parent_node);

                    // Flag the old parent for reupload, since we're moving
                    // the remote child. Note that, since we only flag the
                    // remote parent here, we don't need to handle
                    // reparenting and repositioning separately.
                    merged_node.merge_state = merged_node.merge_state.with_new_structure();
                },

                ConflictResolution::Remote | ConflictResolution::Unchanged => {
                    // The remote move is newer, so we merge the remote
                    // child now and ignore the local move.
                    trace!("Remote child {} moved locally to {} and remotely to {}; \
                            keeping child in newer remote parent and position",
                           remote_child_node,
                           local_parent_node,
                           remote_parent_node);

                    let mut merged_child_node = self.two_way_merge(local_child_node,
                                                                   remote_child_node)?;
                    if remote_child_node.diverged() || merged_node.remote_guid_changed() {
                        merged_child_node.merge_state =
                            merged_child_node.merge_state.with_new_structure();
                    }
                    merged_node.merged_children.push(merged_child_node);
                },
            }

            return Ok(());
        }

        // Remote child is not a root, and doesn't exist locally. Try to find a
        // content match in the containing folder, and dedupe the local item if
        // we can.
        trace!("Remote child {} doesn't exist locally; looking for local content match",
               remote_child_node);

        let mut merged_child_node = if let Some(local_child_node_by_content) =
            self.find_local_node_matching_remote_node(merged_node,
                                                      local_parent_node,
                                                      remote_parent_node,
                                                      remote_child_node)
        {
            self.two_way_merge(local_child_node_by_content, remote_child_node)
        } else {
            self.merge_remote_node(remote_child_node)
        }?;
        if remote_child_node.diverged() || merged_node.remote_guid_changed() {
            merged_child_node.merge_state = merged_child_node.merge_state.with_new_structure();
        }
        merged_node.merged_children.push(merged_child_node);
        Ok(())
    }

    /// Merges a local child node into a merged folder node.
    ///
    /// This is the inverse of `merge_remote_child_into_merged_node`.
    ///
    /// Returns `true` if the merged structure state changed because the local
    /// child doesn't exist remotely or was locally moved; `false` otherwise.
    fn merge_local_child_into_merged_node(&mut self,
                                          merged_node: &mut MergedNode<'t>,
                                          local_parent_node: Node<'t>,
                                          remote_parent_node: Option<Node<'t>>,
                                          local_child_node: Node<'t>)
                                          -> Result<()>
    {
        if self.merged_guids.contains(&local_child_node.guid) {
            // We already merged the child when we walked another folder.
            trace!("Local child {} already seen in another folder and merged",
                   local_child_node);
            return Ok(());
        }

        trace!("Merging local child {} of {} into {}",
               local_child_node,
               local_parent_node,
               merged_node);

        // Check if the local child is remotely deleted, and move any new local
        // descendants to the merged node if so.
        if self.check_for_remote_structure_change_of_local_node(merged_node,
                                                                local_parent_node,
                                                                local_child_node)? ==
           StructureChange::Deleted
        {
            // Since we're merging local nodes, we don't need to flag the merged
            // parent for reupload.
            return Ok(());
        }

        // At this point, we know the local child isn't deleted. See if it
        // exists in the remote tree.
        if let Some(remote_child_node) = self.remote_tree.node_for_guid(&local_child_node.guid) {
            // The local child exists remotely. It must have moved; otherwise, we
            // would have seen it when we walked the remote children.
            let remote_parent_node =
                remote_child_node.parent()
                                 .expect("Can't merge existing local child without remote parent");

            trace!("Local child {} exists locally in {} and remotely in {}",
                   local_child_node,
                   local_parent_node,
                   remote_parent_node);

            if self.local_tree.is_deleted(&remote_parent_node.guid) {
                trace!("Unconditionally taking local move for {} to {} because remote parent {} is \
                        deleted locally",
                       local_child_node,
                       local_parent_node,
                       remote_parent_node);

                // Merge and flag the new parent *and the locally moved child* for
                // reupload. The parent references the child in its `children`; the
                // child points back to the parent in its `parentid`.
                //
                // Reuploading the child isn't necessary for newer Desktops, which
                // ignore the child's `parentid` and use the parent's `children`.
                //
                // However, older Desktop and Android use the child's `parentid` as
                // canonical, while iOS is stricter and requires both to match.
                let mut merged_child_node = self.two_way_merge(local_child_node,
                                                               remote_child_node)?;
                merged_node.merge_state = merged_node.merge_state.with_new_structure();
                merged_child_node.merge_state = merged_child_node.merge_state.with_new_structure();
                merged_node.merged_children.push(merged_child_node);
                return Ok(());
            }

            match self.resolve_structure_conflict(local_parent_node,
                                                  local_child_node,
                                                  remote_parent_node,
                                                  remote_child_node)
            {
                ConflictResolution::Local => {
                    // The local move is newer, so we merge the local child now
                    // and ignore the remote move.
                    if local_parent_node.guid != remote_parent_node.guid {
                        // The child moved to a different folder.
                        trace!("Local child {} reparented locally to {} and remotely to {}; \
                                keeping child in newer local parent",
                               local_child_node,
                               local_parent_node,
                               remote_parent_node);

                        // Merge and flag both the new parent and child for
                        // reupload. See above for why.
                        let mut merged_child_node = self.two_way_merge(local_child_node,
                                                                       remote_child_node)?;
                        merged_node.merge_state = merged_node.merge_state.with_new_structure();
                        merged_child_node.merge_state =
                            merged_child_node.merge_state.with_new_structure();
                        merged_node.merged_children.push(merged_child_node);
                    } else {
                        trace!("Local child {} repositioned locally in {} and remotely in {}; \
                                keeping child in newer local position",
                               local_child_node,
                               local_parent_node,
                               remote_parent_node);

                        // For position changes in the same folder, we only need to
                        // merge and flag the parent for reupload.
                        let mut merged_child_node = self.two_way_merge(local_child_node,
                                                                       remote_child_node)?;
                        merged_node.merge_state = merged_node.merge_state.with_new_structure();
                        if remote_child_node.diverged() || merged_node.remote_guid_changed() {
                            // ...But repositioning an item in a diverged folder, or in a folder
                            // with an invalid GUID, should reupload the item.
                            merged_child_node.merge_state =
                                merged_child_node.merge_state.with_new_structure();
                        }
                        merged_node.merged_children.push(merged_child_node);
                    }
                },

                ConflictResolution::Remote | ConflictResolution::Unchanged => {
                    // The remote move is newer, so we ignore the local
                    // move. We'll merge the local child later, when we
                    // walk its new remote parent.
                    if local_parent_node.guid != remote_parent_node.guid {
                        trace!("Local child {} reparented locally to {} and remotely to {}; \
                                keeping child in newer remote parent",
                               local_child_node,
                               local_parent_node,
                               remote_parent_node);
                    } else {
                        trace!("Local child {} repositioned locally in {} and remotely in {}; \
                                keeping child in newer remote position",
                               local_child_node,
                               local_parent_node,
                               remote_parent_node);
                    }
                },
            }

            return Ok(());
        }

        // Local child is not a root, and doesn't exist remotely. Try to find a
        // content match in the containing folder, and dedupe the local item if
        // we can.
        trace!("Local child {} doesn't exist remotely; looking for remote content match",
               local_child_node);

        let merged_child_node = if let Some(remote_child_node_by_content) =
            self.find_remote_node_matching_local_node(merged_node,
                                                      local_parent_node,
                                                      remote_parent_node,
                                                      local_child_node)
        {
            // The local child has a remote content match, so take the remote GUID
            // and merge.
            let mut merged_child_node = self.two_way_merge(local_child_node,
                                                           remote_child_node_by_content)?;
            if remote_child_node_by_content.diverged() || merged_node.remote_guid_changed() {
                merged_node.merge_state = merged_node.merge_state.with_new_structure();
                merged_child_node.merge_state =
                    merged_child_node.merge_state.with_new_structure();
            }
            merged_child_node
        } else {
            // The local child doesn't exist remotely, so flag the merged parent and
            // new child for upload, and walk its descendants.
            let mut merged_child_node = self.merge_local_node(local_child_node)?;
            merged_node.merge_state = merged_node.merge_state.with_new_structure();
            merged_child_node.merge_state = merged_child_node.merge_state.with_new_structure();
            merged_child_node
        };
        merged_node.merged_children.push(merged_child_node);
        Ok(())
    }

    /// Determines which side to prefer, and which children to merge first,
    /// for an item that exists on both sides.
    fn resolve_value_conflict(&self,
                              local_node: Node<'t>,
                              remote_node: Node<'t>)
                              -> (ConflictResolution, ConflictResolution)
    {
        if remote_node.is_root() {
            // Don't reorder local roots.
            return (ConflictResolution::Local, ConflictResolution::Local);
        }

        match (local_node.needs_merge, remote_node.needs_merge) {
            (true, true) => match (local_node.diverged(), remote_node.diverged()) {
                (true, false) => (ConflictResolution::Remote, ConflictResolution::Remote),
                (false, true) => (ConflictResolution::Local, ConflictResolution::Local),
                _ => {
                    // The item changed locally and remotely.
                    if local_node.age < remote_node.age {
                        // The local change is newer, so merge local children first,
                        // followed by remaining unmerged remote children.
                        (ConflictResolution::Local, ConflictResolution::Local)
                    } else {
                        // The remote change is newer, so walk and merge remote
                        // children first, then remaining local children.
                        if remote_node.is_user_content_root() {
                            // Don't update root titles or other properties, but
                            // use their newer remote structure.
                            (ConflictResolution::Local, ConflictResolution::Remote)
                        } else {
                            (ConflictResolution::Remote, ConflictResolution::Remote)
                        }
                    }
                },
            },

            (true, false) => {
                // The item changed locally, but not remotely. Keep the local
                // state, then merge local children first, followed by remote
                // children.
                (ConflictResolution::Local, ConflictResolution::Local)
            },

            (false, true) => {
                // The item changed remotely, but not locally. Take the
                // remote state, then merge remote children first, followed
                // by local children.
                if remote_node.is_user_content_root() {
                    (ConflictResolution::Local, ConflictResolution::Remote)
                } else {
                    (ConflictResolution::Remote, ConflictResolution::Remote)
                }
            },

            (false, false) => {
                // The item is unchanged on both sides.
                (ConflictResolution::Unchanged, ConflictResolution::Unchanged)
            }
        }
    }

    /// Determines where to keep a child of a folder that exists on both sides.
    fn resolve_structure_conflict(&self,
                                  local_parent_node: Node<'t>,
                                  local_child_node: Node<'t>,
                                  remote_parent_node: Node<'t>,
                                  remote_child_node: Node<'t>)
                                  -> ConflictResolution
    {
        if remote_child_node.is_user_content_root() {
            // Always use the local parent and position for roots.
            return ConflictResolution::Local;
        }

        match (local_parent_node.needs_merge, remote_parent_node.needs_merge) {
            (true, true) => match (local_parent_node.diverged(), remote_parent_node.diverged()) {
                (true, false) => ConflictResolution::Remote,
                (false, true) => ConflictResolution::Local,
                _ => {
                    // If both parents changed, compare timestamps to decide where
                    // to keep the local child.
                    let latest_local_age = local_child_node.age.min(local_parent_node.age);
                    let latest_remote_age = remote_child_node.age.min(remote_parent_node.age);

                    if latest_local_age < latest_remote_age {
                        ConflictResolution::Local
                    } else {
                        ConflictResolution::Remote
                    }
                },
            },

            // If only the local or remote parent changed, keep the child in its
            // new parent.
            (true, false) => ConflictResolution::Local,
            (false, true) => ConflictResolution::Remote,

            (false, false) => ConflictResolution::Unchanged
        }
    }

    /// Checks if a remote node is locally moved or deleted, and reparents any
    /// descendants that aren't also remotely deleted to the merged node.
    ///
    /// This is the inverse of
    /// `check_for_remote_structure_change_of_local_node`.
    fn check_for_local_structure_change_of_remote_node(&mut self,
                                                       merged_node: &mut MergedNode<'t>,
                                                       remote_parent_node: Node<'t>,
                                                       remote_node: Node<'t>)
                                                       -> Result<StructureChange>
    {
        if !remote_node.is_syncable() {
            // If the remote node is known to be non-syncable, we unconditionally
            // delete it from the server, even if it's syncable locally.
            self.delete_remotely.insert(remote_node.guid.clone());
            if remote_node.is_folder() {
                // If the remote node is a folder, we also need to walk its descendants
                // and reparent any syncable descendants, and descendants that only
                // exist remotely, to the merged node.
                self.relocate_remote_orphans_to_merged_node(merged_node, remote_node)?;
            }
            return Ok(StructureChange::Deleted);
        }

        if !self.local_tree.is_deleted(&remote_node.guid) {
            if let Some(local_node) = self.local_tree.node_for_guid(&remote_node.guid) {
                if !local_node.is_syncable() {
                    // The remote node is syncable, but the local node is non-syncable.
                    // For consistency with Desktop, we unconditionally delete the
                    // node from the server.
                    self.delete_remotely.insert(remote_node.guid.clone());
                    if remote_node.is_folder() {
                        self.relocate_remote_orphans_to_merged_node(merged_node, remote_node)?;
                    }
                    return Ok(StructureChange::Deleted);
                }
                let local_parent_node =
                    local_node.parent()
                              .expect("Can't check for structure changes without local parent");
                if local_parent_node.guid != remote_parent_node.guid {
                    return Ok(StructureChange::Moved);
                }
                return Ok(StructureChange::Unchanged);
            } else {
                return Ok(StructureChange::Unchanged);
            }
        }

        if remote_node.needs_merge {
            if !remote_node.is_folder() {
                // If a non-folder child is deleted locally and changed remotely, we
                // ignore the local deletion and take the remote child.
                trace!("Remote non-folder {} deleted locally and changed remotely; taking remote \
                        change",
                       remote_node);
                self.structure_counts.remote_revives += 1;
                return Ok(StructureChange::Unchanged);
            }
            // For folders, we always take the local deletion and relocate remotely
            // changed grandchildren to the merged node. We could use the remote
            // tree to revive the child folder, but it's easier to relocate orphaned
            // grandchildren than to partially revive the child folder.
            trace!("Remote folder {} deleted locally and changed remotely; taking local deletion",
                   remote_node);
            self.structure_counts.local_deletes += 1;
        } else {
            trace!("Remote node {} deleted locally and not changed remotely; taking local \
                    deletion",
                   remote_node);
        }

        // Take the local deletion and relocate any new remote descendants to the
        // merged node.
        self.delete_remotely.insert(remote_node.guid.clone());
        if remote_node.is_folder() {
            self.relocate_remote_orphans_to_merged_node(merged_node, remote_node)?;
        }
        Ok(StructureChange::Deleted)
    }

    /// Checks if a local node is remotely moved or deleted, and reparents any
    /// descendants that aren't also locally deleted to the merged node.
    ///
    /// This is the inverse of
    /// `check_for_local_structure_change_of_remote_node`.
    fn check_for_remote_structure_change_of_local_node(&mut self,
                                                       merged_node: &mut MergedNode<'t>,
                                                       local_parent_node: Node<'t>,
                                                       local_node: Node<'t>)
                                                       -> Result<StructureChange>
    {
        if !local_node.is_syncable() {
            // If the local node is known to be non-syncable, we unconditionally
            // delete it from the local tree, even if it's syncable remotely.
            self.delete_locally.insert(local_node.guid.clone());
            if local_node.is_folder() {
                self.relocate_local_orphans_to_merged_node(merged_node, local_node)?;
            }
            return Ok(StructureChange::Deleted);
        }

        if !self.remote_tree.is_deleted(&local_node.guid) {
            if let Some(remote_node) = self.remote_tree.node_for_guid(&local_node.guid) {
                if !remote_node.is_syncable() {
                    // The local node is syncable, but the remote node is non-syncable.
                    // This can happen if we applied an orphaned left pane query in a
                    // previous sync, and later saw the left pane root on the server.
                    // Since we now have the complete subtree, we can remove the item.
                    self.delete_locally.insert(local_node.guid.clone());
                    if remote_node.is_folder() {
                        self.relocate_local_orphans_to_merged_node(merged_node, local_node)?;
                    }
                    return Ok(StructureChange::Deleted);
                }
                let remote_parent_node =
                    remote_node.parent()
                               .expect("Can't check for structure changes without remote parent");
                if remote_parent_node.guid != local_parent_node.guid {
                    return Ok(StructureChange::Moved);
                }
                return Ok(StructureChange::Unchanged);
            } else {
                return Ok(StructureChange::Unchanged);
            }
        }

        if local_node.needs_merge {
            if !local_node.is_folder() {
                trace!("Local non-folder {} deleted remotely and changed locally; taking local \
                        change",
                       local_node);
                self.structure_counts.local_revives += 1;
                return Ok(StructureChange::Unchanged);
            }
            trace!("Local folder {} deleted remotely and changed locally; taking remote deletion",
                   local_node);
            self.structure_counts.remote_deletes += 1;
        } else {
            trace!("Local node {} deleted remotely and not changed locally; taking remote \
                    deletion",
                   local_node);
        }

        // Take the remote deletion and relocate any new local descendants to the
        // merged node.
        self.delete_locally.insert(local_node.guid.clone());
        if local_node.is_folder() {
            self.relocate_local_orphans_to_merged_node(merged_node, local_node)?;
        }
        Ok(StructureChange::Deleted)
    }

    /// Takes a local deletion for a remote node by marking the node as deleted,
    /// and relocating all remote descendants that aren't also locally deleted
    /// to the closest surviving ancestor. We do this to avoid data loss if
    /// the user adds a bookmark to a folder on another device, and deletes
    /// that folder locally.
    ///
    /// This is the inverse of `relocate_local_orphans_to_merged_node`.
    fn relocate_remote_orphans_to_merged_node(&mut self,
                                              merged_node: &mut MergedNode<'t>,
                                              remote_node: Node<'t>)
                                              -> Result<()>
    {
        for remote_child_node in remote_node.children() {
            if self.merged_guids.contains(&remote_child_node.guid) {
                trace!("Remote child {} can't be an orphan; already merged", remote_child_node);
                continue;
            }
            match self.check_for_local_structure_change_of_remote_node(merged_node,
                                                                       remote_node,
                                                                       remote_child_node)?
            {
                StructureChange::Moved | StructureChange::Deleted => {
                    // The remote child is already moved or deleted locally, so we should
                    // ignore it instead of treating it as a remote orphan.
                    continue;
                },
                StructureChange::Unchanged => {
                    trace!("Relocating remote orphan {} to {}",
                           remote_child_node,
                           merged_node);

                    // Flag the new parent and moved remote orphan for reupload.
                    let mut merged_orphan_node = if let Some(local_child_node) =
                        self.local_tree.node_for_guid(&remote_child_node.guid)
                    {
                        self.two_way_merge(local_child_node, remote_child_node)
                    } else {
                        self.merge_remote_node(remote_child_node)
                    }?;
                    merged_node.merge_state = merged_node.merge_state.with_new_structure();
                    merged_orphan_node.merge_state =
                        merged_orphan_node.merge_state.with_new_structure();
                    merged_node.merged_children.push(merged_orphan_node);
                },
            }
        }
        Ok(())
    }

    /// Takes a remote deletion for a local node by marking the node as deleted,
    /// and relocating all local descendants that aren't also remotely deleted
    /// to the closest surviving ancestor.
    ///
    /// This is the inverse of `relocate_remote_orphans_to_merged_node`.
    fn relocate_local_orphans_to_merged_node(&mut self,
                                             merged_node: &mut MergedNode<'t>,
                                             local_node: Node<'t>)
                                             -> Result<()>
    {
        for local_child_node in local_node.children() {
            if self.merged_guids.contains(&local_child_node.guid) {
                trace!("Local child {} can't be an orphan; already merged", local_child_node);
                continue;
            }
            match self.check_for_remote_structure_change_of_local_node(merged_node,
                                                                       local_node,
                                                                       local_child_node)?
            {
                StructureChange::Moved | StructureChange::Deleted => {
                    // The local child is already moved or deleted remotely, so we should
                    // ignore it instead of treating it as a local orphan.
                    continue;
                },
                StructureChange::Unchanged => {
                    trace!("Relocating local orphan {} to {}",
                           local_child_node,
                           merged_node);

                    // Flag the new parent and moved local orphan for reupload.
                    let mut merged_orphan_node = if let Some(remote_child_node) =
                        self.remote_tree.node_for_guid(&local_child_node.guid)
                    {
                        self.two_way_merge(local_child_node, remote_child_node)
                    } else {
                        self.merge_local_node(local_child_node)
                    }?;
                    merged_node.merge_state = merged_node.merge_state.with_new_structure();
                    merged_orphan_node.merge_state =
                        merged_orphan_node.merge_state.with_new_structure();
                    merged_node.merged_children.push(merged_orphan_node);
                },
            }
        }
        Ok(())
    }

    /// Finds all children of a local folder with similar content as children of
    /// the corresponding remote folder. This is used to dedupe local items that
    /// haven't been uploaded yet, to remote items that don't exist locally.
    ///
    /// Recall that we match items by GUID as we walk down the tree. If a GUID
    /// on one side doesn't exist on the other, we fall back to a content
    /// match in the same folder.
    ///
    /// This method is called the first time that
    /// `find_remote_node_matching_local_node` merges a local child that
    /// doesn't exist remotely, and
    /// the first time that `find_local_node_matching_remote_node` merges a
    /// remote child that doesn't exist locally.
    ///
    /// Finding all possible dupes is O(m + n) in the worst case, where `m` is
    /// the number of local children, and `n` is the number of remote
    /// children. We cache matches in
    /// `matching_dupes_by_local_parent_guid`, so deduping all
    /// remaining children of the same folder, on both sides, only needs two
    /// O(1) map lookups per child.
    fn find_all_matching_dupes_in_folders(&self,
                                          local_parent_node: Node<'t>,
                                          remote_parent_node: Node<'t>)
                                          -> MatchingDupes<'t>
    {
        let mut dupe_key_to_local_nodes: HashMap<&Content, VecDeque<_>> = HashMap::new();

        for local_child_node in local_parent_node.children() {
            if let Some(local_child_content) =
                self.new_local_contents
                    .and_then(|contents| contents.get(&local_child_node.guid))
            {
                if let Some(remote_child_node) =
                    self.remote_tree.node_for_guid(&local_child_node.guid)
                {
                    trace!("Not deduping local child {}; already exists remotely as {}",
                           local_child_node,
                           remote_child_node);
                    continue;
                }
                if self.remote_tree.is_deleted(&local_child_node.guid) {
                    trace!("Not deduping local child {}; deleted remotely",
                           local_child_node);
                    continue;
                }
                // Store matching local children in an array, in case multiple children
                // have the same dupe key (for example, a toolbar containing multiple
                // empty folders, as in bug 1213369).
                let local_nodes_for_key = dupe_key_to_local_nodes.entry(local_child_content)
                                                                 .or_default();
                local_nodes_for_key.push_back(local_child_node);
            } else {
                trace!("Not deduping local child {}; already uploaded",
                       local_child_node);
            }
        }

        let mut local_to_remote = HashMap::new();
        let mut remote_to_local = HashMap::new();

        for remote_child_node in remote_parent_node.children() {
            if remote_to_local.contains_key(&remote_child_node.guid) {
                trace!("Not deduping remote child {}; already deduped",
                       remote_child_node);
                continue;
            }
            // Note that we don't need to check if the remote node is deleted
            // locally, because it wouldn't have local content entries if it
            // were.
            if let Some(remote_child_content) =
                self.new_remote_contents
                    .and_then(|contents| contents.get(&remote_child_node.guid))
            {
                if let Some(mut local_nodes_for_key) =
                    dupe_key_to_local_nodes.get_mut(remote_child_content)
                {
                    if let Some(local_child_node) = local_nodes_for_key.pop_front() {
                        trace!("Deduping local child {} to remote child {}",
                               local_child_node,
                               remote_child_node);
                        local_to_remote.insert(local_child_node.guid.clone(), remote_child_node);
                        remote_to_local.insert(remote_child_node.guid.clone(), local_child_node);
                    } else {
                        trace!("Not deduping remote child {}; no remaining local content matches",
                               remote_child_node);
                        continue;
                    }
                } else {
                    trace!("Not deduping remote child {}; no local content matches",
                           remote_child_node);
                    continue;
                }
            } else {
                trace!("Not deduping remote child {}; already merged",
                       remote_child_node);
            }
        }

        (local_to_remote, remote_to_local)
    }

    /// Finds a remote node with a different GUID that matches the content of a
    /// local node.
    ///
    /// This is the inverse of `find_local_node_matching_remote_node`.
    fn find_remote_node_matching_local_node(&mut self,
                                            merged_node: &MergedNode<'t>,
                                            local_parent_node: Node<'t>,
                                            remote_parent_node: Option<Node<'t>>,
                                            local_child_node: Node<'t>)
                                            -> Option<Node<'t>>
    {
        if let Some(remote_parent_node) = remote_parent_node {
            let mut matching_dupes_by_local_parent_guid =
                mem::replace(&mut self.matching_dupes_by_local_parent_guid,
                             HashMap::new());
            let new_remote_node =
                {
                    let (local_to_remote, _) = matching_dupes_by_local_parent_guid
                    .entry(local_parent_node.guid.clone())
                    .or_insert_with(|| {
                        trace!("First local child {} doesn't exist remotely; finding all \
                                matching dupes in local {} and remote {}",
                                local_child_node,
                                local_parent_node,
                                remote_parent_node);
                        self.find_all_matching_dupes_in_folders(
                            local_parent_node,
                            remote_parent_node,
                        )
                    });
                    let new_remote_node = local_to_remote.get(&local_child_node.guid);
                    new_remote_node.map(|node| {
                        self.structure_counts.dupes += 1;
                        *node
                    })
                };
            mem::replace(&mut self.matching_dupes_by_local_parent_guid,
                         matching_dupes_by_local_parent_guid);
            new_remote_node
        } else {
            trace!("Merged node {} doesn't exist remotely; no potential dupes for local child {}",
                   merged_node,
                   local_child_node);
            None
        }
    }

    /// Finds a local node with a different GUID that matches the content of a
    /// remote node.
    ///
    /// This is the inverse of `find_remote_node_matching_local_node`.
    fn find_local_node_matching_remote_node(&mut self,
                                            merged_node: &MergedNode<'t>,
                                            local_parent_node: Option<Node<'t>>,
                                            remote_parent_node: Node<'t>,
                                            remote_child_node: Node<'t>)
                                            -> Option<Node<'t>>
    {
        if let Some(local_parent_node) = local_parent_node {
            let mut matching_dupes_by_local_parent_guid =
                mem::replace(&mut self.matching_dupes_by_local_parent_guid,
                             HashMap::new());
            let new_local_node =
                {
                    let (_, remote_to_local) = matching_dupes_by_local_parent_guid
                    .entry(local_parent_node.guid.clone())
                    .or_insert_with(|| {
                        trace!("First remote child {} doesn't exist locally; finding all \
                                matching dupes in local {} and remote {}",
                                remote_child_node,
                                local_parent_node,
                                remote_parent_node);
                        self.find_all_matching_dupes_in_folders(
                            local_parent_node,
                            remote_parent_node,
                        )
                    });
                    let new_local_node = remote_to_local.get(&remote_child_node.guid);
                    new_local_node.map(|node| {
                        self.structure_counts.dupes += 1;
                        *node
                    })
                };
            mem::replace(&mut self.matching_dupes_by_local_parent_guid,
                         matching_dupes_by_local_parent_guid);
            new_local_node
        } else {
            trace!("Merged node {} doesn't exist locally; no potential dupes for remote child {}",
                   merged_node,
                   remote_child_node);
            None
        }
    }
}
