# MHW RE — souhrn a plán dokončení Gargauth/mhworld-dlss

Statická RE `MonsterHunterWorld.exe` (build 15539686, PE ts 0x66d13cb6, SHA-256 1ad7f7c4…bd08). Detaily s adresami a důkazy: `ReJitterCamera.md`, `ReDlss1Path.md`, `ReFramePresent.md`, `RePostProcess.md`, `ReLoadInitDx11.md`, `ReSignatures.md` + `signatures.json`. Nástroje: `re/tools/g` (offline index + Ghidra server nad uloženým projektem `re/ghidra/mhw.gpr`).

Vše je statické (OBSERVED v kódu / INFERENCE). Co rozhoduje jen běh, je v sekci 4.

## 1. Verdikty sporů

| Spor | Verdikt | Důkaz |
|---|---|---|
| Dělitel jitteru | **Výstupní (display) šířka/výška** — odt1 má pravdu, Gargauth ne. Halton u Gargautha pokrývá jen `rs` px. Hlášený jitter do DLSS je přesto konzistentní (čte se z proj[8]/[9]). | EDX = vp rect z cfg+0x198/+0x19c (`0x142290420`) |
| MV jitterované? | **Ne.** Prev VP nese *aktuální* jitter, takže MV jsou bez jitteru. `MV_DEJITTER=0` je správně; korekce Jcur−Jprev u LostRiveru je nulová. | `0x1423d0e70`: fViewProjPF = Vprev·Pprev·J_cur·R |
| Znaménko Y jitteru SR vs SL | **SR `(hx,−hy)` správně, SL `(hx,hy)` špatně** a navíc mu chybí ×rs. | DLSS PG + `sl_consts.h`; +u_y posune geometrii nahoru |
| clipToPrevClip pro SL | Obsahuje konjugaci jitterem, SL chce bez jitteru → použít J·C·J⁻¹ nebo matice bez jitteru. | ReJitterCamera §2 |
| Reset | Hra posílá Reset jen v EvalSS: `taa+0x185 ? 0 : taa+0x180 ≤ 0` (rebuild historie, změna scale). **Detekce střihu kamery v enginu neexistuje.** | 13 zápisů vyjmenováno |
| SS cesta (bit0) | Crash je deterministický: barrier planner `0x14229cee0` čte depth přes `+0xf0`, který má R32F depth copy null. Oprava odt1 (alias `+0xf0 := +0x60` + `lock inc native+8`) je **nutná a dostatečná**. SS pak dodá depth (`[view+0x628]+0xa0` R32F) a dense velocity (`taa+0x1a8` R32G32F) a může nahradit DSV heuristiku i MV dekód. | ReDlss1Path §2–3.3 |
| Pořadí passů | bloom → AutoExposure → TAA/EvalSS → DOF → MB → Tonemap → FXAA → EvalISP. Slot route výsledky DOF/MB/bloom **nezahazuje**; DLSS ale dostává obraz **po tonemapu** (`expert_dlss_hdr=1` možná špatně). | sort klíče filtrů |
| AA volba | `rs+0xe9e4` = TAA, `+0xe9e5` = FXAA. AA Off/FXAA ukončí TAA draw → Gargauth ztratí vstupy. `flags=2` + herní „DLSS=On“ vynutí FXAA (před DLSS v upscale). | setter `0x142287a00` |
| TAA sharpen | Engine ho maže jen při SS flagu; při `flags=2` běží (default 0.2) a ostří vstup DLSS. Tvrzení v tune.rs:33-35 je **vyvrácené**. | ctor `0x142391210` |
| Wait helper `0x23ADE30` | Potvrzeno; latch `+0x10DA` je **trvalý**. Wait je na začátku každé iterace main loopu, i bez Presentu. | ReFramePresent §2 |
| Swapchain | Vzniká **jen jednou** (`0x1423a48e0`, IDXGIFactory5, 3 buffery, waitable, tearing pokud jde). Rozlišení i HDR jdou přes ResizeBuffers. Gargauthova představa „rebuild“ je **vyvrácená**. | |
| CopyToBackBuffer | Jde jen o **výstupní gamma/PQ, ne tonemap**. HUD-less = scénový target `primaryScene+0x4a0` těsně před 1. GUI draw, stačí aplikovat tu samou gamma/PQ transformaci. | CBSystem konstanty |
| Proxy bez launcheru | Během ověřeno (ReLoadInitDx11 §2.7): ze složky hry se před device načtou jen `steam_api64.dll` (`WinMain`) a `amd_ags_x64.dll` (`agsInit`, před device). Oba jsou soubory hry. `winmm`/`version` drží Steam ze System32, `dinput8`/`xinput`/`XAudio2_9` přijdou po swapchainu, d3d12/dxgi/nvapi64 jdou přes SEARCH_SYSTEM32. `atiadlxx.dll` (ADL, `agsInit`) na AMD koliduje se skutečným ADL. **Launcher zůstává.** Stracker's Loader s ním koexistuje. | ReLoadInitDx11 §2.6–2.7 |
| D3D12CreateDevice | Hra ho volá **delay-importem přes ordinal #101**, Gargauth ordinaly zahazuje (`hooks.rs:107-108`), takže náhrada se **nespouští**. | slot `0x144c97a50` |
| graphics_option.ini BOM | Hra čte přes GetPrivateProfileStringW; s BOM spadne do DX11, Gargauthův gate přesto hlásí DX12. Po GPU crashi hra zapíše marker a při dalším startu `DirectX12Enable=Off`. | |
| DX11 port | Oddělené NGX tabulky a vlastní DX11 Evaluate; kamera/matice sdílené. Odhad ~12–15k řádků, jen SR. | |
| Ultra Performance | Clamp ≥0.5 v 6 instrukcích se sdílenou konstantou; patch je možný jen přesměrováním operandů, riziko. Při 0.5 a šířkách 3440/1680/1366 hrozí stejný mismatch → opravit `snapped()`. | |
| Update hry | Všech 16 gate sites má unikátní AOB (15/16 i s wildcardy offsetů), 46/49 struct offsetů jde odvodit z instrukcí, 25 offsetů má MT reflection jméno. | `signatures.json` |

