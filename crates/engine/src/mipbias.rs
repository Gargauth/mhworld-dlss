//! The texture mip bias: which constant sampler bank 1 is built from
//! (`docs/re/RePostProcess.md` §2.5).
//!
//! The engine builds 15 sampler banks once, when it loads its shader
//! package at start-up; bank 1's LOD bias is `-log2(c)` with `c` loaded by
//! `movss xmm0,[rip+disp32]` at `BANK_ONE_SOURCE` (`0x1423d73ee`, stock
//! `c = 2.0` → −1.0). Materials bake the bank selected by
//! `mSamplerLodBias` (`rs+0x1a8`) when they load. So a bias of `b` levels is:
//! point that instruction at a float `2^(−b)` that already exists in the
//! image's read-only data (the shared 2.0 is not touched: other code reads
//! it), before the banks are built, and select bank 1.
//!
//! Pure; the search runs over the bytes of the constant's section.

/// The constant the instruction loads in the known build.
pub const STOCK_CONSTANT: f32 = 2.0;
/// Below this magnitude a bias is no bias.
const NO_BIAS: f32 = 0.007;
/// The most negative bias accepted (the engine's own ladder ends at
/// `-log2(5.0)` ≈ −2.32).
pub const MOST_NEGATIVE: f32 = -3.0;
/// How far a found constant may be from the wanted one, relative.
const TOLERANCE: f32 = 0.01;
/// Length of the `movss xmm0,[rip+disp32]` the operand belongs to.
pub const INSTRUCTION_LEN: usize = 8;
/// Offset of its displacement.
pub const DISPLACEMENT_AT: usize = 4;

/// The constant bank 1 must be built from for a bias of `bias` mip levels,
/// or `None` for no bias (|bias| under 0.007, positive, not finite, or
/// beyond [`MOST_NEGATIVE`]).
pub fn bank_one_constant(bias: f32) -> Option<f32> {
    (bias.is_finite() && (MOST_NEGATIVE..=-NO_BIAS).contains(&bias)).then(|| (-bias).exp2())
}

/// The 4-byte-aligned float in `data` nearest `wanted`, within 1 %: its
/// offset and value. `data_rva` is the RVA of `data[0]`; alignment is
/// relative to RVA 0.
pub fn constant_near(data: &[u8], data_rva: usize, wanted: f32) -> Option<(usize, f32)> {
    let first = data_rva.next_multiple_of(4) - data_rva;
    let mut best: Option<(usize, f32)> = None;
    let mut offset = first;
    while let Some(word) = data.get(offset..offset + 4) {
        let value = f32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        let error = (value - wanted).abs();
        if value.is_finite()
            && error <= wanted.abs() * TOLERANCE
            && best.is_none_or(|(_, found)| error < (found - wanted).abs())
        {
            best = Some((offset, value));
        }
        offset += 4;
    }
    best
}

/// The new displacement for the instruction at `instruction_rva` to load
/// the float at `literal_rva`; `None` if out of `rel32` reach.
pub fn displacement_to(instruction_rva: usize, literal_rva: usize) -> Option<i32> {
    let next = i64::try_from(instruction_rva.checked_add(INSTRUCTION_LEN)?).ok()?;
    let literal = i64::try_from(literal_rva).ok()?;
    i32::try_from(literal - next).ok()
}

/// The bias bank 1 has when built from `constant`.
pub fn bias_of(constant: f32) -> f32 {
    -constant.log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constant_is_two_to_minus_the_bias() {
        assert_eq!(bank_one_constant(-1.0), Some(2.0));
        let quality = (0.667f32).log2();
        assert!((bank_one_constant(quality).unwrap() - 1.4993).abs() < 1e-3);
        assert!((bias_of(bank_one_constant(-0.585).unwrap()) + 0.585).abs() < 1e-5);
        assert_eq!(bank_one_constant(0.0), None);
        assert_eq!(bank_one_constant(-0.005), None);
        assert_eq!(bank_one_constant(0.5), None);
        assert_eq!(bank_one_constant(-3.5), None);
        assert_eq!(bank_one_constant(f32::NAN), None);
    }

    fn data(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn the_nearest_constant_within_one_percent_is_found() {
        let bytes = data(&[0.0, 1.0, 1.49, 1.5, 1.52, 7.0]);
        assert_eq!(constant_near(&bytes, 0x1000, 1.4993), Some((12, 1.5)));
        assert_eq!(constant_near(&bytes, 0x1000, 3.0), None);
    }

    #[test]
    fn only_aligned_words_are_read() {
        // A 1.5 at an odd offset is not a constant the engine could have.
        let mut bytes = vec![0u8];
        bytes.extend(1.5f32.to_le_bytes());
        bytes.extend([0, 0, 0]);
        assert_eq!(constant_near(&bytes, 0x1000, 1.5), None);
        // The same bytes at an RVA that makes offset 1 aligned.
        assert_eq!(constant_near(&bytes, 0x0fff, 1.5), Some((1, 1.5)));
    }

    #[test]
    fn the_displacement_is_relative_to_the_next_instruction() {
        assert_eq!(displacement_to(0x23d73ee, 0x2f0f358), Some(0x00b3_7f62));
        assert_eq!(displacement_to(0x1000, 0x1000), Some(-8));
        assert_eq!(displacement_to(0, 0x1_0000_0000), None);
    }
}
