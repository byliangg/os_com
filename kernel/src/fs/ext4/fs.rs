// SPDX-License-Identifier: MPL-2.0

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use aster_block::{BlockDevice, SECTOR_SIZE};
use aster_cmdline::{KCMDLINE, ModuleArg};
use ext4_rs::{
    BLOCK_SIZE as EXT4_BLOCK_SIZE, BlockDevice as Ext4BlockDevice, EXT4_ROOT_INODE, Ext4,
    SimpleDirEntry, SimpleInodeMeta,
};
use ostd::mm::VmIo;

use crate::{
    fs::{
        registry::{FsProperties, FsType},
        utils::{FileSystem, FsEventSubscriberStats, FsFlags, Inode, NAME_MAX, SuperBlock},
    },
    prelude::*,
};

const EXT4_MAGIC: u64 = 0xEF53;
const EXT4_SUPERBLOCK_OFFSET: usize = 1024;
const EXT4_SB_LOG_BLOCK_SIZE_OFFSET: usize = 24;
const EXT4_SB_BLOCKS_PER_GROUP_OFFSET: usize = 32;
const EXT4_SB_INODES_PER_GROUP_OFFSET: usize = 40;
const EXT4_SB_MAGIC_OFFSET: usize = 56;
const EXT4_SB_DESC_SIZE_OFFSET: usize = 254;
const CRASH_JOURNAL_OFFSET: usize = 0;
const CRASH_JOURNAL_MAGIC: u32 = 0x4A42_5232; // "JBR2"
const CRASH_JOURNAL_VERSION: u32 = 1;
const CRASH_JOURNAL_HEADER_SIZE: usize = 24;
const CRASH_JOURNAL_MAX_PAYLOAD: usize = SECTOR_SIZE - CRASH_JOURNAL_HEADER_SIZE;
const CRASH_JOURNAL_STATE_EMPTY: u32 = 0;
const CRASH_JOURNAL_STATE_PREPARED: u32 = 1;
const CRASH_JOURNAL_STATE_COMMITTED: u32 = 2;
const CRASH_JOURNAL_OP_CREATE: u32 = 1;
const CRASH_JOURNAL_OP_MKDIR: u32 = 2;
const CRASH_JOURNAL_OP_UNLINK: u32 = 3;
const CRASH_JOURNAL_OP_RMDIR: u32 = 4;
const CRASH_JOURNAL_OP_RENAME: u32 = 5;
const CRASH_JOURNAL_OP_WRITE: u32 = 6;
const CRASH_JOURNAL_OP_TRUNCATE: u32 = 7;
const CRASH_JOURNAL_MAX_WRITE_BYTES: usize = 192;

#[derive(Clone, Debug)]
enum CrashJournalOp {
    Create {
        parent: u32,
        mode: u16,
        name: String,
    },
    Mkdir {
        parent: u32,
        mode: u16,
        name: String,
    },
    Unlink {
        parent: u32,
        name: String,
    },
    Rmdir {
        parent: u32,
        name: String,
    },
    Rename {
        old_parent: u32,
        old_name: String,
        new_parent: u32,
        new_name: String,
    },
    Write {
        ino: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Truncate {
        ino: u32,
        new_size: u64,
    },
}

#[derive(Clone, Debug)]
struct CrashJournalRecord {
    state: u32,
    op: u32,
    payload: Vec<u8>,
}

impl CrashJournalOp {
    fn for_small_write(ino: u32, offset: usize, data: &[u8]) -> Option<Self> {
        if data.is_empty() || data.len() > CRASH_JOURNAL_MAX_WRITE_BYTES {
            return None;
        }
        Some(Self::Write {
            ino,
            offset: offset as u64,
            data: data.to_vec(),
        })
    }

    fn encode(&self) -> Option<(u32, Vec<u8>)> {
        fn push_u16(dst: &mut Vec<u8>, value: u16) {
            dst.extend_from_slice(&value.to_le_bytes());
        }

        fn push_u32(dst: &mut Vec<u8>, value: u32) {
            dst.extend_from_slice(&value.to_le_bytes());
        }

        fn push_u64(dst: &mut Vec<u8>, value: u64) {
            dst.extend_from_slice(&value.to_le_bytes());
        }

        fn push_name(dst: &mut Vec<u8>, name: &str) -> Option<()> {
            let name_bytes = name.as_bytes();
            let len = u16::try_from(name_bytes.len()).ok()?;
            push_u16(dst, len);
            dst.extend_from_slice(name_bytes);
            Some(())
        }

        let mut payload = Vec::new();
        let op = match self {
            Self::Create { parent, mode, name } => {
                push_u32(&mut payload, *parent);
                push_u16(&mut payload, *mode);
                push_name(&mut payload, name)?;
                CRASH_JOURNAL_OP_CREATE
            }
            Self::Mkdir { parent, mode, name } => {
                push_u32(&mut payload, *parent);
                push_u16(&mut payload, *mode);
                push_name(&mut payload, name)?;
                CRASH_JOURNAL_OP_MKDIR
            }
            Self::Unlink { parent, name } => {
                push_u32(&mut payload, *parent);
                push_name(&mut payload, name)?;
                CRASH_JOURNAL_OP_UNLINK
            }
            Self::Rmdir { parent, name } => {
                push_u32(&mut payload, *parent);
                push_name(&mut payload, name)?;
                CRASH_JOURNAL_OP_RMDIR
            }
            Self::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
            } => {
                push_u32(&mut payload, *old_parent);
                push_name(&mut payload, old_name)?;
                push_u32(&mut payload, *new_parent);
                push_name(&mut payload, new_name)?;
                CRASH_JOURNAL_OP_RENAME
            }
            Self::Write { ino, offset, data } => {
                let data_len = u16::try_from(data.len()).ok()?;
                push_u32(&mut payload, *ino);
                push_u64(&mut payload, *offset);
                push_u16(&mut payload, data_len);
                payload.extend_from_slice(data);
                CRASH_JOURNAL_OP_WRITE
            }
            Self::Truncate { ino, new_size } => {
                push_u32(&mut payload, *ino);
                push_u64(&mut payload, *new_size);
                CRASH_JOURNAL_OP_TRUNCATE
            }
        };

