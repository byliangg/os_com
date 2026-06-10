#![feature(error_in_core)]

//! S6 (Phase 6) host-side safety probe for unwritten-extent preallocation.
//!
//! Drives the userspace ext4_rs harness against a host image so the on-disk
//! state can be validated with e2fsck/debugfs:
//!
//!   cargo run -p ext4_rs --bin unwritten_probe -- all <image>
//!
//! Scenarios:
//! - seq-append: sequential 4K appends through the preallocated tail
//!   (conversion fast path / left merge); expects one written run plus an
//!   unwritten tail past EOF.
//! - sparse-mid: writes into the middle of an unwritten extent (3-way split)
//!   and across hole boundaries; expects zeros from every unwritten block.
//!
//! Afterwards run `e2fsck -fn <image>` (done by the caller) -- it must report
//! a clean filesystem, and `debugfs -R "extents <file>" <image>` should show
//! the tail extents flagged Uninit.

use std::{
    env,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    process,
    sync::{Arc, Mutex},
};

use ext4_rs::{BlockDevice, Ext4, InodeFileType, BLOCK_SIZE, EXT4_ROOT_INODE};

#[derive(Debug)]
struct FileBackedDisk {
    file: Mutex<std::fs::File>,
}

impl FileBackedDisk {
    fn open(path: &str) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|err| panic!("failed to open {}: {}", path, err));
        Self {
            file: Mutex::new(file),
        }
    }
}

impl BlockDevice for FileBackedDisk {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(offset as u64)).unwrap();
        let mut buf = vec![0u8; BLOCK_SIZE];
        file.read_exact(&mut buf).unwrap();
        buf
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(offset as u64)).unwrap();
        file.write_all(data).unwrap();
        file.flush().unwrap();
    }

    fn sync(&self) -> core::result::Result<(), ext4_rs::Ext4Error> {
        let file = self.file.lock().unwrap();
        file.sync_all().map_err(|_| {
            ext4_rs::Ext4Error::with_message(ext4_rs::Errno::EIO, "file sync failed")
        })?;
        Ok(())
    }
}

