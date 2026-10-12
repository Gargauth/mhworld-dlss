# ReSignatures — odolnost Gargauth gate vůči updatu exe

Build: `MonsterHunterWorld.exe` 15539686, PE ts 0x66d13cb6, base 0x140000000.
Skripty (reprodukovatelné, offline, ~10 s): `re/scratch/ReSignatures/sigs.py` (gate sites → ověření + AOB),
`offsets.py` (globál→`[reg+off]` páry), `reflect.py` (MT reflection jména polí), `emit.py` (→ `reports/re/signatures.json`, 69 záznamů, self-check displacementů).
Scanner: maskovaný regex nad **celým .text** (VirtualAddress..+VirtualSize z PE), wildcard = RIP disp32 a rel8/rel32 cílů jmp/jcc/call (capstone `disp_offset`/`imm_offset`).

## 1. Otázky a verdikty

| # | Otázka | Verdikt |
|---|---|---|
| 1 | Sedí všech 16 gate sites (hijack 9, native 5, tune 2) bajt po bajtu s exe? | **OBSERVED: ano, 16/16** (`verified_bytes=true`; BANK_ONE_SOURCE s Gargauthovým `wild 4..8`) |
| 2 | Má každá adresa unikátní AOB v .text? | **OBSERVED: ano, 16/16 unikátní** (minimální i „robustní“ ≥16 pevných bajtů + 2 instrukce rezervy); 15/16 unikátní i po wildcardu struct disp/imm („loose“), výjimka RATIO_SETTER_TAIL (loose 2 hity) |
| 3 | Globály lze odvodit z RIP-relative instrukce v signatuře? | **OBSERVED: ano** — cfg 0x1451c4480, context 0x1451c4400, settings 0x1451c4368, konstanty 0x142e4fe54/58, 0x142f0f358 |
| 4 | Gargauthovy komentáře k globálům (pathdump.rs:124 „+0x51c4368“) | **CONFIRMED** (SETTINGS_LOAD → 0x1451c4368) |
| 5 | Struct offsety lze kotvit signaturou? | **OBSERVED: 46/49** kotveno (gate site / pár „load globálu → [reg+off]“ / reflection registrace / setter); 3 bez kotvy (cfg+0xb0, cfg+0x1f44c, settings+0xe6a0 – jen log/komentář) |
| 6 | Jména polí (MT reflection) pro offsety Gargauthu | **OBSERVED: 25 pojmenováno** (např. cfg+0xa5 `mUseDLSS`, +0xa6 `mUseDLISP`, +0xa8 `mDLPerfQuality`, +0xbc `mUseCAS`, +0x1f450 `mpPrimaryScene`, settings+0xb43f `mSSLRDitherResolve`, +0xe964 `mCheckerboardAlphaUnrollNear`) |
| 7 | odt1 globály = Gargauthovy? | **CONFIRMED**: odt1 `g_renderer`=cfg 0x1451c4480, `g_scene`=settings 0x1451c4368, `g_camera`=render context 0x1451c4400 (vše z kTaaUpdatePattern) |
| 8 | odt1 signatury v tomto buildu unikátní? | **CONFIRMED**: všech 6 vzorů game.cpp:49-72 má právě 1 hit |
| 9 | tune.rs objekt pro SSLR/contact/unroll/sampler bank = settings 0x1451c4368? | **CONFIRMED** (reflection registrace 0x142270c60 registruje `mTextureTargetMode`@0x260 = cíl SETTINGS_LOAD store; sampler bank pár 0x14238355d) |
| 10 | Gargauth cfg+0x1f8 „previous“ | **PARTIAL**: reflection jméno `ContentScalePF` (previous-frame content scale) – význam sedí, jméno upřesňuje |
| 11 | Detour prologue signatury přežijí update? | **INFERENCE: rizikové** — obsahují frame size (`sub rsp,imm`) a pořadí push; viz §5 |
| 12 | Je hardcoded RDATA range (tune.rs:447) nutný? | **REFUTED jako nutnost**: rozsah = PE sekce, odvoditelné z hlavičky za běhu |
| 13 | Doplněk po 0.2: `DownsampleVolume` | **OBSERVED**: 17. gate site `TONEMAP_RELEASE` (`0x1424ce3df`, destruktor tonemap filtru) dává globály `g_tonemap` `0x1451cbc08` a zámek `0x1451cbc48`; offset `tonemap+0x210` (`mDownsampleVolume`) kotvený v registraci vlastností `0x1424cfe02`. Viz `ReVolumetrics.md` §3. |

