use crate::ext4_defs::*;
use crate::prelude::*;
use crate::return_errno_with_message;

use super::JournalDevice;

#[derive(Debug, Clone)]
pub struct JournalSuperblockState {
    raw: JournalSuperblock,
}

impl JournalSuperblockState {
    pub fn load(
        block_device: &Arc<dyn BlockDevice>,
        device: &JournalDevice,
        expected_uuid: Option<[u8; 16]>,
    ) -> Result<Self> {
        let raw_bytes = device.read_raw_block(block_device, 0)?;
        if raw_bytes.len() < JBD2_SUPERBLOCK_SIZE {
            return_errno_with_message!(Errno::EIO, "short journal superblock read");
        }

        let raw = unsafe {
            (raw_bytes.as_ptr() as *const JournalSuperblock).read_unaligned()
        };
        let state = Self { raw };
        state.validate(device, expected_uuid)?;
        Ok(state)
    }

    pub fn validate(
        &self,
        device: &JournalDevice,
        expected_uuid: Option<[u8; 16]>,
    ) -> Result<()> {
        if !self.raw.s_header.is_valid_magic() {
            return_errno_with_message!(Errno::EINVAL, "invalid JBD2 magic");
        }
        if self.raw.s_header.blocktype() != JBD2_SUPERBLOCK_V2 {
            return_errno_with_message!(Errno::EINVAL, "unsupported JBD2 superblock version");
        }
        if self.block_size() != device.fs_block_size() {
            return_errno_with_message!(Errno::EINVAL, "journal block size mismatch");
        }
        if self.maxlen() == 0 || self.maxlen() > device.logical_blocks() {
            return_errno_with_message!(Errno::EINVAL, "journal maxlen out of range");
        }
        if self.first() == 0 || self.first() >= self.maxlen() {
            return_errno_with_message!(Errno::EINVAL, "journal first block out of range");
        }
        if self.start() >= self.maxlen() {
            return_errno_with_message!(Errno::EINVAL, "journal start out of range");
        }
        if self.head() >= self.maxlen() && self.head() != 0 {
            return_errno_with_message!(Errno::EINVAL, "journal head out of range");
        }

        if let Some(expected_uuid) = expected_uuid {
            if expected_uuid.iter().any(|byte| *byte != 0) && self.uuid() != expected_uuid {
                return_errno_with_message!(Errno::EINVAL, "journal UUID mismatch");
            }
        }

        if self.has_checksum_v2_or_v3() && self.checksum() != self.raw.compute_checksum() {
            return_errno_with_message!(Errno::EINVAL, "journal superblock checksum mismatch");
        }

        Ok(())
    }

    pub fn raw(&self) -> &JournalSuperblock {
        &self.raw
    }

    pub fn block_size(&self) -> u32 {
        self.raw.blocksize()
    }

    pub fn maxlen(&self) -> u32 {
        self.raw.maxlen()
    }

    pub fn first(&self) -> u32 {
        self.raw.first()
    }

    pub fn sequence(&self) -> u32 {
        self.raw.sequence()
    }

    pub fn start(&self) -> u32 {
        self.raw.start()
    }

    pub fn head(&self) -> u32 {
        self.raw.head()
    }

    pub fn errno(&self) -> u32 {
        self.raw.errno()
    }

    pub fn feature_compat(&self) -> u32 {
        self.raw.feature_compat()
    }

    pub fn feature_incompat(&self) -> u32 {
        self.raw.feature_incompat()
    }

    pub fn feature_ro_compat(&self) -> u32 {
        self.raw.feature_ro_compat()
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.raw.uuid()
    }

    pub fn checksum_type(&self) -> u8 {
        self.raw.checksum_type()
    }

    pub fn num_fc_blocks(&self) -> u32 {
        self.raw.num_fc_blocks()
    }

    pub fn checksum(&self) -> u32 {
        self.raw.checksum()
    }

    pub fn has_checksum_v2_or_v3(&self) -> bool {
        let incompat = self.feature_incompat();
        (incompat & (JBD2_FEATURE_INCOMPAT_CSUM_V2 | JBD2_FEATURE_INCOMPAT_CSUM_V3)) != 0
    }

    pub fn has_incompat_feature(&self, feature: u32) -> bool {
        (self.feature_incompat() & feature) != 0
    }

    pub fn update_sequence(&mut self, sequence: u32) {
        self.raw.set_sequence(sequence);
        self.raw.update_checksum();
    }

    pub fn update_start(&mut self, start: u32) {
        self.raw.set_start(start);
        self.raw.update_checksum();
    }

    pub fn update_head(&mut self, head: u32) {
        self.raw.set_head(head);
        self.raw.update_checksum();
    }

    pub fn store(
        &self,
        block_device: &Arc<dyn BlockDevice>,
        device: &JournalDevice,
    ) -> Result<()> {
        let bytes = self.raw.to_bytes();
        device.write_block(block_device, 0, &bytes)
    }
}