        if payload.len() > CRASH_JOURNAL_MAX_PAYLOAD {
            return None;
        }
        Some((op, payload))
    }

    fn decode(op: u32, payload: &[u8]) -> Option<Self> {
        fn read_u16(payload: &[u8], cursor: &mut usize) -> Option<u16> {
            let end = cursor.checked_add(2)?;
            let bytes: [u8; 2] = payload.get(*cursor..end)?.try_into().ok()?;
            *cursor = end;
            Some(u16::from_le_bytes(bytes))
        }

        fn read_u32(payload: &[u8], cursor: &mut usize) -> Option<u32> {
            let end = cursor.checked_add(4)?;
            let bytes: [u8; 4] = payload.get(*cursor..end)?.try_into().ok()?;
            *cursor = end;
            Some(u32::from_le_bytes(bytes))
        }

        fn read_u64(payload: &[u8], cursor: &mut usize) -> Option<u64> {
            let end = cursor.checked_add(8)?;
            let bytes: [u8; 8] = payload.get(*cursor..end)?.try_into().ok()?;
            *cursor = end;
            Some(u64::from_le_bytes(bytes))
        }

        fn read_name(payload: &[u8], cursor: &mut usize) -> Option<String> {
            let len = usize::from(read_u16(payload, cursor)?);
            let end = cursor.checked_add(len)?;
            let bytes = payload.get(*cursor..end)?;
            let name = core::str::from_utf8(bytes).ok()?.to_string();
            *cursor = end;
            Some(name)
        }

        let mut cursor = 0usize;
        let op = match op {
            CRASH_JOURNAL_OP_CREATE => Self::Create {
                parent: read_u32(payload, &mut cursor)?,
                mode: read_u16(payload, &mut cursor)?,
                name: read_name(payload, &mut cursor)?,
            },
            CRASH_JOURNAL_OP_MKDIR => Self::Mkdir {
                parent: read_u32(payload, &mut cursor)?,
                mode: read_u16(payload, &mut cursor)?,
                name: read_name(payload, &mut cursor)?,
            },
            CRASH_JOURNAL_OP_UNLINK => Self::Unlink {
                parent: read_u32(payload, &mut cursor)?,
                name: read_name(payload, &mut cursor)?,
            },
            CRASH_JOURNAL_OP_RMDIR => Self::Rmdir {
                parent: read_u32(payload, &mut cursor)?,
                name: read_name(payload, &mut cursor)?,
            },
            CRASH_JOURNAL_OP_RENAME => Self::Rename {
                old_parent: read_u32(payload, &mut cursor)?,
                old_name: read_name(payload, &mut cursor)?,
                new_parent: read_u32(payload, &mut cursor)?,
                new_name: read_name(payload, &mut cursor)?,
            },
            CRASH_JOURNAL_OP_WRITE => {
                let ino = read_u32(payload, &mut cursor)?;
                let offset = read_u64(payload, &mut cursor)?;
                let data_len = usize::from(read_u16(payload, &mut cursor)?);
                let end = cursor.checked_add(data_len)?;
                let data = payload.get(cursor..end)?.to_vec();
                cursor = end;
                Self::Write { ino, offset, data }
            }
            CRASH_JOURNAL_OP_TRUNCATE => Self::Truncate {
                ino: read_u32(payload, &mut cursor)?,
                new_size: read_u64(payload, &mut cursor)?,
            },
            _ => return None,
        };

        if cursor != payload.len() {
            return None;
        }
        Some(op)
    }
}

#[derive(Debug, Default)]
struct DirEntryCache {
    loaded: bool,
    entries: BTreeMap<String, u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirLookupCacheResult {
    Hit(u32),
    Miss,
    Unknown,
}

#[derive(Debug)]
struct KernelBlockDeviceAdapter {
    inner: Arc<dyn BlockDevice>,
    io_failed: AtomicBool,
}

impl KernelBlockDeviceAdapter {
    fn new(inner: Arc<dyn BlockDevice>) -> Self {
        Self {
            inner,
            io_failed: AtomicBool::new(false),
        }
    }

    #[inline]
    fn align_down(offset: usize) -> usize {
        offset / SECTOR_SIZE * SECTOR_SIZE
    }

    #[inline]
    fn align_up(offset: usize) -> usize {
        offset.div_ceil(SECTOR_SIZE) * SECTOR_SIZE
    }

    #[inline]
    fn mark_io_failure(&self) {
        self.io_failed.store(true, Ordering::Release);
    }

    fn clear_io_failure(&self) {
        self.io_failed.store(false, Ordering::Release);
    }

    fn consume_io_failure(&self) -> bool {
        self.io_failed.swap(false, Ordering::AcqRel)
    }