## 2. Nálezy s důkazy

### 2.1 Inventura hardcoded adres (Gargauth src)

| Konst. | RVA | Účel | soubor:řádek | v gate? |
|---|---|---|---|---|
| OPTIMAL | 0x2383ef0 | detour: DLSS optimal settings | hijack.rs:57-58 | ano (hijack SITES[0]) |
| CREATE | 0x2384420 | detour: DLSS feature create | hijack.rs:69-70 | ano |
| EVAL_SS | 0x2383cf0 | detour: SS evaluate | hijack.rs:77-78 | ano |
| EVAL_ISP | 0x2383990 | detour: ISP evaluate | hijack.rs:85-86 | ano |
| SS_ENQUEUE | 0x23913f0 | detour: SS enqueue guard | hijack.rs:112-113 | ano |
| SCALING_OPTION | 0x43eeb0 | detour: menu Resolution Scaling | hijack.rs:136-137 | ano |
| CONFIG_LOAD | 0x2290426 | `mov rax,[rip]` → cfg global, `[rax+0x198]` | hijack.rs:150-156 | ano |
| RATIO_SETTER | 0x229be60 | volaná engine fce (ratio) | hijack.rs:791-792 | ano |
| RATIO_SETTER_TAIL | 0x229beb2 (=+0x52) | store `[rcx+0x200]` | hijack.rs:801-802 | ano |
| LOAD_CONTEXT | 0x228f3f4 | `mov rax,[rip]` → render context global | native.rs:67-68 | ano (native SITES[0]) |
| READ_X | 0x228faaf | `movss xmm1,[rax+0x38]` jitter x | native.rs:75-76 | ano |
| READ_Y | 0x228facc | `movss xmm1,[rax+0x3c]` jitter y | native.rs:83-84 | ano |
| CAMERA_SETUP | 0x228f3b0 | detour: camera setup | native.rs:327-328 | ano |
| FATAL | 0x21fd060 | detour: engine fatal reporter | native.rs:822-823 | ano |
| SETTINGS_LOAD | 0x2412ef | `mov rax,[rip]` → settings global, `[rax+0x260]` | tune.rs:58-59 | ano (tune SITES[0]) |
| BANK_ONE_SOURCE | 0x23d73ee | `movss xmm0,[rip]` — disp přepisován | tune.rs:437-438 | ano (wild 4..8) |
| RDATA | 0x2e13000..0x3e78000 | rozsah .rdata pro hledání konstanty | tune.rs:447 | **ne** |
| KNOWN_TIMESTAMP / SIZE_OF_IMAGE | 0x66d13cb6 / 0x55d3000 | info v logu | gate.rs:135-136 | info, ne test |
| (komentáře) 0x23d0e70, 0x228f3b0 caller, FUN_14043dcb0, FUN_1423c3790, 0x142e4fe54/58 | — | dokumentace | native.rs:10,31; pathdump.rs:120,194; hijack.rs:561 | ne (nepoužito v kódu) |

