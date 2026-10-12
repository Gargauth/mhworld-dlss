# ReDlss1Path — herní DLSS 1.x / NGX cesta v MHW:I (build 15539686)

Statická RE nad `MonsterHunterWorld.exe` (offline capstone index `re/tools/g`, vlastní skripty v
`re/scratch/ReDlss1Path-2/`). Značky: **OBSERVED** = přímo z disasm, **INFERENCE** = odvozeno,
**UNKNOWN** = staticky nerozhodnutelné. Verdikty k cizím tvrzením: CONFIRMED / REFUTED / PARTIAL.

## 1. Otázky a verdikty

| # | Otázka | Verdikt (zkráceně) |
|---|--------|--------------------|
| 1 | Parametry create/evaluate, type bity, zdroje, formáty | **OBSERVED, kompletní.** Flags = `{scale, flags, sharpness}` z `GetOptimalSettings`; bit0 = SuperSampling (feature 1), bit1 = ImageSignalProcessing (feature 9). DLSSMode 1 → flags 3 (1 při scale==1.0), DLSSMode 2 → flags 2. EvalSS: `Rect.X/Y/W/H`, `Color`, `MotionVectors`, `Output`, `PreviousOutput=null`, `Reset`, `MV.Scale.X/Y=1.0`, `MV.Offset.X/Y=0`, `Depth`. EvalISP: `Color`, `Output`, `Sharpness`, `Rect.*` (jen když W≠0). Formáty: TAA history R11G11B10_FLOAT (26), staticky OBSERVED; hlavní colour 26 jen podle měření odt1. Dále dense MV R32G32_FLOAT (16), depth copy R32_FLOAT (41), tGBuffer4 R32_UINT (42) a UpscaleOut R11G11B10 nebo R16G16B16A16_FLOAT podle bitu 16 v `view+0x58`. |
| 2 | Reset při evaluate | **ANO, jen v EvalSS.** `Reset = (desc+0x28 <= 0.0f)`, kde `desc+0x28 = taa+0x185 ? 0.0 : taa+0x180 (fBlendRate)`. `taa+0x185` = „history invalid“: nastaví ho přestavba historie TAA (`0x142391f30`), změna render scale v tomto snímku (`renderer+0x1fc` → TAA late update) a property-copy `0x14262c010`; maže ho `0x142393360`. **Detekci střihu kamery v TAA cestě jsem nenašel** (hranice v §2.4). EvalISP Reset nemá a jeho `desc+0x28` je vždy 0. Čitelné v Gargauthových hookách: v EvalSS jako `*(f32*)(desc+0x28) <= 0`. Při flags=2, tedy bez SS, je zdroj `**(u8**)(capture+0x38)` v už nainstalovaném `hook_ss_enqueue`. |
| 3 | SS cesta a pád na null depth | **OBSERVED.** Planner bariér `0x14229cee0` pro příkaz 0x17/sub 3 čte depth přes `desc+0x20 → +0xf0`. Hodnota je null a `0x1422927f0` pak padá na `mov r10,[rdx+0x70]` (`0x1422927f2`). `EvalSS` přitom čte depth přes `+0x60`. Textura má po vytvoření nastavené vždy jen jedno z `+0x60`/`+0xf0`: typ 6 (depth) přesune native do `+0xf0`. Herní DX12 SS cesta je proto vnitřně nekonzistentní. Alias `+0xf0 := +0x60` s AddRef (odt1) je **CONFIRMED** jako správná oprava: release uvolňuje oba sloty zvlášť (`0x14239dd66..95`). SS cesta **dokáže nahradit** DSV heuristiku i MV dekód; postup je v §3. |
| 4 | tGBuffer4 a dense velocity | **OBSERVED.** tGBuffer4 = `view+0x620` (GBuffer set) `+0x78`, formát R32_UINT (42), vytváří ho `0x14257c6f5`. Dense velocity = `taa+0x1a8`, R32G32_FLOAT (16), v render rozlišení. Alokuje ho `0x142391f30` jen při `renderer+0xa5 && renderer+0xb4&1 && device+0x42718`. Plní ho compute `TemporalAA2_ResolveDenseVelocity` uvnitř SS enqueue, nad bindingy TAA (tGBuffer4, tDepthMap, CB_DL s view/proj a prev-view-proj). |
| 5 | Pořadí passů; hypotéza odt1; zahazuje Gargauthův slot route výsledky? | **OBSERVED (výchozí priority).** Klíč řazení = (pass, prio). Bloom (14,99), FFT bloom (14,100), AutoExposureApply (15,0), Haze (15,1), **TAA + EvalSS (15,2)**, DOF/Bokeh (15,16), MotionBlur a MB reconstruction (15,100), ToneMappingApply_ColorGrading (15,0x7fffff1), FXAA (15,0x7fffff3), **EvalISP / upscale point (16,0)**. odt1 („DLISP po DOF/MB/bloom“) = **PARTIAL**: DOF, MB i tonemap/grading jsou před ISP, bloom však ve výchozím stavu běží **před** TAA. Pass/prio filtrů může přepsat SDL (`mFilterData.m*Pass/Prio`). Slot route výsledky **nezahazuje**, protože DLSS čte `desc+0x08` = hlavní colour po těchto passech. Zahazoval je starý route `resolve_at_pass=1`. |
| 6 | NGX init | **OBSERVED.** `NVSDK_NGX_D3D12_Init(0x5f8a85f, L"DLSS", device, 0x12)` (statická lib NGX `rel_1_3`). Volá se jen pro VendorId 0x10de a `device+0x2da≠0`. Core se načítá jako `<exe dir>\nvngx.dll`, pak přes D3DKMTQueryAdapterInfo → `NGXPath`, pak `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore\FullPath`, vždy s příponou `\nvngx.dll`. Feature-path API neexistuje. Krátká jména featur jsou `dlss`/`dlisp`. Gargauth `ngx.rs:399-504`: app id, verze 18 a pořadí argumentů **CONFIRMED**. Pětici `a4..a5` však Gargauth předává bez čištění a `a4` = adapter LUID (**nová** informace). OTA 1.2.110 = UNKNOWN (staticky v exe nic). |
| 7 | Early-load „type 2“ | **CONFIRMED mechanismus.** Driver callback vrátí DLSSMode 2 → flags 2 → žádný SS pass a `taa+0x1a8` se nealokuje. Pozdější přepnutí na bit0 bez přestavby historie skončí null texturou v obecném planneru (`0x142292687/0x14229268b`). Gargauth hookuje IAT `GetProcAddress` (`0x142e13118`) od startu procesu (launcher), takže tento problém nemá. Flags=2 („type 2“) volí **záměrně** v `hijack.rs:1071`. |

