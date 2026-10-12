# ReFramePresent: snímek, Present, pacing, UI

Cíl: `MonsterHunterWorld.exe` build 15539686 (PE ts `0x66d13cb6`), statická RE přes offline capstone index (`re/tools/g`) a skripty v `re/scratch/ReFramePresent/` (`rfdis.py` disasembluje libovolnou VA, `rfref.py` hrubou silou hledá RIP-relativní reference, `imports.json` obsahuje mapu IAT). Ghidra server během práce neběžel, takže pseudokód tu není a všechno je přímo z disasm.
Značky: **OBSERVED** (přímo v disasm), **INFERENCE** (odvozeno), **UNKNOWN**. Verdikty k cizím tvrzením: **CONFIRMED / REFUTED / PARTIAL**.

Objekty používané v reportu:
- `sRender` = `*(0x1451c4480)`. Zapisuje se v `0x142290dec`, DTI jméno je `sRender` (`0x14350d280`).
- `dev` = `sRender+0x78`, tj. renderovací device objekt (cDeviceDX12). Viz `0x14229b06f mov rcx,[rsi+0x78]`.
- `sMain` = `*(0x1451c4148)`. Zapisuje se v `0x14225c3d1`; `[+0x58]` má default 60.0f (`0x14225c472 mov dword [rdi+0x58],0x42700000`).
- `sGUI` = `*(0x1451c4640)` (ctor `0x1422a8b40` s DTI `0x1451c4648` „sGUI“), `sScene` = `*(0x1451c4368)`, `sPrimitive` = `*(0x1451cc1f8)`, `sCamera` = `*(0x1451c4400)` (DTI `0x1451c4408` „sCamera“), `sMovie` = `*(0x1451c4af0)` (DTI `0x1451c4b30` „sMovie“).
- DX12 flag: byte `0x1451cc080`. Čte ho `0x1424e9630` (isDX12) a `0x1424e9620` (vrací `!flag`), zapisuje jen `0x1424e9990`, volaný z device ctoru `0x1423a1921`.

---

## 1. Otázky a verdikty

