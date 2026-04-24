use crate::prelude::*;
use super::runtime_block_size;

pub trait BlockDevice: Send + Sync + Any {
    fn read_offset(&self, offset: usize) -> Vec<u8>;
    fn read_offset_into(&self, offset: usize, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }

        let data = self.read_offset(offset);
        let copy_len = core::cmp::min(out.len(), data.len());
        out[..copy_len].copy_from_slice(&data[..copy_len]);
        if copy_len < out.len() {
            out[copy_len..].fill(0);
        }
    }
    fn write_offset(&self, offset: usize, data: &[u8]);
}

pub trait MetadataWriter: Send + Sync {
    fn write_metadata(&self, offset: usize, data: &[u8]);
}

pub struct PassthroughMetadataWriter {
    block_device: Arc<dyn BlockDevice>,
}

impl PassthroughMetadataWriter {
    pub fn new(block_device: Arc<dyn BlockDevice>) -> Self {
        Self { block_device }
    }
}

impl MetadataWriter for PassthroughMetadataWriter {
    fn write_metadata(&self, offset: usize, data: &[u8]) {
        self.block_device.write_offset(offset, data);
    }
}

pub struct Block {
    pub disk_offset: usize,
    pub data: Vec<u8>,
}

impl Block {
    /// Load the block from the disk.
    pub fn load(block_device: &Arc<dyn BlockDevice>, offset: usize) -> Self {
        let block_size = runtime_block_size();
        let mut data = block_device.read_offset(offset);
        if data.len() < block_size {
            data.resize(block_size, 0);
        } else if data.len() > block_size {
            data.truncate(block_size);
        }
        Block {
            disk_offset: offset,
            data,
        }
    }

    /// Load the block from inode block
    pub fn load_inode_root_block(data: &[u32; 15]) -> Self {
        let data_bytes: &[u8; 60] = unsafe {
            core::mem::transmute(data)
        };
        Block {
            disk_offset: 0, 
            data: data_bytes.to_vec(),
        }
    }

    /// Read the block as a specific type.
    pub fn read_as<T: Copy>(&self) -> T {
        unsafe {
            let ptr = self.data.as_ptr() as *const T;
            ptr.read_unaligned()
        }
    }

    /// Read the block as a specific type at a specific offset.
    pub fn read_offset_as<T: Copy>(&self, offset: usize) -> T {
        unsafe {
            let ptr = self.data.as_ptr().add(offset) as *const T;
            ptr.read_unaligned()
        }
    }

    /// Read the block as a specific type mutably.
    pub fn read_as_mut<T: Copy>(&mut self) -> &mut T {
        unsafe {
            let ptr = self.data.as_mut_ptr() as *mut T;
            &mut *ptr
        }
    }

    /// Read the block as a specific type mutably at a specific offset.
    pub fn read_offset_as_mut<T: Copy>(&mut self, offset: usize) -> &mut T {
        unsafe {
            let ptr = self.data.as_mut_ptr().add(offset) as *mut T;
            &mut *ptr
        }
    }

    /// Write data to the block starting at a specific offset.
    pub fn write_offset(&mut self, offset: usize, data: &[u8], len: usize) {
        let end = offset + len;
        if end <= self.data.len() {
            let slice_end = len.min(data.len());
            self.data[offset..end].copy_from_slice(&data[..slice_end]);
        } else {
            panic!("Write would overflow the block buffer");
        }
    }
}

impl Block{
    pub fn sync_blk_to_disk(&self, metadata_writer: &Arc<dyn MetadataWriter>){
        metadata_writer.write_metadata(self.disk_offset, &self.data);
    }
}