    fn device_size_bytes(&self) -> usize {
        self.inner.metadata().nr_sectors.saturating_mul(SECTOR_SIZE)
    }
}

impl Ext4BlockDevice for KernelBlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let dev_size = self.device_size_bytes();
        let Some(read_end) = offset.checked_add(EXT4_BLOCK_SIZE) else {
            self.mark_io_failure();
            error!("ext4 block read overflow at offset {}", offset);
            return vec![0u8; EXT4_BLOCK_SIZE];
        };
        if read_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block read out of range: offset={} len={} device_size={}",
                offset, EXT4_BLOCK_SIZE, dev_size
            );
            return vec![0u8; EXT4_BLOCK_SIZE];
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + EXT4_BLOCK_SIZE);
        let aligned_len = aligned_end - aligned_start;

        let mut aligned = vec![0u8; aligned_len];
        let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
        if let Err(err) = self.inner.read(aligned_start, &mut writer) {
            self.mark_io_failure();
            error!("ext4 block read failed at offset {}: {:?}", offset, err);
            return vec![0u8; EXT4_BLOCK_SIZE];
        }

        let start = offset - aligned_start;
        aligned[start..start + EXT4_BLOCK_SIZE].to_vec()
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let dev_size = self.device_size_bytes();
        let Some(write_end) = offset.checked_add(data.len()) else {
            self.mark_io_failure();
            error!("ext4 block write overflow at offset {} len={}", offset, data.len());
            return;
        };
        if write_end > dev_size {
            self.mark_io_failure();
            error!(
                "ext4 block write out of range: offset={} len={} device_size={}",
                offset, data.len(), dev_size
            );
            return;
        }

        let aligned_start = Self::align_down(offset);
        let aligned_end = Self::align_up(offset + data.len());
        let aligned_len = aligned_end - aligned_start;
        let mut aligned = vec![0u8; aligned_len];

        // Preserve neighboring bytes when ext4_rs issues unaligned writes.
        if aligned_start != offset || aligned_len != data.len() {
            let mut writer = VmWriter::from(aligned.as_mut_slice()).to_fallible();
            if let Err(err) = self.inner.read(aligned_start, &mut writer) {
                self.mark_io_failure();
                error!(
                    "ext4 block pre-read failed at offset {}: {:?}",
                    aligned_start, err
                );
                return;
            }
        }

        let start = offset - aligned_start;
        aligned[start..start + data.len()].copy_from_slice(data);

        let mut reader = VmReader::from(aligned.as_slice()).to_fallible();
        if let Err(err) = self.inner.write(aligned_start, &mut reader) {
            self.mark_io_failure();
            error!("ext4 block write failed at offset {}: {:?}", offset, err);
        }
    }
}

pub(super) struct Ext4Fs {
    inner: Mutex<Ext4>,
    block_device: Arc<dyn BlockDevice>,
    adapter: Arc<KernelBlockDeviceAdapter>,
    crash_journal_lock: Mutex<()>,
    dir_entry_cache: Mutex<BTreeMap<u32, DirEntryCache>>,
    fs_event_subscriber_stats: FsEventSubscriberStats,
    self_ref: Weak<Self>,
}

impl Ext4Fs {
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let adapter = Arc::new(KernelBlockDeviceAdapter::new(block_device.clone()));
        let ext4 = Ext4::open(adapter.clone());

        let fs = Arc::new_cyclic(|weak_ref| Self {
            inner: Mutex::new(ext4),
            block_device,
            adapter,
            crash_journal_lock: Mutex::new(()),
            dir_entry_cache: Mutex::new(BTreeMap::new()),
            fs_event_subscriber_stats: FsEventSubscriberStats::new(),
            self_ref: weak_ref.clone(),
        });

