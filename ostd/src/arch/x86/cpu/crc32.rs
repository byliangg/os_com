// SPDX-License-Identifier: MPL-2.0

//! Hardware-accelerated CRC-32C (Castagnoli) via the SSE4.2 `crc32` instruction.
//!
//! The x86 `crc32` instruction computes the *reflected* CRC-32C over its
//! operand with no pre- or post-inversion, treating the incoming value as the
//! running CRC state. This is exactly the raw convention Linux's `__crc32c_le`
//! and e2fsprogs' `crc32c_le` use, so [`crc32c`] is a drop-in accelerator for a
//! software byte-at-a-time / slice-by-8 implementation of the same function:
//! for identical `(seed, data)` it returns the identical residue.
//!
//! The instruction operates on general-purpose registers (not XMM/FPU state),
//! so it is safe to use in the kernel without touching the soft-float context.

use super::extension::{IsaExtensions, has_extensions};

/// Folds `data` into the running CRC-32C `seed` using the SSE4.2 `crc32`
/// instruction, returning the new running value.
///
/// Returns `None` when the running CPU does not implement SSE4.2, in which case
/// the caller should fall back to a software implementation. The result, when
/// `Some`, is byte-for-byte identical to that software implementation (raw
/// reflected CRC-32C, no inversion — see the module docs).
pub fn crc32c(seed: u32, data: &[u8]) -> Option<u32> {
    if !has_extensions(IsaExtensions::SSE4_2) {
        return None;
    }

    // SAFETY: `has_extensions` just confirmed the CPU implements SSE4.2, which
    // is the precondition of `crc32c_sse42`.
    Some(unsafe { crc32c_sse42(seed, data) })
}

/// Computes the raw reflected CRC-32C of `data` continuing from `seed` using the
/// SSE4.2 `crc32` instruction: an 8-byte main loop (`crc32q`) plus a byte tail
/// (`crc32b`).
///
/// # Safety
///
/// The running CPU must implement the SSE4.2 instruction set. Callers should
/// gate on [`has_extensions`]`(`[`IsaExtensions::SSE4_2`]`)` (as [`crc32c`]
/// does); invoking this on a CPU without SSE4.2 is undefined behavior.
#[target_feature(enable = "sse4.2")]
unsafe fn crc32c_sse42(seed: u32, data: &[u8]) -> u32 {
    use core::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};

    // `#[target_feature(enable = "sse4.2")]` makes the whole body an unsafe
    // context in which the SSE4.2-gated `_mm_crc32_*` intrinsics are available.
    let mut crc = seed as u64;
    let mut chunks = data.chunks_exact(8);
    for chunk in chunks.by_ref() {
        // `crc32` consumes bytes least-significant first, so a little-endian
        // load of the 8 in-memory bytes matches processing them one at a time.
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        crc = _mm_crc32_u64(crc, word);
    }

    let mut crc = crc as u32;
    for &byte in chunks.remainder() {
        crc = _mm_crc32_u8(crc, byte);
    }
    crc
}