| # | Otázka | Verdikt (stručně) |
|---|---|---|
| 1a | Vytvoření swapchainu | **OBSERVED.** `IDXGIFactory5::CreateSwapChainForHwnd(mCmdQueue=[dev+0x20], hwnd, desc)` v `0x1423a48e0` (`0x1423a63ab`, fallback `0x1423a66aa`). Desc: B8G8R8A8_UNORM (0x57), BufferCount=3, FLIP_DISCARD (4), STRETCH, Usage 0x20, Sample {1,0}, Flags `0x42` = ALLOW_MODE_SWITCH\|FRAME_LATENCY_WAITABLE_OBJECT, k tomu `\|0x800` ALLOW_TEARING, pokud ho `CheckFeatureSupport` podporuje. |
| 1b | SetMaximumFrameLatency | **OBSERVED.** `SetMaximumFrameLatency(3)` (`0x1423a6553`), hned potom `GetFrameLatencyWaitableObject` → `[dev+0x10F0]` (`0x1423a657a`). Handle se pak už nikdy neobnovuje. |
| 1c | Znovuvytvoření / ResizeBuffers | **OBSERVED.** CreateSwapChainForHwnd je jen v init cestě (`0x1423a0950` ← `0x1422994a0` ← `0x142290880`). Rozlišení, fullscreen i HDR jdou přes `ResizeTarget` / `SetFullscreenState` / **`ResizeBuffers`** na stejném swapchainu (`0x14239f810`). Gargauthovo „swapchain se ničí a znovu vytváří uvnitř Present“ (`swap.rs:23-28, 561-563, 996-998`): **REFUTED**. Uvnitř Present (`0x1423a9ce0`) se volá jen ResizeBuffers, a to po ztrátě exkluzivního fullscreenu. |
| 2a | Sémantika wait helperu `0x1423ade30` | **OBSERVED.** `if (dev+0x10D9 && !dev+0x10DA) { if (WaitForSingleObjectEx(dev+0x10F0, 1000, TRUE) == WAIT_TIMEOUT) dev+0x10DA = 1; }`. `+0x10D9` = „aspoň jednou se prezentovalo“: nastavuje se na 1 po **každém** volání `Present` (`0x1423aa1ed`). |
| 2b | Trvalost latche `+0x10DA` | **OBSERVED.** Existují jen dva zápisy: ctor (`0x1423a0caf` = 0) a helper (`0x1423ade6a` = 1). Latch se tedy **nikdy nenuluje**. Jeden 1s timeout vypne waitable wait natrvalo, do konce procesu. |
| 2c | Umístění ve snímku | **OBSERVED.** Úplně první akce iterace hlavní smyčky (vlákno vytvořené `CreateThread` s procedurou `0x1422e4ae0`). Pořadí: `0x14229b020` → helper (ret `0x14229b078`) → teprve pak app update `0x141af2b60` (limiter, kick renderu, sim, frame routine, Present). Běží jen pro DX12. |
| 2d | Důsledky přeskočení pro DLSS-G/Reflex | **INFERENCE.** Přeskočení odstraní druhý, nekoordinovaný sleep, který přichází *po* Reflex sleep a *po* Gargauthově `SIMULATION_START`, takže dnes nafukuje měřenou „simulaci“. Latenci pak drží fence throttle enginu (GPU dokončí n-1 před Present n) plus Reflex. Bez Reflexu (nebo bez limiteru) by CPU běželo napřed až po fence limit. Doporučení: při aktivním SL nastavit latch `dev+0x10DA=1`, což je datový zápis bez detouru (§3, R1). |
| 3a | Vlákna | **OBSERVED.** Vlákno hlavní smyčky dělá sim, kick renderu, čekání na render, ExecuteCommandLists (ECL), Signal a Present. „Rendering Thread“ (`0x14229aba0`) jen nahrává command listy, pak `SetEvent(done)`. Další jsou „RenderingSub Thread %d“. **Present i jediné per-frame ECL na grafickou frontu běží na vlákně hlavní smyčky, ne na render threadu.** |
| 3b | Kam PCL markery | **INFERENCE.** Gargauthovo pořadí je strukturálně správné: konstanty a tagy se nastavují na render threadu, ten musí doběhnout (`done` event), než hlavní vlákno udělá ECL a Present. Takže `PRESENT_START` vždy přijde po konstantách stejného snímku (MHWFG to vynucuje odložením, tady to platí samo, **CONFIRMED**). Doporučení: Reflex sleep + `SIMULATION_START` přesunout na místo herního waitu, případně wait vypnout latchem (R1, R2). |
| 4 | Kdy frame routine skončí bez Present | **OBSERVED.** (a) `mParallelRendering`(+0x93) && pending(+0x90) && `mFrameSkip`(+0x91) a render thread nesignalizuje `done` do 1000/fps ms → `ret` z `0x142298176`. Snímek ale **nepropadne**: pending zůstane, další kick se přeskočí a týž snímek se prezentuje v další iteraci. (b) `dev+0x1482` (init) → Present přeskočen. (c) Present vrátí OCCLUDED/chybu → `sRender+0x68cbd \|= 1`. (d) Neaktivní aplikace → `Sleep(1)`, nevolá se nic. Gargauthovo „snímek může začít a nikdy se neprezentuje“ (`fg.rs:17-20`): **PARTIAL**. Early return potvrzen, snímek se ale jen zpozdí. |
| 5a | Kam kreslí uGUI | **OBSERVED.** `uGUI::draw 0x142353030` nebinduje RT (nevolá `setRenderTarget 0x1423ce3c0`), jen nastaví viewport (`0x1423d0330`) a kreslí přes vfunc +0x128/+0x130. Gargauth `hudless.rs:6-9`: **CONFIRMED**. |
| 5b | Vůči tonemapu / CopyToBackBuffer | **OBSERVED + INFERENCE.** `System_CopyToBackBuffer 0x1422979f0` je poslední pass `0x14229a080`, běží po `sCamera::draw` (viewporty, a tedy i GUI) a po `sMovie`. Jeho CB `CBSystem` obsahuje jen `fSourceMipLevel`, `fGammaCorrect=1/[sRender+0x1c8]` a `fGammaCorrectEx=[sRender+0x1cc]`, technika je `System_CopyToBackBuffer` (SDR) nebo `…PQSingle` (HDR). Jde tedy o **výstupní kódování (gamma/PQ), ne o tonemap**. Tonemap je scénový filtr (`uToneMapFilter`, `CBToneMapping`). Tvrzení Gargauthu „tonemap v compositu“ (`hudless.rs:61-65`, report §2.3): **REFUTED** (INFERENCE z konstant a jmen). |
| 5c | UI-free post-tonemap bod | **INFERENCE.** Samostatný UI-free resource neexistuje, existuje jen *okamžik*: obsah scénového targetu (`[sRender+0x1f450]+0x4a0`, zdroj `tBaseMap` CopyToBackBufferu) těsně před prvním GUI draw. Ten je post-tonemap a pre-gamma. HUD-less tedy stačí převést tou samou výstupní funkcí, kterou používá CopyToBackBuffer (R6). |
| 5d | Přesměrovat UI do vlastního targetu (UIColorAndAlpha) | **INFERENCE: nepraktické.** UI se míchá destruktivně do alpha-less targetu se sdíleným depth (`+0x568`, world-space nameplaty). Přesměrování by vyžadovalo znát blend stavy GUI (UNKNOWN, E5) a znovu složit obraz. Doporučeno jen HUD-less. |
| 6 | HDR výstup | **OBSERVED.** `ResizeBuffers(count, w, h, R10G10B10A2_UNORM (0x18), flags)` + `SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 = 12)`, po `CheckColorSpaceSupport(12)` (`0x1423a22f0`, volá ji `0x14239fc0c`). SDR: B8G8R8A8_UNORM + `SetColorSpace1(0)` (`0x1423a23d0`). Shader: `System_CopyToBackBufferPQSingle`, pokud `sRender+0x21a`. |

---

## 2. Nálezy s důkazy

### 2.1 Swapchain (Q1)

**Factory.** `dxgi.dll` se načítá přes `LoadLibraryExW` a funkce přes `GetProcAddress("CreateDXGIFactory2")` (`0x1423a15c9`→`0x1423a15d0`, uloženo do `[dev+0x2e8]`).
- Volání `CreateDXGIFactory2(0, IID 7632e1f5-ee65-4dca-87fd-84cd75f8838d = IID_IDXGIFactory5, …)` (`0x1423a15e8-0x1423a15fb`, IID na `0x14351cee0`).
- Gargauth `swap.rs:15-17` tvrdí, že engine „žádá IDXGIFactory2“: **REFUTED**. Engine žádá IDXGIFactory5, slot 28 (`CheckFeatureSupport`) je tedy legitimní.
- **OBSERVED:** v DX12 mode-change `0x1423ac040` (`0x1423ac188 call [rax+0x68]` = `IsCurrent`) se při `IsCurrent()==FALSE` factory uvolní a vytvoří znovu přes `[dev+0x2e8]` (`0x1423ac1c1`).