        fs.replay_mount_crash_journal();
        fs
    }

    pub(super) fn lock_inner(&self) -> MutexGuard<'_, Ext4> {
        self.inner.lock()
    }

    fn prepare_ext4_io(&self) {
        self.adapter.clear_io_failure();
    }

    fn finish_ext4_io(&self) -> Result<()> {
        if self.adapter.consume_io_failure() {
            return_errno_with_message!(Errno::EIO, "ext4 block I/O failure");
        }
        Ok(())
    }

    pub(super) fn run_ext4<T>(
        &self,
        f: impl FnOnce(&Ext4) -> core::result::Result<T, ext4_rs::Ext4Error>,
    ) -> Result<T> {
        self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            f(&inner).map_err(map_ext4_error)?
        };
        self.finish_ext4_io()?;
        Ok(result)
    }

    pub(super) fn run_ext4_noerr<T>(&self, f: impl FnOnce(&Ext4) -> T) -> Result<T> {
        self.prepare_ext4_io();
        let result = {
            let inner = self.lock_inner();
            f(&inner)
        };
        self.finish_ext4_io()?;
        Ok(result)
    }

    fn crash_journal_checksum(data: &[u8]) -> u32 {
        // FNV-1a (32-bit) for lightweight corruption detection.
        let mut hash: u32 = 0x811C_9DC5;
        for byte in data {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        hash
    }

    fn read_u32_at(buf: &[u8], offset: usize) -> Option<u32> {
        let end = offset.checked_add(4)?;
        let bytes: [u8; 4] = buf.get(offset..end)?.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    fn serialize_crash_journal_record(
        state: u32,
        op: u32,
        payload: &[u8],
    ) -> Result<[u8; SECTOR_SIZE]> {
        if payload.len() > CRASH_JOURNAL_MAX_PAYLOAD {
            return_errno_with_message!(Errno::EINVAL, "crash journal payload too large");
        }

        let mut sector = [0u8; SECTOR_SIZE];
        sector[0..4].copy_from_slice(&CRASH_JOURNAL_MAGIC.to_le_bytes());
        sector[4..8].copy_from_slice(&CRASH_JOURNAL_VERSION.to_le_bytes());
        sector[8..12].copy_from_slice(&state.to_le_bytes());
        sector[12..16].copy_from_slice(&op.to_le_bytes());
        sector[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        if !payload.is_empty() {
            sector[CRASH_JOURNAL_HEADER_SIZE..CRASH_JOURNAL_HEADER_SIZE + payload.len()]
                .copy_from_slice(payload);
        }
        let checksum =
            Self::crash_journal_checksum(&sector[0..CRASH_JOURNAL_HEADER_SIZE - 4 + payload.len()]);
        sector[20..24].copy_from_slice(&checksum.to_le_bytes());
        Ok(sector)
    }

    fn parse_crash_journal_record(sector: &[u8; SECTOR_SIZE]) -> Result<Option<CrashJournalRecord>> {
        let Some(magic) = Self::read_u32_at(sector, 0) else {
            return Ok(None);
        };
        if magic != CRASH_JOURNAL_MAGIC {
            return Ok(None);
        }

        let version = Self::read_u32_at(sector, 4)
            .ok_or_else(|| Error::with_message(Errno::EIO, "corrupted crash journal version"))?;
        if version != CRASH_JOURNAL_VERSION {
            return_errno_with_message!(Errno::EIO, "unsupported crash journal version");
        }

        let state = Self::read_u32_at(sector, 8)
            .ok_or_else(|| Error::with_message(Errno::EIO, "corrupted crash journal state"))?;
        if state == CRASH_JOURNAL_STATE_EMPTY {
            return Ok(None);
        }

        let op = Self::read_u32_at(sector, 12)
            .ok_or_else(|| Error::with_message(Errno::EIO, "corrupted crash journal op"))?;
        let payload_len = Self::read_u32_at(sector, 16)
            .ok_or_else(|| Error::with_message(Errno::EIO, "corrupted crash journal len"))?
            as usize;
        if payload_len > CRASH_JOURNAL_MAX_PAYLOAD {
            return_errno_with_message!(Errno::EIO, "crash journal payload length overflow");
        }

        let stored_checksum = Self::read_u32_at(sector, 20)
            .ok_or_else(|| Error::with_message(Errno::EIO, "corrupted crash journal checksum"))?;
        let expected_checksum =
            Self::crash_journal_checksum(&sector[0..CRASH_JOURNAL_HEADER_SIZE - 4 + payload_len]);
        if stored_checksum != expected_checksum {
            return_errno_with_message!(Errno::EIO, "crash journal checksum mismatch");
        }

        let payload = if payload_len == 0 {
            Vec::new()
        } else {
            sector[CRASH_JOURNAL_HEADER_SIZE..CRASH_JOURNAL_HEADER_SIZE + payload_len].to_vec()
        };
        Ok(Some(CrashJournalRecord { state, op, payload }))
    }

    fn read_crash_journal_record(&self) -> Result<Option<CrashJournalRecord>> {
        let mut sector = [0u8; SECTOR_SIZE];
        let mut writer = VmWriter::from(sector.as_mut_slice()).to_fallible();
        self.block_device
            .read(CRASH_JOURNAL_OFFSET, &mut writer)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to read crash journal"))?;
        Self::parse_crash_journal_record(&sector)
    }

    fn write_crash_journal_record(&self, state: u32, op: u32, payload: &[u8]) -> Result<()> {
        let sector = Self::serialize_crash_journal_record(state, op, payload)?;
        let mut reader = VmReader::from(sector.as_slice()).to_fallible();
        self.block_device
            .write(CRASH_JOURNAL_OFFSET, &mut reader)
            .map_err(|_| Error::with_message(Errno::EIO, "failed to write crash journal"))?;
        self.block_device
            .sync()
            .map_err(|_| Error::with_message(Errno::EIO, "failed to sync crash journal"))?;
        Ok(())
    }

    fn clear_crash_journal(&self) -> Result<()> {
        self.write_crash_journal_record(CRASH_JOURNAL_STATE_EMPTY, 0, &[])
    }

    fn crash_journal_op_name(op: u32) -> &'static str {
        match op {
            CRASH_JOURNAL_OP_CREATE => "create",
            CRASH_JOURNAL_OP_MKDIR => "mkdir",
            CRASH_JOURNAL_OP_UNLINK => "unlink",
            CRASH_JOURNAL_OP_RMDIR => "rmdir",
            CRASH_JOURNAL_OP_RENAME => "rename",
            CRASH_JOURNAL_OP_WRITE => "write",
            CRASH_JOURNAL_OP_TRUNCATE => "truncate",
            _ => "unknown",
        }
    }

    fn crash_journal_op_from_name(name: &[u8]) -> Option<u32> {
        match name {
            b"create" => Some(CRASH_JOURNAL_OP_CREATE),
            b"mkdir" => Some(CRASH_JOURNAL_OP_MKDIR),
            b"unlink" => Some(CRASH_JOURNAL_OP_UNLINK),
            b"rmdir" => Some(CRASH_JOURNAL_OP_RMDIR),
            b"rename" => Some(CRASH_JOURNAL_OP_RENAME),
            b"write" => Some(CRASH_JOURNAL_OP_WRITE),
            b"truncate" => Some(CRASH_JOURNAL_OP_TRUNCATE),
            _ => None,
        }
    }

    fn should_hold_after_commit_for_injected_crash(op_code: u32) -> bool {
        let Some(kcmd) = KCMDLINE.get() else {
            return false;
        };
        let Some(args) = kcmd.get_module_args("ext4fs") else {
            return false;
        };

        let mut enabled = false;
        let mut op_filter: Option<u32> = None;
        for arg in args {
            match arg {
                ModuleArg::Arg(key) => {
                    if key.as_c_str().to_bytes() == b"replay_hold" {
                        enabled = true;
                    }
                }
                ModuleArg::KeyVal(key, value) => {
                    let key = key.as_c_str().to_bytes();
                    let value = value.as_c_str().to_bytes();
                    if key == b"replay_hold" {
                        if value == b"1" || value == b"true" || value == b"yes" {
                            enabled = true;
                        }
                    } else if key == b"replay_hold_op" {
                        op_filter = Self::crash_journal_op_from_name(value);
                    }
                }
            }
        }

        if !enabled {
            return false;
        }
        match op_filter {
            Some(filter_op) => filter_op == op_code,
            None => true,
        }
    }

    fn run_journaled<T>(&self, op: Option<CrashJournalOp>, apply: impl FnOnce() -> Result<T>) -> Result<T> {
        let Some(op) = op else {
            return apply();
        };
        let Some((op_code, payload)) = op.encode() else {
            return apply();
        };

        let _journal_guard = self.crash_journal_lock.lock();
        self.write_crash_journal_record(CRASH_JOURNAL_STATE_PREPARED, op_code, &payload)?;
        self.write_crash_journal_record(CRASH_JOURNAL_STATE_COMMITTED, op_code, &payload)?;

        let result = apply();
        if result.is_ok() && Self::should_hold_after_commit_for_injected_crash(op_code) {
            warn!(
                "ext4: replay hold point reached for op={} (kill VM now to simulate power loss)",
                Self::crash_journal_op_name(op_code)
            );
            loop {
                core::hint::spin_loop();
            }
        }
        if let Err(err) = self.clear_crash_journal() {
            warn!("ext4: failed to clear crash journal: {:?}", err);
        }
        result
    }

    fn replay_journal_op(&self, op: &CrashJournalOp) -> Result<()> {
        match op {
            CrashJournalOp::Create { parent, mode, name } => {
                let parent = *parent;
                let mode = *mode;
                let name = name.clone();
                self.run_ext4(|ext4| {
                    if ext4.ext4_lookup_at(parent, name.as_str()).is_ok() {
                        return Ok(());
                    }
                    ext4.ext4_create_at(parent, name.as_str(), mode).map(|_| ())
                })?;
            }
            CrashJournalOp::Mkdir { parent, mode, name } => {
                let parent = *parent;
                let mode = *mode;
                let name = name.clone();
                self.run_ext4(|ext4| {
                    if ext4.ext4_lookup_at(parent, name.as_str()).is_ok() {
                        return Ok(());
                    }
                    ext4.ext4_mkdir_at(parent, name.as_str(), mode).map(|_| ())
                })?;
            }
            CrashJournalOp::Unlink { parent, name } => {
                let parent = *parent;
                let name = name.clone();
                self.run_ext4(|ext4| {
                    let ino = match ext4.ext4_lookup_at(parent, name.as_str()) {
                        Ok(ino) => ino,
                        Err(err) if err.error() == ext4_rs::Errno::ENOENT => return Ok(()),
                        Err(err) => return Err(err),
                    };
                    let meta = ext4.ext4_stat(ino);
                    if meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits() {
                        return Ok(());
                    }
                    ext4.ext4_unlink_at(parent, name.as_str()).map(|_| ())
                })?;
            }
            CrashJournalOp::Rmdir { parent, name } => {
                let parent = *parent;
                let name = name.clone();
                self.run_ext4(|ext4| {
                    let ino = match ext4.ext4_lookup_at(parent, name.as_str()) {
                        Ok(ino) => ino,
                        Err(err) if err.error() == ext4_rs::Errno::ENOENT => return Ok(()),
                        Err(err) => return Err(err),
                    };
                    let meta = ext4.ext4_stat(ino);
                    if meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
                        return Ok(());
                    }
                    ext4.ext4_rmdir_at(parent, name.as_str()).map(|_| ())
                })?;
            }
            CrashJournalOp::Rename {
                old_parent,
                old_name,
                new_parent,
                new_name,
            } => {
                let old_parent = *old_parent;
                let new_parent = *new_parent;
                let old_name = old_name.clone();
                let new_name = new_name.clone();
                self.run_ext4(|ext4| {
                    let old = ext4.ext4_lookup_at(old_parent, old_name.as_str());
                    let new = ext4.ext4_lookup_at(new_parent, new_name.as_str());
                    match (old, new) {
                        (Ok(old_ino), Ok(new_ino)) if old_ino == new_ino => Ok(()),
                        (Err(old_err), Ok(_)) if old_err.error() == ext4_rs::Errno::ENOENT => Ok(()),
                        (Ok(_), _) => ext4
                            .ext4_rename_at(
                                old_parent,
                                old_name.as_str(),
                                new_parent,
                                new_name.as_str(),
                            )
                            .map(|_| ()),
                        (Err(old_err), Err(new_err))
                            if old_err.error() == ext4_rs::Errno::ENOENT
                                && new_err.error() == ext4_rs::Errno::ENOENT =>
                        {
                            Ok(())
                        }
                        (Err(old_err), _) => Err(old_err),
                    }
                })?;
            }
            CrashJournalOp::Write { ino, offset, data } => {
                let ino = *ino;
                let offset = usize::try_from(*offset)
                    .map_err(|_| Error::with_message(Errno::EFBIG, "write offset overflow"))?;
                let data = data.clone();
                self.run_ext4(|ext4| {
                    let written = ext4.ext4_write_at(ino, offset, data.as_slice())?;
                    if written == data.len() {
                        Ok(())
                    } else {
                        Err(ext4_rs::Ext4Error::new(ext4_rs::Errno::EIO))
                    }
                })?;
            }
            CrashJournalOp::Truncate { ino, new_size } => {
                let ino = *ino;
                let new_size = *new_size;
                self.run_ext4(|ext4| {
                    let meta = ext4.ext4_stat(ino);
                    if meta.size <= new_size {
                        return Ok(());
                    }
                    ext4.ext4_truncate(ino, new_size).map(|_| ())
                })?;
            }
        }
        Ok(())
    }

    fn replay_mount_crash_journal(&self) {
        let _journal_guard = self.crash_journal_lock.lock();
        let record = match self.read_crash_journal_record() {
            Ok(record) => record,
            Err(err) => {
                warn!("ext4: failed to read crash journal at mount: {:?}", err);
                if let Err(clear_err) = self.clear_crash_journal() {
                    warn!(
                        "ext4: failed to reset crash journal after read error: {:?}",
                        clear_err
                    );
                }
                return;
            }
        };

        let Some(record) = record else {
            return;
        };

        match record.state {
            CRASH_JOURNAL_STATE_PREPARED => {
                warn!("ext4: discarding uncommitted crash journal record");
            }
            CRASH_JOURNAL_STATE_COMMITTED => {
                if let Some(op) = CrashJournalOp::decode(record.op, record.payload.as_slice()) {
                    if let Err(err) = self.replay_journal_op(&op) {
                        warn!("ext4: crash journal replay failed: op={:?} err={:?}", op, err);
                    } else {
                        info!("ext4: crash journal replay succeeded: op={:?}", op);
                    }
                } else {
                    warn!("ext4: invalid crash journal payload (op={})", record.op);
                }
            }
            other => {
                warn!("ext4: unknown crash journal state {}", other);
            }
        }

        if let Err(err) = self.clear_crash_journal() {
            warn!("ext4: failed to clear crash journal at mount: {:?}", err);
        }
    }

    pub(super) fn stat(&self, ino: u32) -> Result<SimpleInodeMeta> {
        self.run_ext4_noerr(|ext4| ext4.ext4_stat(ino))
    }

    fn lookup_cache(&self, parent: u32, name: &str) -> DirLookupCacheResult {
        let caches = self.dir_entry_cache.lock();
        let Some(cache) = caches.get(&parent) else {
            return DirLookupCacheResult::Unknown;
        };
        if let Some(ino) = cache.entries.get(name) {
            return DirLookupCacheResult::Hit(*ino);
        }
        if cache.loaded {
            return DirLookupCacheResult::Miss;
        }
        DirLookupCacheResult::Unknown
    }

    fn load_dir_cache_if_needed(&self, parent: u32) -> Result<()> {
        {
            let caches = self.dir_entry_cache.lock();
            if let Some(cache) = caches.get(&parent) {
                if cache.loaded {
                    return Ok(());
                }
            }
        }

        let meta = self.stat(parent)?;
        if meta.file_type != ext4_rs::InodeFileType::S_IFDIR.bits() {
            return_errno_with_message!(Errno::ENOTDIR, "parent inode is not a directory");
        }

        let entries = self.readdir(parent)?;
        let mut entry_map = BTreeMap::new();
        for entry in entries {
            entry_map.insert(entry.name, entry.inode);
        }

        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        if !cache.loaded {
            cache.entries = entry_map;
            cache.loaded = true;
        }
        Ok(())
    }

    fn cache_insert_entry(&self, parent: u32, name: &str, child: u32) {
        let mut caches = self.dir_entry_cache.lock();
        let cache = caches.entry(parent).or_default();
        cache.entries.insert(name.to_string(), child);
    }

    fn cache_remove_entry(&self, parent: u32, name: &str) {
        let mut caches = self.dir_entry_cache.lock();
        if let Some(cache) = caches.get_mut(&parent) {
            cache.entries.remove(name);
        }
    }

    fn cache_remove_dir(&self, ino: u32) {
        let mut caches = self.dir_entry_cache.lock();
        caches.remove(&ino);
    }

    pub(super) fn lookup_at(&self, parent: u32, name: &str) -> Result<u32> {
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino) => return Ok(ino),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "No such file or directory");
            }
            DirLookupCacheResult::Unknown => {}
        }

        self.load_dir_cache_if_needed(parent)?;
        match self.lookup_cache(parent, name) {
            DirLookupCacheResult::Hit(ino) => return Ok(ino),
            DirLookupCacheResult::Miss => {
                return_errno_with_message!(Errno::ENOENT, "No such file or directory");
            }
            DirLookupCacheResult::Unknown => {}
        }

        let ino = self.run_ext4(|ext4| ext4.ext4_lookup_at(parent, name))?;
        self.cache_insert_entry(parent, name, ino);
        Ok(ino)
    }

    pub(super) fn dir_open(&self, path: &str) -> Result<u32> {
        self.run_ext4(|ext4| ext4.ext4_dir_open(path))
    }

    pub(super) fn create_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let op = CrashJournalOp::Create {
            parent,
            mode,
            name: name.to_string(),
        };
        let ino = self.run_journaled(Some(op), || {
            self.run_ext4(|ext4| ext4.ext4_create_at(parent, name, mode))
        })?;
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        Ok(ino)
    }

    pub(super) fn mkdir_at(&self, parent: u32, name: &str, mode: u16) -> Result<u32> {
        let op = CrashJournalOp::Mkdir {
            parent,
            mode,
            name: name.to_string(),
        };
        let ino = self.run_journaled(Some(op), || {
            self.run_ext4(|ext4| ext4.ext4_mkdir_at(parent, name, mode))
        })?;
        self.cache_insert_entry(parent, name, ino);
        self.cache_remove_dir(ino);
        Ok(ino)
    }

    pub(super) fn unlink_at(&self, parent: u32, name: &str) -> Result<()> {
        let op = CrashJournalOp::Unlink {
            parent,
            name: name.to_string(),
        };
        self.run_journaled(Some(op), || self.run_ext4(|ext4| ext4.ext4_unlink_at(parent, name)))?;
        self.cache_remove_entry(parent, name);
        Ok(())
    }

    pub(super) fn rmdir_at(&self, parent: u32, name: &str) -> Result<()> {
        let child_ino = self.lookup_at(parent, name).ok();
        let op = CrashJournalOp::Rmdir {
            parent,
            name: name.to_string(),
        };
        self.run_journaled(Some(op), || self.run_ext4(|ext4| ext4.ext4_rmdir_at(parent, name)))?;
        self.cache_remove_entry(parent, name);
        if let Some(child_ino) = child_ino {
            self.cache_remove_dir(child_ino);
        }
        Ok(())
    }

    pub(super) fn rename_at(
        &self,
        old_parent: u32,
        old_name: &str,
        new_parent: u32,
        new_name: &str,
    ) -> Result<()> {
        if old_parent == new_parent && old_name == new_name {
            return Ok(());
        }

        let old_ino = self.lookup_at(old_parent, old_name)?;
        let overwritten_ino = self.lookup_at(new_parent, new_name).ok();
        let overwritten_is_dir = overwritten_ino
            .and_then(|ino| self.stat(ino).ok().map(|meta| (ino, meta)))
            .map(|(ino, meta)| (ino, meta.file_type == ext4_rs::InodeFileType::S_IFDIR.bits()));

        let op = CrashJournalOp::Rename {
            old_parent,
            old_name: old_name.to_string(),
            new_parent,
            new_name: new_name.to_string(),
        };
        self.run_journaled(Some(op), || {
            self.run_ext4(|ext4| ext4.ext4_rename_at(old_parent, old_name, new_parent, new_name))
        })?;

        self.cache_remove_entry(old_parent, old_name);
        self.cache_insert_entry(new_parent, new_name, old_ino);

        if let Some((ino, true)) = overwritten_is_dir {
            self.cache_remove_dir(ino);
        }
        if let Some(ino) = overwritten_ino {
            if ino != old_ino {
                self.cache_remove_entry(new_parent, new_name);
                self.cache_insert_entry(new_parent, new_name, old_ino);
            }
        }

        Ok(())
    }

    pub(super) fn read_at(&self, ino: u32, offset: usize, data: &mut [u8]) -> Result<usize> {
        self.run_ext4(|ext4| ext4.ext4_read_at(ino, offset, data))
    }

    pub(super) fn write_at(&self, ino: u32, offset: usize, data: &[u8]) -> Result<usize> {
        let op = CrashJournalOp::for_small_write(ino, offset, data);
        self.run_journaled(op, || self.run_ext4(|ext4| ext4.ext4_write_at(ino, offset, data)))
    }

    pub(super) fn truncate(&self, ino: u32, new_size: u64) -> Result<()> {
        let op = CrashJournalOp::Truncate { ino, new_size };
        self.run_journaled(Some(op), || self.run_ext4(|ext4| ext4.ext4_truncate(ino, new_size)))?;
        Ok(())
    }

    pub(super) fn readdir(&self, ino: u32) -> Result<Vec<SimpleDirEntry>> {
        self.run_ext4_noerr(|ext4| ext4.ext4_readdir(ino))
    }

    pub(super) fn dev_id(&self) -> u64 {
        self.block_device.id().as_encoded_u64()
    }

    pub(super) fn this(&self) -> Arc<Self> {
        self.self_ref.upgrade().unwrap()
    }

    pub(super) fn make_inode(self: &Arc<Self>, ino: u32, path: String) -> Arc<dyn Inode> {
        Arc::new(super::inode::Ext4Inode::new(Arc::downgrade(self), ino, path))
    }
}