## 2. Nálezy s důkazy

### 2.1 NGX parametrový objekt a exportované helpery
- Exe exportuje statickou NGX lib. Mezi exporty jsou `NVSDK_NGX_D3D12_Init 0x14273f230`, `..._CreateFeature 0x14273f060`, `..._EvaluateFeature 0x14273f0d0`, `..._EvaluateFeature_C 0x14273f140`, `..._GetParameters 0x14273f180`, `..._GetScratchBufferSize 0x14273f1d0`, `..._ReleaseFeature 0x14273f430`, `..._Shutdown 0x14273f480` a `NVSDK_NGX_Parameter_*` `0x14273e370..0x14273e8f0`. Stejná sada existuje i pro D3D11 (`0x14273ec00..0x14273f020`). **OBSERVED**: PE export table (pefile).
- Vtable `NVSDK_NGX_Parameter` je v MSVC uspořádaná s obráceným pořadím overloadů. **OBSERVED** (wrapper helper → offset slotu):
  - Set: `SetVoidPointer +0x00`, `SetD3d12Resource +0x08` (`0x14273e741 mov rbx,[rax+8]`), `SetD3d11Resource +0x10`, `SetI +0x18`, `SetUI +0x20`, `SetD +0x28`, `SetF +0x30`, `SetULL +0x38`.
  - Get: `GetVoidPointer +0x40`, `GetD3d12Resource +0x48`, `GetD3d11Resource +0x50`, `GetI +0x58`, `GetUI +0x60`, `GetD +0x68`, `GetF +0x70`, `GetULL +0x78`.
  - Gargauth `ngx.rs:31-35` (`GET_I32=11, GET_U32=12, SET_I32=3, SET_U32=4`) = **CONFIRMED**.
- NGX tabulka `0x1451d8808..0x1451d883f` (sloty 8 B):
  - `+0x00` Init → `0x1451d8808`
  - `+0x08` Shutdown → `0x1451d8810`
  - `+0x10` GetScratchBufferSize → `0x1451d8818`
  - `+0x18` GetParameters → `0x1451d8820`
  - `+0x20` CreateFeature → `0x1451d8828`
  - `+0x28` EvaluateFeature → `0x1451d8830`
  - `+0x30` ReleaseFeature → `0x1451d8838`
  - Pořadí zápisů je v `0x14273f317..0x14273f397`. Init tabulku nejdřív vynuluje (`0x14273f298..0x14273f2c2`), sloty plní přes `call [0x142e13118]` (= `KERNEL32!GetProcAddress`, ověřeno z importů) a pak volá `Init` přes `call rbx` na `0x14273f3ff`. odt1 `CLAUDE.md:127-130` = **CONFIRMED**.

### 2.2 Init (otázka 6)
- `0x1423842a0` (D3D12 větev, volá ji `0x1423a72e8` v device init `0x1423a48e0`). **OBSERVED**:
  ```
  0x1423842b6 mov rcx,[0x144be5c80] ; "DLSS" (char*)  -> 0x142184e00 = CreateDirectory("DLSS")
  0x1423842c2 mov rdx,[0x144be5c88] ; L"DLSS" (0x14351a270)
  0x1423842c9 mov r9d,0x12          ; API version 18
  0x1423842d2 mov ecx,0x5f8a85f     ; application id 100182111
  0x1423842d7 call 0x14273f230      ; NVSDK_NGX_D3D12_Init(id, L"DLSS", device, 0x12)
  ```
  - Potom následuje `GetParameters` a kontrola `GetI("SuperSampling.Available")` a `GetI("ImageSignalProcessing.Available")`. Selhání zapíše 1/2/3 do `0x1451c6c28`.
  - Kontext se alokuje jako 0x48 B (`lea ecx,[rbx+0x48]; call 0x142175210`) s `[ctx]=params`.
  - Kontext se ukládá do `device+0x42700` (`0x1423a72ed`). `device+0x42718 = 0x142384400(ctx)`, tedy `ctx+0x38 || ctx+0x40` neboli „existuje aspoň jeden feature handle“.
  - Volání nastane jen pokud `[rbp+0x690]==0x10de` (VendorId) a `device+0x2da≠0` (`0x1423a72c9..0x1423a72e3`).
