/*++

Licensed under the Apache-2.0 license.

File Name:

    lib.rs

Abstract:

    MCU ROM patch blob format and the `apply` routine shared by the ROM and
    the host-side patch builder.

--*/

//! A patch is a Start Header, zero or more operations, and an End Header.
//! Every element is a multiple of 4 bytes and all multi-byte fields are
//! little-endian.
//!
//! | Element          | Byte 0          | Byte 1           | Bytes 2-3          |
//! |------------------|-----------------|------------------|--------------------|
//! | Start Header     | [`OP_START`]    | reserved         | patch size (words) |
//! | Operation Header | operation opcode| data size (words)| offset (words)     |
//! | End Header       | [`OP_END`]      | reserved         | reserved           |
//!
//! Each Operation Header is followed by `data size × 4` bytes of data. The
//! patch size counts every word from the Start Header through the End Header.
//!
//! `apply` runs from non-patchable ROM before the patched copy executes, so it
//! must not reach any patchable code or data: no panic paths, no `memcpy`, and
//! no jump tables. It uses checked slice access, volatile byte copies, and an
//! `if` chain for opcode dispatch to that end. Every function it calls carries
//! the same section under the `rom` feature, and its error codes are
//! compile-time constants. This holds only in an optimized build, which every
//! ROM profile is: at opt-level 0 the slice and pointer helpers stay out-of-line
//! calls into patchable `.text`. The linked ROM is checked in step 4.

#![no_std]

use caliptra_mcu_error::McuError;

/// Start Header opcode.
pub const OP_START: u8 = 0xA1;
/// End Header opcode.
pub const OP_END: u8 = 0xA2;
/// Overwrite bytes (code or rodata) at a word offset into Instruction RAM.
pub const OP_OVERWRITE_IRAM: u8 = 0xB1;
/// Overwrite bytes at a word offset into Data RAM.
pub const OP_OVERWRITE_DRAM: u8 = 0xB2;
/// Place a duplicated function at a word offset into Instruction RAM. Applied
/// exactly like [`OP_OVERWRITE_IRAM`]; the builder keeps it inside `.patch_funcs`.
pub const OP_WRITE_PATCH_FUNCTION: u8 = 0xB3;

/// Size of one format word, and of every header, in bytes.
pub const WORD_BYTES: usize = 4;
/// Largest data size one operation can carry (its size field is one byte).
pub const MAX_OP_DATA_WORDS: usize = u8::MAX as usize;

/// The patch was applied, or the blob holds no patch (no Start Header).
pub const PATCH_OK: u32 = 0;

// `apply` returns these as plain immediates; converting an `McuError` at run
// time could call into patchable `.text`.
const INVALID_SIZE: u32 = McuError::ROM_PATCH_INVALID_SIZE.0.get();
const END_HEADER_MISMATCH: u32 = McuError::ROM_PATCH_END_HEADER_MISMATCH.0.get();
const UNKNOWN_OPCODE: u32 = McuError::ROM_PATCH_UNKNOWN_OPCODE.0.get();
const EMPTY_OP: u32 = McuError::ROM_PATCH_EMPTY_OP.0.get();
const OP_OVERRUN: u32 = McuError::ROM_PATCH_OP_OVERRUN.0.get();
const OP_OUT_OF_RANGE: u32 = McuError::ROM_PATCH_OP_OUT_OF_RANGE.0.get();

/// Encodes a Start Header for a patch of `patch_size_words` words in total.
pub const fn start_header(patch_size_words: u16) -> [u8; 4] {
    let size = patch_size_words.to_le_bytes();
    [OP_START, 0, size[0], size[1]]
}

/// Encodes an End Header.
pub const fn end_header() -> [u8; 4] {
    [OP_END, 0, 0, 0]
}

/// Encodes an Operation Header. `data_size_words` data words must follow it.
pub const fn op_header(opcode: u8, data_size_words: u8, offset_words: u16) -> [u8; 4] {
    let offset = offset_words.to_le_bytes();
    [opcode, data_size_words, offset[0], offset[1]]
}