impl FileSystem for Ext4Fs {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn sync(&self) -> Result<()> {
        let _journal_guard = self.crash_journal_lock.lock();
        if let Err(err) = self.clear_crash_journal() {
            warn!("ext4: failed to clear crash journal during sync: {:?}", err);
        }
        self.block_device.sync()?;
        Ok(())
    }

    fn root_inode(&self) -> Arc<dyn Inode> {
        self.this().make_inode(EXT4_ROOT_INODE, String::new())
    }

    fn sb(&self) -> SuperBlock {
        let ext4_sb = self.lock_inner().super_block;
        let block_size = ext4_sb.block_size() as usize;
        let blocks = ext4_sb.blocks_count() as usize;
        let bfree = ext4_sb
            .free_blocks_count()
            .min(usize::MAX as u64) as usize;
        let files = ext4_sb.total_inodes() as usize;
        let ffree = ext4_sb.free_inodes_count() as usize;
        let fsid = u64::from_le_bytes(ext4_sb.uuid[..8].try_into().unwrap_or([0u8; 8]));

        SuperBlock {
            magic: EXT4_MAGIC,
            bsize: block_size,
            blocks,
            bfree,
            bavail: bfree,
            files,
            ffree,
            fsid,
            namelen: NAME_MAX,
            frsize: block_size,
            flags: 0,
        }
    }