Struct offsety v kódu (ne v gate jako adresa, ale implicitně závislé na buildu):
hijack.rs:120-122 (node 0x1a8/0x150/0x168), 270-280 (DLSS obj 0x38/0x40, descriptor 0x08/0x18/0x2c), 832-834 (cfg 0x1f0/0x200/0x204), 975 (cfg 0x7b3b0), 156 (0x198 – **v gate**, z bajtů); native.rs:93-94 (0x38/0x3c – **v gate**, odvozené z READ_X/Y_BYTES[4] ✓); tune.rs:82-83 (0xe964/8), 202 (0x1a8), 255-261 (0x1f450, 0x58, 0x7b3b4), 334-335 (0xbc/0xc0), 389-393 (0xb43f/0xb440/0xb500/0xb501); pathdump.rs:183-276 (cfg 0x78, device 0x42700/0x42718, settings 0xe9e4/5, set 0x58/0x420/0x460/0x6a0-0x6b8, ctx 0xd50-0xd5c, cfg 0xa5-0xc0,0x198,0x1dc,0x1f0-0x208,0x1f448,0x7b3b0-0x7b3e0). camera.rs:27-30,233-236 a fg.rs:104 jsou offsety v `CBViewProjection` (RDEF shaderu, ne exe) — mimo rozsah AOB.

### 2.2 Ověření gate bajtů — OBSERVED
`sigs.py`: pro každou z 16 sites `img[rva:rva+len] == Gargauth bytes` (mimo `wild`) → 16× `verified`. Např. `0x14228f3f4 mov rax,[rip+0x2f35005] ; ->0x1451c4400` (g d), `0x14229beb2..: xorps; and eax,0xfffffff0; cvtsi2ss; divss; movss [rcx+0x200],xmm0; ret`.

### 2.3 Signatury gate sites (robustní varianta; offset k cíli = 0 u všech)

| Site | Pattern (`??` = RIP disp / rel) | pevné B | loose hits | RIP cíle |
|---|---|---|---|---|
| OPTIMAL | `48 89 5C 24 18 56 57 41 54 41 56 41 57 48 83 EC 30 8B 9C 24 80 00 00 00 33 F6` | 26 | 1 | – |
| CREATE | `48 89 5C 24 20 44 89 44 24 18 89 54 24 10 55 56 57 41 54 41 55 41 56 41 57 48 83 EC 70 41 8B D8 41 8B F9` | 35 | 1 | – |
| EVAL_SS | `48 89 5C 24 08 48 89 6C 24 18 48 89 74 24 20 48 89 54 24 10 57 41 54 41 55 41 56 41 57 48 83 EC 30 48 8B 19 4C 8B E9` | 39 | 1 | – |
| EVAL_ISP | `48 89 5C 24 08 48 89 6C 24 10 48 89 74 24 18 57 41 54 41 55 41 56 41 57 48 83 EC 50 48 8B 19 48 8B F1 0F 29 74 24 40 4C 8B F2 0F 29 7C 24 30 44 0F 29 44 24 20` | 53 | 1 | – |
| SS_ENQUEUE | `4C 8B DC 41 56 48 81 EC 90 00 00 00 48 8B 01 4C 8B F1 48 83 38 00 0F 84 ?? ?? ?? ??` | 24 | 1 | – |
| SCALING_OPTION | `40 53 48 83 EC 60 0F 29 74 24 50 0F B6 D9 0F 29 7C 24 40 44 0F 29 44 24 30 44 0F 29 4C 24 20` | 31 | 1 | – |
| CONFIG_LOAD | `48 8B 05 ?? ?? ?? ?? 48 8B D9 48 8B 90 98 01 00 00 48 8B CA 48 C1 E9 20 85 D2` | 22 | 1 | cfg 0x1451c4480 |
| RATIO_SETTER | `F3 0F 10 05 ?? ?? ?? ?? 0F 2F C8 77 ?? F3 0F 10 15 ?? ?? ?? ?? 0F 2F CA 72 ?? 0F 28 D1 F3 0F 5D D0` | 23 | 1 | 0x142e4fe58, 0x142e4fe54 |
| RATIO_SETTER_TAIL | `0F 57 C0 83 E0 F0 F3 48 0F 2A C0 F3 0F 5E C1 F3 0F 11 81 00 02 00 00 C3 CC` | 25 | **2** | – |
| LOAD_CONTEXT | `48 8B 05 ?? ?? ?? ?? 44 89 4C 24 60 44 89 54 24 64 89 54 24 68 44 89 44 24 6C 75 ??` | 23 | 1 | ctx 0x1451c4400 |
| READ_X | `F3 0F 10 48 38 66 0F 6E C2 0F 5B C0 F3 0F 5E C8 66 41 0F 6E C0 F3 0F 11 8D F0 00 00 00` | 29 | 1 | – |
| READ_Y | `F3 0F 10 48 3C 48 8B 43 08 0F 5B C0 F3 0F 5E C8 F3 0F 11 8D F4 00 00 00 48 85 C0` | 27 | 1 | – |
| CAMERA_SETUP | `40 55 53 57 48 8D AC 24 60 FE FF FF 48 81 EC A0 02 00 00 80 79 20 00 48 8B FA` | 26 | 1 | – |
| FATAL | `48 8B C4 48 89 50 10 4C 89 40 18 4C 89 48 20 53 56 57 48 81 EC 50 04 00 00 48 8B DA` | 28 | 1 | – |
| SETTINGS_LOAD | `48 8B 05 ?? ?? ?? ?? C7 80 60 02 00 00 00 00 00 00 48 8B 0D ?? ?? ?? ?? E8 ?? ?? ?? ?? 48 8B 0D ?? ?? ?? ??` | 20 | 1 | settings 0x1451c4368 |
| BANK_ONE_SOURCE | `F3 0F 10 05 ?? ?? ?? ?? 44 89 6C 24 48 E8 ?? ?? ?? ?? F3 44 0F 10 1D ?? ?? ?? ?? 41 0F 57 C3 F3 0F 11 44 24 4C F3 0F 10 05 ?? ?? ?? ??` | 29 | 1 | 0x142f0f358 (2.0f) |

