# RePostProcess — post-process, AA, render scale, mip bias (MHW:I build 15539686)

Statická RE nad `MonsterHunterWorld.exe` (offline capstone index `re/tools/g`, vlastní skripty v `re/scratch/RePostProcess/`).
Globály: **rs** = render settings `[0x1451c4368]` (odt1 „scene“), **cfg** = graphics config `[0x1451c4480]` (odt1 „renderer“), shader/device manager `[0x1451c73d0]`, PC option gate `[0x1451c5320]+0x2cda`.

> Pozor na adresu v zadání: Gargauthův `SETTINGS_LOAD` je RVA `0x2412ef` = VA **`0x1402412ef`** (`48 8b 05 72 30 f8 04` → `0x1451c4368`, pak `mov dword [rax+0x260],0`, tj. `mTextureTargetMode`). VA `0x1420412ef` je jiný kód. OBSERVED (`g fn 0x1402412e0`).

---

## 0. Souhrnná tabulka volba → dopad → co dělat

| Volba / stav | Dopad na engine (důkaz) | Dopad na Gargauth | Co dělat |
|---|---|---|---|
| **Anti-Aliasing = Off** (`rs+0xe9e4` word 0x0000) | TAA draw `0x142392150` vrací hned (`0x14239217d cmp byte [rs+0xe9e4],0; je 0x1423931c3`), late update `0x142393260` shodí bit 0x2 unitu | Žádný `CSTemporalAA2` dispatch → žádné vstupy → `hijack` po 5 slotech padá na plné okno, **DLSS neběží** | Držet `rs+0xe9e4` = `0x0001`, dokud DLSS běží |
| **AA = FXAA** (0x0100) | TAA draw neběží (jako Off), FXAA draw `0x1423939d0` běží (stage 0xf, order 0x7fffff3) | Jako Off + FXAA na render-res obrazu | Držet `0x0001` |
| **AA = TAA+FXAA** (0x0101) | TAA i FXAA běží; FXAA až po TAA a ToneMappingu, **před** upscale tailem (stage 0x10) | `render_scale<1`: FXAA rozmaže **vstup** DLSS (slot route bere barvu po stage 0xf). `render_scale=1`: FXAA běží **po** DLSS výstupu (odt1 to naměřil) | Držet `0x0001` (FXAA byte `+0xe9e5` = 0) |
| **AA = TAA** (0x0001) | Jen TAA (+ TAA sharpen, viz níže) | Požadovaný stav | — |
| **Herní „NVIDIA DLSS = On“** v `graphics_option.ini` + Gargauthova odpověď flags=2 | AA apply `0x14043dcb0` přepíše volbu: flags==2 → **TAA+FXAA**, flags==3 → TAA, jinak volba uživatele (`0x14043dd27..0x14043dd36`) | Pokud uživatel zapne herní DLSS (Gargauth ho „odkrývá“), **FXAA se zapne nuceně** | Hold `0x0001` to řeší; nebo zakázat/neodkrývat herní volbu |
| **TAA `mSharpen` (+0x187)**, default 1, `mSharpenAmount` (+0x198) default 0.2 | Late update maže +0x187 jen když `cfg+0xa5 && (cfg+0xb4 & 1) && dev+0x42718 && dev+0x42700` (`0x142393321..0x14239334a`) | Gargauth vrací flags=**2** → bit0=0 → **sharpen se nemaže** → `TemporalAA2_Sharpen` běží: slot route ostří surový jitterovaný vstup DLSS, DLAA ostří výstup DLSS | Vynulovat +0x187 na TAA unitech (vtable `0x14351bda8`); opravit komentář `tune.rs:33-39` |
| **Image Quality** (Low/Mid/High/Variable) | Handler `0x14043eeb0`: High vynucen, pokud je herní DLSS „usable“ **nebo** `FidelityFX CAS=On`; jinak 0.75/0.9/1.0, Variable zapne dyn-res `0x14229bf30` | Gargauth detouruje handler a dává vždy High (`hijack.rs:124-142`) | Ponechat |
| **render_scale < 0.5** | Setter `0x14229be60` clamp ≥0.5 (`0x14229be6d → 0x142e4fe54`); product `cfg+0x208` a `set+0x6b8` clamp ≥0.5, ale textury se staví z neclampnutého ratio `set+0x6b4` | Při obejití clampu nesoulad rect (≥0.5) vs. textura (<0.5) → `E_INVALIDARG` při `Close` [INFERENCE] | 0.333 jen přesměrováním 6 operandů (ne konstanty), na 0.3 (`0x142e4fe4c`), + runtime test |
| **render_scale = 0.5 při šířce, kde w/2 není násobek 16** (3440, 1680, 1366, 1360) | Setter ořízne na násobek 16 *po* clampu → ratio 0.4977 (3440) < 0.5 → product clamp 0.5 ≠ textura | Stejný nesoulad jako <0.5 → pravděpodobný pád na ultrawide 3440×1440 [INFERENCE] | `snapped()` v `hijack.rs:610-621` musí vracet ≥0.5 (zaokrouhlit nahoru na násobek 16) |
| **Motion Blur = Off** | Volba píše `rs+0xe9ec` (`0x14043ed6c`); getter `0x141b19d70` vrací 1 bez ohledu na volbu, když `[0x14500de70]+0x58 > 1`. `uMotionBlurFilter` (default fur shutter 0.4 v ctor `0x1424fba2c`) i `uMotionBlurReconstructionFilter` nemají ve draw žádnou kontrolu volby | Fur/vlasy dostávají pohybový blur na **render-res, před DLSS** (stage 0xf, order 100) | Runtime test: vynulovat `+0x16c` (fur shutter) u `uMotionBlurFilter`; DLSS dostane ostřejší srst |
| **mip_bias** (`rs+0x1a8` = `mSamplerLodBias`) | 15 bank × 63 samplerů, staví se jednou při načtení shader package (`0x1423d6690`); materiál si sampler „zapeče“ při loadu (`0x142383566`) | Gargauthův přístup (přepsat operand `0x1423d73ee` před startem + zvolit bank 1) je správný | Ponechat; logovat bit 0x800000 (už dělá) |
| **Dynamic resolution / temporal-upsample mode** (bit `0x800000` v `set+0x58`) | TAA update/late/draw se přeskočí (`bt …,0x17`), běží `CSTemporalUpsample`, ISP tail se přeskočí (`r15b`), materiálová vazba nahradí samplery bankou 0 s dyn-res biasem `log2(level)−1` | S bitem by Gargauth přišel o TAA pass → žádné DLSS | Držet High (dyn-res off), logovat |
| **Alpha checkerboard unroll** (`rs+0xe964/0xe968`) | **Žádný čtenář v .text** (jen ctor a registrace vlastnosti) | `tune::unroll` je mrtvý zápis | Odstranit nebo označit za no-op |
| **SSLR dither / GBuffer jitter / contact noise** (`+0xb43f/+0xb440/+0xb500/+0xb501`) | Offsety CONFIRMED registrací; konzumenti existují; ctor defaulty 0/0/0/**1** | `undither` vypne i facial noise (default 1) | Logovat původní hodnoty před zápisem |

---

## 1. Otázky a verdikty

| # | Otázka | Verdikt |
|---|---|---|
| 1a | `rs+0xe9e4/+0xe9e5` | **OBSERVED.** `+0xe9e4` = TAA switch, `+0xe9e5` = FXAA switch. Píše je setter `0x142287a00` (0→0x0000, 1→0x0001, 2→0x0100, 3→0x0101) a ctor `0x142265a92` (0). Čtenáři: AA apply unit loop (`0x14043dd9b`, `0x14043ddeb`), TAA draw (`0x14239217d`), TAA late (`0x1423932eb`), FXAA draw (`0x1423939f4`), FXAA late (`0x142394239`). Byte-scan disp32 `e4 e9 00 00`/`e5 e9 00 00` v celé `.text` nenašel nic dalšího. odt1 CLAUDE.md:235-241 **CONFIRMED**. |
| 1b | Kdy FXAA po DLSS | Pro Gargauthovu slot route (`render_scale<1`) **nikdy po**: FXAA (stage 0xf) běží před upscale tailem (stage 0x10), tedy na vstupu DLSS. Pro DLAA (`render_scale=1`) **po** DLSS, protože resolve je v TAA dispatchi (order 2) a FXAA má order 0x7fffff3. odt1 měření (DLAA, FXAA po DLSS) **CONFIRMED**. |
| 1c | Kdy AA Off/FXAA vypne TAA draw | Kdykoli `rs+0xe9e4 == 0` a `[0x1451c5320]+0x2cda != 0` (PC). Gargauth pak nemá vstupy (`hijack.rs:625-633` to sám popisuje). **CONFIRMED.** |
| 1d | Co Gargauth musí držet | Word `rs+0xe9e4 = 0x0001` po celou dobu, kdy DLSS běží, s per-frame re-assertem. AA apply ho přepisuje při každém apply voleb (callers `0x14043df90`, `0x14043e220`, `0x14043f070`, `0x14043f380`, `0x14053d160`). Unit bit 0x2 se dorovná sám přes late updaty. |
| 2 | TAA sharpen vs DLSS | `mSharpen` +0x187 (default 1), `mSharpenAmount` +0x198 (default 0.2). Separátní pass `TemporalAA2_Sharpen[_IGNORE_EDGES]` po TAA (`0x142392f8e`). Engine ho maže jen při **SS flagu** (`cfg+0xb4 & 1`); Gargauth posílá flags=2, takže **sharpen běží**. Tvrzení `tune.rs:33-35` „engine turns its own post-resolve sharpen off the moment DLSS is on“ je pro Gargauthovu konfiguraci **REFUTED**. |
| 3 | Pořadí post passů; motion blur Off | Stage 0xf (render-res, RT = `set+0x420`): AutoExposureApply (ord 0) → **TAA** (ord 2) [+ TAA sharpen] → MotionBlur + MB Reconstruction (ord 100, ctor default) → … → ToneMappingApply_ColorGrading (ord 0x7fffff1) → FXAA (0x7fffff3) → … → CopyGBufferID (0x7ffffff). Pak stage 0x10: upscale tail (ISP → `UpscaleOut` / CAS / prostý copy) → System_Copy do `set+0x460` → UI. Motion blur Off neruší draw; fur shutter 0.4 je ctor default. LostRiver TEXTURE-GRAPH.md:59-70 **CONFIRMED** (statika sedí s jejich runtime: shutter 0, fur 0.4). |
| 4 | Render scale cesty, clamp, align, <0.5, 0.333 | Ratio setter `0x14229be60` (clamp [0.5,1], šířka `&~15`, → `cfg+0x200`); own-scale setter `0x14229bde0` (clamp ≥ 0.5/`cfg+0x204`, → `cfg+0x1f0`); optimal path `0x14229a8c0`/`0x14229bed0` (clamp [0.5,1] na `cfg+0xc4·scale`); per-frame `0x1422980b0` (product → `cfg+0x208` clamp [0.5,1]); target set ctor `0x14255ba70` (content scale a product clamp [0.5,1], **velikost textur z neclampnutého `+0xc`**). Pád <0.5: [INFERENCE] nesoulad product (≥0.5) vs. textury. 0.333 **nejde** patchem konstanty (`0x142e4fe54` = 0.5 má 1560 čtenářů). Jde (nejisté) přesměrováním 6 operandů na `0x142e4fe4c` (0.3). |
| 5 | Mip bias | Banky staví `0x1423d6690` (volá ho `0x1423d3c90` při načtení `system\shader\ShaderPackage` při startu aplikace): 15 bank, bias 0, −log2(2.0…5.0), +log2(2.0…5.0); dyn-res skupiny v templatech = `log2(cfg+0x7b3cc[i]) + log2(0.5)`. `rs+0x1a8` = **`mSamplerLodBias`** (registrace `0x142266159`, enum „Default, -log2(2.0)…“). Ctor nuluje (`0x14226559e mov qword [rdi+0x1a4],rsi`). Čte se při resolve materiálu `0x142383566` → zapečeno. Za běhu se nepřestavuje. Override bit 0x800000 v material bind `0x14235f60c` → bank 0, index `8·cfg+0x7b3b4 + 0xf..0x16`. Nativní LOD bias pro dyn-res **existuje** (jen v temporal-upsample režimu). Gargauth tune.rs:21-32, 197-202, 263-275 **CONFIRMED**. |
| 6 | Další temporal/stochastické efekty | Offsety SSLR `+0xb43f` `mSSLRDitherResolve`, `+0xb440` `mSSLRGBufferJitter`, contact `+0xb500`, facial `+0xb501`, checkerboard `+0xe964/+0xe968` **CONFIRMED** registrací vlastností (`0x142270c60`). Nové nálezy: SSLR má **vlastní temporal mapu** (`tSSLR_TemporalMapUAV`, `bSSLRTemporalReset` = `rs+0xb43c`). Globální `tDitherMap`/`fDitherSize`/`iRenderFrame` je v `CBRenderFrame` (`0x142288bd0`) pro všechny shadery. Volumetrika `CSVRCheckers*` (checkerboard + `_RECOMPUTE`). `mSpecularTemporalBlendRate` `rs+0xe7d0`. Checkerboard alpha (`e964/e968/e96c/e973`) **nemá čtenáře**. |

---

## 2. Nálezy s důkazy

### 2.1 AA přepínače (Q1)

**Setter** `0x142287a00` (leaf bez .pdata, raw disasm):
```
movzx r8d,dl ; test dl,dl ; je →[rcx+0xe9e4]=0x0000
sub 1 → 0x0001 (TAA) ; sub 1 → 0x0100 (FXAA) ; ==1 → 0x0101 (TAA+FXAA)
```
**AA apply** `0x14043dcb0` (bl = volba uživatele):
```
0x14043dcb9 call 0x1404431f0          ; DLSS usable?
0x14043dd0f call 0x142383ef0          ; GetOptimalSettings(out, ngx, w, h, mode=cfg+0xa8)
0x14043dd23 shr rcx,0x20 ; sub ecx,2  ; flags = out[1]
            je → bl=3 (TAA+FXAA)      ; flags==2
            cmp ecx,1 ; jne keep      ; flags==3 → bl=1 (TAA)
0x14043dd42 call 0x142287a00(rs, bl)
unit loop [0x1451238c8]+0x15d0 (next +0x30): TAA DTI [0x1451c6c78] → bit 0x2 z rs+0xe9e4; FXAA DTI [0x1451c6cb0] → bit 0x2 z rs+0xe9e5
```
**DLSS usable** `0x1404431f0`: ini `graphics_option.ini/GraphicsOption/"NVIDIA DLSS" == "On"` (`0x140441850`, klíče `0x143e89d00`, „On“ `0x143e89fb0`) && `dev+0x42700 != 0` && `optimal(mode=2).flags != 0`. OBSERVED.
→ Gargauthův `hook_optimal` vrací flags=2 (`hijack.rs:1071`). Pokud má uživatel herní volbu NVIDIA DLSS = On, AA apply **vynutí TAA+FXAA**. Gargauth píše, že „every session so far has run without“ (`hijack.rs:730-732`), takže dosud netestováno. INFERENCE.

**TAA draw** `0x142392150`:
```
0x14239216d cmp byte [[0x1451c5320]+0x2cda],0 ; je skip-gate
0x14239217d cmp byte [rs+0xe9e4],0 ; je 0x1423931c3 (ret)
0x1423921a1 view=ctx+0x1d0 ; bt [view+0x58],0x12 / 0x17 → ret
```
**TAA late** `0x142393260`: `cl=rs+0xe9e4; al=(type+0xc==8 && (flags+0x14 & 0x12)==0x12); if cl!=al: flags+0x14 = cl ? |2 : &~2` (`0x1423932eb..0x142393317`). Analogicky FXAA late `0x142394140` z `+0xe9e5` (`0x142394239`). OBSERVED. Hold tedy stačí na word: unit bit se dorovná, pokud filter manager volá late update i u vypnutého unitu. Staticky to neověřeno [INFERENCE], ale odt1 drží jen word a podle jejich runtime měření to funguje (`odt1 CLAUDE.md:245-250`, `game.cpp:301-339`).

**FXAA draw** `0x1423939d0`: gate `rs+0xe9e5` (`0x1423939f4`); stage z `+0x138` přes `0x142585410(ctx,0x14a)`, kde tabulka `0x14354cb70[0x14a]=0xf` (stage) a `[0x14b]=0x7fffff3` (order). Vstup `+0x150` (slot RT) → `System_CopyWithLuminance` → `+0x168` (`FXAApLuminance`, vytvořen v late `0x1423941bc`) → `FXAA` zpět do slotu. odt1 „+0x168 input texture“ je **PARTIAL**: je to FXAA-vstup, ale jde o luma-kopii, ne o scénu.

### 2.2 Stage/order a pořadí passů (Q1b, Q3)

`0x142585410(ctx, idx)`: `cdf70(ctx, T[idx])` (stage = RT slot `view+0x60+stage·0x40`) a `ctx+0x154 |= T[idx+1]<<5` (order). `T` = `0x14354cb70` (trojice hash/stage/order). Mapování callerů (`re/scratch/RePostProcess/prio_map.txt`):

| idx | stage | order | pass |
|---|---|---|---|
| 0x13b | 0xf | 0 | AutoExposureApply (`0x1424d0780`) |
| 0x141 | 0xf | 2 | TAA `CSTemporalAA2` (`0x142392150`) |
| — | 0xf (ctor `+0x138`) | 100 (`+0x13c`=0x64) | uMotionBlurFilter / uMotionBlurReconstructionFilter (ctor `0x1424fb9e4`, `0x1424c9694`) |
| 0x144 | 0xf | 0x7fffff1 | ToneMappingApply_ColorGrading (`0x1424d1651`) |
| 0x14a | 0xf | 0x7fffff3 | FXAA |
| 0x150 | 0xf | 0x7fffffe | `0x142597870` |
| 0x156 | 0xf | 0x7ffffff | CSTiledDeferredCopyGBufferID |
| — | **0x10** | — | upscale tail v `0x1423c3790` (`0x1423c69fd call cdf70(ctx, edi=0x10)`) |

Tail `0x1423c3790` (`0x1423c6b93..0x1423c7831`):
```
rdi = [set+0x420]+0x58                        ; chain colour = stage-0xf RT
r8b = cfg+0xbc (mUseCAS) && set+0x6a8 (has UpscaleOut)
r15b = (set+0x58 >> 23)&1                      ; 0x1423c39cf, temporal-upsample mode
if r15b: CSTemporalUpsample path (0x1423c6c03…)
elif cfg+0xa6 && (cfg+0xb4&2) && dev+0x42718 && set+0x6a8 && cfg+0x204 != 1.0 && ngx && set+0x6a0:
     enqueue packet type 4 (EvaluateImageSignalProcessing, 0x1423c7801) ; rdi = UpscaleOut
elif r8b: CAS (0x1423c2620)
System_Copy rdi → set+0x460 (stage 0x10)
```
Pořadí se stage stejně jako u čtení `set+0x420` v tailu dává: všechny stage-0xf filtry (TAA, MB, tonemap, FXAA) proběhnou před ISP. [INFERENCE: příkazy se vykonávají v pořadí (stage, order). Sedí to s Gargauthovým runtime pozorováním „TAA → 2 passy nesoucí tón → upscale slot“ (`gpu.rs:2054-2066`).]
→ **Gargauthova barva pro DLSS ve slot route je po ToneMappingApply_ColorGrading** (a po MB a případně FXAA). Gargauth přitom posílá `expert_dlss_hdr=1`. Zda je to ještě HDR rozsah, je UNKNOWN, viz §4.

`Evaluate*` volá packet executor `0x14259dae0` (typ 3 = EvalSS `0x142383cf0`, typ 4 = EvalISP `0x142383990`). OBSERVED.

### 2.3 TAA sharpen (Q2)

Ctor `0x142391210` (rbx = this+0x178):
```
[+0x138]=0xf (stage)  [+0x13c]=2 (order)  [+0x180]=0.95 mBlendRate
[+0x184] dword 0x01010001 → +0x184 mJitterEnable=1, +0x186 mVelocityBase=1, +0x187 mSharpen=1
[+0x188]=2 (YCoCgFixed) [+0x18c]=1 (AABB) [+0x194]=1.2 [+0x198]=0.2 mSharpenAmount
[+0x19c] word 0x100 → IgnoreEdges=0, mReprojection=1 ; [+0x19e]=1 mCheckOutput
```
Jména z registrace `0x142391830` (props.py). Late `0x14239334a mov byte [rbx+0x187],0` jen pod podmínkou `cfg+0xa5 && cfg+0xb4&1 && dev+0x42718 && dev+0x42700`. Draw `0x142392f8e cmp byte [rbx+0x187],0`: když není 0, spustí sharpen pass `tBaseMap=history[f%3] → rwOutput=history[(f+1)%3]` a finální kopie `0x1423917a0` bere `history[(f+1)%3]` do `[[+0x158]+0xb0]`. `cfg+0xb4` = flags z optimal answer (`0x142299802 movsd [rsi+0xb0]`, `0x14229a98b`). Gargauth flags=2 → `+0x187` zůstává 1. Gargauthův dispatch hook rozpoznává temporal pass podle bindů `tBaseMap,tBlendMap,tDepthMap,rwOutput` (`gpu.rs:480-484`). Sharpen binduje `tBaseMap,tGBuffer1,rwOutput` → **není zachycen a běží**. Slot route: `copy_through` (`gpu.rs:2083-2088`) zapíše surovou barvu do `history[f%3]` a sharpen ji vyostří **před DLSS**. DLAA: sharpen vyostří **výstup DLSS** (a s Gargauthovým RCAS ostří dvakrát).

### 2.4 Render scale (Q4)

| Funkce | Co dělá | Clamp operand |
|---|---|---|
| `0x14229be60` ratio setter | `x=clamp(x,0.5,1)`; `w=(cfg+0x2cd0 if gate else cfg+0x198)`; `(int)(w·x) & ~15 / w → cfg+0x200` | `0x14229be6d f30f1015 df3fbb00` |
| `0x14229bde0` own-scale setter | `max(0.5/cfg+0x204, min(x,1))`; r8b → align 16, jinak přímo → `cfg+0x1f0` | `0x14229bdeb f30f1015 6140bb00` |
| `0x14229a8c0` po optimal query | `clamp(cfg+0xc4·scale,0.5,1) → cfg+0x200` | `0x14229a9b4 f30f1005 9854bb00` |
| `0x14229bed0` (odt1: „clears DLSS type“) | stejné jako výše | `0x14229bf05 f30f1005 473fbb00` |
| `0x1422980b0` per-frame | při `+0x204≠+0x200` rebuild, `+0x1f0=1.0`; `+0x208 = clamp(+0x1f0·+0x200, 0.5, 1)` | `0x142298380 f30f1005 cc7abb00` |
| `0x14255ba70` target set ctor | content scale `[+8]` clamp, product → `set+0x6b8` clamp; **velikost textur `w·[+0xc]`, `h·[+0xc]` bez clampu** (`0x14255bc2d`) | `0x14255bab9 f30f1035 93438f00` (xmm6 pro oba clampy) |
| `0x1423d66c4` bank builder | `log2(0.5)` pro dyn-res bias — **NEměnit** | — |

Konstanty: `0x142e4fe4c`=0.3, `0x142e4fe50`=0.3333, `0x142e4fe54`=0.5 (1560 čtenářů), `0x142e4fe58`=1.0. Zadání uvádí „clamp `0x142e4fe58`“. To je horní mez 1.0, dolní mez je `0x142e4fe54`.

**Proč <0.5 padá** [INFERENCE, silná]: setter by sice uložil 0.4, ale `cfg+0x208` a `set+0x6b8` se clampnou na 0.5, zatímco textury mají `0.4·display`. Viewport/rect render oblasti (`0x1423c38a8 mulss [rcx+0x6b8]`, `0x1423cb61c divss [rax+0x6b8]`) je pak větší než textura. Neplatný copy box nebo region vede k `E_INVALIDARG` při `Close`, což sedí s `hijack.rs:558-564`.
**Důsledek pro 0.5:** setter clampuje *před* zarovnáním na 16. Při 3440 px: `1720 & ~15 = 1712` → ratio 0.4977 < 0.5 → stejný nesoulad. Gargauthův `snapped()` (`hijack.rs:610-621`) počítá stejně, takže `render_scale=0.5` na 3440×1440 (taky 1680, 1366, 1360) je pravděpodobně pád. [INFERENCE]
**0.333:** konstantu nepatchovat (sdílená). Bylo by nutné přesměrovat 6 operandů (tabulka výše, bez `0x1423d66c4`) na **0.3** (`0x142e4fe4c`), ne na 0.3333: `3840·0.3333 = 1279.9 → 1264 → 0.329 < 0.3333` by znovu vyrobilo nesoulad. Další předpoklady ≥0.5 (half-res buffery, VR checkers) staticky vyloučit nelze. UNKNOWN, runtime test v §4.

### 2.5 Mip bias (Q5)

- Builder `0x1423d6690`: dyn-res bias `bias_i = log2(cfg+0x7b3cc[i]) + log2(0.5)` (`0x1423d66e0..0x1423d6711`) se zapisuje do 63 sampler templates. Banky (`r12d=0xf`, `0x1423d6949`) mají bias `0, −log2{2,2.5,3,3.5,4,4.5,5}, +log2{…}` (konstanty `0x142f0f358`=2.0 … `0x142f1fd50`=5.0). Bias se přičte, pokud ≠0, s `min(...,MaxLOD)` (`0x1423d7540..0x1423d756e`). Operand `0x1423d73ee` (2.0 pro bank 1) je ten, který Gargauth přesměrovává. **CONFIRMED.**
- Volání: `0x1423d4281` v `0x1423d3c90` ← `0x1423d2cf0` (`system\shader\ShaderPackage`) / `0x1423d3670` ← `0x141aec7d0` (start aplikace: chunk/nativePC). Žádná cesta za běhu. INFERENCE (silná).
- `rs+0x1a8` = `mSamplerLodBias` (`0x142266159`, jméno `0x1435094f0`, enum `0x143509170`), ctor 0 (`0x14226559e`). Konzument `0x142383566`: `bank=[0x1451c73d0]+0x2488+8·rs+0x1a8; slot = bank[idx]` → uloží pointer do objektu materiálu. Zapečeno při resolve. OBSERVED.
- Herní ini volba `LODBias` je mesh LOD (`0x14043f6af`), `rs+0x1a8` nepíše. „No option“ **CONFIRMED.**
- Override `0x14235f60c test dword [view+0x58],0x800000` (view = `ctx+0x1d0`, ne `cfg+0x1f450`): `idx=[sampler+0x78]` (0..0xb) → jump table → bank0 `[8·cfg+0x7b3b4 + 0xf..0x16]`. Gargauth `tune.rs:263-275` **CONFIRMED**. Upřesnění: **bit 0x800000 = temporal-upsample mode** (stejný bit vyřazuje TAA `0x1423931fb`/`0x142393284`/`0x1423921ae` a zapíná `CSTemporalUpsample` v tailu `0x1423c39cf`). Nativní LOD bias pro dyn-res tedy existuje jen v tomto režimu. Gargauthův DLSS režim ho mít nesmí, jinak nebude TAA pass.

### 2.6 Stochastické efekty (Q6)

Registrace rs vlastností `0x142270c60` (výpis `rs_props.txt`):

| off | jméno | ctor default | konzument |
|---|---|---|---|
| 0xb43c | (bez jména) `bSSLRTemporalReset` | 0 (`0x14228001c`) | `0x14228ce53` CBSSLR |
| 0xb43f | mSSLRDitherResolve | 0 (`0x14227ffba`) | `0x142286318` (výběr `SSLR_Resolve_Dither*`) |
| 0xb440 | mSSLRGBufferJitter | 0 (`0x14226567c`, esi=0 po smyčce) | `0x142286320`, `0x14228c1a2` |
| 0xb500 | mContactShadowEnableNoise | 0 (`0x1422663ed word 0x100`) | `0x142288147` `bIsUseNoise` |
| 0xb501 | mFacialContactShadowEnableNoise | **1** | `0x14228813e` |
| 0xe634 | mSSLRDitherRadius | 8.0 | `0x14228c94a` `fDitherRadius` |
| 0xe7d0 | mSpecularTemporalBlendRate | ? | `0x1422698ea` aj. |
| 0xe964/0xe968 | mCheckerboardAlphaUnrollNear/Far | 1e-5 / 1e8 | **žádný** |
| 0xe96c/0xe973 | CheckerboardHistoryBlendRate/BlendDither | — | **žádný** |

Negativní hledání: byte-scan disp32 `64 e9 00 00`/`68 e9 00 00` v celé `.text` našel jen ctor `0x14226624b/0x142266255` a registraci `0x142271eba/0x142271f53`. Výpis `[r+0xe95x..0xe97x]` mimo ctor/registraci: jen `e950`, `e958`, `e960-e963`, `e970`, `e971` (zápis `0x140edc1fa`). Hodnoty mohou přepsat data scény přes property systém, proto jsou defaulty jen z ctoru (UNKNOWN runtime).
Další: `CBRenderFrame` (`0x142288bd0`) má `tDitherMap`, `fDitherSize`, `iRenderFrame`, `iTotalTime`, tj. globální (pravděpodobně per-frame animovaný) dither pro shadery, rs ho neovládá. `SSLR_EffectReflection` taky. SSLR má vlastní temporal akumulaci (`tSSLR_TemporalMapUAV`, `0x142286260`), takže Gargauthova premisa „the engine's temporal pass is built to absorb“ (`tune.rs:41-45`) je **PARTIAL**: SSR dither tlumí i vlastní SSR temporal. Volumetrika: `CSVRCheckers[_RECOMPUTE][_LOW/_MEDIUM/_HIGH]` (stage 0xc).

### 2.7 Motion blur (Q3)

- `uMotionBlurFilter` vtable `0x14353b858` (draw `+0x60` = `0x1424fbfb0`), ctor `0x1424fb9a0`: `+0x138=0xf`, `+0x13c=100`, `+0x168=0.016` (shutter), **`+0x16c=0.4` (fur shutter, `0x1424fba2c`)**, `+0x170=16`. CB `CBMotionBlur{iMotionBlurSampleNum, fMotionBlurShutterSpeed=+0x168, fTransform, fMaxAdditionalVelocity, fMotionBlurFurShutterSpeed=+0x16c}`, vstupy `tBaseMap, tGBuffer4, tGBuffer5`.
- `uMotionBlurReconstructionFilter` vtable `0x143537a70` (draw `0x1424ca1f0`), ctor `0x1424c9650`: stage 0xf, order 100, `+0x188 = 0.4`.
- Draw nemá test volby. Volba → `rs+0xe9ec` (`0x14043ed6c`). Getter `0x141b19d70` (`[0x14500de70]+0x58 > 1 → 1`, jinak `rs+0xe9ec`) je registrován jako vlastnost jinde (ptr `0x1433fc920`). [INFERENCE: řídí Sdl/scheduler track shutteru, proto Off = shutter 0, fur zůstává.] Žádný jiný čtenář `+0xe9ec` (byte-scan). V cutscénách je motion blur vynucený.

---

## 3. Doporučené změny pro Gargauth

1. **Hold AA wordu**: přidat `tune::hold_taa_only(active)` volané z `gpu.rs:2411-2415` (blok per submission, kde je `unroll/mip_bias/undither`). Když `gate::dlss_ready()`: pokud `u16 [rs+0xe9e4] != 0x0001`, zapsat `0x0001` a zalogovat původní hodnotu. Při ukončení DLSS vrátit hodnotu hry jen tehdy, když tam stále je náš zápis (vzor odt1 `game.cpp:301-339`). `rs` už je (`tune.rs:121-135`). Opraví AA Off/FXAA (DLSS neběží) i FXAA nucené přes herní „NVIDIA DLSS=On“.
2. **TAA sharpen**: vynulovat `+0x187` na TAA unitech. Vzor: chodit seznamem `[0x1451238c8]+0x15d0`, next `+0x30` (stejně jako AA apply `0x14043dd47..0x14043de06`), match `*(unit) == image+0x351bda8`. Load `0x14043dd47 mov rax,[rip+0x4ce5b7a]` přidat do `gate` SITES. Alternativně `+0x198 = 0.0`. Opravit komentář `tune.rs:33-39` („engine turns its own post-resolve sharpen off…“ platí jen při flags bit0).
3. **`snapped()`** `hijack.rs:610-621` a `SMALLEST` `hijack.rs:565`: zaručit, že uložené ratio je ≥ 0.5 po zarovnání. `render_w = max(((w·r) as u32) & !15, ceil16(w/2))`, ratio pro setter `= (render_w + 0.5)/w`, aby truncation v setteru dala stejné `render_w`. Stejně upravit `ratio_wanted()` (`hijack.rs:493-495`) a overlay snap (`overlay.rs:301-304`).
4. **`tune::unroll`** (`tune.rs:137-191`, volání `gpu.rs:2403-2411`): `rs+0xe964/0xe968` nemá čtenáře. Odstranit (ušetří VirtualQuery a zavádějící log „alpha is drawn whole…“) nebo komentářem označit za no-op.
5. **`tune::undither`** (`tune.rs:402-428`): před zápisem zalogovat 4 původní bajty. `mFacialContactShadowEnableNoise` má default 1, ostatní 0, takže efekt může být jen u facial. Pokud data scény nastavují jinak, log to ukáže.
6. **`pathdump.rs:119-120`**: `FUN_14043dcb0` je handler **Anti-Aliasing** (s DLSS override), ne „menu's NVIDIA DLSS option“. Opravit text. Do `engine_state` přidat `rs+0xe9ec/0xe9ed` (MB/DOF switch) a TAA `+0x187/+0x198`.
7. **`tune.rs:263-275`** (komentář k bitu 0x800000) doplnit: jde o temporal-upsample mode. Se zapnutým bitem neběží TAA, takže by nebylo ani DLSS.

---

## 4. UNKNOWN + runtime experimenty

| UNKNOWN | Experiment (soubor/funkce) | Co rozhodne |
|---|---|---|
| Běží v Gargauthově session `TemporalAA2_Sharpen`? | `gpu.rs` dispatch hook: při `pathdump::recording()` logovat jméno PSO z `dxbc` reflexe (název entry/bindy `tBaseMap,tGBuffer1,rwOutput`) pro dispatche po temporal passu. Nebo přečíst `+0x187` na TAA unitu (bod 3.2) | Dispatch existuje a `+0x187==1` → potvrzeno, opravit |
| Je barva ve slot route ještě HDR? | `pathdump::slot`/`gpu.rs` slot route: jednou za N snímků readback 64×64 výřezu colour (R11G11B10) → max/percentil | max ≤ ~1.0 → po tonemapu je LDR → `expert_dlss_hdr=0` test |
| Pád 0.5 na 3440 px (a <0.5 obecně) | Zapnout `debug_layer` + `render_scale=0.5` při 3440×1440 (nebo okno 1680 px). Logovat `cfg+0x200/+0x204/+0x208`, `set+0x6b4/+0x6b8` a texture size (`pathdump` je umí) | `+0x208=0.5` ≠ `+0x204=0.4977` a pád/E_INVALIDARG → mechanismus potvrzen |
| 0.333 (UltraPerf) | Experimentální větev: CodeWrite 6 operandů z §2.4 na `0x142e4fe4c` (jako `prepare_exact_bias`), `debug_layer=1`, `render_scale=0.333` | Bez D3D12 erroru 1000 snímků + korektní obraz → proveditelné |
| Kdo nastavuje `set+0x58` bit 0x800000 | Gargauth už loguje (`tune::note_sampler_override`). Dočasně vypnout hold Image Quality (`hook_scaling_option`) a zvolit Variable | Bit při Variable → dyn-res = temporal-upsample mode |
| Runtime hodnoty SSLR/contact (data scény) | Bod 3.5 (log před zápisem) | Hodnoty ≠ ctor default |
| Vliv fur shutteru 0.4 na DLSS | Najít `uMotionBlurFilter` v unit listu (vtable RVA `0x353b858`), dočasně `+0x16c=0.0` (klíč `diag`) a porovnat srst v pohybu | Méně smearu/ghostingu srsti → zavést |
| Pořadí příkazů = (stage, order)? | `pathdump` F10 frame list se zapnutým TAA+FXAA (bez holdu) | FXAA draw mezi temporal dispatch a upscale slot → potvrzeno |

---

## 5. Mapa adres

| Adresa | Co |
|---|---|
| `0x1451c4368` | rs (render settings) slot; load `0x1402412ef` |
| `0x1451c4480` | cfg (graphics config) |
| `0x1451c73d0` | shader/device manager; `+0x2488` sampler banky |
| `0x1451c5320`+0x2cda | PC option gate |
| `0x1451238c8`+0x15d0 | unit list (next +0x30) |
| `0x142287a00` | AA setter → `rs+0xe9e4` |
| `0x14043dcb0` | Anti-Aliasing apply (+ DLSS override z flags) |
| `0x1404431f0` | DLSS usable (ini „NVIDIA DLSS“=On, ngx, flags≠0) |
| `0x14043eeb0` | Image Quality handler |
| `0x14043e220` | apply voleb (MotionBlur → `rs+0xe9ec`, DOF → `+0xe9ed`, Z-Prepass → `cfg+0x7b41c`) |
| `0x14351bda8` | uTemporalAAFilter2 vtable (ctor `0x142391210`, props `0x142391830`, update `0x1423931e0`, late `0x142393260`, draw `0x142392150`, alloc `0x142391f30`, SS enqueue `0x1423913f0`, final copy `0x1423917a0`) |
| `0x14351c1a8` | uFXAAFilter vtable (late `0x142394140`, draw `0x1423939d0`) |
| `0x14353b858` | uMotionBlurFilter vtable (ctor `0x1424fb9a0`, draw `0x1424fbfb0`) |
| `0x143537a70` | uMotionBlurReconstructionFilter vtable (ctor `0x1424c9650`, draw `0x1424ca1f0`) |
| `0x1424d0780` | AutoExposureApply / ToneMappingApply_ColorGrading |
| `0x142585410`, `0x14354cb70` | stage/order setter a tabulka |
| `0x1423c3790` | frame/view funkce; tail ISP/CAS/copy `0x1423c6b93-0x1423c7831` |
| `0x14259dae0` | packet executor (typ 3 EvalSS, 4 EvalISP) |
| `0x14229be60` / `0x14229bde0` | ratio setter / own-scale setter |
| `0x14229a8c0`, `0x14229bed0`, `0x1422980b0` | optimal→ratio, DLSS type, per-frame product `cfg+0x208` |
| `0x14255ba70` | target set ctor (`+0x6b0/+0x6b4/+0x6b8`, `+0x58` flags) |
| `0x142293f10` | vytvoření primary target set (`cfg+0x1f450`, flags `cfg+0x1dc|1`) |
| `0x142e4fe4c/50/54/58` | 0.3 / 0.3333 / 0.5 / 1.0 |
| `0x1423d6690` | stavba sampler bank (operand bank 1 `0x1423d73ee`) |
| `0x142383566` | čtení `rs+0x1a8` při resolve materiálu |
| `0x14235f450` | material bind; override bit `0x14235f60c` |
| `0x1422646f0` / `0x142270c60` | rs ctor / registrace vlastností rs |
| `0x14227ff00` | rs SSLR init (defaulty) |
| `0x142286260`, `0x14228c6b0`, `0x142287e00`, `0x142288bd0` | SSLR resolve, CBSSLR, CBContactShadowInfo, CBRenderFrame |
| `0x141b19d70` | MotionBlur getter |

Skripty: `re/scratch/RePostProcess/{s.py,props.py,strs.py,prio.py,rdis.py}`, výstupy `rs_props.txt`, `prio_map.txt`, `c3790*.txt`.
