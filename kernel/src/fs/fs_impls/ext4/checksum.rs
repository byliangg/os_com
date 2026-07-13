// SPDX-License-Identifier: MPL-2.0

//! crc32c (Castagnoli) — the checksum kernel behind `metadata_csum` (Phase 6b)
//! and the JBD2 journal checksums (Phase 7a).
//!
//! This is the *raw* reflected CRC-32C used by ext4: [`crc32c`] runs the
//! reflected polynomial `0x82F63B78` over the bytes with `seed` as the running
//! state and applies **no** pre- or post-inversion — exactly Linux's
//! `__crc32c_le` and e2fsprogs' `crc32c_le`, which ext4's `ext4_chksum` wraps
//! verbatim. Callers own the conventional `~0` seed (superblock) or the
//! per-filesystem / per-inode seed (group descriptors, inodes, directory and
//! extent blocks); ext4 stores the running value directly, so this function
//! must not invert or e2fsck would reject every checksum it writes.
//!
//! The well-known CRC-32C "check" constant `0xE3069283` (init and xorout both
//! `~0`) therefore equals `!crc32c(!0, b"123456789")` here — the outer `!`
//! being the caller-side xorout that ext4 does not use internally.
//!
//! # Implementation (Phase 9b)
//!
//! crc32c lands on every `metadata_csum` write-back (extent leaf, inode,
//! bitmap, group descriptor, directory block, superblock), every JBD2
//! descriptor/commit block, and every verify-on-read, so it is a hot path. Two
//! interchangeable back-ends compute the identical residue:
//!
//! * [`crc32c_sw`] — a portable *slice-by-8* table implementation that folds
//!   eight bytes per iteration, breaking the byte-serial dependency of the
//!   classic one-byte-per-step loop.
//! * The SSE4.2 `crc32` instruction, owned by [`ostd`] (the kernel crate forbids
//!   `unsafe`), selected at runtime behind a CPUID gate.
//!
//! [`crc32c`] dispatches to hardware when the CPU supports it and to the
//! slice-by-8 table otherwise. Both are diffed byte-for-byte against a
//! bit-serial oracle in the tests below.

use super::inode::Ext4Ino;

/// The slice-by-8 CRC-32C lookup tables, generated at compile time from the
/// reflected polynomial `0x82F63B78`. Row `0` is the classic byte table (one
/// 32-bit residue per input byte); row `k` holds that byte's residue advanced a
/// further `k` positions, which is what lets the main loop consume eight bytes
/// with eight independent table lookups instead of a serial chain.
const CRC32C_SLICE: [[u32; 256]; 8] = {
    const POLY: u32 = 0x82F63B78;
    let mut table = [[0u32; 256]; 8];

    // Row 0: the residue of each single byte.
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[0][i] = crc;
        i += 1;
    }

    // Rows 1..8: advance the previous row by one more byte position.
    let mut i = 0;
    while i < 256 {
        let mut k = 1;
        while k < 8 {
            let prev = table[k - 1][i];
            table[k][i] = (prev >> 8) ^ table[0][(prev & 0xFF) as usize];
            k += 1;
        }
        i += 1;
    }

    table
};

/// Folds `data` into the running CRC-32C `seed` and returns the new running
/// value (no pre/post inversion — see the module docs).
///
/// Segments chain: `crc32c(crc32c(seed, a), b) == crc32c(seed, a ++ b)`, which
/// is how ext4 checksums a structure across the gap left by its own (zeroed)
/// checksum field.
///
/// Runs the SSE4.2 `crc32` instruction when the CPU supports it, falling back to
/// the slice-by-8 table; both produce the identical residue.
pub(super) fn crc32c(seed: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if let Some(crc) = ostd::arch::cpu::crc32::crc32c(seed, data) {
        return crc;
    }

    crc32c_sw(seed, data)
}