Minimální unikátní varianty (`pattern_min`) jsou v JSON; nejkratší READ_X/READ_Y 9 B a SS_ENQUEUE/LOAD_CONTEXT/CAMERA_SETUP 12 B — doporučeno používat robustní.

### 2.4 Globály (resolve = konec instrukce + disp32 z match)
| Globál | VA | Signatura | disp @ match+ |
|---|---|---|---|
| render config (cfg, odt1 g_renderer) | 0x1451c4480 | CONFIG_LOAD | 3 (insn len 7) |
| render context (odt1 g_camera) | 0x1451c4400 | LOAD_CONTEXT | 3 |
| render settings (odt1 g_scene) | 0x1451c4368 | SETTINGS_LOAD | 3 |
| ratio clamp hi / lo | 0x142e4fe58 / 0x142e4fe54 | RATIO_SETTER | 4 / 0xd+4 |
| bank-1 zdroj 2.0f | 0x142f0f358 | BANK_ONE_SOURCE | 4 (přepisováno) |

Křížová kontrola: SCALING_OPTION samo čte 0x142e4fe58 (`0x14043eeea movss xmm6,[rip+0x2a10f66]`) a volá setter 0x14229bf30/0x14229bde0 s `rcx=[0x1451c4480]` (`0x14043ef73`, `0x14043efce`) — OBSERVED.

### 2.5 Struct offsety — kotvy (vše `disp_check=true`: na místě matche leží v `offset` přesně daný displacement)
Prioritně (1) gate site, kterou Gargauth sám čte; (2) MT reflection registrace (`lea r,[rip->"mName"]` … `lea rax,[obj+off]`) — **nejodolnější**, jméno je stabilní string; (3) pár load globálu → `[reg+off]`; (4) setter funkce.