/// Applies the patch in `blob` to `iram` (Instruction RAM) and `dram` (Data
/// RAM), each the whole declared window starting at offset 0.
///
/// Returns [`PATCH_OK`] or the `u32` value of a `ROM_PATCH_*` [`McuError`].
/// A blob whose first byte is [`OP_START`] is checked strictly, in this order:
///
/// | Condition                                                  | Result                          |
/// |------------------------------------------------------------|---------------------------------|
/// | Blob empty, or first byte is not `OP_START`                | `PATCH_OK`, nothing written     |
/// | Blob under 4 bytes; patch size under 2 words or past blob  | `ROM_PATCH_INVALID_SIZE`        |
/// | Word `patch size - 1` is not an End Header                 | `ROM_PATCH_END_HEADER_MISMATCH` |
/// | Operation opcode is not `0xB1`-`0xB3`                      | `ROM_PATCH_UNKNOWN_OPCODE`      |
/// | Operation carries zero data words                          | `ROM_PATCH_EMPTY_OP`            |
/// | Operation data runs into the End Header                    | `ROM_PATCH_OP_OVERRUN`          |
/// | Operation writes past the end of its RAM                   | `ROM_PATCH_OP_OUT_OF_RANGE`     |
/// | Zero operations, reserved bytes, bytes after End Header    | ignored                         |
///
/// Operations apply in order as they are read, so an error leaves earlier
/// ones applied; a later operation overwrites an earlier overlapping one.
#[cfg_attr(feature = "rom", link_section = ".text.nonpatchable")]
#[inline(never)]
pub fn apply(blob: &[u8], iram: &mut [u8], dram: &mut [u8]) -> u32 {
    match blob.first() {
        Some(&OP_START) => {}
        _ => return PATCH_OK,
    }
    let start = match read_word(blob, 0) {
        Some(word) => word,
        None => return INVALID_SIZE,
    };

    let patch_words = u16::from_le_bytes([start[2], start[3]]) as usize;
    if patch_words < 2 || patch_words * WORD_BYTES > blob.len() {
        return INVALID_SIZE;
    }
    let end = patch_words - 1;
    match read_word(blob, end) {
        Some(word) if word[0] == OP_END => {}
        _ => return END_HEADER_MISMATCH,
    }

    let mut word = 1;
    while word < end {
        let header = match read_word(blob, word) {
            Some(header) => header,
            None => return OP_OVERRUN,
        };
        let opcode = header[0];
        let data_words = header[1] as usize;
        let offset_words = u16::from_le_bytes([header[2], header[3]]) as usize;

        let ram: &mut [u8] = if opcode == OP_OVERWRITE_IRAM || opcode == OP_WRITE_PATCH_FUNCTION {
            &mut *iram
        } else if opcode == OP_OVERWRITE_DRAM {
            &mut *dram
        } else {
            return UNKNOWN_OPCODE;
        };
        if data_words == 0 {
            return EMPTY_OP;
        }
        let data = word + 1;
        if data + data_words > end {
            return OP_OVERRUN;
        }

        let len = data_words * WORD_BYTES;
        let src_start = data * WORD_BYTES;
        let src = match blob.get(src_start..src_start + len) {
            Some(src) => src,
            None => return OP_OVERRUN,
        };
        let dst_start = offset_words * WORD_BYTES;
        let dst = match ram.get_mut(dst_start..dst_start + len) {
            Some(dst) => dst,
            None => return OP_OUT_OF_RANGE,
        };
        copy_bytes(src, dst);

        word = data + data_words;
    }
    PATCH_OK
}

/// Reads the 4-byte word at word index `index`, if it lies inside `blob`.
#[cfg_attr(feature = "rom", link_section = ".text.nonpatchable")]
fn read_word(blob: &[u8], index: usize) -> Option<[u8; 4]> {
    let start = index * WORD_BYTES;
    match blob.get(start..start + WORD_BYTES) {
        Some(bytes) => bytes.try_into().ok(),
        None => None,
    }
}