/// The portable slice-by-8 software implementation of [`crc32c`]: an eight-byte
/// main loop over [`CRC32C_SLICE`] followed by a byte-at-a-time tail.
fn crc32c_sw(seed: u32, data: &[u8]) -> u32 {
    let mut crc = seed;
    let mut chunks = data.chunks_exact(8);
    for chunk in chunks.by_ref() {
        // XOR the running CRC into the low four bytes, then index each of the
        // eight bytes into its position-appropriate table row and XOR the rows.
        let low = u32::from_le_bytes(chunk[0..4].try_into().unwrap()) ^ crc;
        let high = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
        crc = CRC32C_SLICE[7][(low & 0xFF) as usize]
            ^ CRC32C_SLICE[6][((low >> 8) & 0xFF) as usize]
            ^ CRC32C_SLICE[5][((low >> 16) & 0xFF) as usize]
            ^ CRC32C_SLICE[4][((low >> 24) & 0xFF) as usize]
            ^ CRC32C_SLICE[3][(high & 0xFF) as usize]
            ^ CRC32C_SLICE[2][((high >> 8) & 0xFF) as usize]
            ^ CRC32C_SLICE[1][((high >> 16) & 0xFF) as usize]
            ^ CRC32C_SLICE[0][((high >> 24) & 0xFF) as usize];
    }

    for &byte in chunks.remainder() {
        crc = (crc >> 8) ^ CRC32C_SLICE[0][((crc ^ byte as u32) & 0xFF) as usize];
    }
    crc
}

/// The per-filesystem metadata_csum seed (`crc32c(!0, uuid)`): feeds the group
/// descriptor and bitmap checksums directly, and folds into a per-inode seed for
/// the inode, directory-block, and extent-node checksums.
#[derive(Clone, Copy, Debug)]
pub(super) struct FsCsumSeed(u32);

impl FsCsumSeed {
    pub(super) fn new(seed: u32) -> Self {
        Self(seed)
    }

    pub(super) fn get(self) -> u32 {
        self.0
    }

    /// Folds `ino` and `generation` into the fs seed (Linux `ext4_inode_csum_seed`).
    pub(super) fn derive_inode(self, ino: Ext4Ino, generation: u32) -> InodeCsumSeed {
        let s = crc32c(self.0, &ino.to_le_bytes());
        InodeCsumSeed(crc32c(s, &generation.to_le_bytes()))
    }
}

/// The per-inode metadata_csum seed `crc32c(crc32c(fs_seed, ino), generation)`.
#[derive(Clone, Copy, Debug)]
pub(super) struct InodeCsumSeed(u32);

impl InodeCsumSeed {
    pub(super) fn get(self) -> u32 {
        self.0
    }
}

#[cfg(ktest)]
mod tests {
    use ostd::prelude::*;

    use super::{crc32c, crc32c_sw};