    fn fs_event_subscriber_stats(&self) -> &FsEventSubscriberStats {
        &self.fs_event_subscriber_stats
    }
}

pub(super) struct Ext4Type;

impl FsType for Ext4Type {
    fn name(&self) -> &'static str {
        "ext4"
    }

    fn properties(&self) -> FsProperties {
        FsProperties::NEED_DISK
    }

    fn create(
        &self,
        _flags: FsFlags,
        _args: Option<CString>,
        disk: Option<Arc<dyn BlockDevice>>,
    ) -> Result<Arc<dyn FileSystem>> {
        let disk =
            disk.ok_or_else(|| Error::with_message(Errno::EINVAL, "missing block device"))?;
        verify_ext4_superblock(disk.as_ref())?;
        Ok(Ext4Fs::open(disk) as Arc<dyn FileSystem>)
    }

    fn sysnode(&self) -> Option<Arc<dyn aster_systree::SysNode>> {
        None
    }
}

pub(super) fn map_ext4_error(err: ext4_rs::Ext4Error) -> Error {
    Error::new(map_ext4_errno(err.error()))
}

fn map_ext4_errno(errno: ext4_rs::Errno) -> Errno {
    match errno {
        ext4_rs::Errno::EPERM => Errno::EPERM,
        ext4_rs::Errno::ENOENT => Errno::ENOENT,
        ext4_rs::Errno::EINTR => Errno::EINTR,
        ext4_rs::Errno::EIO => Errno::EIO,
        ext4_rs::Errno::ENXIO => Errno::ENXIO,
        ext4_rs::Errno::E2BIG => Errno::E2BIG,
        ext4_rs::Errno::EBADF => Errno::EBADF,
        ext4_rs::Errno::EAGAIN => Errno::EAGAIN,
        ext4_rs::Errno::ENOMEM => Errno::ENOMEM,
        ext4_rs::Errno::EACCES => Errno::EACCES,
        ext4_rs::Errno::EFAULT => Errno::EFAULT,
        ext4_rs::Errno::ENOTBLK => Errno::ENOTBLK,
        ext4_rs::Errno::EBUSY => Errno::EBUSY,
        ext4_rs::Errno::EEXIST => Errno::EEXIST,
        ext4_rs::Errno::EXDEV => Errno::EXDEV,
        ext4_rs::Errno::ENODEV => Errno::ENODEV,
        ext4_rs::Errno::ENOTDIR => Errno::ENOTDIR,
        ext4_rs::Errno::EISDIR => Errno::EISDIR,
        ext4_rs::Errno::EINVAL => Errno::EINVAL,
        ext4_rs::Errno::ENFILE => Errno::ENFILE,
        ext4_rs::Errno::EMFILE => Errno::EMFILE,
        ext4_rs::Errno::ENOTTY => Errno::ENOTTY,
        ext4_rs::Errno::ETXTBSY => Errno::ETXTBSY,
        ext4_rs::Errno::EFBIG => Errno::EFBIG,
        ext4_rs::Errno::ENOSPC => Errno::ENOSPC,
        ext4_rs::Errno::ESPIPE => Errno::ESPIPE,
        ext4_rs::Errno::EROFS => Errno::EROFS,
        ext4_rs::Errno::EMLINK => Errno::EMLINK,
        ext4_rs::Errno::EPIPE => Errno::EPIPE,
        ext4_rs::Errno::ENAMETOOLONG => Errno::ENAMETOOLONG,
        ext4_rs::Errno::ENOTEMPTY => Errno::ENOTEMPTY,
        ext4_rs::Errno::ENOTSUP => Errno::EOPNOTSUPP,
    }
}

