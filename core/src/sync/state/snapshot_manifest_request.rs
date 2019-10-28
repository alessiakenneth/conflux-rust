// Copyright 2019 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use crate::{
    block_data_manager::BlockExecutionResult,
    message::{HasRequestId, Message, MsgId, RequestId},
    parameters::consensus_internal::REWARD_EPOCH_COUNT,
    sync::{
        message::{
            msgid, Context, DynamicCapability, Handleable, KeyContainer,
        },
        request_manager::Request,
        state::{
            delta::{ChunkKey, RangedManifest},
            snapshot_manifest_response::SnapshotManifestResponse,
        },
        Error, ProtocolConfiguration,
    },
};
use cfx_types::H256;
use rlp_derive::{RlpDecodable, RlpEncodable};
use std::{any::Any, time::Duration};

#[derive(Debug, Clone, RlpDecodable, RlpEncodable)]
pub struct SnapshotManifestRequest {
    pub request_id: u64,
    pub checkpoint: H256,
    pub start_chunk: Option<ChunkKey>,
    pub trusted_blame_block: Option<H256>,
}

build_msg_impl! { SnapshotManifestRequest, msgid::GET_SNAPSHOT_MANIFEST, "SnapshotManifestRequest" }
build_has_request_id_impl! { SnapshotManifestRequest }

impl Handleable for SnapshotManifestRequest {
    fn handle(self, ctx: &Context) -> Result<(), Error> {
        let manifest = match RangedManifest::load(
            &self.checkpoint,
            self.start_chunk.clone(),
        ) {
            Ok(Some(m)) => m,
            _ => RangedManifest::default(),
        };

        let (state_blame_vec, receipt_blame_vec, bloom_blame_vec) =
            self.get_blame_states(ctx).unwrap_or_default();
        let block_receipts = self.get_block_receipts(ctx).unwrap_or_default();
        ctx.send_response(&SnapshotManifestResponse {
            request_id: self.request_id,
            checkpoint: self.checkpoint.clone(),
            manifest,
            state_blame_vec,
            receipt_blame_vec,
            bloom_blame_vec,
            block_receipts,
        })
    }
}

impl SnapshotManifestRequest {
    pub fn new(checkpoint: H256, trusted_blame_block: H256) -> Self {
        SnapshotManifestRequest {
            request_id: 0,
            checkpoint,
            start_chunk: None,
            trusted_blame_block: Some(trusted_blame_block),
        }
    }

    pub fn new_with_start_chunk(
        checkpoint: H256, start_chunk: ChunkKey,
    ) -> Self {
        SnapshotManifestRequest {
            request_id: 0,
            checkpoint,
            start_chunk: Some(start_chunk),
            trusted_blame_block: None,
        }
    }

    fn get_block_receipts(
        &self, ctx: &Context,
    ) -> Option<Vec<BlockExecutionResult>> {
        let mut epoch_receipts = Vec::new();
        let mut epoch_hash = self.checkpoint;
        for _ in 0..REWARD_EPOCH_COUNT {
            if let Some(block) =
                ctx.manager.graph.data_man.block_header_by_hash(&epoch_hash)
            {
                match ctx
                    .manager
                    .graph
                    .consensus
                    .inner
                    .read()
                    .block_hashes_by_epoch(block.height())
                {
                    Ok(ordered_executable_epoch_blocks) => {
                        for hash in &ordered_executable_epoch_blocks {
                            match ctx
                                .manager
                                .graph
                                .data_man
                                .block_execution_result_by_hash_with_epoch(
                                    hash,
                                    &epoch_hash,
                                    false, /* update_cache */
                                ) {
                                Some(block_execution_result) => {
                                    epoch_receipts.push(block_execution_result);
                                }
                                None => {
                                    return None;
                                }
                            }
                        }
                    }
                    Err(_) => {
                        return None;
                    }
                }
                // We have reached original genesis
                if block.height() == 0 {
                    break;
                }
                epoch_hash = block.parent_hash().clone();
            } else {
                warn!(
                    "failed to find block={} in db, peer={}",
                    epoch_hash, ctx.peer
                );
                return None;
            }
        }
        Some(epoch_receipts)
    }

    /// return an empty vec if some information not exist in db, caller may find
    /// another peer to send the request; otherwise return a state_blame_vec
    /// of the requested block
    fn get_blame_states(
        &self, ctx: &Context,
    ) -> Option<(Vec<H256>, Vec<H256>, Vec<H256>)> {
        let trusted_block = ctx
            .manager
            .graph
            .data_man
            .block_header_by_hash(&self.trusted_blame_block?)?;
        let checkpoint_block = ctx
            .manager
            .graph
            .data_man
            .block_header_by_hash(&self.checkpoint)?;
        if trusted_block.height() < checkpoint_block.height() {
            warn!(
                "receive invalid snapshot manifest request from peer={}",
                ctx.peer
            );
            return None;
        }
        let mut loop_cnt = if checkpoint_block.height() == 0 {
            trusted_block.height() - checkpoint_block.height() + 1
        } else {
            trusted_block.height() - checkpoint_block.height()
                + REWARD_EPOCH_COUNT
        };
        if loop_cnt < trusted_block.blame() as u64 + 1 {
            loop_cnt = trusted_block.blame() as u64 + 1;
        }

        let mut state_blame_vec = Vec::with_capacity(loop_cnt as usize);
        let mut receipt_blame_vec = Vec::with_capacity(loop_cnt as usize);
        let mut bloom_blame_vec = Vec::with_capacity(loop_cnt as usize);
        let mut block_hash = trusted_block.hash();
        loop {
            if let Some(exec_info) = ctx
                .manager
                .graph
                .data_man
                .consensus_graph_execution_info_from_db(&block_hash)
            {
                state_blame_vec.push(exec_info.original_deferred_state_root);
                receipt_blame_vec
                    .push(exec_info.original_deferred_receipt_root);
                bloom_blame_vec
                    .push(exec_info.original_deferred_logs_bloom_hash);
                if state_blame_vec.len() == loop_cnt as usize {
                    break;
                }
                if let Some(block) =
                    ctx.manager.graph.data_man.block_header_by_hash(&block_hash)
                {
                    block_hash = block.parent_hash().clone();
                } else {
                    warn!(
                        "failed to find block={} in db, peer={}",
                        block_hash, ctx.peer
                    );
                    return None;
                }
            } else {
                warn!("failed to find ConsensusGraphExecutionInfo for block={} in db, peer={}", block_hash, ctx.peer);
                return None;
            }
        }

        Some((state_blame_vec, receipt_blame_vec, bloom_blame_vec))
    }
}

impl Request for SnapshotManifestRequest {
    fn as_message(&self) -> &dyn Message { self }

    fn as_any(&self) -> &dyn Any { self }

    fn timeout(&self, conf: &ProtocolConfiguration) -> Duration {
        conf.headers_request_timeout
    }

    fn on_removed(&self, _inflight_keys: &KeyContainer) {}

    fn with_inflight(&mut self, _inflight_keys: &KeyContainer) {}

    fn is_empty(&self) -> bool { false }

    fn resend(&self) -> Option<Box<dyn Request>> {
        Some(Box::new(self.clone()))
    }

    fn required_capability(&self) -> Option<DynamicCapability> {
        Some(DynamicCapability::ServeCheckpoint(Some(
            self.checkpoint.clone(),
        )))
    }
}
