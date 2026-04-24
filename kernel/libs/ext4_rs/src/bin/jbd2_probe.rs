#![feature(error_in_core)]

use std::{
    env,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    process,
    sync::{Arc, Mutex},
};

use ext4_rs::{BlockDevice, Ext4};

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

        let mut buf = vec![0u8; ext4_rs::BLOCK_SIZE];
        file.read_exact(&mut buf).unwrap();
        buf
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        let mut file = self.file.lock().unwrap();
        file.seek(SeekFrom::Start(offset as u64)).unwrap();
        file.write_all(data).unwrap();
        file.flush().unwrap();
    }
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  cargo run -p ext4_rs --bin jbd2_probe -- show-super <image>");
    eprintln!("  cargo run -p ext4_rs --bin jbd2_probe -- write-probe-tx <image> [target_fs_block]");
    eprintln!("  cargo run -p ext4_rs --bin jbd2_probe -- recover <image>");
    process::exit(2);
}

fn open_fs(image: &str) -> (Arc<FileBackedDisk>, Ext4) {
    let disk = Arc::new(FileBackedDisk::open(image));
    let ext4 = Ext4::open(disk.clone());
    (disk, ext4)
}

fn cmd_show_super(image: &str) {
    let (_disk, ext4) = open_fs(image);
    let journal = ext4
        .load_journal()
        .unwrap_or_else(|err| panic!("failed to load journal: {:?}", err))
        .unwrap_or_else(|| panic!("filesystem does not advertise HAS_JOURNAL"));

    println!("image={}", image);
    println!("fs_block_size={}", ext4.super_block.block_size());
    println!("fs_has_journal={}", ext4.super_block.has_journal());
    println!("fs_needs_recovery={}", ext4.super_block.needs_recovery());
    println!("journal_inode={}", journal.device.journal_inode());
    println!("journal_inode_bytes={}", journal.device.inode_size_bytes());
    println!("journal_mapped_blocks={}", journal.device.logical_blocks());
    println!("journal_block_size={}", journal.superblock.block_size());
    println!("journal_maxlen={}", journal.superblock.maxlen());
    println!("journal_first={}", journal.superblock.first());
    println!("journal_sequence={}", journal.superblock.sequence());
    println!("journal_start={}", journal.superblock.start());
    println!("journal_head={}", journal.superblock.head());
    println!("journal_errno={}", journal.superblock.errno());
    println!("journal_feature_compat=0x{:x}", journal.superblock.feature_compat());
    println!(
        "journal_feature_incompat=0x{:x}",
        journal.superblock.feature_incompat()
    );
    println!(
        "journal_feature_ro_compat=0x{:x}",
        journal.superblock.feature_ro_compat()
    );
    println!("journal_checksum_type={}", journal.superblock.checksum_type());
    println!("journal_num_fc_blocks={}", journal.superblock.num_fc_blocks());
    println!("journal_free_blocks={}", journal.space.free_blocks());
}

fn cmd_write_probe_tx(image: &str, target_fs_block: u64) {
    let (_disk, mut ext4) = open_fs(image);
    let block_size = ext4.super_block.block_size() as usize;
    let mut data = vec![0u8; block_size];
    ext4.block_device
        .read_offset_into((target_fs_block as usize) * block_size, &mut data);

    let mut journal = ext4
        .load_journal()
        .unwrap_or_else(|err| panic!("failed to load journal: {:?}", err))
        .unwrap_or_else(|| panic!("filesystem does not advertise HAS_JOURNAL"));

    let write = journal
        .write_probe_transaction(&ext4.block_device, target_fs_block, &data)
        .unwrap_or_else(|err| panic!("failed to write probe transaction: {:?}", err));

    ext4.super_block.set_needs_recovery(true);
    ext4.super_block.sync_to_disk_with_csum(&ext4.metadata_writer);

    println!("image={}", image);
    println!("target_fs_block={}", target_fs_block);
    println!("sequence={}", write.sequence);
    println!("descriptor_block={}", write.descriptor_block);
    println!("data_block={}", write.data_block);
    println!("commit_block={}", write.commit_block);
    println!("next_head={}", write.next_head);
    println!("needs_recovery=true");
}

fn cmd_recover(image: &str) {
    let (_disk, mut ext4) = open_fs(image);
    let mut journal = ext4
        .load_journal()
        .unwrap_or_else(|err| panic!("failed to load journal: {:?}", err))
        .unwrap_or_else(|| panic!("filesystem does not advertise HAS_JOURNAL"));

    let result = journal
        .recover(&ext4.block_device)
        .unwrap_or_else(|err| panic!("failed to recover journal: {:?}", err));

    ext4.super_block.set_needs_recovery(false);
    ext4.super_block.sync_to_disk_with_csum(&ext4.metadata_writer);

    println!("image={}", image);
    println!("transactions_replayed={}", result.transactions_replayed);
    println!("metadata_blocks_replayed={}", result.metadata_blocks_replayed);
    println!("revoked_blocks={}", result.revoked_blocks);
    println!("last_sequence={:?}", result.last_sequence);
    println!("needs_recovery=false");
}

fn main() {
    let mut args = env::args();
    let _program = args.next();
    let Some(command) = args.next() else {
        usage();
    };
    let Some(image) = args.next() else {
        usage();
    };

    match command.as_str() {
        "show-super" => {
            cmd_show_super(&image);
        }
        "write-probe-tx" => {
            let target_fs_block = args
                .next()
                .map(|s| s.parse::<u64>().unwrap())
                .unwrap_or(0);
            cmd_write_probe_tx(&image, target_fs_block);
        }
        "recover" => {
            cmd_recover(&image);
        }
        _ => usage(),
    }
}