**Desc** (`0x1423a4d98-0x1423a4e02`, `r12=0`, `r13=1`):
```
mov [rbp+0xf8],ecx / [rbp+0xfc],eax          ; Width/Height = [sRender+0x198] (mScreenSize)
mov qword [rbp+0x100],0x57                    ; Format=B8G8R8A8_UNORM, Stereo=0
mov qword [rbp+0x108],r13                     ; SampleDesc {1,0}
mov [rbp+0x110],0x20                          ; BufferUsage = RENDER_TARGET_OUTPUT
mov [rbp+0x114],ebx(3)                        ; BufferCount = 3
mov [rbp+0x118],r12d / [rbp+0x120],r12d      ; Scaling=STRETCH, AlphaMode=UNSPECIFIED
mov [rbp+0x11c],4                             ; SwapEffect = FLIP_DISCARD
mov [rbp+0x124],0x42                          ; Flags = ALLOW_MODE_SWITCH | FRAME_LATENCY_WAITABLE_OBJECT
call [rax+0xe0]                               ; IDXGIFactory5::CheckFeatureSupport(0=ALLOW_TEARING,&[rbp+8],4)
or [rbp+0x124],0x800                          ; když tearing podporován
```
- Create: `0x1423a63ab call [rbx+0x78]` (Factory slot 15 `CreateSwapChainForHwnd`), `rdx=[rdi+0x20]` je **mCmdQueue** (`0x1423a6385`) a odpovídá stringu `0x14351e320` „mDXGIFactory5->CreateSwapChainForHwnd(mCmdQueue.Get(), …)“. Gargauth `queue.rs:15-21` (fronta = 1. argument): **CONFIRMED**.
- `QueryInterface(IID_IDXGISwapChain3 94d99bdb-…)` → `[dev+0x1470]` (`0x1423a63d4`, `0x1423a643e`).
- `[dev+0x1481] = tearing podporován` (`0x1423a6448`), `[dev+0x1482] = 0`, `MakeWindowAssociation(NO_ALT_ENTER|NO_WINDOW_CHANGES|NO_PRINT_SCREEN)` (`0x1423a645b`).
- Smyčka vytváří swapchain pro výstupy, jejichž monitor odpovídá oknu. Fallback se stejným desc (BufferCount 3, FLIP_DISCARD) je na `0x1423a6656-0x1423a66aa`.
- `0x1423a6546-0x1423a657a`:
```
mov edx,r14d(3) ; call [rax+0xf8]   ; IDXGISwapChain2::SetMaximumFrameLatency(3)  (slot 31)
call [rax+0x108]                    ; GetFrameLatencyWaitableObject (slot 33)
mov [rdi+0x10f0],rax                ; HANDLE pro wait helper
```

**Present** (`0x1423a9ce0(dev, syncInterval)`, volá ho jen `0x1423aa290`, a ten zase `0x1422983da`):
1. `EnterCriticalSection(dev+0x1620)`.
2. **GPU throttle:** `target = ring[([dev+0x1e8]+2)%3]` (`0x1423a9d4a-0x1423a9d73`), tj. fence hodnota snímku **n-1**. Kód: `if ([dev+0x10d8]) { fence->SetEventOnCompletion(target,[dev+0x10e0]); WaitForSingleObject(ev, INFINITE); } else spin pause` (`0x1423a9d6c-0x1423a9de8`). `[dev+0x10d8]=1` (`0x1423a710b`, `r12d=1` z `0x1423a6f9e`).
3. Pro každý display se zjistí `GetFullscreenState` (slot 11, `0x1423a9e6e`).
4. `Flags = ALLOW_TEARING(0x200)`, jen když `syncInterval==0 && [dev+0x1481] && !fullscreen` (`0x1423a9ee3-0x1423a9f02`, `0x1423aa1df shl r8d,9`).
5. `if (!fullscreen && [dev+0x26c] /*byl fullscreen*/)` → mode change `0x1423ac040` (DX12), kde se znovu vytvoří DLSS feature na `mDLSSCmdList` + ECL + flush (`0x1423aa07b-0x1423aa1bc`) a `GetBuffer` (`0x1423ac9e0`).
6. `call [rax+0x40]` = `Present(syncInterval=[sRender+0x1f51c], flags)` (`0x1423aa1ea`). Hned potom **`mov byte [rbx+0x10d9],1`** (`0x1423aa1ed`, ještě před `test eax`).
7. `0x087A0001` (OCCLUDED) se nehlásí jako chyba, ale vrací false (`0x1423aa212-0x1423aa227`).

**ResizeBuffers.** DX12 mode change `0x1423ac040` → `0x14239f810`:
- `ResizeTarget` (`[r14]+0x70`), `SetFullscreenState` (`+0x50`, `0x14239f939`), `ResizeBuffers(count=[desc+0x28], w, h, fmt, flags=(waitable<<6)+2 | (tearing?0x800))` (`0x14239fb93`). `desc` pochází z `GetDesc` (string `0x14351d370`).
- Formát předává volající: **0x57** (`0x1423ac4bc mov dword [rsp+0x30],0x57`).
- Vstupy do mode change: Present po ztrátě exkluzivního FS, `0x1423ac560` ← `0x14229b020` (změna nastavení na začátku iterace) a HDR toggle (`sRender+0x1f548 |= 0x4000`, `0x14229b36a`).
- Gargauth `swap.rs:996-998` („ResizeBuffers jen na HDR cestě“): **REFUTED**.
- Hook `hook_resize_buffers` aktualizuje jen `size`, ne `format`, a nevolá `fg::note_reset()` (`swap.rs:1003-1007`). Viz R4.