fn verify_ext4_superblock(block_device: &dyn BlockDevice) -> Result<()> {
    let mut superblock_sector = [0u8; SECTOR_SIZE];
    let mut writer = VmWriter::from(superblock_sector.as_mut_slice()).to_fallible();
    block_device
        .read(EXT4_SUPERBLOCK_OFFSET, &mut writer)
        .map_err(|_| Error::with_message(Errno::EIO, "failed to read ext4 superblock"))?;

    let magic = u16::from_le_bytes([
        superblock_sector[EXT4_SB_MAGIC_OFFSET],
        superblock_sector[EXT4_SB_MAGIC_OFFSET + 1],
    ]);
    if magic != EXT4_MAGIC as u16 {
        return_errno_with_message!(Errno::EINVAL, "not an ext4 filesystem");
    }

    let log_block_size = u32::from_le_bytes([
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 1],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 2],
        superblock_sector[EXT4_SB_LOG_BLOCK_SIZE_OFFSET + 3],
    ]);
    let Some(block_size) = 1024usize.checked_shl(log_block_size) else {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 block size");
    };
    if !matches!(block_size, 1024 | 2048 | 4096) {
        return_errno_with_message!(Errno::EINVAL, "unsupported ext4 block size");
    }

    let blocks_per_group = u32::from_le_bytes([
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 1],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 2],
        superblock_sector[EXT4_SB_BLOCKS_PER_GROUP_OFFSET + 3],
    ]);
    if blocks_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 blocks_per_group");
    }

    let inodes_per_group = u32::from_le_bytes([
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 1],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 2],
        superblock_sector[EXT4_SB_INODES_PER_GROUP_OFFSET + 3],
    ]);
    if inodes_per_group == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 inodes_per_group");
    }

    let desc_size = u16::from_le_bytes([
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET],
        superblock_sector[EXT4_SB_DESC_SIZE_OFFSET + 1],
    ]);
    if desc_size == 0 {
        return_errno_with_message!(Errno::EINVAL, "invalid ext4 group descriptor size");
    }
    let desc_size = desc_size as usize;
    if desc_size > block_size || (block_size % desc_size) != 0 {
        return_errno_with_message!(Errno::EINVAL, "unsupported ext4 group descriptor size");
    }

    Ok(())
}
