use std::{cmp::Ordering, fmt::Debug, sync::Arc};

use rethnet_eth::{
    block::{BlockOptions, Header},
    Address, B256, B64, U256,
};
use revm::primitives::{CfgEnv, ExecutionResult, InvalidTransaction, SpecId};

use crate::{
    block::BlockBuilderCreationError,
    blockchain::SyncBlockchain,
    mempool::OrderedTransaction,
    state::SyncState,
    trace::{Trace, TraceCollector},
    BlockBuilder, BlockTransactionError, BuildBlockResult, MemPool, PendingTransaction, SyncBlock,
};

/// The result of mining a block.
pub struct MineBlockResult<BlockchainErrorT, StateErrorT> {
    /// Mined block
    pub block: Arc<dyn SyncBlock<Error = BlockchainErrorT>>,
    /// State after mining the block
    pub state: Box<dyn SyncState<StateErrorT>>,
    /// Transaction results
    pub transaction_results: Vec<ExecutionResult>,
    /// Transaction traces
    pub transaction_traces: Vec<Trace>,
}

/// The type of ordering to use when selecting blocks to mine.
#[derive(Debug)]
pub enum MineOrdering {
    /// Insertion order
    Fifo,
    /// Effective miner fee
    Priority,
}

/// An error that occurred while mining a block.
#[derive(Debug, thiserror::Error)]
pub enum MineBlockError<BE, SE> {
    /// An error that occurred while aborting the block builder.
    #[error(transparent)]
    BlockAbort(SE),
    /// An error that occurred while constructing a block builder.
    #[error(transparent)]
    BlockBuilderCreation(#[from] BlockBuilderCreationError),
    /// An error that occurred while executing a transaction.
    #[error(transparent)]
    BlockTransaction(#[from] BlockTransactionError<BE, SE>),
    /// An error that occurred while finalizing a block.
    #[error(transparent)]
    BlockFinalize(SE),
    /// A blockchain error
    #[error(transparent)]
    Blockchain(BE),
    /// An error that occurred while updating the mempool.
    #[error(transparent)]
    MemPoolUpdate(SE),
    /// The block is expected to have a prevrandao, as the executor's config is on a post-merge hardfork.
    #[error("Post-merge transaction is missing prevrandao")]
    MissingPrevrandao,
}

/// Mines a block using as many transactions as can fit in it.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(feature = "tracing", tracing::instrument(skip_all))]
pub async fn mine_block<BlockchainErrorT, StateErrorT>(
    blockchain: &mut dyn SyncBlockchain<BlockchainErrorT, StateErrorT>,
    mut state: Box<dyn SyncState<StateErrorT>>,
    mem_pool: &mut MemPool,
    cfg: &CfgEnv,
    timestamp: U256,
    beneficiary: Address,
    min_gas_price: U256,
    mine_ordering: MineOrdering,
    reward: U256,
    base_fee: Option<U256>,
    prevrandao: Option<B256>,
) -> Result<
    MineBlockResult<BlockchainErrorT, StateErrorT>,
    MineBlockError<BlockchainErrorT, StateErrorT>,
>
where
    BlockchainErrorT: Debug + Send + 'static,
    StateErrorT: Debug + Send + 'static,
{
    let parent_block = blockchain
        .last_block()
        .await
        .map_err(MineBlockError::Blockchain)?;

    let parent_header = parent_block.header();
    let base_fee = if cfg.spec_id >= SpecId::LONDON {
        Some(base_fee.unwrap_or_else(|| calculate_next_base_fee(parent_header)))
    } else {
        None
    };

    let mut block_builder = BlockBuilder::new(
        cfg.clone(),
        parent_header,
        BlockOptions {
            beneficiary: Some(beneficiary),
            number: Some(parent_header.number + U256::from(1)),
            gas_limit: Some(*mem_pool.block_gas_limit()),
            timestamp: Some(timestamp),
            mix_hash: if cfg.spec_id >= SpecId::MERGE {
                Some(prevrandao.ok_or(MineBlockError::MissingPrevrandao)?)
            } else {
                None
            },
            nonce: Some(if cfg.spec_id >= SpecId::MERGE {
                B64::ZERO
            } else {
                B64::from_limbs([66u64.to_be()])
            }),
            base_fee,
            ..Default::default()
        },
    )?;

    let mut pending_transactions = {
        type MineOrderComparator =
            dyn Fn(&OrderedTransaction, &OrderedTransaction) -> Ordering + Send;

        let comparator: Box<MineOrderComparator> = match mine_ordering {
            MineOrdering::Fifo => Box::new(|lhs, rhs| lhs.order_id().cmp(&rhs.order_id())),
            MineOrdering::Priority => Box::new(move |lhs, rhs| {
                let effective_miner_fee = |transaction: &PendingTransaction| {
                    let max_fee_per_gas = transaction.gas_price();
                    let max_priority_fee_per_gas = transaction
                        .max_priority_fee_per_gas()
                        .unwrap_or(max_fee_per_gas);

                    base_fee.map_or(max_fee_per_gas, |base_fee| {
                        max_priority_fee_per_gas.min(max_fee_per_gas - base_fee)
                    })
                };

                // Invert lhs and rhs to get decreasing order by effective miner fee
                let ordering = effective_miner_fee(rhs.transaction())
                    .cmp(&effective_miner_fee(lhs.transaction()));

                // If two txs have the same effective miner fee we want to sort them
                // in increasing order by orderId
                if ordering == Ordering::Equal {
                    lhs.order_id().cmp(&rhs.order_id())
                } else {
                    ordering
                }
            }),
        };

        mem_pool.iter(comparator)
    };

    let mut results = Vec::new();
    let mut traces = Vec::new();

    while let Some(transaction) = pending_transactions.next() {
        let mut tracer = TraceCollector::default();

        if transaction.gas_price() < min_gas_price {
            pending_transactions.remove_caller(transaction.caller());
            continue;
        }

        let caller = *transaction.caller();
        match block_builder.add_transaction(blockchain, &mut state, transaction, Some(&mut tracer))
        {
            Err(
                BlockTransactionError::ExceedsBlockGasLimit
                | BlockTransactionError::InvalidTransaction(
                    InvalidTransaction::GasPriceLessThanBasefee,
                ),
            ) => {
                pending_transactions.remove_caller(&caller);
                continue;
            }
            Err(e) => {
                return Err(MineBlockError::BlockTransaction(e));
            }
            Ok(result) => {
                results.push(result);
                traces.push(tracer.into_trace());
            }
        }
    }

    let rewards = vec![(beneficiary, reward)];
    let BuildBlockResult { block, state_diff } = block_builder
        .finalize(&mut state, rewards, None)
        .map_err(MineBlockError::BlockFinalize)?;

    let block = blockchain
        .insert_block(block, state_diff)
        .await
        .map_err(MineBlockError::Blockchain)?;

    mem_pool
        .update(&state)
        .map_err(MineBlockError::MemPoolUpdate)?;

    Ok(MineBlockResult {
        block,
        state,
        transaction_results: results,
        transaction_traces: traces,
    })
}

/// Calculates the next base fee for a post-London block, given the parent's header.
///
/// # Panics
///
/// Panics if the parent header does not contain a base fee.
fn calculate_next_base_fee(parent: &Header) -> U256 {
    let elasticity = U256::from(2);
    let base_fee_max_change_denominator = U256::from(8);

    let parent_gas_target = parent.gas_limit / elasticity;
    let parent_base_fee = parent
        .base_fee_per_gas
        .expect("Post-London headers must contain a baseFee");

    match parent.gas_used.cmp(&parent_gas_target) {
        std::cmp::Ordering::Less => {
            let gas_used_delta = parent_gas_target - parent.gas_used;

            let delta = parent_base_fee * gas_used_delta
                / parent_gas_target
                / base_fee_max_change_denominator;

            parent_base_fee.saturating_sub(delta)
        }
        std::cmp::Ordering::Equal => parent_base_fee,
        std::cmp::Ordering::Greater => {
            let gas_used_delta = parent.gas_used - parent_gas_target;

            let delta = parent_base_fee * gas_used_delta
                / parent_gas_target
                / base_fee_max_change_denominator;

            parent_base_fee + delta.max(U256::from(1))
        }
    }
}

#[cfg(test)]
mod tests {
    use itertools::izip;

    use super::*;

    #[test]
    fn test_calculate_next_base_fee() {
        let base_fee = [
            1000000000, 1000000000, 1000000000, 1072671875, 1059263476, 1049238967, 1049238967, 0,
            1, 2,
        ];
        let gas_used = [
            10000000, 10000000, 10000000, 9000000, 10001000, 0, 10000000, 10000000, 10000000,
            10000000,
        ];
        let gas_limit = [
            10000000, 12000000, 14000000, 10000000, 14000000, 2000000, 18000000, 18000000,
            18000000, 18000000,
        ];
        let next_base_fee = [
            1125000000, 1083333333, 1053571428, 1179939062, 1116028649, 918084097, 1063811730, 1,
            2, 3,
        ];

        for (base_fee, gas_used, gas_limit, next_base_fee) in
            izip!(base_fee, gas_used, gas_limit, next_base_fee)
        {
            let parent_header = Header {
                base_fee_per_gas: Some(U256::from(base_fee)),
                gas_used: U256::from(gas_used),
                gas_limit: U256::from(gas_limit),
                ..Default::default()
            };

            assert_eq!(
                U256::from(next_base_fee),
                calculate_next_base_fee(&parent_header)
            );
        }
    }
}