    /// The definitional bit-serial reflected CRC-32C — the slow, obviously
    /// correct oracle that the fast paths are diffed against. It uses neither the
    /// slice-by-8 tables nor the `crc32` instruction, so it independently pins
    /// both; it also equals the pre-Phase-9 byte-table implementation.
    fn crc32c_reference(seed: u32, data: &[u8]) -> u32 {
        const POLY: u32 = 0x82F63B78;
        let mut crc = seed;
        for &byte in data {
            crc ^= byte as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ POLY
                } else {
                    crc >> 1
                };
                bit += 1;
            }
        }
        crc
    }

    /// A tiny deterministic xorshift64 so the differential vectors are
    /// reproducible run to run.
    struct XorShift64(u64);
    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    /// Lengths that straddle the eight-byte main loop and its tail (`0`, sub-word,
    /// exactly one/two words, and full block sizes).
    const EDGE_LENS: &[usize] = &[
        0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 63, 64, 65, 4088, 4096,
    ];

    /// Checks `subject` against the bit-serial oracle over the edge lengths (at
    /// every 8-byte start alignment) and 1000 random `(offset, length, seed)`
    /// triples. Random start offsets exercise unaligned inputs and reads that
    /// cross 8-byte boundaries.
    fn diff_against_reference(subject: impl Fn(u32, &[u8]) -> u32) {
        // 4104 = 4096 + 8, so a length-4096 window fits at any offset in 0..=7.
        let mut buf = [0u8; 4104];
        let mut rng = XorShift64(0x1234_5678_9abc_def0);

        for &len in EDGE_LENS {
            for start in 0..8usize {
                if start + len > buf.len() {
                    continue;
                }
                for byte in buf.iter_mut() {
                    *byte = rng.next() as u8;
                }
                let seed = rng.next() as u32;
                let data = &buf[start..start + len];
                assert_eq!(subject(seed, data), crc32c_reference(seed, data));
            }
        }

        for _ in 0..1000 {
            for byte in buf.iter_mut() {
                *byte = rng.next() as u8;
            }
            let start = (rng.next() % 8) as usize;
            let len = (rng.next() as usize) % (buf.len() - start + 1);
            let seed = rng.next() as u32;
            let data = &buf[start..start + len];
            assert_eq!(subject(seed, data), crc32c_reference(seed, data));
        }
    }

    /// The canonical CRC-32C check vector: `0xE3069283` over `b"123456789"`
    /// with the conventional `~0` init and `~0` xorout. Our raw function omits
    /// the xorout, so the caller applies the outer `!`.
    #[ktest]
    fn crc32c_canonical_check_vector() {
        assert_eq!(!crc32c(!0u32, b"123456789"), 0xE306_9283);
    }

    /// Pins the oracle itself to the canonical check value, so the differential
    /// tests below diff against a trustworthy reference on every arch.
    #[ktest]
    fn crc32c_reference_is_canonical() {
        assert_eq!(!crc32c_reference(!0u32, b"123456789"), 0xE306_9283);
    }

    /// The empty input leaves the running state untouched (the identity of the
    /// fold), so a `~0` seed over nothing xors back to zero.
    #[ktest]
    fn crc32c_empty_is_identity() {
        assert_eq!(crc32c(0, &[]), 0);
        assert_eq!(!crc32c(!0u32, &[]), 0);
    }

    /// Chaining two segments equals one pass over their concatenation — the
    /// property ext4 relies on to checksum a struct around its checksum field.
    #[ktest]
    fn crc32c_segments_chain() {
        let whole = crc32c(!0u32, b"123456789");
        let split = crc32c(crc32c(!0u32, b"12345"), b"6789");
        assert_eq!(whole, split);
    }

    /// A single-byte change flips the result (guards against a stuck table).
    #[ktest]
    fn crc32c_detects_single_bit() {
        let a = crc32c(!0u32, b"metadata_csum");
        let b = crc32c(!0u32, b"metadata_csun");
        assert_ne!(a, b);
    }

    /// The slice-by-8 software path must be byte-identical to the oracle across
    /// all lengths and alignments.
    #[ktest]
    fn crc32c_sw_matches_reference() {
        diff_against_reference(crc32c_sw);
    }

    /// The public dispatcher (whichever back-end the running CPU selects) must
    /// match the oracle too.
    #[ktest]
    fn crc32c_dispatch_matches_reference() {
        diff_against_reference(crc32c);
    }

    /// The SSE4.2 hardware path must be byte-identical to the oracle. The ktest
    /// QEMU CPU model (Icelake-Server) exposes SSE4.2, so we assert the CPUID
    /// gate actually admits the hardware arm here rather than silently testing
    /// the software fallback again.
    #[cfg(target_arch = "x86_64")]
    #[ktest]
    fn crc32c_hw_matches_reference() {
        assert!(
            ostd::arch::cpu::crc32::crc32c(0, &[]).is_some(),
            "the ktest CPU is expected to expose SSE4.2 so the hardware arm is exercised",
        );
        diff_against_reference(|seed, data| ostd::arch::cpu::crc32::crc32c(seed, data).unwrap());
    }

    /// A coarse, non-asserting microbenchmark: it prints the raw cycle counts for
    /// the software and hardware paths over a 4088-byte span (an ext4 directory
    /// checksum tail) so the report has an order-of-magnitude figure. Real
    /// numbers come from the host bench harness.
    #[cfg(target_arch = "x86_64")]
    #[ktest]
    fn crc32c_microbench() {
        const ROUNDS: u32 = 4096;
        let buf = [0xa5u8; 4088];

        // Fold results into an accumulator so the loops are not optimized away.
        let mut acc = 0u32;
        let sw_start = ostd::arch::read_tsc();
        for _ in 0..ROUNDS {
            acc ^= crc32c_sw(!0, &buf);
        }
        let sw_cycles = ostd::arch::read_tsc() - sw_start;

        let hw_start = ostd::arch::read_tsc();
        for _ in 0..ROUNDS {
            acc ^= ostd::arch::cpu::crc32::crc32c(!0, &buf).unwrap_or(0);
        }
        let hw_cycles = ostd::arch::read_tsc() - hw_start;

        // `println!` (early_println) writes straight to the serial console so
        // the numbers survive the ktest logger's level filter.
        println!(
            "crc32c microbench (4088 B x {} rounds): sw={} cyc, hw={} cyc (lower is faster; acc={:#x})",
            ROUNDS, sw_cycles, hw_cycles, acc,
        );
    }
}