| Pole | jméno (reflection) | kotva | 
|---|---|---|
| cfg+0x198 | (`mScreenSize`, lea rsi @0x14229472b) | CONFIG_LOAD disp@13 |
| cfg+0x1f0 OWN_SCALE | – | setter 0x14229bde0, `movss [rcx+0x1f0],xmm0` disp@99 (z SCALING_OPTION) |
| cfg+0x200 LIVE_RATIO | – | RATIO_SETTER_TAIL disp@19 |
| cfg+0x204 BUILT_RATIO | – | pár 0x141fd5181 `movss xmm1,[rcx+0x204]`; též setter 0x14229bdf3 |
| cfg+0x1f8 | ContentScalePF | pár 0x1423cec56 + reflection |
| cfg+0xa5/0xa6/0xa8 | mUseDLSS / mUseDLISP / mDLPerfQuality | pár 0x14239204a / reflection 0x142296a65 / pár 0x14043dce4 |
| cfg+0xbc/0xc0 | mUseCAS / mCASUserSharpness | pár 0x14043de5c / 0x14043de6c |
| cfg+0x1f448 | mBackBufferSize | pár 0x14043dcc2 |
| cfg+0x1f450 | mpPrimaryScene | pár 0x141c27a97 |
| cfg+0x7b3b0 | – | setter 0x14229bf30 `cmp dl,[rcx+0x7b3b0]` disp@8 (pár 0x140241cc4 také) |
| cfg+0x7b3b1/bc/c0/cc | mDynamicResolutionActive/Level/UpThreshold/ContentScale | reflection |
| cfg+0x7b3b4 | mDynamicResolutionMode (getter 0x142298e60 `mov eax,[rcx+0x7b3b4]; ret`, setter 0x14229bf70) | pár 0x14235f62c |
| cfg+0x78, 0xb4, 0x1dc, 0x1f4, 0x1fc, 0x208 | – | páry (viz JSON) |
| ctx+0x38 / 0x3c | – | READ_X / READ_Y disp@4 (= Gargauthův `READ_*_BYTES[4]` ✓) |
| ctx+0xd50 | – | pár 0x141ac3ede |
| settings+0x260 | mTextureTargetMode | SETTINGS_LOAD disp@9 |
| settings+0x1a8 SAMPLER_BANK | – | pár 0x14238355d `mov ecx,[rax+0x1a8]` |
| settings+0xe964/0xe968 | mCheckerboardAlphaUnrollNear/Far | reflection 0x142271ea6 / 0x142271f3f |
| settings+0xe9e4/5 | – | páry 0x14043dd91 / 0x14043dde1 (handler menu NVIDIA DLSS) |
| settings+0xb43f/0xb440/0xb500/0xb501 | mSSLRDitherResolve / mSSLRGBufferJitter / mContactShadowEnableNoise / mFacialContactShadowEnableNoise | reflection |
| ss_node+0x150/0x168/0x1a8 | – | SS_ENQUEUE+0x178/+0x1b2/+0x184 |
| eval desc +0x18 / +0x2c | – | EVAL_ISP+0x3f / EVAL_SS+0x41, EVAL_ISP+0x54 |
| DLSS obj +0x40 | – | OPTIMAL+0x3d `cmp [rdx+0x40],rsi` |
| **bez kotvy** | cfg+0xb0 (jen pathdump log), cfg+0x1f44c (= mBackBufferSize+4, odvoď), settings+0xe6a0 (jen komentář tune.rs; jediné `+0xe6a0` v .text je `lea rdx,[rsp+0xe6a0]` @0x142614e3e, nerelevantní) | — |

Negativní hledání: `g grep "\+ 0xe6a0"` nad celým disasm indexem → 1 hit (stack). Hraniční: pathdump offsety přes `set`/`device` (0x42700, 0x6a8…) neověřeny – jen log.

## 3. Porovnání s odt1 (game.cpp:49-72, ngx_shim.cpp:905-939)

| odt1 | hit (tento build) | vztah ke Gargauthu |
|---|---|---|
| kTaaUpdatePattern | 0x1423931e0 (1) | RIP → 0x1451c4480 (`[rax+0x1f450]`), 0x1451c4368, 0x1451c4400 → **stejné 3 globály** |
| kTaaDrawPrologue | 0x142392150 (1) | jen prolog, 19 B, bez wildcard – stejné riziko jako Gargauthovy detoury |
| kRecomputePattern | 0x14229a8c0 (1) | obsahuje struct disp 0x78, 0x42700, 0x1f448, 0x7b440 natvrdo |
| kSceneAaSetterPattern | 0x142287a00 (1) | **dobrý vzor**: disp offsetu je `??` a čte se z matche (kSceneAaSetterDisp=29) |
| kApiFlagPattern, kOperatorNewPattern | 0x1424e9620, 0x142175210 (1) | – |
| ngx_shim FindTable | exporty `NVSDK_NGX_D3D12_*` 0x273f060… (OBSERVED v PE export table) | **nejodolnější** princip: export name → první `mov r64,[rip]` → slot; konzistence 5 slotů. Gargauth hookuje herní wrappery (0x2383xxx), ne exporty |