/// Copies `src` into `dst`, which have equal lengths. The volatile writes keep
/// LLVM from turning the loop into a `memcpy` call, which would land in
/// patchable `.text`.
#[cfg_attr(feature = "rom", link_section = ".text.nonpatchable")]
fn copy_bytes(src: &[u8], dst: &mut [u8]) {
    let len = if src.len() < dst.len() {
        src.len()
    } else {
        dst.len()
    };
    let mut i = 0;
    while i < len {
        // SAFETY: `i` is below the length of both slices.
        unsafe {
            dst.as_mut_ptr()
                .add(i)
                .write_volatile(src.as_ptr().add(i).read());
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use std::vec;
    use std::vec::Vec;

    const IRAM_LEN: usize = 64;
    const DRAM_LEN: usize = 16;

    /// One operation: opcode, word offset, data words.
    type Op<'a> = (u8, u16, &'a [u32]);

    fn encode(ops: &[Op]) -> Vec<u8> {
        let words = 2 + ops.iter().map(|(_, _, d)| 1 + d.len()).sum::<usize>();
        let mut blob = Vec::new();
        blob.extend_from_slice(&start_header(words as u16));
        for (opcode, offset, data) in ops {
            blob.extend_from_slice(&op_header(*opcode, data.len() as u8, *offset));
            for w in *data {
                blob.extend_from_slice(&w.to_le_bytes());
            }
        }
        blob.extend_from_slice(&end_header());
        blob
    }

    struct Ram {
        iram: Vec<u8>,
        dram: Vec<u8>,
    }

    impl Ram {
        fn new() -> Self {
            Self {
                iram: vec![0x11; IRAM_LEN],
                dram: vec![0x22; DRAM_LEN],
            }
        }

        fn apply(&mut self, blob: &[u8]) -> u32 {
            apply(blob, &mut self.iram, &mut self.dram)
        }

        fn assert_untouched(&self) {
            assert_eq!(self.iram, vec![0x11; IRAM_LEN]);
            assert_eq!(self.dram, vec![0x22; DRAM_LEN]);
        }
    }

    #[test]
    fn no_start_header_is_baseline() {
        let mut ram = Ram::new();
        assert_eq!(ram.apply(&[0u8; 1024]), PATCH_OK);
        ram.assert_untouched();
    }

    #[test]
    fn overwrite_iram() {
        let mut ram = Ram::new();
        let blob = encode(&[(OP_OVERWRITE_IRAM, 2, &[0xDEAD_BEEF])]);
        assert_eq!(ram.apply(&blob), PATCH_OK);
        let mut expected = vec![0x11; IRAM_LEN];
        expected[8..12].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
        assert_eq!(ram.iram, expected);
        assert_eq!(ram.dram, vec![0x22; DRAM_LEN]);
    }

    #[test]
    fn invalid_size() {
        let mut ram = Ram::new();
        assert_eq!(
            ram.apply(&[OP_START, 0, 2]),
            u32::from(McuError::ROM_PATCH_INVALID_SIZE)
        );
        let mut blob = encode(&[]);
        blob[..4].copy_from_slice(&start_header(3));
        assert_eq!(
            ram.apply(&blob),
            u32::from(McuError::ROM_PATCH_INVALID_SIZE)
        );
        ram.assert_untouched();
    }

    #[test]
    fn end_header_mismatch() {
        let mut ram = Ram::new();
        let mut blob = encode(&[(OP_OVERWRITE_IRAM, 0, &[1])]);
        let end = blob.len() - 4;
        blob[end] = 0x00;
        assert_eq!(
            ram.apply(&blob),
            u32::from(McuError::ROM_PATCH_END_HEADER_MISMATCH)
        );
        ram.assert_untouched();
    }

    #[test]
    fn op_overrun() {
        let mut ram = Ram::new();
        // Claims two data words but carries one, so its data reaches the End Header.
        let mut blob = encode(&[(OP_OVERWRITE_IRAM, 0, &[1])]);
        blob[5] = 2;
        assert_eq!(ram.apply(&blob), u32::from(McuError::ROM_PATCH_OP_OVERRUN));
        ram.assert_untouched();
    }

    #[test]
    fn op_out_of_range() {
        let mut ram = Ram::new();
        let past_end = (IRAM_LEN / WORD_BYTES) as u16;
        let blob = encode(&[(OP_OVERWRITE_IRAM, past_end, &[1])]);
        assert_eq!(
            ram.apply(&blob),
            u32::from(McuError::ROM_PATCH_OP_OUT_OF_RANGE)
        );
        ram.assert_untouched();
    }
}
