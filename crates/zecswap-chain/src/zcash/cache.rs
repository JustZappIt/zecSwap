use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use zcash_client_backend::data_api::chain::{BlockCache, BlockSource, error::Error as ChainError};
use zcash_client_backend::data_api::scanning::ScanRange;
use zcash_client_backend::proto::compact_formats::CompactBlock;
use zcash_protocol::consensus::BlockHeight;

/// Holds downloaded compact blocks only between download and scan.
#[derive(Default)]
pub(crate) struct MemoryBlockCache(Mutex<BTreeMap<BlockHeight, CompactBlock>>);

impl MemoryBlockCache {
    fn blocks(&self) -> MutexGuard<'_, BTreeMap<BlockHeight, CompactBlock>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl BlockSource for MemoryBlockCache {
    type Error = Infallible;

    fn with_blocks<F, WalletErrT>(
        &self,
        from_height: Option<BlockHeight>,
        limit: Option<usize>,
        with_block: F,
    ) -> Result<(), ChainError<WalletErrT, Self::Error>>
    where
        F: FnMut(CompactBlock) -> Result<(), ChainError<WalletErrT, Self::Error>>,
    {
        let blocks: Vec<CompactBlock> = self
            .blocks()
            .range(from_height.unwrap_or(BlockHeight::from_u32(0))..)
            .take(limit.unwrap_or(usize::MAX))
            .map(|(_, block)| block.clone())
            .collect();
        blocks.into_iter().try_for_each(with_block)
    }
}

#[async_trait]
impl BlockCache for MemoryBlockCache {
    fn get_tip_height(
        &self,
        range: Option<&ScanRange>,
    ) -> Result<Option<BlockHeight>, Self::Error> {
        let blocks = self.blocks();
        Ok(match range {
            Some(range) => blocks.range(range.block_range().clone()).next_back(),
            None => blocks.iter().next_back(),
        }
        .map(|(height, _)| *height))
    }

    async fn read(&self, range: &ScanRange) -> Result<Vec<CompactBlock>, Self::Error> {
        Ok(self
            .blocks()
            .range(range.block_range().clone())
            .map(|(_, block)| block.clone())
            .collect())
    }

    async fn insert(&self, compact_blocks: Vec<CompactBlock>) -> Result<(), Self::Error> {
        let mut blocks = self.blocks();
        for block in compact_blocks {
            blocks.insert(block.height(), block);
        }
        Ok(())
    }

    async fn delete(&self, range: ScanRange) -> Result<(), Self::Error> {
        self.blocks()
            .retain(|height, _| !range.block_range().contains(height));
        Ok(())
    }
}
