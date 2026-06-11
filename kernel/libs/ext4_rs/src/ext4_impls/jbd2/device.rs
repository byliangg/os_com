use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;

#[derive(Debug, Clone)]
pub struct JournalDevice {
    journal_inode: u32,
    fs_block_size: u32,
    inode_size_bytes: u64,
    logical_blocks: u32,
    physical_blocks: Vec<Ext4Fsblk>,
}

impl JournalDevice {
    pub fn load(ext4: &Ext4, journal_inode: u32) -> Result<Self> {
        let inode_ref = ext4.get_inode_ref(journal_inode);
        let fs_block_size = ext4.super_block.block_size();
        let inode_size_bytes = ext4.super_block.inode_size_file(&inode_ref.inode);
        if inode_size_bytes == 0 {
            return_errno_with_message!(Errno::EINVAL, "journal inode is empty");
        }

        let block_size_u64 = fs_block_size as u64;
        let logical_blocks_u64 = inode_size_bytes
            .checked_add(block_size_u64 - 1)
            .ok_or_else(|| Ext4Error::with_message(Errno::EINVAL, "journal inode size overflow"))?
            / block_size_u64;
        let logical_blocks = u32::try_from(logical_blocks_u64)
            .map_err(|_| Ext4Error::with_message(Errno::EINVAL, "journal inode too large"))?;
        if logical_blocks < 2 {
            return_errno_with_message!(Errno::EINVAL, "journal inode too small");
        }

        let mut physical_blocks = Vec::with_capacity(logical_blocks as usize);
        for lblock in 0..logical_blocks {
            let pblock = ext4.get_pblock_idx(&inode_ref, lblock)?;
            physical_blocks.push(pblock);
        }

        Ok(Self {
            journal_inode,
            fs_block_size,
            inode_size_bytes,
            logical_blocks,
            physical_blocks,
        })
    }

    pub fn journal_inode(&self) -> u32 {
        self.journal_inode
    }

    pub fn fs_block_size(&self) -> u32 {
        self.fs_block_size
    }

    pub fn inode_size_bytes(&self) -> u64 {
        self.inode_size_bytes
    }

    pub fn logical_blocks(&self) -> u32 {
        self.logical_blocks
    }

    pub fn physical_blocks(&self) -> &[Ext4Fsblk] {
        &self.physical_blocks
    }

    pub fn logical_to_physical(&self, logical_block: u32) -> Result<Ext4Fsblk> {
        self.physical_blocks
            .get(logical_block as usize)
            .copied()
            .ok_or_else(|| Ext4Error::with_message(Errno::EINVAL, "journal logical block out of range"))
    }

    pub fn read_raw_block(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        logical_block: u32,
    ) -> Result<Vec<u8>> {
        let physical_block = self.logical_to_physical(logical_block)?;
        let offset = physical_block
            .checked_mul(self.fs_block_size as u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| Ext4Error::with_message(Errno::EINVAL, "journal block offset overflow"))?;
        let block = Block::load(block_device, offset, self.fs_block_size as usize);
        Ok(block.data)
    }

    /// P3 (Phase 6): writes a sequence of journal blocks, coalescing runs
    /// that are consecutive in BOTH logical journal order and physical disk
    /// placement into single large writes (one bio each, instead of one
    /// synchronous virtio round trip per block). mkfs journals are typically
    /// fully contiguous, so a whole commit (descriptor + N metadata blocks)
    /// usually lands as one write.
    pub fn write_blocks_coalesced(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        blocks: &[(u32, &[u8])],
    ) -> Result<()> {
        let block_size = self.fs_block_size as usize;
        let mut i = 0usize;
        while i < blocks.len() {
            let (run_logical, _) = blocks[i];
            let run_phys = self.logical_to_physical(run_logical)?;
            let mut run_len = 1usize;
            while i + run_len < blocks.len() {
                let (next_logical, _) = blocks[i + run_len];
                if next_logical != run_logical.wrapping_add(run_len as u32) {
                    break;
                }
                let next_phys = self.logical_to_physical(next_logical)?;
                if next_phys != run_phys + run_len as u64 {
                    break;
                }
                run_len += 1;
            }

            if run_len == 1 {
                let (logical, data) = blocks[i];
                self.write_block(block_device, logical, data)?;
            } else {
                let mut buf = vec![0u8; run_len * block_size];
                for (j, (_, data)) in blocks[i..i + run_len].iter().enumerate() {
                    if data.len() > block_size {
                        return_errno_with_message!(
                            Errno::EINVAL,
                            "journal block write too large"
                        );
                    }
                    buf[j * block_size..j * block_size + data.len()].copy_from_slice(data);
                }
                let offset = run_phys
                    .checked_mul(self.fs_block_size as u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| {
                        Ext4Error::with_message(Errno::EINVAL, "journal block offset overflow")
                    })?;
                block_device.write_offset(offset, &buf);
            }
            i += run_len;
        }
        Ok(())
    }

    pub fn write_block(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        logical_block: u32,
        data: &[u8],
    ) -> Result<()> {
        if data.len() > self.fs_block_size as usize {
            return_errno_with_message!(Errno::EINVAL, "journal block write too large");
        }

        let physical_block = self.logical_to_physical(logical_block)?;
        let offset = physical_block
            .checked_mul(self.fs_block_size as u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| Ext4Error::with_message(Errno::EINVAL, "journal block offset overflow"))?;

        let mut block = vec![0u8; self.fs_block_size as usize];
        block[..data.len()].copy_from_slice(data);
        block_device.write_offset(offset, &block);
        Ok(())
    }
}