### 2.2 Wait helper `0x1423ade30` (Q2)

```
0x1423ade36 cmp byte [rcx+0x10d9],0 ; je ret          ; ještě nikdy neprezentováno → nečekat
0x1423ade42 cmp byte [rcx+0x10da],0 ; jne ret         ; latch → nečekat
0x1423ade4b mov rcx,[rcx+0x10f0]                      ; waitable z GetFrameLatencyWaitableObject
0x1423ade52 mov edx,0x3e8 ; mov r8d,1                 ; 1000 ms, bAlertable=TRUE
0x1423ade5d call [0x142e132d8]                        ; KERNEL32!WaitForSingleObjectEx
0x1423ade63 cmp eax,0x102 ; jne ret                   ; jen WAIT_TIMEOUT
0x1423ade6a mov byte [rbx+0x10da],1                   ; latch natrvalo
```
- **Bajty:** 71 B na RVA `0x23ade30` = signatura MHWFG `FG_Hooks.cpp:62-67` bajt po bajtu (**CONFIRMED**, `g bytes 0x1423ade30 71`).
- **Jediný volající:** `0x14229b073` v `0x14229b020`. Kód: `call 0x1424e9630 (isDX12); je skip; mov rcx,[rsi+0x78]; call helper` → návrat `0x14229b078`. MHWFG `kMhwSwapchainWaitCallerReturnRva = 0x229b078`: **CONFIRMED**.
- **Trvalost latche:** zápisy do `+0x10DA` existují jen v ctoru (`0x1423a0caf mov [rdi+0x10da],al`, `eax=0` z `0x1423a0b7b`) a v helperu. Hledáno `g grep '\+ 0x10d[9a]\]'` v celém .text a v device modulu jsou jen tyto. Inicializace `+0x10d8/0x10d9` = 0 (`0x1423a0ca8 mov word`). **Latch je per-process.**
- `WAIT_IO_COMPLETION` (alertable) ani `WAIT_FAILED` latch nenastaví.
- **Umístění:**
  - `0x1422e4ae0` je procedura vlákna (`0x1422e5266 call CreateThread` s `lea r8,0x1422e4ae0`).
  - Smyčka: `while(!quit){ if(!active) Sleep(1); else { …; 0x14229b020(sRender); app->vfunc+0x30 (=0x141af2b60, vtable 0x1433fa9b8 slot 6); … } }` (`0x1422e4b50-0x1422e4c06`).
  - `0x14229b020` navíc na začátku vrací hned, když `[dev+0x1482]` (`0x14229b035-0x14229b040`).
- **INFERENCE (drift semaforu):** helper čeká jednou za iteraci smyčky bez ohledu na to, zda předchozí iterace prezentovala. Iterace s early-return (Q4a) tak spotřebuje jeden „slot“ waitable bez Presentu, který by ho vrátil. Při opakovaném frame-skipu se počet může vyčerpat, wait pak vyprší po 1 s a latch zapadne. Stejný mechanismus může nastat s cizím proxy swapchainem. Rozhodne experiment E1.
- **Důsledky pro DLSS-G/Reflex (INFERENCE):**
  - Gargauth volá `slReflexSleep` a `SIMULATION_START` v `after_present` (`fg.rs:849-853`), tedy **před** tímto waitem. Blokující čas waitu se proto připíše k simulaci a Reflex sleep pak čeká podruhé.
  - Waitable s latencí 3 (Gargauth default `fg_max_latency=0` = beze změny) přidává frontu, kterou Reflex neřídí.
  - Pacing po vypnutí waitu: (1) fence throttle n-1 v Present (§2.1, krok 2), (2) blokování SL proxy Present, (3) Reflex sleep. Stačí to, protože Gargauth Reflex low-latency vynucuje (`streamline.rs:2024-2049`).
  - MHWFG obchází wait detourem jen při aktivním DLSSG (`FG_Hooks.cpp:192-257`). Latch zápisem dosáhne téhož bez patchování kódu.

### 2.3 Vlákna, ECL, Present, frame routine (Q3, Q4)

**Kick (main thread, brzy v app update)** `0x142293280` (volán z `0x141af2cd1`/`0x141af3a32`):
- `if (!sRender+0x90) { ++[sRender+0x1f208]; …; dev beginFrame 0x1423a3460→0x1423a2b50 (++[dev+0x1e8], reset allocatorů slotu n%3, ECL jen na CopyQueue); [sRender+0x90]=1; if (mParallelRendering) SetEvent([sRender+0x1b8]); }` (`0x1422932a4-0x142293453`).
- **Index snímku roste jen tady a jen bez pending snímku.** Každý nakicknutý snímek se tedy právě jednou odešle a prezentuje.