- Statická lib je `C:\dvs\p4\build\sw\devrel\libdev\NGX\core\rel_1_3\...\nvsdk_ngx_lib.cpp` (`0x14359b8c0`). Načtení core (`0x14273fe90`), **OBSERVED**:
  1. `GetModuleFileNameW(NULL)` → odřízne se poslední `\` → připojí se `\nvngx.dll` → `NGXLoadSignedFromPath` (`0x142741800`).
  2. Při neúspěchu `NGXGetPathUsingQAI` (`0x14273fb50`, `gdi32!D3DKMTEnumAdapters2/QueryAdapterInfo`, shoda LUID).
  3. Pak registry `System\CurrentControlSet\Services\nvlddmkm\NGXCore` / `NGXPath`.
  4. Nakonec `SOFTWARE\NVIDIA Corporation\Global\NGXCore` / `FullPath`.
  - Vždy se připojí `\nvngx.dll` (`0x14359bab8`).
  - **Riziko (INFERENCE):** soubor `nvngx.dll` vedle exe má přednost před driverem, pokud projde podpisem.
- Při volání Init na `0x14273f3ff` drží zásobník volajícího `0x14273f230` v `[rsp+0x20]` LUID adaptéru. Ten tam zapsal `ID3D12Device::GetAdapterLuid` přes `vtbl+0x158` s návratovým bufferem `lea rdx,[rsp+0x20]` (`0x14273f2cd..0x14273f2e5`). V `[rsp+0x28]` je 0 (`0x14273f275`). Gargauthův `observe_game_init(a0..a5)` proto dostává `a4 = LUID`, `a5 = 0` a s `version=21` je posílá driveru beze změny (`ngx.rs:545,560`). **OBSERVED** (zásobník) + **INFERENCE** (dopad: pokud legacy export pro verzi ≥0x13 čte 5. argument jako `FeatureCommonInfo*`, dostane LUID jako pointer).
- V exe jsou jen krátká jména featur: `dlss`, `dlisp`, … (`0x14359b2bc`, `0x14359b324`). Jména typu `nvngx_dlss.dll` nebo `nvngx_dlisp.dll` ani verze modelu (1.2.110) v exe **nejsou** (hledáno ASCII i UTF-16, celé .rdata a .data). OTA = UNKNOWN.
- Gargauth `ngx.rs:500-504` („game asks for 18“, `app_id = a0`) = **CONFIRMED**. `ngx.rs:78-86` hledá `_nvngx.dll`, ale exe žádá `nvngx.dll`. Zda driverův `nvngx.dll` dotahuje `_nvngx.dll`, je UNKNOWN (viz §4).

### 2.3 GetOptimalSettings `0x142383ef0` a CreateFeatures `0x142384420` (otázka 1)
- **GetOptimalSettings(out, ctx, w, h, mode)**:
  - Vstup: výchozí `out={1.0, 0, 0}` (`0x142383f16`).
  - `mode`: 1→PerfQuality 1 (Balanced), 2→0 (MaxPerf), 3→2 (MaxQuality), jinak 1 (`0x142383f3d..0x142383f57`).
  - Postup: `GetVoidPointer("DLSSOptimalSettingsCallback")` → `SetUI Width/Height` → `SetI PerfQualityValue` → `SetI RTXValue=0` → `callback(params)` → `GetUI OutWidth/OutHeight`, `GetI DLSSMode`, `GetF Sharpness`.
  - `scale = OutWidth/w`. Odmítnutí, když `|OutW/w − OutH/h| > 0.01` (`0x1423840b3..0x1423840c1`).
  - **Flags**: DLSSMode 1 → 3, nebo 1 když `scale==1.0` (`0x1423840dd..0x1423840eb`); DLSSMode 2 → 2; jinak 0.
  - Výsledek se ukládá do `renderer+0xb0/+0xb4/+0xb8` (`0x142299802..0x14229980a`). Při `flags&2` se scale zapíše do `+0x200/+0x204` (`0x142299a54..5e`).
  - odt1 („+0xb4 type 1 = DLSS 1.0, 3 = DLSS+DLISP“) = **CONFIRMED**; je to pole flags.
- **CreateFeatures(ctx, outW, outH, mode, _, list)**, **OBSERVED**:
  - Na sdíleném param objektu nastaví `SetI Width/Height = out*scale`, `SetUI OutWidth/OutHeight = out`, `SetF "ImageSignalProcessing.ScaleFactor" = 1/scale`.
  - Pro **bit0**: `SetUI CreationNodeMask=1, VisibilityNodeMask=1, Width, Height, Hint.HDR=1`, `SetI PerfQualityValue, RTXValue=0`, `D3D12_CreateFeature(list, 1, params, &h)` (`0x14238460d`).
  - Pro **bit1**: totéž, ale bez `Hint.HDR` a s `OutWidth/OutHeight`, `D3D12_CreateFeature(list, 9, …)` (`0x1423847b0`).
  - Handly jde do `ctx+0x38` (SS) a `ctx+0x40` (ISP) (`0x142384886..0x14238488c`). Při selhání následuje release.
  - Na D3D11 se volá `0x14273ec00` s `[ctx+8]`. Přepínač DX12 je `0x1424e9630 = movzx eax, byte [0x1451cc080]`; odt1 „DX12 flag `0x1451cc080`“ = **CONFIRMED**.

### 2.4 Evaluate a deskriptor (otázky 1–2)
- Příkaz typu 0x17 (64 B) má `+0x10` subtyp (3 = SS, 4 = ISP) a `+0x20` = deskriptor (64 B). Vykonává ho DX12 translátor `0x14259dae0`, slot 0x17 tabulky `0x14354e4c0` (**OBSERVED**):
  1. `0x14259ba50(cmd)` = bariéry z `cmd+8`;
  2. flush `ResourceBarrier` (`vtbl+0xd0`);
  3. `EvalSS/EvalISP(desc, native list [ctx+0x20])`;
  4. obnova `SetDescriptorHeaps` (`vtbl+0xe0`, `0x14259dbe0`/`0x14259dd0b`) a zneplatnění cache root-state (`+0x1b0`, `+0x2b0` memset, `+0x108..+0x118`).
  - **Důsledek:** cokoli Gargauthův hook nahraje do listu uvnitř Eval*, po tom engine sám znovu nastaví heapy.
- Deskriptor, potvrzeno oběma enqueuery. Gargauth `hijack.rs:273-280` = **CONFIRMED + doplnění** (`+0x00` ctx, `+0x3c` sharpness):

  | off | SS (`0x1423913f0`) | ISP (`0x1423c76de..0x1423c780c`) |
  |---|---|---|
  | +0x00 | ctx = `device+0x42700` | ctx |
  | +0x08 | colour = `taa+0x150` (= `[view+0x420]+0x58`, hlavní colour) | colour = `[view+0x420]+0x58` |
  | +0x10 | MV = `taa+0x1a8` (dense velocity; `0x142391574 mov rax,[rcx+0x1a8]`) | 0 |
  | +0x18 | output = `taa+0x168[ mSharpen ? (f+1)%3 : f%3 ]` (TAA history) | `view+0x6a0` = **UpscaleOut** |
  | +0x20 | depth = `0x14255ce10(view)` = `[view+0x628]+0xa0` (R32F depth copy) | 0 |
  | +0x28 | blend = `taa+0x185 ? 0.0 : taa+0x180` | 0 |
  | +0x2c..+0x38 | rect L,T,R,B (float z `listctx+0x5c..0x68`, TAA `[rsp+0x30]`, `0x142392252..0x142392272`) | rect L,T,R,B (float z `listctx+0x5c..0x68`) |
  | +0x3c | 0 | `renderer+0xb8 + renderer+0xac` (driver + user sharpness) |

- **EvalSS `0x142383cf0`**, **OBSERVED**:
  ```
  SetI Rect.X=(int)d[0x2c], Rect.Y=(int)d[0x30], Rect.W=(int)(d[0x34]-d[0x2c]), Rect.H=(int)(d[0x38]-d[0x30])
  comiss 0.0,[d+0x28]; setae bpl            ; 0x142383db4..0x142383dd1  -> Reset = (blend <= 0)
  SetD3d12Resource Color=[[d+08]+0x60]+0x20, MotionVectors=[[d+10]+0x60]+0x20, Output=[[d+18]+0x60]+0x20, PreviousOutput=0
  SetI Reset=bpl; SetF MV.Scale.X=1, MV.Scale.Y=1, MV.Offset.X=0, MV.Offset.Y=0
  SetD3d12Resource Depth=[[d+20]+0x60]+0x20   ; depth z +0x60 (SRV native)
  EvaluateFeature_C(list, ctx+0x38, params, NULL)
  ```
  - Bez jitteru a bez exposure. odt1 „MV.Scale=1.0“ = **CONFIRMED**.
- **EvalISP `0x142383990`**: `Color`, `Output`, `SetF Sharpness=d[0x3c]`, a jen pro `W≠0` `SetUI Rect.X/Y/W/H`. Pak `EvaluateFeature_C(list, ctx+0x40, …)`. odt1 („Sharpness se nastavuje jen na DLISP evaluate“) = **CONFIRMED**.
- **Reset (otázka 2)**, **OBSERVED**:
  - Hodnota `taa+0x185` se kopíruje do `[rbp+0xa0]` na začátku TAA draw (`0x14239231a`). Když je nastavená, volá se `0x142393360` (`0x14239233a`), která ji vynuluje (`0x14239353f`). Capture enqueueru má `+0x38 = &[rbp+0xa0]` (`0x142392f7e..0x142392f85`) a enqueue z něj čte (`0x1423915c0..0x1423915d2`).
  - Zápisy `taa+0x185 := 1`:
    - `0x1423920fe`: konec `0x142391f30`, tedy přestavba history. Volá se z ctoru TAA unitu a z late update při `renderer+0x1f560≠0`.
    - `0x1423932c2`: late update `0x142393260` při `renderer+0x1fc≠0`. `renderer+0x1fc` = „změnil se vlastní scale v tomto snímku“, zapisuje ho `0x142298345` ve frame update `0x1422980b0`.
    - `0x14262ca09`: `al = ([[x+0x278]+0x198]+0x3f ≠ 0)`, kopie property. Význam UNKNOWN, kandidát na SDL/cutscénu.
    - `0x1406080f0/0x1406083e0/0x1406087e0`: vlastní TAA unity UI scén (guild card), ne hlavní view.
  - **Hranice negativního hledání:** všechny zápisy byte `+0x185` v celém .text (13 míst). V TAA kódu (`0x142391*–0x142393*`) ani v kameře (`0x1451c4400`) jsem žádný „camera cut“ nenašel. `fBlendRate` (`taa+0x180`) zapisuje přes 90 funkcí s generickým offsetem; zápis 0 při střihu jsem nedokázal.
- Gargauth posílá vždy `Reset=0`: `dlss.rs:358`, zdroje `gpu.rs:1394, 2078, 2123, 2246` (`reset: false`).

### 2.5 SS cesta a null-depth pád (otázka 3)
- **Vznik pádu**, **OBSERVED**. Planner bariér `0x14229cee0` (jump table `0x14229eccc`, case 0x17 = `0x14229e848`):
  ```
  0x14229e851 mov rbx,[r15+0x20]        ; desc
  ... colour  [rbx+8 ]->+0x60, state 4  (0x1422927f0)
  ... MV      [rbx+10]->+0x60, state 4
  0x14229e883 mov rdx,[rbx+0x20]
  0x14229e892 mov rdx,[rdx+0xf0]        ; depth-view native  <-- null pro R32F copy
  0x14229e899 call 0x1422927f0          ; 0x1422927f2: mov r10,[rdx+0x70]  -> AV
  ... output  [rbx+18]->+0x60, state edi (0x400 při vstupu do smyčky = UAV)
  ```
  - Stavy převádí `0x14259ba50`: herní 4 → `D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE (0x40)`, herní 0x400 → `UNORDERED_ACCESS (8)`.
  - Pro ISP (sub 4) planner zpracuje jen colour a output.
  - Obecný planner bindingů `0x142292610` naopak bere `+0x60` a na `+0xf0` přechází jen u typu 6 (`0x14229268b..0x14229269a`).
- **Proč je `+0xf0` null**, **OBSERVED**:
  - `0x14239c150` (vytvoření native): vytvoří native do `+0x60`. Pokud je typ `tex+0x58 == 6` (depth), přesune ho do `+0xf0` a `+0x60` vynuluje (`0x14239c546..0x14239c557`).
  - Typ 6 dostane jen textura s flagem 0x40000 (`0x14239b7fe..0x14239b83e`).
  - Depth copy `targets+0xa0` vzniká v `0x14269dcb3` s formátem 0x10 a flagy 0x600000, tedy jako typ 2 (colour): `+0x60` je nastavené, `+0xf0` null.
  - Typ-6 depth (např. RTDepth, formát 0x11 → R32_TYPELESS) má naopak `+0x60` null, a EvalSS by pak padal na `[null+0x20]`.
  - **Závěr:** herní DX12 SS cesta je bez aliasu vždy chybná.
- **Release**, **OBSERVED**: `0x14239dbb0` uvolňuje `+0x78`, `+0x80`, `+0xf0` (`0x14239dd66..77`) i `+0x60` (`0x14239dd7e..95`), každý zvlášť přes `0x142554f90` (`lock xadd [rcx+8],-1`; při 1 → `jmp [vtbl]`). Engine sám umí aliasovat oba sloty s AddRef `0x142553fe0` (`lock inc [rcx+8]`) v clone větvi `0x14239c1f4..0x14239c22c`.
- **odt1 `game.cpp:175-209`** (alias `+0xf0 := +0x60`, `InterlockedIncrement(native+8)`, CAS, při prohraném CAS decrement) = **CONFIRMED**. Komentář „game's own copy command falls back to the colour native“ = **PARTIAL**: obecný planner bere primárně `+0x60`, `+0xf0` jen u typu 6.
- **Gargauth `hijack.rs:295-305` a `1065-1070`** („depth target fetched through a call that cannot be predicted… can be null“) = **REFUTED/PARTIAL**:
  - Volání je deterministické: `0x14255ce10(view) = [view+0x628]+0xa0`, a depth target null není.
  - Null je až jeho `+0xf0` native.
  - Pád nastává v planneru `0x1422927f2`, ne v „shared resource-state code that names nothing“ v obecném smyslu.
- **Gargauth `hijack.rs:92-111`** (SS enqueue binduje `node+0x1a8` bez kontroly; `0x142391f30` ho alokuje jen při aktivním DLSS, jinak ho nechá být) = **CONFIRMED**:
  - `0x1423914b4 call 0x1423d0130(list, "rwOutput", taa+0x1a8)`;
  - `0x142392059..0x14239207e` testují `renderer+0xa5`, `+0xb4&1`, `device+0x42718`; při nesplnění skok na `0x1423920f1` přeskočí zápis do `+0x1a8`.
- **odt1 crash `0x14229268b`** (přepnutí type 2→1) = **CONFIRMED** jako null textura v obecném planneru bindingů: `mov rbx,[r14+rax*8]; mov rdx,[rbx+0x60]` s `rbx=null`, odpovídá nealokovanému `taa+0x1a8`.
- **Vstupy SS**, **OBSERVED**:
  - colour = hlavní colour před TAA, R11G11B10_FLOAT (formát history `0x22` → 26). Tabulka engine formát → DXGI je `0x14351c990`, aplikuje ji `0x14239d84b`.
  - MV = `taa+0x1a8`, formát 5 → **R32G32_FLOAT (16)**, velikost = render (`cfg+0x198 * cfg+0x204`, `0x142391f5c..0x142391fa5`).
  - depth = `targets+0xa0`, formát 0x10 → **R32_FLOAT (41)**, velikost = view.
  - output = TAA history slot.
  - Dense velocity počítá compute `TemporalAA2_ResolveDenseVelocity` (`0x142391447`). Binduje jen `rwOutput`, dispatch `(w+7)/8 × (h+7)/8` (`0x142391503`). Ostatní vstupy dědí z TAA bindingů: tGBuffer4, tDepthMap, tGBuffer1, `CB_DL{time, customUInt, screenSize, viewMatrix, projMatrix, viewProjMatrix, crViewProjMatrix, prevViewProjMatrix}`, které TAA nahraje jen při ctx≠0 (`0x142392a49..0x142392ec3`). Konvence MV (pixely/NDC, znaménko) je UNKNOWN; DLSS 1 jim dával `MV.Scale = 1`.

### 2.6 tGBuffer4 vs dense velocity (otázka 4)
- tGBuffer4 vzniká v GBuffer setu `0x14257c440` (`view+0x620`) jako slot 4 (`+0x58+4*8 = +0x78`). Vytváří ho `0x14239b7b0(…, fmt=0x26)` (`0x14257c6e5`) → R32_UINT (42), packed. Gargauth `mv.rs:3` = **CONFIRMED**. TAA ho binduje vždy (`0x142392826 0x14257ca70(gbuf,4)`).
- Dense velocity `taa+0x1a8` vzniká jen v SS režimu (§2.5). Vyrábí ho engine z tGBuffer4 a depth ve stejném TAA průchodu, takže odpadá Gargauthův dekód `mv.rs` (**INFERENCE**: obsah zatím neověřen).

### 2.7 Pořadí passů (otázka 5)
- Klíč příkazu: `0x1423cabf0` → `key = (ctx+0x154 & 0x1f) << 27 | ((ctx+0x15c>>5) + (ctx+0x154>>5)) & 0x7ffffff`. Tedy primárně **pass** (5 bitů, `0x1423cdf70`), sekundárně **prio** (`0x142585410`: `pass = T[id]`, `prio = T[id+1]`, `T = 0x14354cb70`). **OBSERVED** výpočet; řazení podle klíče = **INFERENCE**.
- Hodnoty: TAA draw `id 0x141` → (15,2) (`0x142392235/0x14239227e`). Filtry mají výchozí `pass = word +0x138`, `prio = +0x13c` z ctorů (název třídy je řetězec těsně před vtable):

  | pass,prio | zdroj | pass |
  |---|---|---|
  | 14,99 | `uBloomFilter` ctor `0x1424cb5ff..0x1424cb60b` | bloom |
  | 14,100 | `uFFTBloomFilter` ctor `0x1424f5d43` | FFT bloom |
  | 15,0 | `0x1424d08ac id 0x13b` | AutoExposureApply |
  | 15,1 | `uHazeFilter` ctor `0x1424c82b4` | haze |
  | **15,2** | TAA + **SS enqueue** (`0x142392f89`) | TAA / EvalSS |
  | 15,16 | `uDOFFilter` `0x14251876c`, `uBokehFilter` `0x14251c05f` (DOF draw `0x14251979f` bere pass z `+0x138`) | DOF |
  | 15,100 | `uMotionBlurFilter` `0x1424fbaea`, `uMotionBlurReconstructionFilter` `0x1424c9762..c3` | MB |
  | 15,0x7fffff1 | `0x1424d1649 id 0x144` → `ToneMappingApply_ColorGrading` (`0x1424d174a`) | tonemap + LUT |
  | 15,0x7fffff3 | `0x142393a9f id 0x14a` (FXAA) | FXAA |
  | **16,0** | scene pass `0x1423c69fd` (`edi=0x10`), `and [r14+0x154],0x1f` | **upscale point: EvalISP / TemporalUpsample / prostý upscale** |
- ISP colour = `[view+0x420]+0x58` (`0x1423c6b9a..0x1423c6ba6`) je **týž objekt**, který TAA čte jako `tBaseMap` (`0x142392200..0x142392210`). TAA do něj zapisuje svůj výsledek zpět (`0x1423917a0` → CopyRect history → `[[taa+0x158]+0xb0]`, **INFERENCE**). Po ISP engine přepne řetězec na UpscaleOut (`0x1423c7811 mov rdi,rsi`).
- Podmínky upscale pointu, **OBSERVED** (`0x1423c7691..0x1423c76f1`): `renderer+0xa6 && (+0xb4&2) && device+0x42718 && view+0x6a8 && renderer+0x204 ≠ 1.0 && ctx && view+0x6a0`. Režim TemporalUpsample je bit 23 `view+0x58` (`0x1423c39cc..d4`); v něm TAA draw vůbec neběží (`0x1423921ae`).
- `renderer+0xa5/+0xa6` zapisuje jen ctor rendereru, `mov word [rsi+0xa5],0x101` (`0x1422908b7`; hledány všechny zápisy s bází renderer nebo `rcx`). Prakticky jsou to konstantní 1 (**INFERENCE**). odt1 „+0xa5 DLSS on / +0xa6 DLISP on“ = **PARTIAL**: nejde o uživatelský přepínač, skutečné brány jsou `+0xb4` a `device+0x42718`.
- **Gargauthův slot route** (`hijack.rs:1294-1303`, `gpu.rs:1236+`) čte `desc+0x08`, tedy hlavní colour po DOF/MB/tonemap+grading (a po FXAA, je-li zapnuté), takže výsledky **nezahazuje**. DLSS tím ale dostává tonemapovaný (display-referred) a rozmazaný (DOF/MB) vstup; odt1 naměřil max ≈1.4. Starý route `resolve_at_pass` je skutečně zahazoval. Gargauthův komentář `gpu.rs:2059-2066` = **CONFIRMED**.
- AA apply `0x14043dcb0` při `dlssUsable` (`0x1404431f0`): flags 2 → AA=3 (TAA+FXAA), flags 3 → AA=1 (TAA), flags 1 → beze změny (`0x14043dd27..0x14043dd36`). odt1 `CLAUDE.md:237-241` = **CONFIRMED** (formulováno přes DLSSMode). **Riziko:** s Gargauthovým flags=2 a herní volbou NVIDIA DLSS=On engine vnutí FXAA na (15,0x7fffff3), tedy před ISP.
- Image Quality `0x14043eeb0`: při `dlssUsable()` nebo `0x140441520(opt)` se pozice nastaví na 2. Pozice 0 → 0.75 (`0x142f74ac8`), 1 → 0.9 (`0x142f18a94`), 2 → 1.0, 3/4 → dynamic (`dl=1`, `0x14229bf30`). Gargauth `hijack.rs:124-135` = **CONFIRMED**.

### 2.8 Early-load „type 2“ (otázka 7)
- Driverův callback s 1.x core vrací DLSSMode 2 → `flags=2` (`0x1423840d6`) → `renderer+0xb4=2`. TAA pak nemá ctx (`0x142392a28 test byte [rax+0xb4],1`), SS enqueue se ukončí na `0x142391406` a `0x142391f30` nealokuje `+0x1a8`.
- Přepnutí na bit0 za běhu bez přestavby historie (`renderer+0x1f560`) vede na null bind `taa+0x1a8` a pád na `0x14229268b`. odt1 `CLAUDE.md:124-138` = **CONFIRMED**.
- Gargauth: launcher + IAT hook `GetProcAddress` (`hooks.rs:103-165`) běží od startu procesu, takže herní `Init`, `GetParameters` a `ReleaseFeature` jdou vždy přes něj. Problém odt1 (pozdní adopce kontextu) tím odpadá. Type 2 = flags 2 Gargauth volí **záměrně** (`hijack.rs:1071`). `hook_ss_enqueue` (`hijack.rs:1308-1354`) chrání jen přechod na bit0.

## 3. Doporučené změny pro Gargauth

1. **Reset** (otázka 2) — `hijack.rs:1308-1354` (`hook_ss_enqueue`), `dlss.rs:358`, `gpu.rs:1394, 2078, 2123, 2246`.
   - Enqueue se volá při **každém** TAA draw i s flags=2 (`0x142392f89`). Přečti v něm `flag = **(u8**)(node_list+0x38)` (kopie `taa+0x185` před vynulováním) a `blend = *(f32*)(node+0x180)`.
   - Ulož `RESET_PENDING |= flag != 0 || blend <= 0`. Při flags=2 vrať 0 bez logu `skipped`: ctx je null a originál by stejně skončil na `0x142391406`.
   - V `run_dlss`/`note_frame` reset spotřebuj a předej do `Frame.reset`.
   - Navíc ber `cfg+0x1fc` (změna scale v tomto snímku), který `follow_configured_ratio` už stejně čte z cfg (`hijack.rs:843`).
2. **Deterministická depth i bez SS** — `gpu.rs:2701` (`pick_depth`).
   - Capture `+0x30` = `&view`, depth copy = `[[view+0x628]+0xa0]+0x60 → +0x20` (R32_FLOAT, 41).
   - V `hook_ss_enqueue` ji zaznamenej a v `pick_depth` dej přednost tomuto zdroji před DSV heuristikou, s porovnáním a logem (§4 E3).
3. **SS cesta jako náhrada DSV heuristiky + MV dekódu** (otázka 3), postup:
   1. `hook_optimal` (`hijack.rs:1064-1072`): `flags = 3` pro ratio < 1 a `1` pro 1.0. Odpověď musí platit už při první otázce engine (`0x1422994a0`), před stavbou TAA unitů, jinak se `taa+0x1a8` nealokuje. Případně vynutit přestavbu změnou ratio (`cfg+0x200`), která nastaví `renderer+0x1f560`/`+0x1fc`.
   2. Alias depth (odt1 `PatchViewDepth`): v `hook_ss_enqueue` **před** voláním originálu, protože planner běží později na render threadu. `d = [[view+0x628]+0xa0]`; když `[d+0xf0]==0 && [d+0x60]≠0 && [[d+0x60]+8]>0`: `lock inc [[d+0x60]+8]`, `CAS([d+0xf0], 0, [d+0x60])`, při neúspěchu `lock dec`.
   3. `hook_eval_supersampling` (`hijack.rs:1128-1136`): uložit `Color=[[d+08]+0x60]+0x20`, `MV=[[d+10]+0x60]+0x20` (R32G32F), `Depth=[[d+20]+0x60]+0x20`, `Reset=(d[0x28]<=0)` a rect `d[0x2c..0x38]`. Na list pak nahrát `CopyResource(Color→Output)` („copy_through“), protože engine Output (history) zkopíruje zpět do hlavního colour a ten projde DOF/MB/tonemap až do ISP. Stavy při vstupu: inputs `NON_PIXEL_SHADER_RESOURCE`, output `UNORDERED_ACCESS`. Copy proto potřebuje tranzice tam i zpět.
   4. `hook_eval_image_signal`: DLSS 4 z uloženého MV a depth. MV scale a znaménko určit experimentem E2. Jitter zůstává z kamery.
   5. Pak lze vyřadit `mv.rs` dekód i `pick_depth`. `hook_ss_enqueue` guard ponechat.
   6. Vedlejší efekt flags=3: AA apply nastaví TAA-only, tedy bez FXAA (`0x14043dd2c..31`). Late update maže `mSharpen` `+0x187` (`0x142393340..4a`).
4. **Init argumenty** — `ngx.rs:545, 560`: volat `target(app_id, path, device, version, 0, 0)` místo `a4, a5`. `a4` je adapter LUID (§2.2) a s verzí 21 hrozí jeho interpretace jako pointeru.
5. **Komentáře** — `hijack.rs:295-305`, `1065-1070`: depth target je deterministický (`view+0x628 → +0xa0`), null je jeho `+0xf0`. Přesné místo pádu je `0x1422927f2` (planner `0x14229cee0`, case 0x17).
6. **HDR flag na slotu** — `dlss.rs:29-30, 182-199`: na slotu je vstup po `ToneMappingApply_ColorGrading`. Ověřit, zda `IsHDR` platí (E5).
7. **FXAA riziko** — `hijack.rs:1071`: s flags=2 a herní volbou DLSS=On vnutí engine TAA+FXAA a FXAA poběží před ISP. Buď flags=3, nebo hlídat `scene+0xe9e5` (FXAA byte) jako odt1 `HoldTaaOnly`.

## 4. UNKNOWN + runtime experimenty

| # | Neznámá | Experiment (Gargauth) | Co rozhodne |
|---|---|---|---|
| E1 | Spouští `taa+0x185` i střih kamery nebo změna oblasti? (`0x14262ca09` = property `+0x3f`) | `hook_ss_enqueue`: logovat `**(u8**)(capture+0x38)`, `*(f32*)(node+0x180)`, `cfg+0x1fc`, `cfg+0x1f560` a frame # při každé nenulové hodnotě. Projít cutscénu, změnu oblasti a fast travel. | Zda se flag objevuje při střizích. Pokud ne, je třeba vlastní detekce (skok view matice). |
| E2 | Konvence `taa+0x1a8` (pixely vs. NDC, znaménko, jitter) | S flags=3 a aliasem: v `hook_eval_supersampling` vyčíst MV (readback 1 snímku) při posunu kamery a porovnat s dekódem `mv.rs` na tGBuffer4. | Výsledné `MV.Scale` pro DLSS 4. |
| E3 | Je depth copy `targets+0xa0` plněná každý snímek a v jaké konvenci (reversed / lineární)? | V `hook_ss_enqueue` vzít resource `[[[view+0x628]+0xa0]+0x60]+0x20`, logovat desc (očekáváno R32_FLOAT 41, render size) a porovnat pixel se `pick_depth` DSV. | Náhrada `pick_depth` a `DepthInverted`. |
| E4 | Načítá `nvngx.dll` (cíl hry) modul `_nvngx.dll`? OTA 1.2.110 pro app id `0x5f8a85f` | Po `Init` vypsat `GetModuleFileNameW` pro `nvngx.dll`, `_nvngx.dll`, `nvngx_dlss.dll`, `nvngx_dlisp.dll` (EnumProcessModules) a version resource `nvngx_dlss.dll` po `CreateFeature`. | Zda `ngx::core()` hledá správné jméno a zda driver nevnutí OTA snippet místo `mhw-dlss\nvngx_dlss.dll`. |
| E5 | Rozsah hodnot colour na ISP slotu (HDR vs. display-referred) | `resolve_at_slot`: jednorázový readback max/median luminance `desc+0x08`. | Nastavení `expert_dlss_hdr`. |
| E6 | Pořadí ve skutečném snímku (SDL může přepsat pass/prio filtrů) | `pathdump` (`pathdump.rs:34-38`) s DOF a MB zapnutými: pořadí shader jmen mezi TAA dispatchem a upscale slotem. | Potvrzení tabulky v §2.7 pro konkrétní mapy. |
| E7 | Čte legacy `NVSDK_NGX_D3D12_Init` při verzi 21 i 5. argument? | Jednou zavolat Init s `a4=0, a5=0` a porovnat výsledek a capability blok s dnešním stavem (`a4=LUID`). | Bezpečnost doporučení 3.4. |
| E8 | Hodnota `device+0x2da` (brána NGX init) | Log bytu v `observe_game_init` (device ptr ⇒ herní device objekt není přímo dostupný; číst `[renderer+0x78]+0x2da`). | Kdy hra NGX vůbec neinicializuje. |

## 5. Mapa adres

| Adresa | Význam |
|---|---|
| `0x142383ef0` | GetOptimalSettings(out, ctx, w, h, mode) → `{scale, flags, sharpness}` |
| `0x142384420` | CreateFeatures(ctx, outW, outH, mode, _, list); feature 1 (SS) / 9 (ISP) |
| `0x142383cf0` | EvalSS(desc, list) |
| `0x142383990` | EvalISP(desc, list) |
| `0x1423842a0` / `0x142384120` | NGX init D3D12 / D3D11 (app id 0x5f8a85f, L"DLSS", ver 0x12) |
| `0x142384400` | ctx „has feature“ (`ctx+0x38 || ctx+0x40`) → `device+0x42718` |
| `0x14273f230` | export `NVSDK_NGX_D3D12_Init` (statická lib rel_1_3) |
| `0x14273fe90` | načtení NGX core (`nvngx.dll`: exe dir → QAI → registry) |
| `0x1451d8808..0x1451d883f` | NGX D3D12 tabulka (Init, Shutdown, Scratch, GetParams, Create, Evaluate, Release) |
| `0x142e13118` | IAT `KERNEL32!GetProcAddress` |
| `0x14273e370..0x14273e8f0` | `NVSDK_NGX_Parameter_{Get,Set}*` wrappery (sloty vtable v §2.1) |
| `0x1423913f0` | SS enqueue (TAA draw capture: `+0` &ctx, `+8` taa, `+0x10` &listctx, `+0x18` &rect, `+0x20/+0x28` &history idx, `+0x30` &view, `+0x38` &reset byte) |
| `0x142392150` | TAA draw (`uTemporalAAFilter2` vtable `0x14351bda8` +0x60), prio id 0x141 = (15,2) |
| `0x142391f30` | TAA history (re)build + alokace `taa+0x1a8` |
| `0x142393260` / `0x142393360` | TAA late update (nastaví `+0x185` při `renderer+0x1fc`) / reset historie (maže `+0x185`) |
| `0x1423c3790` | scene pass; upscale point `0x1423c6bfa..0x1423c780c` (pass 16) |
| `0x14259dae0` | DX12 translátor příkazu 0x17 (tabulka `0x14354e4c0`) |
| `0x14259ba50` | aplikace bariér z `cmd+8` |
| `0x14229cee0` | planner bariér (case 0x17 `0x14229e848`; čtení depth `+0xf0` na `0x14229e892`) |
| `0x1422927f0` | zápis stavu do planneru (pád na `0x1422927f2` při null native) |
| `0x142292610` | obecný planner bindingů (pád odt1 `0x14229268b`) |
| `0x14239c150` | vytvoření native textury (typ 6 → `+0xf0`) |
| `0x14239dbb0` | release textury (`+0x78/+0x80/+0xf0/+0x60`) |
| `0x142554f90` / `0x142553fe0` | native Release (`lock xadd [+8],-1`) / AddRef (`lock inc [+8]`) |
| `0x14255ce10` | `[view+0x628]+0xa0` (depth copy getter) |
| `0x14257ca70` | `[gbufset+0x58+i*8]` (tGBuffer1 = i 1, tGBuffer4 = i 4) |
| `0x14255ba70` | view ctor (`+0x620` GBuffer set `0x14257c440`, `+0x628` targets `0x14269d8f0`, `+0x6a0` UpscaleOut) |
| `0x14351c990` | tabulka engine formát → DXGI (`tex+0x90 → tex+0x98`) |
| `0x14354cb70` | tabulka priorit (hash, pass, prio) pro `0x142585410` |
| `0x1423cabf0` | push příkazu se sort klíčem (pass<<27) |
| `0x14043eeb0` / `0x14043dcb0` / `0x1404431f0` | Image Quality / AA+DLSS apply / dlssUsable |
| `0x1422994a0` | uložení OptimalSettings do `renderer+0xb0..0xb8`, ratio `+0x200/+0x204` |
| `0x1422980b0` | frame update; `renderer+0x1fc` = změna scale v tomto snímku |
| `0x1451c4480` | renderer/cfg (`+0x78` device, `+0xa5/+0xa6` = 1, `+0xa8` mode, `+0xac` user sharpness, `+0xb0/+0xb4/+0xb8` optimal, `+0x1f0/+0x200/+0x204`, `+0x1fc`, `+0x1f450` main view, `+0x1f560`) |
| `device+0x42700 / +0x42718` | DLSS ctx (0x48 B: `[0]` params, `+8` D3D11 ctx, `+0x10` CS, `+0x38` SS, `+0x40` ISP) / ready |
| `taa+0x150/0x158, +0x168[3], +0x180, +0x185, +0x187, +0x19f, +0x1a8` | colour, history, fBlendRate, history-invalid, mSharpen, „tDepthMap = depth copy“, dense velocity |
| `0x1451c6c28` | NGX init error (1 init, 2 SS n/a, 3 ISP n/a) |
| `0x1451cc080` | DX12 flag |