## 2. Plán dokončení — seřazeno podle dopadu

### P1 — kvalita SR (malé změny, velký dopad)
1. **Jitter amplituda:** `native.rs` — zapisovat `2·px / rs` (dělitel je display). Hlášení do DLSS beze změny.
2. **Jitter jednou za frame:** detour se volá pro každý aktivní viewport, takže dnes posouvá sekvenci vícekrát; klíčovat `sRender+0x1f208` + hlavní viewport.
3. **TAA sharpen vypnout** (`taa+0x187 = 0`) a **AA word `rs+0xe9e4` držet `0x0001`** po každém apply voleb.
4. **Reset:** číst flag z `hook_ss_enqueue` (`(u8**)(capture+0x38)` → `taa+0x185`) a navíc vlastní detekci střihu (skok kamery z viewport matic `+0xa0/+0x120`). Posílat do SR i FG.
5. **HDR flag:** ověřit E-experimentem, zda je vstup po tonemapu (pak `IsHDR=0`, nebo se přesunout před tonemap).

### P2 — DLSS-G
6. **SL jitter:** `(hx·rs, −hy·rs)` stejně jako SR; **clipToPrevClip** bez jitter konjugace.
7. **Pacing:** se SL swapchainem zapsat `[[0x1451c4480]+0x78]+0x10DA = 1` (datový zápis, žádný patch); volitelně Reflex sleep do detouru wait helperu.
8. **ResizeBuffers:** aktualizovat `chain.format` a `fg::note_reset()`; opravit komentáře o rebuildu swapchainu.
9. **HUD-less:** kopie `primaryScene+0x4a0` před 1. GUI draw → vlastní pass s gamma/PQ jako CopyToBackBuffer (ideálně reuse jeho PS bytecode) → zapnout `fg_hudless` jako default. UIColorAndAlpha je nepraktické.
10. **FPS limit:** dokumentovat herní cap × multiplikátor, nebo nabídnout Reflex `frame_limit_us`.

### P3 — robustnost
11. **D3D12CreateDevice ordinal #101:** obsloužit ordinal, nebo přímo přepsat delay-import slot `0x144c97a50`.
12. **AOB gate:** implementovat navržené API (fast path na známé RVA → scan .text → exact bytes, all-or-nothing); offsety a globály číst z instrukcí. Pozor na `RATIO_SETTER_TAIL` (2 hity).
13. **Gate DX12 detekce:** odmítnout BOM, chybějící klíč a fallback marker `rendering_api_fallback.txt`.
14. **`snapped()`** při scale 0.5 a šířkách, kde w/2 není násobek 16.
15. Opravit dokumentaci a komentáře: SETTINGS_LOAD VA `0x1402412ef`; `0x14043dcb0` je AA handler; `tune::unroll` je no-op (nemá čtenáře); zastaralé MV komentáře.

### P4 — strategické volby
16. **SS cesta** místo DSV heuristiky + MV dekódu (alias depth native, `flags=3`). Zjednoduší i případný DX11 port; potřebuje runtime ověření konvence MV `taa+0x1a8` (E2).
17. **Bez launcheru:** jen proxy `amd_ags_x64.dll` s přesunutým a forwardovaným originálem (NVIDIA i AMD; factory v `[dev+0]` už existuje), nebo `steam_api64.dll` (přepisuje soubor Valve). Zatím se nedělá, launcher zůstává (ReLoadInitDx11 §2.7).
18. **DX11:** až po SS cestě, jen SR.

## 3. Ověření za běhu (nejvyšší hodnota)
- **E-jitter:** log `x / proj[8]` → potvrdí display dělitel (měl by vyjít ~2/Wd).
- **E1 Reset:** logovat `taa+0x185` a skoky kamery při cutscénách a teleportu.
- **E2 MV taa+0x1a8:** porovnat s dekódovaným tGBuffer4 (scale/znaménko).
- **E3 depth copy:** porovnat s výsledkem `pick_depth`.
- **E-HDR:** readback rozsahu barvy vstupující do DLSS (>1.0?).
- **E-wait:** čítat timeouty wait helperu a ověřit, jestli se latch nastavuje už dnes.
- ~~**E-proxy:** Procmon, `amd_ags_x64.dll` / `steam_api64.dll` v adresáři hry.~~ Hotovo: ReLoadInitDx11 §2.7.
