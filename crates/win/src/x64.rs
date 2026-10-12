//! The x86-64 encoding knowledge a detour needs: prologue lengths, the
//! trampoline and the entry jump.
//!
//! Pure. The decoder is deliberately not a disassembler: it accepts exactly
//! the instruction forms found in the prologues the mod detours (see
//! `docs/re/signatures.json`, role `detour`) and rejects everything else by
//! name, because a prologue it does not understand is a prologue it cannot
//! prove safe to move:
//!
//! | form                         | encoding            | example site    |
//! |------------------------------|---------------------|-----------------|
//! | `push r64`                   | `[REX] 50+r`        | every site      |
//! | `mov r/m, r` · `mov r, r/m`  | `[REX] 89/8B /r`    | OPTIMAL, FATAL  |
//! | `lea r, m`                   | `[REX] 8D /r`       | CAMERA_SETUP    |
//! | `sub r/m, imm32` · `imm8`    | `[REX] 81/83 /5`    | SS_ENQUEUE      |
//! | `movaps m128, xmm`           | `[REX] 0F 29 /r`    | SCALING_OPTION  |
//! | `movzx r32, r/m8`            | `[REX] 0F B6 /r`    | SCALING_OPTION  |
//!
//! None of these depends on where it executes **unless** its ModRM selects
//! RIP-relative addressing, which is rejected. So relocating an accepted
//! prologue is copying it; no displacement is ever rewritten. Relative
//! branches, calls, returns, legacy prefixes and anything else are refused.

use std::fmt;

/// Length of [`absolute_jump`]: `jmp [rip+0]` (6 bytes) and the 8-byte target.
pub const JUMP_LEN: usize = 14;

/// Longest prologue [`plan`] can return: the last instruction starts at most
/// at byte 13 and an accepted instruction is at most 15 bytes long.
pub const MAX_PROLOGUE: usize = JUMP_LEN - 1 + 15;

/// Bytes a trampoline needs: the moved prologue and the jump back.
pub const MAX_TRAMPOLINE: usize = MAX_PROLOGUE + JUMP_LEN;

/// `int3`, used to pad the entry patch past the jump.
const INT3: u8 = 0xcc;

/// A 14-byte absolute jump: `jmp qword [rip+0]` followed by the target.
///
/// Not `jmp rel32`: the mod and the game share a 64-bit address space with no
/// guarantee of being within 2 GiB of each other, and a displacement that
/// silently does not fit shows up as an unrelated crash.
pub fn absolute_jump(target: u64) -> [u8; JUMP_LEN] {
    let mut bytes = [0u8; JUMP_LEN];
    bytes[0] = 0xff;
    bytes[1] = 0x25;
    bytes[6..].copy_from_slice(&target.to_le_bytes());
    bytes
}

/// The target of an [`absolute_jump`], if `bytes` starts with one.
pub fn absolute_jump_target(bytes: &[u8]) -> Option<u64> {
    let jump = bytes.get(..JUMP_LEN)?;
    if jump[..6] != [0xff, 0x25, 0, 0, 0, 0] {
        return None;
    }
    let mut target = [0u8; 8];
    target.copy_from_slice(&jump[6..]);
    Some(u64::from_le_bytes(target))
}

/// Why a prologue cannot be moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrologueError {
    /// An instruction at `offset` is not one of the accepted forms.
    Unsupported {
        /// Offset of the instruction from the entry.
        offset: usize,
        /// Its first byte (after any REX prefix).
        opcode: u8,
    },
    /// The instruction at `offset` addresses memory relative to RIP.
    RipRelative {
        /// Offset of the instruction from the entry.
        offset: usize,
    },
    /// The bytes ended in the middle of the instruction at `offset`.
    Truncated {
        /// Offset of the instruction from the entry.
        offset: usize,
    },
}

impl fmt::Display for PrologueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrologueError::Unsupported { offset, opcode } => {
                write!(
                    f,
                    "unsupported instruction (opcode {opcode:#04x}) at +{offset:#x}"
                )
            }
            PrologueError::RipRelative { offset } => {
                write!(f, "rip-relative operand at +{offset:#x}")
            }
            PrologueError::Truncated { offset } => {
                write!(f, "code ends inside the instruction at +{offset:#x}")
            }
        }
    }
}

impl std::error::Error for PrologueError {}

/// The whole instructions a detour displaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prologue {
    /// Bytes covered: at least [`JUMP_LEN`], ending on an instruction
    /// boundary, at most [`MAX_PROLOGUE`].
    pub len: usize,
    /// How many instructions that is.
    pub instructions: usize,
}

/// Decode whole instructions from the start of `code` until at least
/// [`JUMP_LEN`] bytes are covered.
pub fn plan(code: &[u8]) -> Result<Prologue, PrologueError> {
    let mut len = 0;
    let mut instructions = 0;
    while len < JUMP_LEN {
        len += instruction_len(code, len)?;
        instructions += 1;
    }
    Ok(Prologue { len, instructions })
}

