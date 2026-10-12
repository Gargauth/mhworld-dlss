use super::*;

/// The first 32 bytes at every function the mod detours, from
/// `MonsterHunterWorld.exe` (Steam build 15539686), with the prologue length
/// and instruction count the decoder must find. Names and RVAs as in
/// `docs/re/signatures.json`.
const SITES: [(&str, u32, [u8; 32], usize, usize); 9] = [
    (
        "OPTIMAL",
        0x2383ef0,
        hex("48895c241856574154415641574883ec308b9c248000000033f6458bf94c8bf2"),
        17,
        7,
    ),
    (
        "CREATE",
        0x2384420,
        hex("48895c242044894424188954241055565741544155415641574883ec70418bd8"),
        14,
        3,
    ),
    (
        "EVAL_SS",
        0x2383cf0,
        hex("48895c240848896c241848897424204889542410574154415541564157 4883ec"),
        15,
        3,
    ),
    (
        "EVAL_ISP",
        0x2383990,
        hex("48895c240848896c2410488974241857415441554156415748 83ec50488b1948"),
        15,
        3,
    ),
    (
        "SS_ENQUEUE",
        0x23913f0,
        hex("4c8bdc415648 81ec90000000488b014c8bf1488338000f84bb020000488b4110"),
        15,
        4,
    ),
    (
        "SCALING_OPTION",
        0x43eeb0,
        hex("40534883ec600f297424500fb6d90f297c2440440f294424 30440f294c2420e8"),
        14,
        4,
    ),
    (
        "CAMERA_SETUP",
        0x228f3b0,
        hex("405553574 88dac2460feffff4881eca00200008079200048 8bfa488bd9750d32"),
        19,
        5,
    ),
    (
        "FATAL",
        0x21fd060,
        hex("488bc4488950104c8940184c894820535657 4881ec50040000488bda488d7018"),
        15,
        4,
    ),
    (
        "TAA_DRAW",
        0x2392150,
        hex("4889542410555348 8d6c24884881ec78010000488b05b631e302488bd980b8da"),
        19,
        5,
    ),
];

/// 32 bytes from hex digits; spaces are ignored.
const fn hex(text: &str) -> [u8; 32] {
    let text = text.as_bytes();
    let mut out = [0u8; 32];
    let (mut i, mut n) = (0, 0);
    while i < text.len() {
        if text[i] == b' ' {
            i += 1;
            continue;
        }
        let nibble = match text[i] {
            b'0'..=b'9' => text[i] - b'0',
            b'a'..=b'f' => text[i] - b'a' + 10,
            _ => panic!("not hex"),
        };
        out[n / 2] |= nibble << if n % 2 == 0 { 4 } else { 0 };
        n += 1;
        i += 1;
    }
    assert!(n == 64, "need 32 bytes");
    out
}

#[test]
fn every_detoured_prologue_is_planned_to_its_first_boundary_past_14() {
    for (name, rva, code, len, instructions) in SITES {
        assert_eq!(
            plan(&code),
            Ok(Prologue { len, instructions }),
            "{name} at {rva:#x}"
        );
    }
}

#[test]
fn instructions_after_the_prologues_decode_to_their_lengths() {
    let optimal = SITES[0].2;
    // mov ebx,[rsp+0x80]: SIB with disp32.
    assert_eq!(instruction_len(&optimal, 17), Ok(7));
    let scaling = SITES[5].2;
    // movaps [rsp+0x40],xmm7 and movaps [rsp+0x30],xmm8 (REX 0F 29).
    assert_eq!(instruction_len(&scaling, 14), Ok(5));
    assert_eq!(instruction_len(&scaling, 19), Ok(6));
    let enqueue = SITES[4].2;
    // mov r14,rcx: register form.
    assert_eq!(instruction_len(&enqueue, 15), Ok(3));
    // [disp32] with SIB and no base is absolute, so accepted.
    assert_eq!(
        instruction_len(&[0x8b, 0x04, 0x25, 0, 0x10, 0, 0], 0),
        Ok(7)
    );
}

#[test]
fn rip_relative_operands_are_refused() {
    // TAA draw continues with mov rax,[rip+0x2e331b6].
    let taa = SITES[8].2;
    assert_eq!(
        instruction_len(&taa, 19),
        Err(PrologueError::RipRelative { offset: 19 })
    );
    let mut code = [0x90u8; 20];
    code[..7].copy_from_slice(&[0x48, 0x8d, 0x05, 1, 2, 3, 4]); // lea rax,[rip+…]
    assert_eq!(plan(&code), Err(PrologueError::RipRelative { offset: 0 }));
}

