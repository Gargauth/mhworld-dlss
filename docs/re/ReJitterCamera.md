# ReJitterCamera — jitter, kamera a CBViewProjection (MHW build 15539686)

Zdroj: statická analýza `MonsterHunterWorld.exe` (offline capstone index `re/tools/g`, raw disasm `re/scratch/ReJitterCamera/rjdis.py`, pomocné skripty `re/scratch/ReJitterCamera-2/*.py`). Ghidra server nebyl k dispozici, takže je vše z disasm. NVIDIA dokumentace: DLSS Programming Guide 310.6.0 (https://raw.githubusercontent.com/NVIDIA/DLSS/main/doc/DLSS_Programming_Guide_Release.pdf), `sl_consts.h` (Streamline 2.x, `repos/inhm112_MHW-DLSSFrameGen/external/streamline/sl_consts.h:177-198`), Donut `View.cpp`/`StreamlineSample.cpp`/`TemporalAntiAliasingPass.cpp` (staženo v `re/scratch/ReJitterCamera/`).

Konvence: řádkové matice, násobení řádkovým vektorem (`v·M`); `0x14028e280(rcx=A, rdx=out, r8=B)` počítá `out = A·B` (OBSERVED: `out[0] = A[0]·B[0] + A[1]·B[4] + …`, `0x14028e28a-0x14028e341`). `0x1402902c0(rcx=src, rdx=out)` je inverze (OBSERVED nepřímo: `fProjI = 0x1402902c0(fProj)`, `fViewProjI = 0x1402902c0(fViewProj)`, viz §2.4).

---

## 1. Otázky a verdikty

| # | Otázka | Verdikt |
|---|---|---|
| Q1 | Dělitel jitteru (EDX) je render šířka, nebo výstupní? | **Výstupní (display) šířka.** `EDX = vp.right − vp.left`, obdélník viewportu plní `0x142290420` z `cfg(0x1451c4480)+0x198/+0x19c` (velikost obrazovky). Render šířka je `cfg+0x198 × cfg+0x204` (`0x142391f30`). Pro Y platí totéž (`r8d = vp.bottom − vp.top` = výška displeje). Gargauthův předpoklad je **REFUTED**, odt1 `FullPixelJitter 2/rs` je **CONFIRMED**. |
| Q2 | Jitter matice, její vstup do matic CB a jestli prev VP nese prev jitter | `J = I` s řádkem 3 = `(jx/Wd, jy/Hd, 0, 1)`. `fProj = P·J·R`, `fViewProj = V·P·J·R`, **`fViewProjPF = Vprev·Pprev·J_cur·R`**, `fProjPF = Pprev·J_cur·R`, `fNoJitterProj = P·R`. R je převracení hloubky (`z' = w − z`). Prev VP tedy nese **aktuální** jitter, ne předchozí, takže MV jsou bez jitteru. Gargauthovo `MV_DEJITTER = 0` je **CONFIRMED**. Interpretace LostRiveru „J_prev“ je **REFUTED** (@704 je `fProjPF` s J_cur, takže `Jcur − J@704 ≡ 0`). |
| Q3 | Znaménka a rozpor `gpu.rs:1849 (hx,−hy)` vs `fg.rs:159 (hx,hy)` | `+u_x` posune geometrii **doprava** o `u_x·Wr/(2·Wd)` render px. `+u_y` ji posune **nahoru** o `u_y·Hr/(2·Hd)` render px. DLSS i SL chtějí posun geometrie v render px s osou +y dolů, tj. `(x, −y)`. **gpu.rs je správně, fg.rs má špatně** znaménko Y a navíc velikost o faktor `rs`. |
| Q4 | Herní Halton, podmínky zápisu, jestli je vanilla jitter aktivní, pořadí vůči Gargauthovi | Tabulka `scene+0xe6a0`: 20× float2 `Halton(2,3)` pro indexy **20..39** minus 0.5, plní ji `0x14227ff80…`. Druhá tabulka je `scene+0xe740` (18 položek z .rdata). Zapisuje ji TAA update `0x1423931e0` přes setter `0x14228f390`, který uloží i razítko `sCamera+0x40 = sRender+0x1f208`. V `0x14229a080` se jitter **vynuluje, pokud razítko ≠ aktuální frame**, a teprve potom proběhne camera update. Gargauthův zápis v detouru `0x14228f3b0` jde **až po** tomto nulování, takže vždy vyhraje. Jestli je vanilla jitter skutečně aktivní, staticky rozhodnout nejde (UNKNOWN, závisí na TAA filtru). |
| Q5 | Kde na CPU číst kameru a jitter bez upload heapu | V Gargauthově detouru `0x14228f3b0` **po** návratu originálu. `camera` je `sCamera::Viewport`: `+0xa0` view, `+0xe0` proj, `+0x120` prevView, `+0x160` prevProj (vše nejitterované), `+0x28..+0x34` rect, a k tomu `sCamera+0x38/+0x3c` (jitter) a `+0x44` (blend). Z těchto vstupů se všechny matice CB dají přesně dopočítat. Offsety v CB jsou za běhu v globálech `0x144be75f8…0x144be7658`. |

Dílčí verdikty k tvrzením projektů:

| Tvrzení | Zdroj | Verdikt |
|---|---|---|
| „Engine divides by its viewport width, which is the render target“ | Gargauth `gpu.rs:1813-1816` | **REFUTED**: viewport rect = display (cfg+0x198) |
| „One unit is half a pixel“ | Gargauth `native.rs:252-254` | **PARTIAL**: ½ **display** px = ½·rs render px |
| `jitter_pixels = 1.0` → „samples spread across one pixel“ | Gargauth `native.rs:309-313` | **REFUTED** pro rs<1: pokrytí je jen `rs` render pixelu; při DLAA platí |
| Výpočet `px = −proj[8]·Wr/2` (pro DLSS) | Gargauth `gpu.rs:1817-1818` | **CONFIRMED**: měří skutečný posun v render px bez ohledu na dělitele |
| Jitter report `(1, −1)` | Gargauth `gpu.rs:58-61, 1849` | **CONFIRMED** |
| `jitter = native::last_jitter()` pro SL | Gargauth `fg.rs:157-159` | **REFUTED**: chybí `·rs` a mínus u Y |
| „engine applies its jitter matrix to the previous frame's projection as well“ | Gargauth `gpu.rs:52-57`, `fg.rs:82-84` | **CONFIRMED** (`0x1423d0f0a-0x1423d0f11`, `0x1423d1837-0x1423d185b`) |
| „prev proj unjittered, vectors = (f_prev−f_curr) − J_curr“ | Gargauth `mv.rs:98-103`, `streamline.rs:1532-1536` | **REFUTED** (zastaralý komentář) |
| `0x23d0e70` má 11 callerů, ne-identity J předává jen `0x228f3b0` | Gargauth `native.rs:30-33` | **CONFIRMED** (zbylých 10 předává `0x143fde920` = identita) |
| Mezi detourem a čtením nic jitter nepřepíše | Gargauth `native.rs:343-347` | **CONFIRMED** (`0x14228f3dc`–`0x14228faaf`: jen `0x142290420`, nesahá na +0x38) |
| Halton 20 položek na `scene+0xe6a0`, index `frame % 20` (`sRender+0x1f210`) | odt1 `CLAUDE.md:207` | **CONFIRMED** (`0x14227db60`) |
| TAA update zapisuje `camera+0x38`, `+0x40` = render frame | odt1 `game.cpp:32-33` | **CONFIRMED** (`0x14228f390`) |
| FullPixelJitter `×2/rs` (dělí výstupní šířkou) | odt1 `game.cpp:258-270` | **CONFIRMED** (dělitel = display, vanilla rozsah = ±0.25 display px) |
| MV decode `(mv + J_cur − J_prev)`, J_prev = CB byte 704 | LostRiver-ss (`MhwPackedMotionDecode.hlsl:10-13,35-37`) | **PARTIAL**: byte 704 = `fProjPF[8]` = **aktuální** jitter, takže s engine jitterem je korekce ≡ 0 (shoduje se s `MV_DEJITTER=0`); jejich pojmenování J_prev je mylné |
| „Vanilla MHW má v TAA CB nulový jitter“ | LostRiver-ss `docs/RESEARCH.md:103-105` | **UNKNOWN**: buď TAA/jitter v jejich konfiguraci vypnutý, nebo četli verzi CB po de-jitteru `0x1423c6511` (§2.6) |

---

## 2. Nálezy s důkazy

### 2.1 Objekty

- `0x1451c4400` = **`sCamera`** (DTI jméno `"sCamera"` @`0x14350d118`, velikost `0xd80`; DTI init `0x14020ef00`: `lea rdx,[0x14350d118]; mov r9d,0xd80`). Vtable `0x14350d188`, slot +0x48 = `0x14228e770` (update loop viewportů), slot +0x30 = `0x14228eb60` (update kamer, prev←cur). Pole: `+0x38/+0x3c` jitter (float2), `+0x40` razítko frame, `+0x44` blend t, `+0x50 + i·0x1a0` 8× **`sCamera::Viewport`** (DTI `"sCamera::Viewport"` @`0x14350d100`, velikost `0x1a0`, `0x14020eeb0`), `+0xd50..+0xd5c` rect, `+0xd74`, `+0xd78` freeze. (OBSERVED, ctor `0x14228d7a0`: `mov [rbx+0xd50],r8 … lea rdx,[rbx+0x73]` + krok 0x1a0.)
  - Gargauthův „render context“ (`native.rs:33`) je tedy `sCamera`. odt1 „camera“ (`CLAUDE.md`) je **CONFIRMED**.
- `sCamera::Viewport` (vp): `+0x08` cCamera*, `+0x18` volitelný cíl (render-to-texture), `+0x20` active, `+0x21`, `+0x23`, `+0x24` typ (split 0..9, 0xa = globální rect), `+0x25` pass, `+0x28..+0x34` rect `(left, top, right, bottom)` int, `+0xa0` view, `+0xe0` proj, `+0x120` prevView, `+0x160` prevProj.
- `0x1451c4480` = `sRender`/graphics cfg (ctor `0x142290880`; DTI-style string `"sRender"` @`0x14350d280`). `+0x198/+0x19c` velikost obrazovky (ctor: `mov [rsi+0x1a0],0x780; mov [rsi+0x1a4],0x438 … mov eax,[rsi+0x1a0]; mov [rsi+0x198],eax`, `0x142290dce-0x142290e42`), `+0x200/+0x204` DLSS ratio requested/built, `+0x710` scene view (pass ctx), `+0x1f208` render frame, `+0x1f210` frame, `+0x1f450` view (flags +0x58), `+0x1f560` „targets rebuild pending“ [INFERENCE], `+0x7b424` override režim jitteru.
- `0x1451c4368` = scene; `+0xe6a0` Halton tabulka, `+0xe740` druhá tabulka, `+0xe9e4` přepínač TAA.

### 2.2 Q1 — kdo je EDX

`0x14228f3b0` (camera setup):
```
0x14228f3dc call 0x142290420                 ; přepočte vp rect
0x14228f3e5 mov r9d,[rbx+0x28] ; r10d,[rbx+0x2c] ; edx,[rbx+0x30] ; r8d,[rbx+0x34]
0x14228f3e1 cmp byte [rbx+0x24],0xa ; jne → jinak rect = sCamera+0xd50..0xd5c
0x14228fa31 sub edx,r9d                      ; W = right − left
0x14228fa34 sub r8d,r10d                     ; H = bottom − top
0x14228faaf movss xmm1,[rax+0x38] ; movd xmm0,edx ; cvtdq2ps ; divss   → jx/W  → [rbp+0xf0]
0x14228facc movss xmm1,[rax+0x3c] ; …cvtdq2ps xmm0(r8d) ; divss       → jy/H  → [rbp+0xf4]
```
`0x142290420` (rect):
```
0x142290426 mov rax,[0x1451c4480] ; mov rdx,[rax+0x198]   ; (W,H) obrazovky jako qword
0x142290446 … mov [sCamera+0xd50],0 ; [sCamera+0xd58]=W ; [sCamera+0xd5c]=H
0x14229046b mov rcx,[rbx+0x18] ; test ; je 0x1422904ac   ; vp+0x18 == null → (0,0,W,H)
0x14229048f   jinak rect = velikost textury z [vp+0x18]+0x2e8 (0x14255cf00: …+0x68 packed u16/u15)
0x1422904ac movzx eax,[rbx+0x24] ; jump table 0..9      ; split-screen poloviny téhož (0,0,W,H)
0x1422904cd mov [rbx+0x28],r10d ; [rbx+0x2c] ; [rbx+0x30]=W ; [rbx+0x34]=H
```
Render velikost (`0x142391f30`): `mov rax,[cfg+0x198]; movss xmm2,[cfg+0x204]; cvtdq2ps …; mulss xmm0,xmm2; cvttss2si r15d` (`0x142391f5c-0x142391fa5`), tj. `Wr = trunc(Wd·ratio_built)`, `Hr = trunc(Hd·ratio_built)`. Ratio setter `0x14229be60`: `ratio = (trunc(W·r) & ~0xF) / W`, kde `W = cfg+0x198` (`0x14229be9b movd xmm1,[rcx+0x198]`).

Závěr (OBSERVED): pro hlavní viewport bez vlastního cíle (`vp+0x18 == 0`) je `EDX = cfg+0x198 = display šířka` a `r8d = display výška`. Jestli má hlavní herní kamera `vp+0x18 == 0`, je INFERENCE. Podporuje to `0x14228ec24 cmp [rbx+0x68],r12; jne` v update kamery: větev pro `vp+0x18 == 0` počítá aspect přes `cfg` (`0x1404a0710`). Ověření je v §4 (E1).

Konzistence: stejný rect se přes `0x1423d0330` (`mov [rcx+0x5c..0x68], rect`, `0x1423d0365-0x1423d0379`) kopíruje do scene view `sRender+0x710+0x5c`. Ten používá druhý čtenář jitteru (§2.6), takže engine dělí v obou místech stejně (display).

Důsledek pro Gargautha (OBSERVED aritmetikou):
- Zápis `u = 2x` (`native.rs:884`) → NDC posun `a = 2x/Wd` → skutečný posun v render px = `a·Wr/2 = x·rs`. S `JITTER_PIXELS = 1`, `x ∈ [−0.5, 0.5]` pokrývá vzorkování jen `rs` render pixelu (při Performance rs = 0.5 je to ±0.25 px).
- Hodnota hlášená DLSS (`gpu.rs:1818`, `−proj[8]·Wr/2`) = `x·rs`, tedy skutečný posun. DLSS proto dostává **konzistentní** offset, jen je vzorkování neúplné.
- Vanilla Halton `±0.5` engine units = ±0.25 **display** px, takže hra jitteruje v rozsahu půl pixelu. Proto odt1 násobí `2/rs`.

### 2.3 Q2 — jitter matice

Camera setup staví lokální 4×4 na `[rbp+0xc0]`:
```
0x14228fa75..fa98  rows 0..2 = [0x143fde920/930/940] = identita (bajty: 1,0,0,0 | 0,1,0,0 | 0,0,1,0)
0x14228fa9f        [rbp+0xf8] = [0x143fde958] = 0.0
0x14228fa37/fa53   [rbp+0xfc] = [0x143fde95c] = 1.0
0x14228fac4/fadc   [rbp+0xf0] = jx/W , [rbp+0xf4] = jy/H
```
→ `J = [[1,0,0,0],[0,1,0,0],[0,0,1,0],[jx/W, jy/H, 0, 1]]`. Volání (`0x14228fb31-0x14228fb6e`): `0x1423d0e70(rcx=pass ctx, rdx=view, r8=proj, r9=&rect, arg5=dist, arg6=&vp+0x120 prevView, arg7=&copy(vp+0x160) prevProj, arg8=&vp+0xe0, arg9=&J)`.

Pokud `sCamera+0x44 = t > 0`, předá se jako view/proj **lerp(cur, prev, t)** (`0x14228f43e-0x14228f9d6`). Jinak se předají přímo vp+0xa0/+0xe0. Kdo zapisuje +0x44, se nenašlo (globální load + store v okně 60 instrukcí přes celý .text: jen čtení `0x14228f43e`).

`prev ← cur`: update kamery `0x14228eb60` (vtable +0x30) kopíruje `vp+0xa0..0xdf → vp+0x120` (`0x14228ec7b mov eax,[rbx+0xf0]; mov [rbx+0x170],eax` …) a `vp+0xe0 → vp+0x160` (`…[rbx+0x1b0..0x1ec]`, `0x14228ed41-0x14228edf5`). Teprve potom volá kameru `vfunc+0x110/+0x118` → nový view/proj (`0x14228ee0f`, `0x14228eeb2`). Při freeze (`[0x1451c4148]+0x8c/+0x8d` nebo `sCamera+0xd78`) se kopie přeskočí (`0x14228ec72 jne 0x14228edfb`). Prev matice jsou tedy **nejitterované surové kopie**.

`0x1423d0e70` (soubor `re/scratch/ReJitterCamera/cbfill.txt`; jména proměnných přes `0x1423d60c0(cbId, "name")`, zápis matic `0x141fad4c0`):

| Výpočet (adresa) | Výsledek | Proměnná CB |
|---|---|---|
| `0x1423d0ef7`: `P·J` | rbp+0x1a0 | — |
| `0x1423d0f11`: `Pprev·J` (arg7·arg9) | rbp+0x220 | — |
| `0x1423d0f25`: `V·(P·J)` | rsp+0x20 | (CBViewFrustum používá jitterovanou VP) |
| `0x1423d1081`: `V·P` | rbp+0x360 | — |
| `0x1423d1712-0x1423d1795`: **R** = `[[1,0,0,0],[0,1,0,0],[0,0,K,0],[0,0,1,1]]`, `K=[0x142e4fe70]=−1.0` | rbp−0x60 | převrácená hloubka `z' = w − z` |
| `0x1423d179c`: `P·J·R` | rbp+0x260 | **fProj** |
| `0x1423d17b1`: `V·P·J·R` | rbp+0xe0 | **fViewProj** |
| `0x1423d17c4`/`17d7`: inverze | rbp+0x3a0 / +0x2a0 | **fProjI** / **fViewProjI** |
| `0x1423d17ea`: `P·R` | rbp+0x460 | **fNoJitterProj** |
| `0x1423d1801`: `V·P·R` | rbp+0x2e0 | **fNoJitterViewProj** (+ inv → **fNoJitterViewProjI**) |
| `0x1423d182b`: `Pprev·J·R` | rbp+0x420 | **fProjPF** |
| `0x1423d1848`+`185b`: `Vprev·(Pprev·J)·R` | rbp+0x1e0 | **fViewProjPF** (+ inv → fViewProjIPF) |
| `0x1423d1888`: `inv(fViewProj)·fViewProjPF` | rbp+0x320 | **fViewProjIViewProjPF** *a zároveň* **fNoJitterViewProjIViewProjPF** (stejný zdroj, `0x1423d1b8f` i `0x1423d1e95`) |
| arg6 surový | — | **fViewPF** |

J a R komutují: `J·R = R·J = [[1,0,0,0],[0,1,0,0],[0,0,−1,0],[a,b,1,1]]`. Proto platí `X_unjit = X·J⁻¹` pro fProj, fViewProj, fProjPF i fViewProjPF.

Důsledek pro MV (OBSERVED v CB, INFERENCE pro shadery, protože DXBC v exe není): reprojekce `inv(V P J R)·(Vp Pp J R) = J⁻¹R⁻¹·[inv(VP)·VpPp]·RJ` posune pixel o `−a` a zpět o `+a`. Statický bod tak dá MV = 0 a jitter se v MV vyruší. `MV_DEJITTER = 0` a `MVJittered = 0` jsou správně.

**Layout CB**: offsety nejsou v exe natvrdo. Resolvují se za běhu ze shader reflection a ukládají do globálů jako handle `h`: offset v bajtech = `((h >> 12) & 0x3ff) · 4`, `(h >> 10) & 3` = počet řádků − 1 (`0x141fad4d6-0x141fad4f0`). Mapa handle → jméno (OBSERVED, `re/scratch/ReJitterCamera-2/hmap.py`):

```
0x144be75ec CBViewFrustum(id)  0x144be75f0 fViewFrustum   0x144be75f4 CBViewProjection(id)
0x144be75f8 fViewProj   0x144be75fc fView   0x144be7600 fViewI   0x144be7604 fProj   0x144be7608 fProjI
0x144be760c fViewProjI  0x144be7610 fViewProjPF  0x144be7614 fViewProjIPF  0x144be7618 fViewPF  0x144be761c fProjPF
0x144be7620 fViewProjIViewProjPF  0x144be7624 fCameraPos  0x144be7628 fCameraDir  0x144be762c fZToLinear
0x144be7630 fCameraTargetDist  0x144be7634 fCameraNearClip  0x144be7638 fCameraFarClip
0x144be763c fNoJitterProj  0x144be7640 fNoJitterViewProj  0x144be7644 fNoJitterViewProjI
0x144be7648 fNoJitterViewProjIViewProjPF  0x144be764c fPassThrough  0x144be7650 fLODBasePos
0x144be7654 fPassThroughCorrect  0x144be7658 bWideMonitor
```
Gargauthovy offsety (`camera.rs:27-30, 233-236`, `fg.rs:104`: fViewProj 0x000, fView 0x040, fProj 0x080, fViewI 0x0c0, fCameraPos 0x180, fCameraDir 0x190, near 0x1ac, far 0x1b0, fViewProjPF 0x1e0, fNoJitterProj 0x320) pocházejí z RDEF/dumpu. Staticky je ověřit nelze (UNKNOWN), ale křížově sedí s LostRiverem (fProj @128, fProjPF @0x2a0 → jitter @704, noJitterProj @800). Experiment E3 je rozhodne.

### 2.4 Q3 — znaménka

- `clip' = clip·J·R`: `x' = x + a·w`, `y' = y + b·w`, w se nemění (sloupec 3 matic J a R = (0,0,0,1)). NDC posun je `(+a, +b)`, `a = u_x/Wd`, `b = u_y/Hd`.
- D3D viewport: `sx = (ndc_x+1)/2·Wr`, `sy = (1−ndc_y)/2·Hr`. Posun geometrie v render px (osa +y dolů) je proto `(u_x·Wr/(2Wd), −u_y·Hr/(2Hd))`. **+u_x = doprava, +u_y = nahoru.**
- `fProj[8] = −a`, `fProj[9] = −b`, protože řádek 2 P je `(0,0,P22,−1)`: `(P·J)[2][0] = −J[3][0]`. Gargauthovo `px = −proj[8]·Wr/2 = a·Wr/2` je správně.
- NVIDIA: DLSS PG 3.7.3 říká *„Jitter Offsets in pixel space … at the render target size … Use the same co-ordinate and direction system as motion vectors“*. MV mají `[0,0]` vlevo nahoře a +y dolů (PG 3.6.1). `sl_consts.h:195-196` říká „pixel space jitter offset“. Donut (referenční sample DLSS/SL) staví `translation(2·px/W, −2·py/H)` (`View.cpp:68-70`) a stejný `pixelOffset` předává do `slConstants.jitterOffset` (`StreamlineSample.cpp:1307,1324`). Konvence je tedy: posun geometrie v px, +y dolů.
- Správná hodnota pro DLSS i SL: `(u_x·Wr/(2Wd), −u_y·Hr/(2Hd))`. Při Gargauthově zápisu `u = 2x` to je `(x·rs, −y·rs)`. **`gpu.rs:1849` (×(1,−1)) je správně.** `fg.rs:159` posílá `(x, y)`, tedy špatné znaménko Y a chybějící `rs`.

### 2.5 Q4 — herní jitter

- **Tabulka 1** (`0x14227ff80…0x14228016e`, init scény): `r8d = 0x14` (start 20), 20 iterací. `x = Σ bity(i)·0.5^k − 0.5` (`xmm4 = 0.5`), `y = radical-inverse base 3 (xmm5 = 1/3) − 0.5`. Uloží se do `[rbx+0xe6a0 + 8k]` (`lea r9,[rbx+0xe6a4]; movss [r9−4],x; movss [r9],y`). Hodnoty pro k = 0..19: `(−0.34375, 0.24074) (0.15625,−0.31481) (−0.09375,0.01852) (0.40625,0.35185) (−0.40625,−0.2037) (0.09375,0.12963) (−0.15625,0.46296) (0.34375,−0.48765) (−0.28125,−0.15432) (0.21875,0.17901) (−0.03125,−0.37654) (0.46875,−0.04321) (−0.48438,0.29012) (0.01562,−0.26543) (−0.23438,0.0679) (0.26562,0.40123) (−0.35938,−0.45062) (0.14062,−0.11728) (−0.10938,0.21605) (0.39062,−0.33951)`.
- **Tabulka 2**: `scene+0xe740`, 18 float2 kopírovaných z .rdata (`0x142280174-0x142280324`), index `frame % 18` (`0x14227f7b0`).
- Getter `0x14227db60`: `scene + 0xe6a0 + (sRender+0x1f210 % 20)·8` (konstanty `0xcccc…cd`, `0x171·20 = 0x1cd4`; `0x1cd4·8 = 0xe6a0`).
- Setter `0x14228f390`: `mov rax,[rdx]; mov [rcx+0x38],rax; mov edx,[sRender+0x1f208]; mov [rcx+0x40],edx`.
- Zapisovatelé:
  1. `uTemporalAAFilter2::update 0x1423931e0`. Podmínky: view `+0x58` bez bitů 0x12 a 0x17, `taa+0x180 ≠ 0`, `taa+0xc == 8`, `(taa+0x14 & 0x12) == 0x12`, `taa+0x184 ≠ 0`. Pak `setJitter(Halton1[frame%20])`.
  2. `late update 0x142393260`. Když `sRender+0x1f560 ≠ 0`, volá `0x142391f30` (rebuild targetů na `cfg+0x198·cfg+0x204`) a `setJitter(0x14503c3e8)` (nulový vektor v .data/.bss). Bit 2 v `taa+0x14` synchronizuje s `scene+0xe9e4` (TAA přepínač v menu) jen tehdy, když `[0x1451c5320]+0x2cda ≠ 0`.
  3. `0x14229a080` (render frame, `rbx = sRender`, `0x14229a2ae-0x14229a334`). Když má view bit `0x800000`, rozhoduje `sRender+0x7b424`: 1 → Halton1, 2 → tabulka 2, jinak 0. **Jinak, když `sCamera+0x40 ≠ sRender+0x1f208` → jitter = 0.** Teprve potom následuje `call [sCamera.vtbl+0x48]` = `0x14228e770` (`0x14229a350`, pass 0) a případně druhý průchod pass 1 „SubDisplay“ (`0x14229a52c`, jen když `device+0x14a0 > 1`).
  4. Jiný TAA filtr `0x1425979b0`: dvoufázový `(0,−0.5)/(−0.5,−0)` podle parity framu. Clear `0x142597a30` při `sRender+0x1f560`.
- Pořadí v jednom framu: TAA update (fáze update) → `0x14229a080` stale-clear → `0x14228e770` → pro každý aktivní vp s `vp+0x25 == pass` volá `0x14228f3b0`. **Gargauthův detour zapisuje tady**, pak `0x14228faaf` čte. Gargauthova hodnota tedy vždy přebije vanilla hodnotu (CONFIRMED). Protože Gargauth nenastavuje `+0x40`, další frame `0x14229a32d` slot vynuluje, pokud nezapsal TAA update. Proto jeho čítač `LOST` (`native.rs:264-271`) roste i bez „konkurenčního zapisovatele“.
- Je vanilla jitter aktivní? Staticky to rozhodnout nejde (UNKNOWN). Záleží na TAA filtru (typ 8, flagy 0x12, `+0x184`), na TAA v menu a na `+0x7b424`. Experiment E4.
- **Riziko (OBSERVED struktura, INFERENCE dopad):** detour běží jednou za každé volání `0x14228f3b0`, tj. za každý aktivní viewport v pass 0 i v pass 1. `apply()` pokaždé posune `SEQUENCE` (`native.rs:298`) a přepíše `LAST_JITTER`. Při více aktivních viewportech dostane hlavní kamera jen každý n-tý Halton vzorek a `last_jitter()` může patřit jiné kameře.

### 2.6 Druhý čtenář jitteru (de-jitter)

`0x1423c3790` (velká render funkce pass ctx, volaná z `0x14228e8d4`) na `0x1423c6511` znovu čte `qword [sCamera+0x38]` a staví `J2` dělené rectem scene view (`[rsp+0x50..0x5c] = r14+0x5c..0x68`, plněno `0x1423c3845-0x1423c3862`). Rect pochází z `0x1423d0330`, tedy jde o tentýž display rect. Pak spočítá `inv(J2)` (`0x1423c65bb`) a přepíše v živém `CBViewProjection` fProj, fViewProj, fProjI, fViewProjI, fViewProjPF, fViewProjIPF, fProjPF a fViewProjIViewProjPF na **nejitterované** verze (`0x1423c67bc-0x1423c6998`). Děje se to po `CSTiledDeferredCopyGBufferID` a před `CBTemporalUpsample`/`tTransparentDensity`.
- Pro Gargautha je to v pořádku, pokud je v daném framu jediný zápis (čte poslední zapsanou hodnotu).
- Tím se může vysvětlovat LostRiverův „nulový jitter v TAA CB“, pokud snímali CB po tomto kroku (UNKNOWN).

### 2.7 Q5 — CPU zdroj kamery

V `hook_camera_setup` (`native.rs:348-367`) je `camera` = ukazatel na `sCamera::Viewport`. **Po** `original(camera, pass)` (rect je pak čerstvý z `0x142290420`) je na CPU k dispozici:
- `camera+0xa0` V, `+0xe0` P (nejitterovaná, standardní hloubka), `+0x120` Vprev, `+0x160` Pprev, `+0x28..+0x34` rect (= dělitel), `sCamera+0x38/+0x3c` (právě zapsaný jitter), `sCamera+0x44` blend t (pokud `t > 0`, engine použil `lerp(cur, prev, t)`; replikovat podle `0x14228f43e-0x14228f9d6`).
- Z toho přesně: `J`, `R(K=−1)`, `fNoJitterProj = P·R`, `fViewProj = V·P·J·R`, `fViewProjPF = Vprev·Pprev·J·R`, `clipToPrevClip(bez jitteru) = inv(V·P·R)·(Vprev·Pprev·R)`.
- Klíč framu: `sRender+0x1f208` (stejný čítač jako razítko). Vyhne se tak závodu, který popisuje `gpu.rs:1739-1746`, i závislosti na mapování upload heapu (`gpu.rs:1839-1842`).
- Alternativa: hook vstupu `0x1423d0e70` filtrovaný na `[rsp+0x48] ≠ 0x143fde920` (jitter matice ≠ identita). Nese přesně všech 9 vstupů.

---

## 3. Doporučené změny pro Gargauth

1. **`src/native.rs:875-885` (`jitter`)**: převádět render px na engine units displejovým dělitelem: `write(x·2·Wd/Wr, y·2·Hd/Hr)`. `Wd, Hd` = rect viewportu (`camera+0x30−0x28`, `+0x34−0x2c`) nebo `cfg+0x198/+0x19c`. `Wr, Hr` = `trunc(Wd·cfg+0x204)` (jako `0x142391f30`) nebo render rozměr z poslední DLSS dávky. `LAST_JITTER` ukládat v DLSS konvenci `(x, −y)`. Bez tohoto pokrývá Halton jen `rs` pixelu (stejný problém řeší odt1 FullPixelJitter).
2. **`src/native.rs:250-255, 309-313`**: opravit komentáře na „1 unit = ½ display px = ½·rs render px“.
3. **`src/gpu.rs:1809-1816`**: opravit komentář (engine dělí šířkou displeje). Výpočet `−proj[8]·render/2` ponechat, protože je správný. `gpu.rs:58-61, 1849` (1, −1) ponechat.
4. **`src/fg.rs:157-159`**: `jitter = (x·rs, −y·rs)`. Ideálně stejná hodnota, jakou `gpu.rs:1848-1849` čte z CB (sdílet ji místo `last_jitter()`), aby DLSS-SR i DLSS-G viděly identický offset.
5. **`src/fg.rs:151-155`**: `clipToPrevClip` dnes obsahuje konjugaci jitterem (`inv(fViewProj)·fViewProjPF`, obě s J_cur), což porušuje `sl_consts.h:177-179` („must NOT contain jitter“). Použít `C' = J·C·J⁻¹`, kde `C = fViewProjIViewProjPF` a `J` je translace `(a, b) = (−fProj[8], −fProj[9])`. Ekvivalentně `inv(fNoJitterViewProj)·(fViewProjPF·J⁻¹)`.
6. **`src/native.rs:296-304, 348-359`**: `apply()` volat jen pro hlavní viewport (pass 0, `vp+0x18 == 0`, index podle E2) a `SEQUENCE` posouvat jednou za frame (klíč `sRender+0x1f208`). Ostatním viewportům v témže framu zapsat 0 nebo tutéž hodnotu.
7. **`src/mv.rs:98-103`, `src/streamline.rs:1532-1536`**: zastaralé komentáře, protože prev proj nese aktuální J, takže `(f_prev − f_curr)` je už bez jitteru.
8. **`src/camera.rs:26-30`** (volitelné): offsety odvozovat/ověřovat za běhu z handle globálů `0x144be75f8…` (`((h>>12)&0x3ff)·4`).
9. **CPU kamera pro FG** (`src/fg.rs:107-134`): plnit `CAMERA` z detouru (§2.7) místo čtení upload heapu. FG pak nebude potřebovat launcher ani hook mapování.

---

## 4. UNKNOWN a runtime experimenty

| ID | Nejistota | Experiment (Gargauth) | Co rozhodne |
|---|---|---|---|
| E1 | Má hlavní kamera `vp+0x18 == 0`, je rect = display? | `native.rs::hook_camera_setup` po `original()`: logovat `camera+0x18`, `+0x24`, `+0x25`, rect `+0x28..+0x34`, `cfg+0x198/+0x19c`, `cfg+0x204` a render rozměr z `gpu.rs` | rect šířka == `cfg+0x198` ≠ render ⇒ Q1 potvrzeno i za běhu |
| E2 | Kolik viewportů a průchodů za frame volá detour | V detouru počítat volání na `sRender+0x1f208`. Logovat index `(camera − (sCamera+0x50))/0x1a0` a pass | >1 volání/frame ⇒ nutná změna 3.6 |
| E3 | Skutečné offsety v CB | Jednou za běhu přečíst `u32` na `image+0x4be75f8 … +0x4be7658`, dekódovat `((h>>12)&0x3ff)·4` | Shoda s `camera.rs`/`fg.rs:104`, přesný offset fProjPF, fNoJitterViewProj atd. |
| E4 | Je vanilla jitter aktivní? | Na vstupu detouru (před `apply`) logovat `sCamera+0x38/+0x3c/+0x40`, `sRender+0x1f208/+0x1f210` a `scene+0xe6a0 + (f%20)·8` | Hodnota = tabulka a razítko = frame ⇒ vanilla jitter běží. 0 ⇒ neaktivní (TAA off / filtr) |
| E5 | MV opravdu bez jitteru | Statická scéna, `expert_jitter_pixels = 1`, `mv_probe = 1`: střední hodnota MV musí být ≈ 0 (< 0.01 px) | Potvrzuje §2.3 a `MV_DEJITTER = 0` |
| E6 | Závod update ↔ render (TAA update framu N+1 přepíše +0x38 před `0x1423c6511`, resp. před záznamem TAA dávky) | Uložit `(frame, jitter)` z detouru do ringu. V `gpu.rs` při DLSS dávce porovnat s jitterem z CB | Neshoda ⇒ používat ring podle framu, ne „poslední hodnotu“ |
| E7 | Kdo zapisuje `sCamera+0x44` (blend t) | V detouru logovat `sCamera+0x44` (≠ 0?) | t > 0 ⇒ CPU rekonstrukce musí lerpovat |
| E8 | LostRiverův „nulový jitter v CB“ | V `gpu.rs` logovat `fProj[8..9]` ze všech CBV verzí CBViewProjection ve framu | Druhá verze s nulou ⇒ jde o de-jitter `0x1423c6511` |

---

## 5. Mapa adres

| Adresa | Význam |
|---|---|
| `0x1451c4400` | `sCamera*` (DTI `0x1451c4408`, jméno `0x14350d118`, velikost 0xd80; ctor `0x14228d7a0`, vtable `0x14350d188`) |
| `0x1451c4440` | DTI `sCamera::Viewport` (0x1a0) |
| `0x1451c4480` | `sRender*`/cfg (ctor `0x142290880`) |
| `0x1451c4368` | scene |
| `0x14228e770` | `sCamera` vtable +0x48: smyčka 8 viewportů pro pass → `0x14228f3b0` |
| `0x14228eb60` | `sCamera` vtable +0x30: prev←cur (`vp+0x120/+0x160`), nové V/P, aspect |
| `0x14228f3b0` | camera setup (Gargauth detour); jitter čtení `0x14228faaf` (x), `0x14228facc` (y); volání CB fill `0x14228fb6e` |
| `0x142290420` | vp rect z `cfg+0x198` (nebo z cíle `vp+0x18`); `sCamera+0xd50` |
| `0x14228f390` | `sCamera::setJitter(float2*)` → +0x38, +0x40 = `sRender+0x1f208` |
| `0x14227db60` / `0x14227f7b0` | Halton getter (`scene+0xe6a0`, %20) / tabulka 2 (`scene+0xe740`, %18) |
| `0x14227ff80…0x142280324` | naplnění Halton tabulky (i = 20..39) a tabulky 2 |
| `0x1423931e0` / `0x142393260` | uTemporalAAFilter2 update (zápis Halton) / late update (clear při `sRender+0x1f560`) |
| `0x1425979b0` / `0x142597a30` | jiný TAA filtr: dvoufázový jitter / clear |
| `0x14229a080` | sRender frame: stale-clear `0x14229a314-0x14229a334`, camera update `0x14229a350` (pass 0), `0x14229a52c` (pass 1 SubDisplay) |
| `0x142391f30` | render velikost = `cfg+0x198·cfg+0x204` |
| `0x14229be60` | DLSS ratio setter (`cfg+0x200`) |
| `0x1423d0e70` | plní CBViewFrustum + CBViewProjection (11 callerů, 10× identita `0x143fde920`) |
| `0x1423d0330` | kopie vp rect → `passctx+0x5c..0x68` |
| `0x1423c3790` / `0x1423c6511` | render funkce pass ctx / druhý čtenář jitteru (de-jitter CB) |
| `0x14028e280` / `0x1402902c0` | matice A·B / inverze |
| `0x1423d60c0` / `0x141fad4c0` | resolve CB proměnné dle jména / zápis matice podle handle |
| `0x144be75f8…0x144be7658` | handle proměnných CBViewProjection (offset = `((h>>12)&0x3ff)·4`) |
| `0x143fde920…0x143fde95c` | identita 4×4 (`[0x143fde958] = 0`, `[0x143fde95c] = 1`) |
| `0x142e4fe70` | K = −1.0 (převrácení hloubky v R) |
| `0x14503c3e8` | nulový float2 (clear jitteru) |