Rozdíl: odt1 = scan-first (bez exact bytes), Gargauth = exact-bytes na pevné RVA (all-or-nothing). Návrh kombinuje oboje.

## 4. Návrh Rust API pro gate.rs (jen návrh, neimplementováno)

```rust
/// Jeden bajt vzoru: pevný nebo libovolný.
pub(crate) struct Pattern { bytes: &'static [u8], mask: &'static [u8] }   // mask 0xFF/0x00; z "48 8B 05 ?? .."
pub(crate) struct Sig {
    pub what: &'static str,
    pub pattern: Pattern,          // stupeň 1: unikátní v .text
    pub to_site: isize,            // match → site (u všech 16 je 0)
    pub site: &'static Site,       // stupeň 2: dnešní exact bytes + wild (beze změny)
    pub known_rva: usize,          // rychlá cesta + log „posunuto o Δ“
}
pub(crate) enum Located { At(usize), Missing, Ambiguous(u32) }
pub(crate) struct Resolved { pub rva: [usize; N_SITES] }   // jen pokud VŠECHNY At + Matches

/// .text z PE hlavičky (žádný RDATA/TEXT natvrdo); jeden průchod pro všechny vzory
/// (první bajt prefiltr / memchr), O(|.text|·k).
unsafe fn scan_all(base: usize, sigs: &[&Sig]) -> Vec<Located>;
/// Stupeň 2: pro každý At(rva) `matches(site, bytes@rva)`; pokud kterýkoli
/// Missing/Ambiguous/Differs → Decision::StandDown (unpacked) / Wait. Žádné částečné zapnutí.
unsafe fn resolve(base: usize, unpacked: bool) -> Result<Resolved, Decision>;
/// Odvozené hodnoty čtené ze stejné paměti, ne z konstant:
fn rip_target(base: usize, insn_rva: usize, disp_at: usize, insn_len: usize) -> usize;
fn disp_u32(base: usize, rva: usize, at: usize, size: usize) -> u32; // struct offsety
```
Pravidla: (a) rychlá cesta `known_rva` → pokud exact match na všech 16, hotovo bez scanu; (b) jinak scan, `Ambiguous`/`Missing` = stand-down s logem `what`+počet hitů; (c) stupeň 2 se exact bytes s posunutou adresou — u prologů je to *záměrně* přísné (frame size se mění = sémantika se mohla změnit); (d) struct offsety se **čtou z displacementu** v nalezené instrukci (jako už dělá native.rs:93-94) a pro reflection pole přes jméno: `find string "mUseDLISP\0"` → xref `lea` → následující `lea rax,[reg+disp]`; (e) scan musí běžet **před** instalací detourů (přepsané prology = 0 hitů) a výsledek se cachuje; `wild` u BANK_ONE_SOURCE zůstává (vzor má disp `??`); (f) testy: tabulka `signatures.json` jako fixture — každý vzor unikátní a `to_site` dává dnešní RVA.

Konkrétní změny (soubor:řádek): gate.rs:117-125 `sites()` → `resolve()`; hijack.rs:57-137, 150, 791-807 / native.rs:67-94, 327, 822 / tune.rs:58-78, 437 — přidat `Sig` vedle `Site`; tune.rs:447 `RDATA` → sekce z PE (tune.rs:502 `section_of` už existuje); hijack.rs:832-834, 975, tune.rs:82-83, 202, 255-261, 334-335, 389-393 → offset z kotvy (§2.5) místo konstanty, s assert na dnešní hodnotu v testu.

## 5. Rizikové signatury

