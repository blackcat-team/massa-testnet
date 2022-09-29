use massa_graph::error::{GraphError, GraphResult};
use massa_graph_2_exports::{
    block_graph_export::BlockGraphExport,
    block_status::{BlockStatus, ExportCompiledBlock},
    GraphConfig,
};
use massa_models::{
    active_block::ActiveBlock,
    api::BlockGraphStatus,
    block::BlockId,
    clique::Clique,
    prehash::{CapacityAllocator, PreHashMap, PreHashSet},
    slot::Slot,
};
use massa_storage::Storage;

#[derive(Clone, Debug)]
pub struct GraphState {
    /// Configuration
    pub config: GraphConfig,
    /// Storage
    pub storage: Storage,
    /// Block ids of genesis blocks
    pub genesis_hashes: Vec<BlockId>,
    /// Incompatibility graph: maps a block id to the block ids it is incompatible with
    /// One entry per Active Block
    pub gi_head: PreHashMap<BlockId, PreHashSet<BlockId>>,
    /// All the cliques
    pub max_cliques: Vec<Clique>,
    /// ids of active blocks
    pub active_index: PreHashSet<BlockId>,
    /// One (block id, period) per thread
    pub latest_final_blocks_periods: Vec<(BlockId, u64)>,
    /// One `(block id, period)` per thread TODO not sure I understand the difference with `latest_final_blocks_periods`
    pub best_parents: Vec<(BlockId, u64)>,
    /// Every block we know about
    pub block_statuses: PreHashMap<BlockId, BlockStatus>,
}

impl GraphState {
    fn get_full_active_block(&self, block_id: &BlockId) -> Option<(&ActiveBlock, &Storage)> {
        match self.block_statuses.get(block_id) {
            Some(BlockStatus::Active { a_block, storage }) => Some((a_block.as_ref(), storage)),
            _ => None,
        }
    }

    /// get the blockclique (or final) block ID at a given slot, if any
    pub(crate) fn get_blockclique_block_at_slot(&self, slot: &Slot) -> Option<BlockId> {
        // List all blocks at this slot.
        // The list should be small: make a copy of it to avoid holding the storage lock.
        let blocks_at_slot = {
            let storage_read = self.storage.read_blocks();
            let returned = match storage_read.get_blocks_by_slot(slot) {
                Some(v) => v.clone(),
                None => return None,
            };
            returned
        };

        // search for the block in the blockclique
        let search_in_blockclique = blocks_at_slot
            .intersection(
                &self
                    .max_cliques
                    .iter()
                    .find(|c| c.is_blockclique)
                    .expect("expected one clique to be the blockclique")
                    .block_ids,
            )
            .next();
        if let Some(found_id) = search_in_blockclique {
            return Some(*found_id);
        }

        // block not found in the blockclique: search in the final blocks
        blocks_at_slot
            .into_iter()
            .find(|b_id| match self.block_statuses.get(b_id) {
                Some(BlockStatus::Active { a_block, .. }) => a_block.is_final,
                _ => false,
            })
    }

    /// get the latest blockclique (or final) block ID at a given slot, if any
    pub(crate) fn get_latest_blockclique_block_at_slot(&self, slot: &Slot) -> BlockId {
        let (mut best_block_id, mut best_block_period) = self
            .latest_final_blocks_periods
            .get(slot.thread as usize)
            .unwrap_or_else(|| panic!("unexpected not found latest final block period"));

        self.max_cliques
            .iter()
            .find(|c| c.is_blockclique)
            .expect("expected one clique to be the blockclique")
            .block_ids
            .iter()
            .for_each(|id| match self.block_statuses.get(id) {
                Some(BlockStatus::Active {
                    a_block,
                    storage: _,
                }) => {
                    if a_block.is_final {
                        panic!(
                            "unexpected final block on getting latest blockclique block at slot"
                        );
                    }
                    if a_block.slot.thread == slot.thread
                        && a_block.slot.period < slot.period
                        && a_block.slot.period > best_block_period
                    {
                        best_block_period = a_block.slot.period;
                        best_block_id = *id;
                    }
                }
                _ => {
                    panic!("expected to find only active block but found another status")
                }
            });
        best_block_id
    }