/// Length of the accepted instruction at `code[offset..]`.
pub fn instruction_len(code: &[u8], offset: usize) -> Result<usize, PrologueError> {
    let byte = |at: usize| {
        code.get(offset + at)
            .copied()
            .ok_or(PrologueError::Truncated { offset })
    };
    let mut at = 0;
    if (0x40..=0x4f).contains(&byte(0)?) {
        at += 1;
    }
    let opcode = byte(at)?;
    at += 1;
    let unsupported = Err(PrologueError::Unsupported { offset, opcode });
    let len = match opcode {
        0x50..=0x57 => at,
        0x89 | 0x8b => at + modrm_len(code, offset, offset + at, ModRm::Any, opcode)?,
        0x8d => at + modrm_len(code, offset, offset + at, ModRm::Memory, opcode)?,
        0x81 => at + modrm_len(code, offset, offset + at, ModRm::Group(5), opcode)? + 4,
        0x83 => at + modrm_len(code, offset, offset + at, ModRm::Group(5), opcode)? + 1,
        0x0f => {
            let second = byte(at)?;
            at += 1;
            match second {
                0x29 | 0xb6 => at + modrm_len(code, offset, offset + at, ModRm::Any, second)?,
                _ => {
                    return Err(PrologueError::Unsupported {
                        offset,
                        opcode: second,
                    })
                }
            }
        }
        _ => return unsupported,
    };
    if offset + len > code.len() {
        return Err(PrologueError::Truncated { offset });
    }
    Ok(len)
}

/// What the ModRM byte of an opcode may encode.
#[derive(Clone, Copy)]
enum ModRm {
    /// Register or memory operand.
    Any,
    /// Memory operand only (`lea`).
    Memory,
    /// Register or memory, with the reg field fixed to this opcode extension.
    Group(u8),
}

/// Length of the ModRM byte with its SIB and displacement, at `at`.
/// `offset` and `opcode` identify the instruction, for errors.
fn modrm_len(
    code: &[u8],
    offset: usize,
    at: usize,
    kind: ModRm,
    opcode: u8,
) -> Result<usize, PrologueError> {
    let modrm = *code.get(at).ok_or(PrologueError::Truncated { offset })?;
    let (mode, reg, rm) = (modrm >> 6, (modrm >> 3) & 7, modrm & 7);
    match kind {
        ModRm::Memory if mode == 3 => return Err(PrologueError::Unsupported { offset, opcode }),
        ModRm::Group(wanted) if reg != wanted => {
            return Err(PrologueError::Unsupported { offset, opcode })
        }
        _ => {}
    }
    if mode == 3 {
        return Ok(1);
    }
    let mut len = 1;
    if rm == 4 {
        let sib = *code
            .get(at + 1)
            .ok_or(PrologueError::Truncated { offset })?;
        len += 1;
        // [index*scale + disp32] with no base: absolute, position-independent.
        if mode == 0 && sib & 7 == 5 {
            len += 4;
        }
    } else if mode == 0 && rm == 5 {
        return Err(PrologueError::RipRelative { offset });
    }
    len += match mode {
        1 => 1,
        2 => 4,
        _ => 0,
    };
    Ok(len)
}

/// The trampoline for a detour: the displaced prologue, moved verbatim (every
/// accepted form is position-independent), then a jump to `resume`, the first
/// byte after the prologue in the original function. Returns the buffer and
/// how much of it is used.
pub fn trampoline(prologue: &[u8], resume: u64) -> Option<([u8; MAX_TRAMPOLINE], usize)> {
    if prologue.len() < JUMP_LEN || prologue.len() > MAX_PROLOGUE {
        return None;
    }
    let mut bytes = [INT3; MAX_TRAMPOLINE];
    bytes[..prologue.len()].copy_from_slice(prologue);
    bytes[prologue.len()..prologue.len() + JUMP_LEN].copy_from_slice(&absolute_jump(resume));
    Some((bytes, prologue.len() + JUMP_LEN))
}

/// The bytes written over a function's entry: a jump to `hook`, padded with
/// `int3` to `len` (the planned prologue length) so a disassembly never shows
/// half an instruction, and executing the padding traps instead of running
/// garbage.
pub fn entry_patch(hook: u64, len: usize) -> Option<[u8; MAX_PROLOGUE]> {
    if !(JUMP_LEN..=MAX_PROLOGUE).contains(&len) {
        return None;
    }
    let mut bytes = [INT3; MAX_PROLOGUE];
    bytes[..JUMP_LEN].copy_from_slice(&absolute_jump(hook));
    Some(bytes)
}

/// Half-width of the window a `rel32` reaches.
const REACH: u64 = 1 << 31;

/// The range `[low, high)` of start addresses at which a block of `size`
/// bytes lies wholly within `rel32` reach of `target` (in both directions),
/// clamped to `[floor, ceiling)` (the user address space).
///
/// Trampolines are allocated inside it so that the trampoline and the code it
/// returns to are always within a `rel32` of each other, whatever the
/// encoding of the jump between them.
pub fn reach_window(target: u64, size: u64, floor: u64, ceiling: u64) -> (u64, u64) {
    let margin = REACH.saturating_sub(size);
    let low = target.saturating_sub(margin).max(floor);
    let high = target.saturating_add(margin).min(ceiling);
    (low, high.max(low))
}

/// Whether a `rel32` at `from` (the address after the instruction) can reach
/// `to`.
pub fn rel32_reaches(from: u64, to: u64) -> bool {
    let delta = to.wrapping_sub(from) as i64;
    i32::try_from(delta).is_ok()
}

#[cfg(test)]
mod tests;