| Signatura | Riziko | Mitigace |
|---|---|---|
| RATIO_SETTER_TAIL | končí `C3 CC` (padding mezi funkcemi); loose = 2 hity | zkrátit o `CC`, kotvit jako RATIO_SETTER+Δ uvnitř stejné fce (scan omezit na tělo z .pdata) |
| všechny detour prology (OPTIMAL, CREATE, EVAL_*, SS_ENQUEUE, SCALING_OPTION, CAMERA_SETUP, FATAL) | `sub rsp,imm`, `lea rbp,[rsp-…]`, pořadí push = mění se s jakoukoli změnou těla; bez RIP = nic k wildcardu | akceptovat (přísnost = bezpečnost), případně alternativní kotva přes volající (`call rel32` z unikátního místa, např. SCALING_OPTION volá 0x14229bde0/0x14229bf30) |
| READ_X / READ_Y | minimální 9 B; robustní obsahuje stack disp `[rbp+0xf0]/[rbp+0xf4]` | hledat jen v těle CAMERA_SETUP (0x228f3b0 .pdata) |
| LOAD_CONTEXT | `75 ??` jcc + stack spill offsety 0x60-0x6c | totéž, v těle CAMERA_SETUP |
| SETTINGS_LOAD | 3 RIP + call — málo pevných bajtů (20) | ověřit cíl RIP = stejný globál jako odt1 TaaUpdate druhý load (cross-check) |
| hook-internal offsety (ss_node, eval desc, DLSS obj) | vzory 12 B | scan jen v těle hooknuté funkce |
| reflection kotvy | spoléhají na to, že registrace zůstane `lea rax,[rdi/r14+off]` | name-driven lookup (string → xref) místo bajtového vzoru |
| cfg+0xb0, +0x1f44c, settings+0xe6a0 | bez kotvy | jen log/komentář; 0x1f44c = 0x1f448+4 |

## 6. UNKNOWN + runtime experimenty
- **UNKNOWN**: zda budoucí build změní prology (nelze staticky). Experiment: v `gate.rs::attempt` logovat pro každou Sig `known_rva` vs. nalezené RVA a počet hitů (dry-run scan i v dnešním buildu) — rozhodne, zda scan i exact-bytes souhlasí (očekáváno Δ=0, 1 hit).
- **UNKNOWN**: význam cfg+0xb0 (pathdump). Experiment: pathdump.rs:220 logovat při přepnutí menu Image Quality; změna hodnoty spolu s +0x1f0 rozhodne.
- **INFERENCE**: objekt registrovaný 0x142270c60 je třída instance v 0x1451c4368 (opora: +0x260 `mTextureTargetMode` = store v SETTINGS_LOAD; ctor 0x1422646f0 inicializuje 0xb440/0xb500/0xe964). Experiment: tune.rs `locate()` — číst vtable ptr objektu a porovnat s vtable, kterou zapisuje ctor 0x1422646f0.

## 7. Mapa adres
Gate: 0x142383ef0, 0x142384420, 0x142383cf0, 0x142383990, 0x1423913f0, 0x14043eeb0, 0x142290426, 0x14229be60, 0x14229beb2, 0x14228f3f4, 0x14228faaf, 0x14228facc, 0x14228f3b0, 0x1421fd060, 0x1402412ef, 0x1423d73ee.
Globály: cfg 0x1451c4480, ctx 0x1451c4400, settings 0x1451c4368; konstanty 0x142e4fe54/58, 0x142f0f358.
Setter/accessory: own-scale 0x14229bde0, DR switch 0x14229bf30, DR mode get/set 0x142298e60/0x14229bf70.
Reflection registrace: cfg 0x142294000, settings 0x142270c60; settings ctor 0x1422646f0.
odt1: TaaUpdate 0x1423931e0, TaaDraw 0x142392150, Recompute 0x14229a8c0, SceneAaSetter 0x142287a00, ApiFlag 0x1424e9620, OperatorNew 0x142175210; NGX D3D12 exporty 0x14273f060–0x14273f480.