**Render thread** `0x14229aba0` („Rendering Thread“, `0x14350d498`, vytváří ho `0x1422913e5`):
- `Wait(kick,INF) → 0x142293480 (draw) → 0x1423a2b40 → (DX12) 0x14229cee0 → 0x14229cd20 → SetEvent([sRender+0x1c0] done)`. Zde se nahrávají listy, včetně Gargauthova upscale slotu („upscale slot is the engine's render thread“, `fg.rs:820`: **CONFIRMED** strukturálně).

**Frame routine** `0x1422980b0` (volá ji `0x141af395c`/`0x141af3e14` na konci app update, plus `0x14229aec6` při resetu):
```
if (+0x93 mParallelRendering && +0x90 pending && +0x91 mFrameSkip) {
   while (elapsed_ms < 1000/[sMain+0x58]) if (WaitForSingleObject(done,0)==0) break;   // 0x142298130
   if (WaitForSingleObject(done,0)!=0) return;          // 0x142298176 → bez submit/Present
   +0x90 = 0;
}
0x14229cbf0: if (+0x90) { if(!threaded) render inline; else Wait(done,INF); +0x90=0; }
… release ring objektů, sScene/sGUI vfunc+0x58, DLSS ratio (0x1422982e7…)
dev 0x1423a7f50 → 0x1423a7930: ECL(mCmdQueue …) + Wait/ECL(Async) + Signal(mFence, ++[dev+0x1e0]); ring[[dev+0x1e8]%3]=v
0x1423aa290(dev, [sRender+0x1f51c], 0) → 0x1423a9ce0 (throttle n-1, Present)
if (!ok) sRender+0x68cbd |= 1
```
- Jména `+0x91`=`mFrameSkip`, `+0x93`=`mParallelRendering`, `+0x92`=`mDisableRendering` pocházejí z registrace DTI vlastností v `0x142294000` (`0x1422946a2 lea rax,[rdi+0x91]` + string `0x14350d590`).
- **Všechna ECL na grafickou frontu `[dev+0x20]`** (sken `call [reg+0x50]` s `mov rcx,[r..+off]` v 10 předchozích instrukcích, rozsah `0x142290000-0x1422a0000` a `0x1423a0000-0x1423b0000`, plus mapování front z `CreateCommandQueue` stringů):
  - `0x1423a7d72`, `0x1423a7e0b`, `0x1423a7ea5`: per-frame submit, jen z `0x1422980b0`.
  - `0x1423aa19b`: Present, mode change.
  - `0x1423ac6c8`: `0x1423ac560` z `0x14229b020`, změna nastavení.
  - CopyQueue `+0x1128`, Async `+0x208`, Concurrent `+0x10a0` a ImmediateRead `+0x1310` jsou jiné fronty.
  - **Gargauthův „první ECL enginu“ je tedy per-frame submit v `0x1422980b0` na main threadu, těsně před Present.** Výjimka: snímek se změnou rozlišení/DLSS, kde první ECL je DLSS re-create na začátku iterace (`0x1423ac6c8`).
- Engine má **vlastní FPS limiter** na začátku app update: `0x14225f9f0` (`Sleep(ms-1)` na `0x14225fe64` + spin), řízený `[sMain+0x58]` (fps, default 60). INFERENCE: odpovídá hernímu fps capu, s FG tedy omezuje *renderované* fps.

**Časová osa jedné iterace (main thread M, render thread R):**
```
M: 0x14229b020 ─ WAIT(waitable,1000)      [helper, ret 0x14229b078]
M: 0x141af2b60 ─ limiter 0x14225f9f0 ─ KICK(k) 0x142293280 ─────────────┐
R:                                         wake → record frame k (upscale slot: SL constants/tags) → SetEvent(done)
M:   … sim systémy (vfunc+0x30) …                                        │
M: 0x1422980b0 ─ wait done (bounded/∞) ─ ECL(k) ─ Signal ─ Present(k): fence(k-1) → swap->Present
M: after_present (Gargauth): PRESENT_END, token++, ReflexSleep, SIM_START
```

**Srovnání PCL:**
- Gargauth (`fg.rs:1-13, 368-378, 788-854`): `SIM_END+RENDERSUBMIT_START` na prvním ECL → konstanty a tagy na render threadu → `RENDERSUBMIT_END, PRESENT_START` → Present → `PRESENT_END`, nový token, sleep, `SIM_START`.
- MHWFG (`FG_Hooks.cpp:1584-1623, 1680-1703`): token → `fg->Present()` (konstanty a tagy) → `ePresentStart` → Present → `ePresentEnd` → `ReflexSleep` → `FrameLimit::sleep(×mult)`. SIM ani RENDERSUBMIT markery nemá.
- **Rozdíl:** podmínku „PRESENT_START po konstantách“ Gargauth splňuje strukturálně (render thread musí doběhnout před ECL a Present, `0x14229cbf0`/`0x142298160`). **CONFIRMED.**
- **INFERENCE (posun o iteraci):** render v iteraci k kreslí data ze simu iterace k-1 (kick předchází sim v `0x141af2b60`), takže SIM markery pro token k popisují sim, jejíž výsledek se ukáže až v k+1. Pro PCL statistiku je to drobnost a pro `useMarkersToOptimize=false` (`streamline.rs:2040`) bez vlivu.

### 2.4 UI, CopyToBackBuffer, HDR (Q5, Q6)

**Konec snímku `0x14229a080`** (sRender vfunc, ptr `0x1431e2ea0`), pořadí `call`:
1. `sScene vfunc+0x60`, `sPrimitive …`.
2. `sCamera->vfunc+0x48(ctx, primaryScene=[sRender+0x1f450], 0)` (`0x14229a350`). Uvnitř je `0x14228e770` = smyčka 8 viewportů: `0x1423c3790` (begin viewportu, bind scénových targetů přes `0x1423ce3c0`) a draw listy `sPrimitive`/`0x1451238c8 vfunc+0x80`. **Zde běží scéna, post, tonemap filtr a GUI unity (INFERENCE: GUI je unit v draw listu viewportu, `uGUI::draw` je vfunc unity, ptr `0x143517690/0x1435178c8/0x143517b28`).**
3. `setRenderTarget(primaryScene+0x4a0, primaryScene+0x568)` (`0x14229a3fe`) → `0x14228db30` (clear okrajů mimo present rect, `0x1423c81a0`) → `sMovie->vfunc+0x48` (`0x14229a474`).
4. Barriers → **`System_CopyToBackBuffer(sRender, ctx, 0)`** (`0x14229a4eb`), pak pro sub-displaye `0x14229a540`.

**`0x1422979f0`** (OBSERVED):
- CB `CBSystem` (`0x1433fb248`): `fSourceMipLevel=0`, `fGammaCorrect=1.0/[sRender+0x1c8]` (`0x142297b1d divss`), `fGammaCorrectEx=[sRender+0x1cc]`.
- `tBaseMap = [[sRender+0x1f450]+0x4a0]+0x58` (`0x142297bb8-0x142297c03`).
- Barrier a RT backbufferu `[sRender+0x41b90+idx*8]` (`0x142297cc6`, `0x142297cd9`).
- Technika `System_CopyToBackBuffer`, nebo `…PQSingle` při `[sRender+0x21a]` (HDR, `0x142297d78 cmovne`). Fullscreen draw `0x1422938b0`.
- V celé funkci není tonemap parametr. Tonemap má vlastní CB (`CBToneMapping`, `iToneMapType`, `bIsLinearToPQ` u `0x143538508`) a unit `uToneMapFilter`.

**HUD-less (INFERENCE):** obsah `primaryScene+0x4a0` těsně před prvním GUI draw. Gargauthův runtime průzkum ukázal R11G11B10_FLOAT target (`hudless.rs:40-50`) svázaný podruhé v rámci snímku. To sedí s „GUI viewport znovu binduje scénový target přes `0x1423c3790`“. Výsledek je **post-tonemap, pre-gamma/PQ**. Do barevného prostoru backbufferu ho dostane stejná transformace jako CopyToBackBuffer (gamma s `fGammaCorrect`/`Ex`, nebo PQ při HDR), ne tonemap.

**Statická kotva pro HUD-less:** buď entry `uGUI::draw 0x142353030` (první volání ve snímku: aktuální RT = `[ctx+0x10]` podle `0x1423ce3d2 cmp [rcx+0x10],rdx` v `setRenderTarget`), nebo současná heuristika „N-tý bind“. `setRenderTarget` deduplikuje (`0x1423ce3d2-0x1423ce3e5`), takže počty bindů z `OMSetRenderTargets` jsou počty *změn* a jsou stabilní. Riziko kotvy na `uGUI::draw`: podtřídy mohou slot přepsat (UNKNOWN, E4).

**HDR** (OBSERVED):
- `0x1423a22f0`: `CheckColorSpaceSupport(12)` → `SetColorSpace1([rsp+0x70]=12)` → `ResizeBuffers(count, w, h, [rsp+0x68]=0x18, flags)`. Konstanty `mov [rsp+0x30],0xc` / `mov [rsp+0x28],0x18` ve volajícím `0x14239fbf8-0x14239fc00`.
- Pak `0x1423a23d0(sc, hdr?12:0)` → `SetColorSpace1` (`0x14239fc1a-0x14239fc1f`).
- Zapnutí: `mUserHdrOutput [sRender+0x21b] && 0x140b77b40()` → `+0x219` (`0x14229b34a-0x14229b374`) → kopie do `+0x21a` při kicku (`0x14229342e`).
- Vlastnosti: `mHdrOutputGamutMappingRatio +0x220`, `mHdrOutputGamma +0x224`, **`mIsGUIHdrGamma +0x228`** (GUI v HDR má vlastní gamma úpravu), `mToneMapShoulderScale +0x22c`.

---

## 3. Doporučené změny pro Gargauth

**R1 — vypnout herní waitable wait, když běží SL/Reflex (pacing).**
- Soubory: `swap.rs:343-367` (vedle `hook_set_maximum_frame_latency`) nebo `fg.rs:807-854`, plus nový gate site v `native.rs:777` nebo `tune.rs:66`.
- Jakmile je swapchain Streamlinový (`fg::announce_swapchain(true, …)`, `swap.rs:594`), zapsat `byte [[sRender]+0x78 + 0x10DA] = 1`.
- Gate sites:
  - `0x22e4bdd`: `48 8b 0d 9c f8 ed 02` (`mov rcx,[rip+…]` → `0x1451c4480`, ukazatel sRender, stejně jako `tune::slot_from`).
  - `0x229b06f`: `48 8b 4e 78 e8 b8 2d 11 00` (`[rsi+0x78]` + call helperu).
  - `0x23ade42`: `80 b9 da 10 00 00 00` (offset latche).
- Proč: odstraní 1 s hitch, double-sleep a nafukování `SIMULATION`. Latch je stejně per-process, takže nic, co engine sám neumí, nezavádíme.
- Alternativa podle MHWFG je detour `0x1423ade30` s kontrolou návratové adresy `0x14229b078`. Je to code patch a bez přínosu.
- `fg_max_latency` (`config.rs:129-133`) pak ztrácí na CPU straně význam. Ponechat jen jako DXGI frontu.

**R2 — Reflex sleep + `SIMULATION_START` na skutečném začátku iterace (volitelné, přesnější).**
- Soubory: `fg.rs:849-853` → nové místo.
- Místo `after_present` volat sleep a `SIM_START` z detouru helperu (`0x1423ade30`, jen při ret `0x14229b078`) a jen pokud od minulého volání proběhl Present. Bez té podmínky by iterace s frame-skipem volaly sleep dvakrát na stejný token.
- Rozdíl oproti současnému stavu je jen konec `0x141af2b60` po `0x1422980b0` a tail smyčky. Pokud se R1 udělá latchem, R2 je nice-to-have.

**R3 — PCL pořadí (potvrdit a zdokumentovat, `fg.rs:1-20, 368-378`).**
- Současné pořadí ponechat: `SIM_END/RENDERSUBMIT_START` na prvním ECL grafické fronty, které je per-frame submit v `0x1422980b0`, main thread. `RENDERSUBMIT_END/PRESENT_START` před SL Present. `PRESENT_END`, token, sleep po něm.
- Opravit komentář `fg.rs:17-20`: early return snímek jen *odkládá* (pending `+0x90`, kick potlačen), index `[sRender+0x1f208]` je 1:1 s Presenty.
- Doplnit výjimku: první ECL může být re-create DLSS feature (`0x1423ac6c8`) na začátku iterace při změně rozlišení. Volitelně ignorovat ECL s jediným listem při mode change.
- `slDLSSGSetOptions`/`slReflexSetOptions` jen při změně (MHWFG). Reflex už je cachován (`streamline.rs:2033`).

**R4 — ResizeBuffers je běžná cesta (`swap.rs:23-28, 561-563, 595-599, 996-1007`).**
- Opravit komentáře: swapchain se nevytváří znovu, resize a fullscreen jdou přes ResizeBuffers, i zevnitř Present po ztrátě exkluzivního FS.
- V `hook_resize_buffers` aktualizovat `chain.format`, když `format != UNKNOWN` (HDR přepne na R10G10B10A2, `tag_hudless` jinak porovnává se zastaralým B8G8R8A8, `fg.rs:726-741`).
- Po úspěšném resize volat `crate::fg::note_reset()`.

**R5 — FPS limit (`streamline.rs:2039`).**
- Herní limiter `0x14225f9f0` omezuje renderované snímky (`[sMain+0x58]`).
- S FG nabídnout `frame_limit_us` přes Reflex (výstup / multiplikátor) a doporučit herní cap „unlimited“, nebo dokumentovat, že herní cap × `fg_multiplier` = výstup (MHWFG `FrameLimit.cpp:64-77`).

**R6 — HUD-less ve formátu backbufferu (`hudless.rs:59-72`, `fg.rs:715-750`).**
- Zachytit kopii jako dnes. Pak vlastním fullscreen passem převést R11G11B10 do B8G8R8A8 (SDR) nebo R10G10B10A2/PQ (HDR) se stejnou transformací jako `System_CopyToBackBuffer`.
- Nejjistěji tak, že se zachytí bytecode PS techniky `System_CopyToBackBuffer`/`…PQSingle` při tvorbě PSO a použije se znovu s parametry z `[sRender+0x1c8]`/`[sRender+0x1cc]`.
- Teprve potom zapnout `fg_hudless` jako default. Komentář „tonemap v compositu“ opravit na „výstupní gamma/PQ“.
- UIColorAndAlpha nedělat, viz Q5d.

---

## 4. UNKNOWN a runtime experimenty

| ID | Neznámá | Experiment (Gargauth) | Rozhodne |
|---|---|---|---|
| E1 | Blokuje herní waitable wait (a zapadá latch) s SL proxy, a nativně při frame-skipu? | Detour `0x1423ade30` jen pro logování (nebo kontrola v `fg::after_present`). Měřit `QueryPerformanceCounter` kolem originálu, každou sekundu logovat počet volání, součet a max doby čekání a `byte [dev+0x10DA]`. Porovnat s `swap::PRESENTS` (`swap.rs:677`), pro fg=0 a fg=1 a pro 30/60/unlimited. | Čekání >0 ms při FG nebo latch=1 potvrzuje R1. Volání > Presenty při skipu potvrzuje drift. |
| E2 | Hodnoty `mFrameSkip`/`mParallelRendering` a četnost early-return | V `fg::after_present` jednou logovat `byte [sRender+0x91]`, `[+0x93]` a rozdíl `[sRender+0x1f208]` mezi Presenty. | Zda Q4a v praxi nastává a jak často. |
| E3 | Co vrací SL proxy z `GetFrameLatencyWaitableObject` a zda SL respektuje `SetMaximumFrameLatency` | Hook slotu 33 na SL proxy vtable v `swap.rs:hook_chain`, logovat handle a srovnat s nativním swapchainem. | Vysvětlí MHWFG bypass. Pokud jde o vlastní semafor SL, je latch jediná bezpečná cesta. |
| E4 | Je první GUI draw vždy přes `0x142353030` (bez override)? | Detour entry `uGUI::draw`, při prvním volání ve snímku (stamp `hudless::FRAME`) logovat `[rdx+0x10]` (RT objekt ctx) a porovnat s resource vybraným `hudless_occurrence`. | Zda nahradit heuristiku statickou kotvou. |
| E5 | Blend stavy GUI (alpha kanál), proveditelnost UI planu | V `hudless` survey pro draw mezi kotvou a CopyToBackBuffer logovat `D3D12_BLEND_DESC` z PSO (hook `CreateGraphicsPipelineState` už existuje v `gpu.rs`). | Pokud Src/DestBlendAlpha dávají premultiplied „over“, UI plane je teoreticky možný. Jinak definitivně ne. |
| E6 | Přesná transformace CopyToBackBuffer (shader není v exe) | Zachytit HUD-less a backbuffer stejného snímku, v pixelech bez HUD fitnout `bb = f(hudless)` (pow s `1/[sRender+0x1c8]`, `[+0x1cc]`). Případně dumpnout PS bytecode PSO s RTV formátem backbufferu. | Implementace R6. |
| E7 | Formát `primaryScene+0x4a0` při HDR | Log formátu resource `tBaseMap` (hook GPU pointeru) při HDR zap/vyp. | Zda je HUD-less v HDR stále R11G11B10 (lineární), nebo v PQ (`bIsLinearToPQ`). |

---

## 5. Mapa adres

| VA | Co | Zdroj |
|---|---|---|
| `0x1422e4ae0` | procedura vlákna hlavní smyčky (CreateThread `0x1422e5266`) | OBSERVED |
| `0x14229b020` | sRender: začátek iterace (wait, mode change, reset) | OBSERVED |
| `0x14229b073` / `0x14229b078` | call helperu / návratová adresa | OBSERVED |
| `0x1423ade30` | wait helper (`WaitForSingleObjectEx` IAT `0x142e132d8`) | OBSERVED |
| `dev+0x10D8` / `+0x10D9` / `+0x10DA` / `+0x10E0` / `+0x10F0` | fence-event flag / „presented“ / latch timeoutu / fence event / waitable HANDLE | OBSERVED |
| `0x141af2b60` | app update (vtable `0x1433fa9b8` slot 6) | OBSERVED |
| `0x14225f9f0` | herní FPS limiter (`Sleep` `0x14225fe64`, `[sMain+0x58]`) | OBSERVED / INFERENCE název |
| `0x142293280` | sRender kick (frame++ `+0x1f208`, `SetEvent [+0x1b8]`) | OBSERVED |
| `0x14229aba0` | Rendering Thread proc | OBSERVED |
| `0x14229cbf0` | finish render (inline nebo `Wait done [+0x1c0]`) | OBSERVED |
| `0x1422980b0` | frame routine (frame-skip ret `0x142298176`, submit `0x1422983c8`, Present `0x1422983da`) | OBSERVED |
| `0x1423a7930` | DX12 submit: ECL `[dev+0x20]` + Signal `[dev+0x168]` | OBSERVED |
| `0x1423aa290` / `0x1423a9ce0` | Present wrapper / DX12 Present (throttle n-1, flags, `+0x10D9=1`) | OBSERVED |
| `0x1423a2b50` | dev beginFrame (`++[dev+0x1e8]`) | OBSERVED |
| `0x1423a0950` / `0x1423a48e0` | device ctor / tvorba front + swapchainu | OBSERVED |
| `0x1423a63ab`, `0x1423a66aa` | `CreateSwapChainForHwnd` (jediná místa) | OBSERVED |
| `0x1423a6553` / `0x1423a6571` | `SetMaximumFrameLatency(3)` / `GetFrameLatencyWaitableObject` | OBSERVED |
| `0x1423ac040`, `0x1423ac560`, `0x14239f810` | DX12 mode change, ResizeTarget/SetFullscreenState/ResizeBuffers | OBSERVED |
| `0x1423a22f0` / `0x1423a23d0` | HDR: `CheckColorSpaceSupport(12)`+`SetColorSpace1`+`ResizeBuffers(R10G10B10A2)` / `SetColorSpace1(0\|12)` | OBSERVED |
| `0x14229a080` | sRender konec snímku (sCamera draw, sMovie, CopyToBackBuffer) | OBSERVED |
| `0x14228e770` | `sCamera::draw` (smyčka 8 viewportů) | OBSERVED |
| `0x1423ce3c0` | `ctx setRenderTarget` (dedup `[ctx+0x10]`/`[ctx+0x50]`) | OBSERVED |
| `0x142353030` | `uGUI::draw` (bez bindu RT) | OBSERVED |
| `0x1422979f0` | `System_CopyToBackBuffer` (gamma/PQ, `tBaseMap=[[sRender+0x1f450]+0x4a0]+0x58`) | OBSERVED |
| `sRender+0x78/+0x90/+0x91/+0x92/+0x93` | dev / pending / mFrameSkip / mDisableRendering / mParallelRendering | OBSERVED (DTI) |
| `sRender+0x1b8/+0x1c0` | kick event / done event | OBSERVED |
| `sRender+0x1c8/+0x1cc` | gamma (→`fGammaCorrect`) / `fGammaCorrectEx` | OBSERVED |
| `sRender+0x219/+0x21a/+0x21b` | HDR aktivní / kopie pro render / mUserHdrOutput | OBSERVED |
| `sRender+0x1f208` | index renderovaného snímku | OBSERVED |
| `sRender+0x1f450` | mpPrimaryScene | OBSERVED (DTI) |
| `sRender+0x1f51c` | sync interval pro Present | OBSERVED |
| `sRender+0x41b90+i*8` | backbuffer textury | OBSERVED |
| `dev+0x20/+0x208/+0x10a0/+0x1128/+0x1310` | mCmdQueue / Async / Concurrent / Copy / ImmediateRead | OBSERVED (stringy `CreateCommandQueue`) |
| `dev+0x1470/+0x1481/+0x1482/+0x14a0` | IDXGISwapChain3 / tearing / skip-present / počet displayů | OBSERVED |
| `0x1451c4480 / 0x1451c4148 / 0x1451c4640 / 0x1451c4400 / 0x1451c4af0 / 0x1451c4368 / 0x1451cc1f8` | sRender / sMain / sGUI / sCamera / sMovie / sScene / sPrimitive | OBSERVED (ctor zápisy, DTI) |