fn create_file(ext4: &Ext4, name: &str) -> u32 {
    let mode = InodeFileType::S_IFREG.bits() | 0o644;
    let inode_ref = ext4
        .create(EXT4_ROOT_INODE, name, mode)
        .unwrap_or_else(|err| panic!("create {} failed: {:?}", name, err));
    inode_ref.inode_num
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BlockState {
    Hole,
    Unwritten,
    Written,
}

fn block_state(ext4: &Ext4, ino: u32, lblock: u32) -> BlockState {
    let inode_ref = ext4.get_inode_ref(ino);
    match ext4.get_pblock_idx_state(&inode_ref, lblock) {
        Ok((_, true)) => BlockState::Unwritten,
        Ok((_, false)) => BlockState::Written,
        Err(e) if e.error() == ext4_rs::Errno::ENOENT => BlockState::Hole,
        Err(e) => panic!("get_pblock_idx_state(ino={}, lblock={}) failed: {:?}", ino, lblock, e),
    }
}

fn expect_state(ext4: &Ext4, ino: u32, lblock: u32, expected: BlockState, what: &str) {
    let actual = block_state(ext4, ino, lblock);
    assert_eq!(
        actual, expected,
        "{}: lblock {} expected {:?}, got {:?}",
        what, lblock, expected, actual
    );
}

fn expect_read(ext4: &Ext4, ino: u32, offset: usize, len: usize, fill: u8, what: &str) {
    let mut buf = vec![0xEEu8; len];
    let read = ext4
        .read_at(ino, offset, &mut buf)
        .unwrap_or_else(|err| panic!("{}: read_at({}, {}) failed: {:?}", what, offset, len, err));
    assert_eq!(read, len, "{}: short read at {}: {} < {}", what, offset, read, len);
    for (i, b) in buf.iter().enumerate() {
        assert_eq!(
            *b, fill,
            "{}: offset {} byte {} expected {:#x}, got {:#x}",
            what, offset, i, fill, *b
        );
    }
}

fn scenario_seq_append(ext4: &Ext4) {
    const APPENDS: usize = 200;
    let ino = create_file(ext4, "seq_append.bin");
    let data = vec![0x41u8; BLOCK_SIZE];

    for i in 0..APPENDS {
        let written = ext4
            .write_at(ino, i * BLOCK_SIZE, &data)
            .unwrap_or_else(|err| panic!("append {} failed: {:?}", i, err));
        assert_eq!(written, BLOCK_SIZE);
    }

    let inode_ref = ext4.get_inode_ref(ino);
    assert_eq!(inode_ref.inode.size() as usize, APPENDS * BLOCK_SIZE);

    for i in 0..APPENDS as u32 {
        expect_state(ext4, ino, i, BlockState::Written, "seq-append body");
    }
    // The final allocation run preallocated past EOF; at least the block right
    // after EOF must be an unwritten extent, never a written one.
    let tail_state = block_state(ext4, ino, APPENDS as u32);
    assert_eq!(
        tail_state,
        BlockState::Unwritten,
        "seq-append: block following EOF should be preallocated unwritten"
    );
    let mut tail_blocks = 0u32;
    let mut lb = APPENDS as u32;
    while block_state(ext4, ino, lb) == BlockState::Unwritten {
        tail_blocks += 1;
        lb += 1;
    }
    println!("seq-append: OK ({} body blocks written, {} unwritten tail blocks past EOF)", APPENDS, tail_blocks);

    expect_read(ext4, ino, 0, BLOCK_SIZE, 0x41, "seq-append head");
    expect_read(ext4, ino, (APPENDS - 1) * BLOCK_SIZE, BLOCK_SIZE, 0x41, "seq-append last");
}

fn scenario_sparse_mid(ext4: &Ext4) {
    let ino = create_file(ext4, "sparse_mid.bin");
    let data_b = vec![0xB1u8; BLOCK_SIZE];
    let data_c = vec![0xC2u8; BLOCK_SIZE];
    let data_d = vec![0xD3u8; BLOCK_SIZE];

    // Block 0: first append; preallocates an unwritten run after it.
    assert_eq!(ext4.write_at(ino, 0, &data_b).unwrap(), BLOCK_SIZE);
    // Block 50: far sparse write, leaves [1..32) unwritten + holes inside i_size.
    assert_eq!(ext4.write_at(ino, 50 * BLOCK_SIZE, &data_c).unwrap(), BLOCK_SIZE);

    let inode_ref = ext4.get_inode_ref(ino);
    assert_eq!(inode_ref.inode.size() as usize, 51 * BLOCK_SIZE);

    expect_state(ext4, ino, 0, BlockState::Written, "sparse body");
    expect_state(ext4, ino, 1, BlockState::Unwritten, "sparse prealloc");
    expect_state(ext4, ino, 50, BlockState::Written, "sparse far block");

    // Everything between block 1 and block 49 must read as zeros, whether it
    // is an unwritten preallocated block or a plain hole.
    expect_read(ext4, ino, BLOCK_SIZE, 49 * BLOCK_SIZE, 0x00, "sparse zeros");
    expect_read(ext4, ino, 50 * BLOCK_SIZE, BLOCK_SIZE, 0xC2, "sparse far data");

    // Write into the middle of the unwritten extent: 3-way split
    // (unwritten head | written piece | unwritten tail).
    let mid = 5u32;
    assert_eq!(ext4.write_at(ino, mid as usize * BLOCK_SIZE, &data_d).unwrap(), BLOCK_SIZE);
    expect_state(ext4, ino, mid - 1, BlockState::Unwritten, "split head");
    expect_state(ext4, ino, mid, BlockState::Written, "split mid");
    expect_state(ext4, ino, mid + 1, BlockState::Unwritten, "split tail");
    expect_read(ext4, ino, (mid as usize) * BLOCK_SIZE, BLOCK_SIZE, 0xD3, "split mid data");
    expect_read(ext4, ino, BLOCK_SIZE, (mid as usize - 1) * BLOCK_SIZE, 0x00, "split head zeros");
    expect_read(ext4, ino, (mid as usize + 1) * BLOCK_SIZE, BLOCK_SIZE, 0x00, "split tail zeros");

    // Sequential conversion through the unwritten head: exercises the
    // left-merge fast path (block 1 has no written left neighbour in-extent,
    // blocks 2..5 merge into the growing written run).
    for lb in 1..mid {
        assert_eq!(ext4.write_at(ino, lb as usize * BLOCK_SIZE, &data_d).unwrap(), BLOCK_SIZE);
    }
    for lb in 1..=mid {
        expect_state(ext4, ino, lb, BlockState::Written, "merge run");
    }
    expect_state(ext4, ino, mid + 1, BlockState::Unwritten, "merge run tail");
    expect_read(ext4, ino, BLOCK_SIZE, (mid as usize) * BLOCK_SIZE, 0xD3, "merge run data");
    expect_read(ext4, ino, 0, BLOCK_SIZE, 0xB1, "block 0 intact");

    println!("sparse-mid: OK (split + left-merge conversions verified)");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 || args[1] != "all" {
        eprintln!("usage: unwritten_probe all <image>");
        process::exit(2);
    }
    let image = &args[2];
    let disk = Arc::new(FileBackedDisk::open(image));
    let ext4 = Ext4::open(disk);

    scenario_seq_append(&ext4);
    scenario_sparse_mid(&ext4);
    println!("unwritten_probe: all scenarios passed");
}