    pub(crate) fn get_block_status(&self, block_id: &BlockId) -> BlockGraphStatus {
        match self.block_statuses.get(block_id) {
            None => BlockGraphStatus::NotFound,
            Some(BlockStatus::Active { a_block, .. }) => {
                if a_block.is_final {
                    BlockGraphStatus::Final
                } else if self
                    .max_cliques
                    .iter()
                    .find(|clique| clique.is_blockclique)
                    .expect("blockclique absent")
                    .block_ids
                    .contains(block_id)
                {
                    BlockGraphStatus::ActiveInBlockclique
                } else {
                    BlockGraphStatus::ActiveInAlternativeCliques
                }
            }
            Some(BlockStatus::Discarded { .. }) => BlockGraphStatus::Discarded,
            Some(BlockStatus::Incoming(_)) => BlockGraphStatus::Incoming,
            Some(BlockStatus::WaitingForDependencies { .. }) => {
                BlockGraphStatus::WaitingForDependencies
            }
            Some(BlockStatus::WaitingForSlot(_)) => BlockGraphStatus::WaitingForSlot,
        }
    }
    pub(crate) fn list_required_active_blocks(&self) -> GraphResult<PreHashSet<BlockId>> {
        // list all active blocks
        let mut retain_active: PreHashSet<BlockId> =
            PreHashSet::<BlockId>::with_capacity(self.active_index.len());

        let latest_final_blocks: Vec<BlockId> = self
            .latest_final_blocks_periods
            .iter()
            .map(|(hash, _)| *hash)
            .collect();

        // retain all non-final active blocks,
        // the current "best parents",
        // and the dependencies for both.
        for block_id in self.active_index.iter() {
            if let Some(BlockStatus::Active {
                a_block: active_block,
                ..
            }) = self.block_statuses.get(block_id)
            {
                if !active_block.is_final
                    || self.best_parents.iter().any(|(b, _p)| b == block_id)
                    || latest_final_blocks.contains(block_id)
                {
                    retain_active.extend(active_block.parents.iter().map(|(p, _)| *p));
                    retain_active.insert(*block_id);
                }
            }
        }

        // retain best parents
        retain_active.extend(self.best_parents.iter().map(|(b, _p)| *b));

        // retain last final blocks
        retain_active.extend(self.latest_final_blocks_periods.iter().map(|(h, _)| *h));

        for (thread, id) in latest_final_blocks.iter().enumerate() {
            let mut current_block_id = *id;
            while let Some((current_block, _)) = self.get_full_active_block(&current_block_id) {
                let parent_id = {
                    if !current_block.parents.is_empty() {
                        Some(current_block.parents[thread as usize].0)
                    } else {
                        None
                    }
                };

                // retain block
                retain_active.insert(current_block_id);

                // stop traversing when reaching a block with period number low enough
                // so that any of its operations will have their validity period expired at the latest final block in thread
                // note: one more is kept because of the way we iterate
                if current_block.slot.period
                    < self.latest_final_blocks_periods[thread]
                        .1
                        .saturating_sub(self.config.operation_validity_periods)
                {
                    break;
                }

                // if not genesis, traverse parent
                match parent_id {
                    Some(p_id) => current_block_id = p_id,
                    None => break,
                }
            }
        }

        // grow with parents & fill thread holes twice
        for _ in 0..2 {
            // retain the parents of the selected blocks
            let retain_clone = retain_active.clone();

            for retain_h in retain_clone.into_iter() {
                retain_active.extend(
                    self.get_full_active_block(&retain_h)
                        .ok_or_else(|| GraphError::ContainerInconsistency(format!("inconsistency inside block statuses pruning and retaining the parents of the selected blocks - {} is missing", retain_h)))?
                        .0.parents
                        .iter()
                        .map(|(b_id, _p)| *b_id),
                )
            }

            // find earliest kept slots in each thread
            let mut earliest_retained_periods: Vec<u64> = self
                .latest_final_blocks_periods
                .iter()
                .map(|(_, p)| *p)
                .collect();
            for retain_h in retain_active.iter() {
                let retain_slot = &self
                    .get_full_active_block(retain_h)
                    .ok_or_else(|| GraphError::ContainerInconsistency(format!("inconsistency inside block statuses pruning and finding earliest kept slots in each thread - {} is missing", retain_h)))?
                    .0.slot;
                earliest_retained_periods[retain_slot.thread as usize] = std::cmp::min(
                    earliest_retained_periods[retain_slot.thread as usize],
                    retain_slot.period,
                );
            }

            // fill up from the latest final block back to the earliest for each thread
            for thread in 0..self.config.thread_count {
                let mut cursor = self.latest_final_blocks_periods[thread as usize].0; // hash of tha latest final in that thread
                while let Some((c_block, _)) = self.get_full_active_block(&cursor) {
                    if c_block.slot.period < earliest_retained_periods[thread as usize] {
                        break;
                    }
                    retain_active.insert(cursor);
                    if c_block.parents.is_empty() {
                        // genesis
                        break;
                    }
                    cursor = c_block.parents[thread as usize].0;
                }
            }
        }

        Ok(retain_active)
    }

    pub fn extract_block_graph_part(
        &self,
        slot_start: Option<Slot>,
        slot_end: Option<Slot>,
    ) -> GraphResult<BlockGraphExport> {
        let mut export = BlockGraphExport {
            genesis_blocks: self.genesis_hashes.clone(),
            active_blocks: PreHashMap::with_capacity(self.block_statuses.len()),
            discarded_blocks: PreHashMap::with_capacity(self.block_statuses.len()),
            best_parents: self.best_parents.clone(),
            latest_final_blocks_periods: self.latest_final_blocks_periods.clone(),
            gi_head: self.gi_head.clone(),
            max_cliques: self.max_cliques.clone(),
        };

        let filter = |&s| {
            if let Some(s_start) = slot_start {
                if s < s_start {
                    return false;
                }
            }
            if let Some(s_end) = slot_end {
                if s >= s_end {
                    return false;
                }
            }
            true
        };

        for (hash, block) in self.block_statuses.iter() {
            match block {
                BlockStatus::Discarded {
                    slot,
                    creator,
                    parents,
                    reason,
                    ..
                } => {
                    if filter(slot) {
                        export
                            .discarded_blocks
                            .insert(*hash, (reason.clone(), (*slot, *creator, parents.clone())));
                    }
                }
                BlockStatus::Active { a_block, storage } => {
                    if filter(&a_block.slot) {
                        let stored_block =
                            storage.read_blocks().get(hash).cloned().ok_or_else(|| {
                                GraphError::MissingBlock(format!(
                                    "missing block in BlockGraphExport::extract_from: {}",
                                    hash
                                ))
                            })?;
                        export.active_blocks.insert(
                            *hash,
                            ExportCompiledBlock {
                                header: stored_block.content.header,
                                children: a_block
                                    .children
                                    .iter()
                                    .map(|thread| {
                                        thread.keys().copied().collect::<PreHashSet<BlockId>>()
                                    })
                                    .collect(),
                                is_final: a_block.is_final,
                            },
                        );
                    }
                }
                _ => continue,
            }
        }

        Ok(export)
    }
}