#[test]
fn branches_prefixes_and_other_forms_are_refused_by_name() {
    let cases: [(&[u8], u8); 9] = [
        (&[0xe9, 0, 0, 0, 0], 0xe9),             // jmp rel32
        (&[0xe8, 0, 0, 0, 0], 0xe8),             // call rel32
        (&[0xc3], 0xc3),                         // ret
        (&[0xcc], 0xcc),                         // int3
        (&[0xff, 0x25, 0, 0, 0, 0], 0xff),       // jmp [rip+0]: already detoured
        (&[0x66, 0x89, 0x44, 0x24, 0x08], 0x66), // operand-size prefix
        (&[0x48, 0x8d, 0xc0], 0x8d),             // lea with a register operand
        (&[0x48, 0x83, 0xc4, 0x08], 0x83),       // add rsp,8 (/0, not /5)
        (&[0x0f, 0x28, 0x44, 0x24, 0x20], 0x28), // movaps xmm,m128 (load)
    ];
    for (code, opcode) in cases {
        let mut padded = [0x50u8; 32];
        padded[..code.len()].copy_from_slice(code);
        assert_eq!(
            plan(&padded),
            Err(PrologueError::Unsupported { offset: 0, opcode }),
            "{code:02x?}"
        );
    }
    // An unsupported instruction after accepted ones reports its own offset.
    let mut code = [0x53u8; 32];
    code[3] = 0xc3;
    assert_eq!(
        plan(&code),
        Err(PrologueError::Unsupported {
            offset: 3,
            opcode: 0xc3
        })
    );
}

#[test]
fn code_that_ends_inside_an_instruction_is_truncated() {
    let camera = SITES[6].2;
    // The cut falls inside sub rsp,0x2a0 (bytes 12..19).
    assert_eq!(
        plan(&camera[..16]),
        Err(PrologueError::Truncated { offset: 12 })
    );
    assert_eq!(
        plan(&camera[..12]),
        Err(PrologueError::Truncated { offset: 12 })
    );
    assert_eq!(plan(&[]), Err(PrologueError::Truncated { offset: 0 }));
    assert_eq!(
        instruction_len(&[0x48], 0),
        Err(PrologueError::Truncated { offset: 0 })
    );
    assert_eq!(
        instruction_len(&[0x48, 0x89, 0x44, 0x24], 0),
        Err(PrologueError::Truncated { offset: 0 })
    );
}

#[test]
fn the_trampoline_runs_the_prologue_then_jumps_back() {
    let (_, rva, code, len, _) = SITES[6];
    let entry = 0x1_4000_0000 + u64::from(rva);
    let (bytes, used) = trampoline(&code[..len], entry + len as u64).unwrap();
    assert_eq!(used, len + JUMP_LEN);
    assert_eq!(&bytes[..len], &code[..len]);
    assert_eq!(
        absolute_jump_target(&bytes[len..used]),
        Some(entry + len as u64)
    );
    // The moved prologue decodes exactly as it did in place.
    assert_eq!(plan(&bytes[..used]).map(|p| p.len), Ok(len));

    assert!(trampoline(&code[..13], 0).is_none());
    assert!(trampoline(&[0x50; MAX_PROLOGUE + 1], 0).is_none());
}

#[test]
fn the_entry_patch_jumps_to_the_hook_and_traps_in_its_padding() {
    let hook = 0x7ffa_0000_1230;
    let patch = entry_patch(hook, 19).unwrap();
    assert_eq!(absolute_jump_target(&patch), Some(hook));
    assert_eq!(&patch[JUMP_LEN..19], &[0xcc; 5]);
    // A patched entry is not a prologue that can be detoured again.
    assert!(matches!(
        plan(&patch),
        Err(PrologueError::Unsupported { opcode: 0xff, .. })
    ));
    assert!(entry_patch(hook, 13).is_none());
    assert!(entry_patch(hook, MAX_PROLOGUE + 1).is_none());
    assert_eq!(absolute_jump_target(&[0xff, 0x25, 1, 0, 0, 0, 0, 0]), None);
}

#[test]
fn the_reach_window_keeps_a_whole_block_within_rel32() {
    let target = 0x1_4228_f3b0u64;
    let size = 0x1_0000;
    let (low, high) = reach_window(target, size, 0x1_0000, 0x7fff_ffff_0000);
    assert!(low < target && target < high);
    for start in [low, high - 1] {
        let end = start + size;
        assert!(rel32_reaches(start, target) && rel32_reaches(target, start));
        assert!(rel32_reaches(end, target) && rel32_reaches(target, end));
    }
    assert!(!rel32_reaches(low - size - 1, target + 1));

    // Clamped to the address space at both ends.
    assert_eq!(
        reach_window(0x2_0000, size, 0x1_0000, 0x7fff_ffff_0000).0,
        0x1_0000
    );
    assert_eq!(
        reach_window(0x7fff_fff0_0000, size, 0x1_0000, 0x7fff_ffff_0000).1,
        0x7fff_ffff_0000
    );
    // A target outside the space yields an empty window, not an inverted one.
    let (low, high) = reach_window(0x10, size, 0x1_0000_0000_0000, 0x7fff_ffff_0000);
    assert!(low >= high);
}

#[test]
fn rel32_reach_is_exact_at_the_limits() {
    let from = 0x1_0000_0000u64;
    assert!(rel32_reaches(from, from + 0x7fff_ffff));
    assert!(!rel32_reaches(from, from + 0x8000_0000));
    assert!(rel32_reaches(from, from - 0x8000_0000));
    assert!(!rel32_reaches(from, from - 0x8000_0001));
}
