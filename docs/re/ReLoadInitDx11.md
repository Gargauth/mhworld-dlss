# ReLoadInitDx11: načítání modulů, inicializace rendereru, DX11/DX12

Cíl: `MonsterHunterWorld.exe` (build 15539686, PE ts `0x66d13cb6`, base `0x140000000`). Analýza je statická (offline capstone index + vlastní raw sweepy). Skripty: `re/scratch/ReLoadInitDx11/{ctx.py,raw.py,calls.py,dslots.py,up.sh}`. `calls.py` hledá přímým bajtovým scanem `E8/E9 rel32` a najde i volání do kódu bez `.pdata`, který index nevidí (např. `0x1424e9630`, `0x14229be60`).

Značky: **OBSERVED** znamená, že to přímo ukazuje disasm nebo data. **INFERENCE** je odvozený závěr. **UNKNOWN** nejde staticky rozhodnout. U cizích tvrzení uvádím verdikt **CONFIRMED / REFUTED / PARTIAL**.

## 0. Shrnutí (TL;DR)

- **Běh (2026-10-12, §2.7): launcher zůstává.** Ze složky hry se před device načtou jen `steam_api64.dll` (840 ms) a `amd_ags_x64.dll` (1635 ms), oba soubory hry. Drop-in proxy bez přepsání souboru hry, které by fungovalo na NVIDIA i AMD, neexistuje: `winmm`/`version`/`iphlpapi` drží Steam ze System32 dřív, `XAudio2_9`/`DINPUT8`/`XINPUT9_1_0` přijdou až po swapchainu a NGX initu. Stracker's Loader (`dinput8`) s launcherem koexistuje.
- **Hra importuje `D3D12CreateDevice` přes delay-import podle ordinalu `#101`, ne podle jména.** Gargauthův hook na `GetProcAddress` proto toto volání nikdy nezachytí: ordinaly přeskakuje v `hooks.rs:107-108` a jméno porovnává v `hooks.rs:170`. Skutečnou příčinou je ordinal, ne „dynamicky resolvovaný kernel32“, jak tvrdí `gpu.rs:3822-3826`. Oprava je triviální, viz §3.
- **SR bez launcheru je proveditelné.** Rychlá varianta je raný proxy `amd_ags_x64.dll`: hra ho z adresáře hry načte bezpodmínečně uvnitř DX12 initu, **před** `D3D12CreateDevice`, swapchainem, RTV, 48 MiB arénou i NGX initem. Druhá varianta je pozdní load s doplněním stavu: arénu přečíst z polí device objektu a cíle donutit k rebuildu přes ratio setter.
- **FG bez launcheru je proveditelné jen s raným proxy** (`steam_api64.dll` nebo `amd_ags_x64.dll`). Swapchain vzniká jednou v `0x1423a48e0` a žádná cesta ho za běhu znovu nevytváří, takže pozdní load (`dinput8`) pro FG nestačí.
- **Hra čte `DirectX12Enable` přes `GetPrivateProfileStringW`** ze sekce `[GraphicsOption]` a hodnotu porovnává s `"On"` přes `_stricmp`. Chybějící klíč znamená DX11. Hodnotu `Off` hra zapisuje sama až **při příštím startu** po GPU crashi, přes marker `app:\rendering_api_fallback.txt`.
- **DX11 má oddělenou NGX tabulku a vlastní evaluate funkce.** CreateFeatures, Shutdown, GetOptimalSettings, camera setup `0x14228f3b0` i matice `0x1423d0e70` jsou API-neutrální. DX11 backend kreslí do immediate contextu a deferred contexty jsem nenašel.

## 1. Otázky a verdikty

| # | Otázka / tvrzení | Verdikt |
|---|---|---|
| 1a | Statický import table | **OBSERVED:** jen `KERNEL32.dll` (188 funkcí). Exe má ale **21 delay-import DLL**, mimo jiné `d3d11`, `d3d12`, `DINPUT8`, `XINPUT9_1_0`, `WINMM`, `steam_api64`, `amd_ags_x64`. Komentář `iat.rs:3-5` („only KERNEL32 … pulls … by name at runtime“) je **PARTIAL**: knihovny se tahají delay-load helperem `0x142167b90` přes `LoadLibraryExA(name,0,0)` a `GetProcAddress` z IAT. |
| 1b | „d3d12.dll se SEARCH_SYSTEM32“ | **CONFIRMED:** `LoadLibraryExW(L"d3d12.dll",0,0x800)` na `0x1423a15a4` a `0x1423a49f3`. Stejně tak `dxgi.dll` (`0x1423a1588`, `0x1423a3e82`) a `d3d9.dll` (`0x142598731`, jen DX11). |
| 1c | „dinput8 až ~101. modul, po rendereru“ | **CONFIRMED (pořadí) / PARTIAL (mechanismus):** `DirectInput8Create` se volá přes delay-load z `sMhMain` ctoru na `0x141aed68d` (cesta `0x140483cd0`→`0x1422e6a20`), tedy **po** vytvoření rendereru na `0x141aed21d`. Nejde o „plain LoadLibrary“, ale o delay-load helper s `LoadLibraryExA(...,0)`. Search order je stejný, adresář hry má přednost. |
| 1d | „winmm ze System32 kvůli Steam overlay“ | **CONFIRMED (běh, §2.7):** `WINMM.dll` ze System32 je v procesu 2 ms po injekci jako statický import `gameoverlayrenderer64.dll`, který Steam vstříkne do suspendovaného procesu. Exe-side delay-load (`timeBeginPeriod`, `0x1422e4687`) pak dostane už načtený modul; kopie z adresáře hry se nikdy nenačte. |
| 1e | „version.dll ze System32“ | **CONFIRMED (běh, §2.7):** exe `version.dll` nenačítá. Načte ho (spolu s `IPHLPAPI`, `CRYPT32`, `bcrypt`, `Secur32`) `steamclient64.dll` jako statické importy, a to už ve SteamStubu před `WinMain` (394 ms), se search path Steamu. |
| 1f | Proxy kandidáti z adresáře hry před rendererem | **OBSERVED:** `steam_api64.dll` (úplně první příkaz `WinMain` `0x1413a58f8`) a `amd_ags_x64.dll` (DX12 init `0x1423a4bdd` / DX11 init `0x1423a3f23`, před vytvořením device). V DX11 navíc `d3d11.dll` přes plain `LoadLibraryW` na `0x1423a40aa`. Viz §2.1. |
| 1g | Co jde vyčíst z `.bind` | **OBSERVED:** jde o pozůstatek **SteamStub** (Valve DRM) s XOR-chain hlavičkou: `kernel32/msvcrt/user32`, `Local\SteamStart_SharedMemFile`, „Application load error“. EP `0x142742a48` leží v `.text`, takže stub v této kopii neběží. Authenticode digest **nesedí** a security dir ukazuje na nezarovnaný offset. **CONFIRMED (§2.7):** soubor byl po podpisu upraven (pravděpodobně Steamless); retail exe (`c2ebbbd2…90ea`) má EP `0x14559f310` v `.bind` a `.text` na disku šifrovaný. |
| 2a | 48 MiB CB aréna | **CONFIRMED:** `mTempCpuConstantBuffer` = `CreateCommittedResource` (`0x1423a6c00`), velikost `3 × 0x1000000` (`lea r14,[rax+rax*2]`, param3 = `0x1000000` z `0x14229972f`). Mapuje se ihned a trvale (`Map` `0x1423a6c36` → CPU ptr `dev+0x1018`), uvnitř DX12 device initu `0x1423a48e0` během `sMhMain` ctoru. Jindy se nevytváří. |
| 2b | Kdy vznikají RTV | **OBSERVED:** backbuffer RTV hned po swapchainu (`0x1423a6ada` `GetBuffer`, `0x1423a6b0b` `CreateRenderTargetView`), tedy před arénou. Znovu se vytvářejí po resize (`0x1423ac9e0`, volané z present `0x1423a9ce0` a z `0x14229b9a0`). Scénové targety se přestavují při změně ratio (`0x1422980b0` → flag `0x40` → `0x14229b43b`). |
| 2c | Lze vynutit rebuild? | **Targety ano** (INFERENCE z kódu): ratio setter `0x14229be60` zapíše `+0x200`, per-frame porovnání `+0x204≠+0x200` (`0x1422982e7`) nastaví flag `0x40` a následuje resize/recreate view v `0x14229b45f..0x14229b539` a recreate DLSS feature (`0x1423ac560`→`0x142384420`). **Aréna a swapchain ne:** jediné callsity jsou v `0x1423a48e0`. Device-lost reset cesta neexistuje, všechny chyby končí `0xdeadbeef` crashem. |
| 2d | SR bez launcheru | **ANO**, viz §2.6. |
| 2e | FG bez launcheru | **ANO jen s raným proxy** (`steam_api64` nebo `amd_ags_x64`). Pozdní load **NE**. |
| 3a | Parsování ini | **OBSERVED:** `0x1404416b0` → `GetPrivateProfileStringW("GraphicsOption","DirectX12Enable","",buf,0x80,<app:\graphics_option.ini>)` → `_stricmp(val,"On")`. Chybějící klíč, sekce nebo soubor znamená **DX11**. Platí jen když `[0x1451c5320]+0x2cda != 0` (PC gate). |
| 3b | UTF-8 BOM | **INFERENCE (sedí s runtime pozorováním odt1/LR):** BOM přilepený k `[GraphicsOption]` rozbije hlavičku sekce. Profile API klíč nenajde, výsledkem je DX11 a defaulty. |
| 3c | Kdy hra zapisuje `Off` | **OBSERVED:** při DXGI `REMOVED/HUNG/RESET` nebo `UNSUPPORTED` v DX12 zapíše handler `0x142553ff0` do `app:\rendering_api_fallback.txt` „1“ (`0x14229c570`→`0x14229a700` mode 1) a crashne. **Při příštím startu** přečte marker (`0x14229111c`), vynutí DX11 a v `sMhMain` ctoru zapíše `DirectX12Enable=Off` (`0x141aed24c`→`0x140444a50(0)`). Marker smaže později (`0x14025f8a3`→`0x1422937c0`). |
| 3d | Gargauth `gate.rs:638-663` | **PARTIAL:** BOM ho nerozbije, protože klíč není na prvním řádku. Právě proto ale řekne „DX12“, zatímco hra s BOM poběží v DX11. Ignoruje také chybějící klíč (hra: DX11, Gargauth: ne-DX11) a fallback marker. Viz §3. |
| 4a | DX11 vlastní NGX tabulka? | **CONFIRMED:** D3D11 tabulka `0x1451d87d0..0x1451d8807`, D3D12 tabulka `0x1451d8808..0x1451d883f` (odt1 CONFIRMED). Init DX11 `0x142384120` / DX12 `0x1423842a0`. |
| 4b | Sdílené funkce | GetOptimalSettings `0x142383ef0`, CreateFeatures `0x142384420` (větví se podle `IsDX12`), Shutdown `0x1423837a0`: **sdílené**. EvalISP: DX11 `0x142383830` / DX12 `0x142383990`. EvalSS: DX11 `0x142383af0` / DX12 `0x142383cf0`. Volají se přes slot 25 (`+0xc8`) vtables render contextu: DX11 `0x14354e108`, DX12 `0x14354e4b0`. |
| 4c | Camera setup `0x14228f3b0`, matice `0x1423d0e70` | **OBSERVED API-neutrální:** čistá matematika, bez volání backendu a bez `IsDX12`. |
| 4d | Deferred contexts | **Nenalezeno (bounded):** DX11 render context bere immediate context `dev+0x1400` (`0x1425982f4`) a volá `ClearState` (`+0x370`). V renderer regionech jsem nenašel žádný `CreateDeferredContext`/`FinishCommandList` na D3D11 objektech. |
| 5 | Device removed / ERR12 | **OBSERVED:** per-frame present `0x1423a9ce0` volá `GetDeviceRemovedReason`, chyba jde do `0x142553ff0`. `0x887a0005/6/7` vede na ERR12 (ERR18/19 jsou v tomto buildu prakticky nedosažitelné), před tím zápis fallback markeru, pokud je zapnuté DX12. Pak `0x1421fd060` zavolá callback, zapíše `0xdeadbeef` na adresu 0 a vznikne AV. `DXGI_STATUS_OCCLUDED` (`0x087a0001`) se ignoruje. odt1 **CONFIRMED**. |

## 2. Nálezy s důkazy

### 2.1 Kompletní mapa načítání DLL (exe-side)

Delay-load helper `0x142167b90` (MSVC `__delayLoadHelper2`) volá `LoadLibraryExA(dli.szDll, 0, 0)` na `0x142167ccc` (`xor r8d,r8d; xor edx,edx`) a pak IAT `GetProcAddress` (`0x142167da6`). Flags 0 znamenají standardní search order: **adresář exe má přednost**, pokud modul se stejným base name ještě není načtený.

| Pořadí / vlákno | Místo | Volání | Flags | Odkud | Proxy? |
|---|---|---|---|---|---|
| 1 main | `WinMain 0x1413a58f8` | delay `steam_api64.dll` (`SteamAPI_RestartAppIfNecessary(0x8e17a)`), pak `SteamAPI_Init` `0x1413a5909` | 0 | **adresář hry** | **ANO, nejdřív ze všech** |
| 2 main | `0x1422e5010` (app ctor) | `CreateThread(start=0x1422e4ae0, CREATE_SUSPENDED)` `0x1422e5266` | – | – | herní vlákno |
| 3 main | `0x1422e4e00` | USER32/GDI32… delay, okno, `ResumeThread` `0x1422e4fc4` | – | KnownDLLs | ne |
| 4 UI | run loop `0x1422e4687` | delay `WINMM.dll` (`timeBeginPeriod`) | 0 | adresář hry, pokud není načtený | závod / UNKNOWN |
| 5 game | `sMhMain` ctor `0x141aec7d0`: `0x141aed1bb` | `GetPrivateProfileStringW` DirectX12Enable | – | – | – |
| 6 game | `0x1423a1588` | `LoadLibraryExW(L"dxgi.dll")` | 0x800 | System32 | ne |
| 7 game | `0x1423a15a4` | `LoadLibraryExW(L"d3d12.dll")` | 0x800 | System32 | ne |
| 8 game | `0x1423a15d0` | `GetProcAddress(dxgi,"CreateDXGIFactory2")` → `dev+0x2e8` | – | – | hook vidí |
| 9 game | `0x1423a1733` | test `D3D12CreateDevice` (delay **ordinal #101**, out=NULL), pak `FreeLibrary(d3d12)` `0x1423a17a7` | 0 (delay) | již načtený | hook nevidí jméno |
| 10 game | DX12 `0x1423a49c0` | `CreateDXGIFactory2` → `dev+0` (factory pro swapchain) | – | – | – |
| 11 game | DX12 `0x1423a4bdd` | delay `amd_ags_x64.dll` `agsInit` (kromě `/disable_ags`, **nezávisle na vendoru**) | 0 | **adresář hry** | **ANO** |
| 12 game | DX12 `0x1423a4e29` | `D3D12CreateDevice` (ordinal #101) → `dev+0x18`, ERR22 při chybě | – | – | – |
| 13 game | DX12 `0x1423a63b0`/`0x1423a66af` | `factory(dev+0)->CreateSwapChainForHwnd` (slot `+0x78` = 15) | – | – | – |
| 14 game | DX12 `0x1423a6b0b`, `0x1423a6c00` | backbuffer RTV, 48 MiB aréna + `Map` | – | – | – |
| 15 game | DX12 `0x1423a72e8` → `0x14273f230` | `NVSDK_NGX_D3D12_Init`: `nvngx.dll` **vedle exe** (podpis ověřen, `0x142741800` r8b=1), jinak driver store. NGX helpery wintrust/crypt32/advapi32/setupapi/gdi32 System32 absolutní cestou (`0x140001960`) | – | – | ne (podpis) |
| 16 game | `0x141aed68d` | delay `DINPUT8.dll` | 0 | adresář hry | po rendereru |
| – | DX11 `0x1423a3e82`, `0x1423a3e9e` | dxgi 0x800, `GetProcAddress("CreateDXGIFactory1")` | | | |
| – | DX11 `0x1423a3f23` | delay `amd_ags_x64.dll` `agsInit` | 0 | adresář hry | ANO (DX11) |
| – | DX11 `0x1423a40aa` | **`LoadLibraryW(L"d3d11.dll")`** (plain) | – | **adresář hry** | ANO (DX11) |
| – | DX11 `0x1423a4233` | delay `D3D11CreateDevice` | 0 | již načtený | |
| – | DX11 ctx ctor `0x1425985b0` | `LoadLibraryExW(L"d3d9.dll",0,0x800)` (D3DPERF) | 0x800 | System32 | ne |
| ? | `0x142959d8e..e05` | `LoadLibraryA("XAudio2_9/8/7.dll")` (Wwise sink) | – | adresář hry | timing UNKNOWN |
| ? | `0x141af2556` | `LoadLibraryA("DBGHELP.DLL")` (crash dump) | – | – | – |

Pořadí kroků 5–16 je **OBSERVED** v rámci jedné funkce nebo call chainu (`0x141aec7d0` → `0x140fba3a0` → `0x142290880` → `0x1422994a0` → `0x1423a0950` → `0x1423a48e0`/`0x1423a3e20`). Vztah UI vlákna (krok 4) a herního vlákna (5+) je **INFERENCE**: oba běží po `ResumeThread`.

Rozhodnutí DX12/DX11 padá v `0x1423a0950`:
```
0x1423a190d  test sil,sil        ; sil = GraphicsOptionDX12 (param6 [rbp+0x8e8])
0x1423a1912  cmp byte [rdi+0x2db],0  ; adapter prošel test D3D12CreateDevice
0x1423a1921  call 0x1424e9990    ; mov [0x1451cc080],cl  (DX12 flag setter, jediný callsite)
0x1423a1926  call 0x1424e9630    ; movzx eax,[0x1451cc080] (getter, ~80 callsites)
0x1423a1957  je 0x1423a1968 → 0x1423a48e0 (DX12) | 0x1423a3e20 (DX11)
```
Index `g x 0x1451cc080` nic nevrátí, protože setter a getter jsou listy bez `.pdata`. Reference najde `calls.py`.

### 2.2 `D3D12CreateDevice` = delay-import ordinal #101

- **OBSERVED:** delay descriptor `d3d12.dll` má INT `0x143e6fb18 = 0x8000000000000065` a IAT slot `0x144c97a50`, který na začátku obsahuje thunk `0x14273ddf7`. Stub `0x14273de03: jmp [0x144c97a50]`.
- Volající jsou `0x1423a1733`, `0x1423a4aac`, `0x1423ac28a` (probe, out=NULL) a `0x1423a4e29` (skutečné vytvoření). Argumenty jsou `(adapter, 0xc000, IID, &dev->+0x18)`. Výsledek se dál používá jako `ID3D12Device`: `+0x78` `GetDescriptorHandleIncrementSize`, `+0xd8` `CreateCommittedResource`. Selhání vede na „ERR22 : Failed to initialize DirectX12.“, takže #101 je `D3D12CreateDevice` (INFERENCE silná).
- Delay helper volá `GetProcAddress(hmod, (LPCSTR)101)`. Gargauth: `hooks.rs:107-108` `if (name.0 as usize) > 0xFFFF` ordinal zahodí, proto se `hooks.rs:170-175` nikdy nespustí. Tím se vysvětluje `gpu.rs:3822-3826` („misses … device creation“). Vysvětlení „resolves much of kernel32 dynamically“ je **REFUTED** jako příčina. Dynamický kernel32 v `0x14273d540` (CRT FlsAlloc atd.) skutečně existuje, ale s device nesouvisí.

### 2.3 Inicializace DX12 device (`0x1423a48e0`, `dev` = objekt `0x42720` B, `renderer+0x78`)

Pořadí (OBSERVED): factory `dev+0` (`0x1423a49c0`) → d3d12 0x800 (`0x1423a49f3`) → adapter (`+0x60` `EnumAdapters1`, `+0x58` `GetDesc1`) → **agsInit** (`0x1423a4bdd`, `rcx = dev+0x41ea8`) → `factory+0xe0` `CheckFeatureSupport` (tearing) → **`D3D12CreateDevice`** `0x1423a4e29` → … → **`CreateSwapChainForHwnd`** `0x1423a63ab` (`factory=[dev+0]`, queue `[dev+0x20]`, hwnd `[[0x1451c5320]+0x148]`) → 3× `GetBuffer`+`CreateRenderTargetView` do heapu `dev+0x258`, CPU base `dev+0x260`, inc `dev+0x1d8`, resources `dev+0x240..0x250` → **aréna** → timestamp buffer → `NVSDK_NGX_D3D12_Init` `0x1423a72e8`.

Aréna (OBSERVED, `0x1423a6b1e..0x1423a6dba`):
```
GetCustomHeapProperties(+0xd0, type=UPLOAD); WRITE_COMBINE→WRITE_BACK (cmove 3)
CreateCommittedResource(+0xd8) size r14 = 3*P (P=[rbp+0x940]=param3=0x1000000) -> dev+0x1058 "mTempCpuConstantBuffer"
GetGPUVirtualAddress(+0x58) -> dev+0x1060 ; Map(+0x40) -> dev+0x1018
mode [dev+0xff0]: 2 => dev+0x1080 = dev+0x1060 (alias)
               0 => mTempGpuConstantBuffer v DEFAULT heapu (heap type 1) -> dev+0x1078, VA dev+0x1080
               1 => totéž v UPLOAD heapu + Map -> dev+0x1040
per-frame pointery: dev+0x1068/+0x1088/+0x1020 = base + P
```
`[dev+0xff0]` se nastavuje na `0x1423a567a`/`0x1423a568e`: ags (AMD) `dev+0x41e9c` nebo feature bit `dev+0x41e88&0x20`. Na NVIDIA je to pravděpodobně 0 (INFERENCE). V tom případě **bound CB VA patří GPU kopii (`+0x1080`), ne mapovanému bufferu (`+0x1060`)**.

Druhý trvale mapovaný upload buffer `mTempDynamicBuffer` vzniká v `0x1423a9350` (volá ho jen renderer ctor `0x14229156d`): VA `dev+0x1108`, CPU `dev+0x1110`, size `dev+0x10f8`.

### 2.4 Rebuild při změně ratio (OBSERVED)
```
0x1422982e7 movss xmm0,[rdi+0x204]; ucomiss xmm0,[rdi+0x200]; je …
0x142298305 or dword [rdi+0x1f548],0x40 ; mov [rdi+0x1f0],1.0 ; mov byte [rdi+0x7b3b3],1
0x142298373 movss [rdi+0x204],xmm0       ; built := live
0x14229b43b test byte [rsi+0x1f548],0x40 → 0x14255cf50/0x14255d390 (resize view) nebo release view (+0x1f450/+0x1f460, vtbl+0x28) → 0x142298db0
```
DLSS feature se znovu vytváří v `0x1423ac560`→`0x142384420` (volá ho `0x14229b020`).

### 2.5 `graphics_option.ini` a fallback

- Jména klíčů jsou v tabulce `0x143e89cf0` (`DirectX12Enable` = `0x143e89d40`), hodnoty v tabulce `0x143e89fa8`: `[0]="Off"`, `[1]="On"`. Soubor je `"app:\graphics_option.ini"` (`0x142f744f0`), sekce `"GraphicsOption"`.
- Čtení (`0x140441b90`): reader `0x1404416b0` → `GetPrivateProfileStringW` (`0x140441775`, default `L""` `0x142f749e4`, buf `0x80`) → `_stricmp` (`0x142c58508`) s `"On"`. Výsledek se předává jako `r14b` do `0x140fba3a0` (`[rsp+0x50]`).
- Fallback: renderer ctor `0x142290880` volá `0x14229a700(r,0)`. Ten otevře `app:\rendering_api_fallback.txt` (`0x14350dc40`), přečte 0x10 B, a když `atoi==1`, nastaví `r+0x7b414=1`. Pak `0x14229116e cmp [rsi+0x7b414],0; cmove ecx,eax` předá DX12=false. Po vytvoření rendereru `0x141aed241` (`[r+0x7b414]!=0`) → `0x140444a50(0)` = `WritePrivateProfileStringW(...,"Off")`.
- Další zapisovače: `0x141e8f4b0` (GUI apply, `+0x7725`) a `0x140444ac0` přes `0x140445080` (celý ini).

### 2.6 Závěr k provozu bez launcheru

Launcher (`launcher.rs:35-41`, `proxy.rs:13-16`) existuje proto, aby hooky byly na místě před **(a)** `Map` arény a **(b)** `CreateRenderTargetView` hlavních cílů a pro FG před **(c)** vytvořením swapchainu.

| Varianta | Kdy kód běží | SR | FG |
|---|---|---|---|
| **A: `steam_api64.dll` proxy** (originál přejmenovat a exporty forwardovat) | první příkaz `WinMain` (`0x1413a58f8`), main vlákno, herní vlákno ještě neexistuje. Init dělat ve forwarderu `SteamAPI_RestartAppIfNecessary`, mimo loader lock | ANO, parita s launcherem | ANO: `GetProcAddress("CreateDXGIFactory2")` (`0x1423a15d0`) ještě přijde, substituce `hooks.rs:192-200` funguje |
| **B: `amd_ags_x64.dll` proxy** | ve forwarderu `agsInit` (`0x1423a4bdd`), herní vlákno, mimo loader lock, **před** `D3D12CreateDevice`, swapchainem, RTV, arénou a NGX initem | ANO: device přes patch delay IAT slotu `0x144c97a50`, NGX hooky přes IAT `GetProcAddress` stihne (NGX resolvuje až v `0x14273f307`) | ANO s úpravou: `CreateDXGIFactory2` už proběhl, ale factory pro swapchain leží v `[dev+0]` a `dev = rcx - 0x41ea8`. Lze ji upgradovat in-place (`slUpgradeInterface(&dev->factory)`) nebo hooknout slot 15 před `0x1423a63ab`. Deterministické, stejné vlákno. Nefunguje s `/disable_ags`. |
| **C: pozdní load** (`dinput8`, plugin loader) | po rendereru a po NGX initu | ANO s úpravami: arénu načíst z `dev+0x1018/0x1058/0x1060/0x1078/0x1080/0xff0`, dynamic buffer z `dev+0x10f0..0x1110`, backbuffer RTV z `dev+0x240..0x260`, scénové RTV vynutit rebuildem přes ratio setter. NGX: `observe_game_init` je zmeškaný, nutná pozdní adopce (odt1 styl: tabulka `0x1451d8808`, release a re-init) | **NE:** swapchain se nikdy znovu nevytváří (`CreateSwapChainForHwnd` jen v `0x1423a48e0`; `0x1424e7420` je oddělený „proxy“ swapchain s vlastní frontou, nesouvisí). |

Pro **NVIDIA i AMD** (na AMD přesměruje OptiScaler volání DLSS na FSR) zbývá z launcher-free variant jen B, s originálem AGS přesunutým jinam a forwardovaným: na AMD hra AGS skutečně používá (`[dev+0x41e9c]`, režim arény `[dev+0xff0]`). Varianta A přepisuje soubor Valve a vyžaduje přejmenovaný originál. Jedna DLL bez souboru hry jde jen jako launcher uvnitř DLL (`rundll32 "<mod>\mhw-dlss.dll",Launch %command%`), tedy pořád s launch option.

### 2.7 Běh: pořadí načítání (E-proxy, 2026-10-12)

Měření: retail exe ze Steamu (SHA-256 `c2ebbbd2…90ea`, EP `0x14559f310` v `.bind`, tedy **SteamStub aktivní**, `.text` na disku šifrovaný; `1ad7f7c4…bd08` z hlavičky je rozbalená kopie), spuštěné přes Steam a launcher. Mod vypnutý, místo něj jednorázový tracer (`LdrRegisterDllNotification` + `RtlCaptureStackBackTrace`) vstříknutý do suspendovaného procesu. RTX, Windows 11 26100. Časy v ms od injekce, vlákno podle zásobníku.

| ms | vlákno | modul | cesta / volající |
|---|---|---|---|
| 2 | main | `WINMM.dll`, `gameoverlayrenderer64.dll` | System32 / Steam, vstříknuto Steamem |
| 25 | main | `SHELL32.dll` | SteamStub (`.bind`, `0x1455a120d`) |
| 394 | main | `IPHLPAPI`, `VERSION`, `CRYPT32`, `SETUPAPI`, `bcrypt`, `Secur32`, `steamclient64.dll` | System32 / Steam, SteamStub |
| 840 | main | **`steam_api64.dll`** | **adresář hry**, delay-load z `WinMain` |
| 917 | main | `uxtheme.dll` | okno (`0x1422e504f`) |
| 1246 / 1248 | herní | `dxgi.dll`, `d3d12.dll` | System32 (`0x1423a158e`, `0x1423a15aa`) |
| 1270 | herní | `D3D12Core.dll` + NVIDIA UMD | zkušební `D3D12CreateDevice` (`0x1423a1738`) |
| **1635** | herní | **`amd_ags_x64.dll`** | **adresář hry**, `agsInit` (`0x1423a4bdd`) |
| 1640 | herní | `nvapi64.dll` | System32 (statický NVAPI loader `0x1400011f6`, flags `0x800`) |
| 1662 | herní | UMD znovu | skutečné `D3D12CreateDevice` (`0x1423a4e29`) |
| 2369 | herní | `dcomp`, `dwmapi` | `CreateSwapChainForHwnd` (`0x1423a63ae`) |
| 2427 | herní | `nvngx.dll` | NGX init (`0x142741981`) |
| 4583 / 4606 | herní | `DINPUT8.dll`, `XINPUT9_1_0.dll` | System32 (žádná kopie v adresáři hry), delay-load |
| 5706 | audio | `XAudio2_9.dll` | System32, Wwise sink (`0x142959d94`, plain `LoadLibraryA`) |
| 5773 | síť | `WININET.dll` | System32 |

Závěry:

- **OBSERVED:** ze složky hry se před device načtou jen `steam_api64.dll` a `amd_ags_x64.dll`. Oba jsou soubory hry, proxy pod jejich jménem originál přepisuje.
- **OBSERVED:** `gameoverlayrenderer64.dll` hookuje `LoadLibraryExW` (`+0xad255` je v zásobníku každého loadu). Search order to nemění.
- `XAudio2_9.dll` by se z adresáře hry načetl (plain `LoadLibraryA`, `0x142959d8e`), ale až 3,3 s po swapchainu: varianta C.
- **OBSERVED (statika `amd_ags_x64.dll` 5.2.0.0):** `agsInit` bezpodmínečně volá `LoadLibraryA("atiadlxx.dll")`, pak `atiadlxy.dll` (`0x180005799` → `0x180001e00`). Na NVIDIA chybí, takže kopie v adresáři hry by se načetla v `agsInit` bez přepsání souboru hry. Na AMD je to skutečné ADL ze System32, a proto je **vyřazeno**.
- **OBSERVED:** nenulový návrat `agsInit` hra bere jako „bez AGS“ (`0x1423a4be4 jne 0x1423a4c51`), `/disable_ags` (`0x14351d4d8`) ho vynechá. Z AGS hra importuje jen `agsInit`, `agsDeInit` a čtyři DX11 funkce.
- **Stracker's Loader 4.0** (`dinput8.dll` → `LoadLibraryA("./loader.dll")` → `nativePC\plugins`) se načte s `DINPUT8`, po rendereru. S launcherem koexistuje: launcher spouští hru s pracovním adresářem ve složce hry, takže relativní `./loader.dll` se najde. Ověřeno během: gate, swapchain Streamline, `CreateFeature … Success`. `QuestLoader` a `MonsterLoader` hookují 10 funkcí (`0x140249b70` … `0x141ccf6c0`), žádná nesdílí funkci s 16 gate sites (nejbližší pár `0x8881` B). Jako hostitel modu je pozdě (varianta C).

## 3. Doporučené změny pro Gargauth

1. **`hooks.rs:107-108` + `hooks.rs:170-175`:** zpracovat i ordinal. Je-li `module` d3d12 a `result == GetProcAddress(d3d12,"D3D12CreateDevice")` (nebo ordinal 101), vrátit `proxy::D3D12CreateDevice`. Alternativa: pod launcherem rovnou přepsat delay IAT slot `0x144c97a50` (`.data`, RW) na `proxy::D3D12CreateDevice`. Pak poběží preferovaná cesta `gpu.rs:3797-3818` `install_from_device` a ne závod `bootstrap_scratch` (`lib.rs:338`). `proxy::D3D12CreateDevice` musí snést `out == NULL` (3 probe callsity).
2. **`gpu.rs:3822-3826`:** opravit komentář. Příčinou je delay-import ordinal #101, ne dynamický kernel32.
3. **`gate.rs:638-663`:** číst jen sekci `[GraphicsOption]` a respektovat, že hlavička s UTF-8 BOM pro hru **neexistuje** (hra potom jede v DX11). Chybějící klíč, sekci nebo soubor brát jako DX11 (`0x1404416b0` vrací false a DX12=false). Navíc kontrolovat `rendering_api_fallback.txt` = „1“, protože hra pak jede v DX11 bez ohledu na ini (`0x14229a79a`).
4. **`gate.rs:681-695` `note_directx11_lookup`:** v DX12 se `NVSDK_NGX_D3D11_*` ani `D3D11CreateDevice` exe-side nikdy neresolvují (callsity jen v `0x14273edd0`, `0x1423a4233`). Detekce je **CONFIRMED** jako spolehlivá. Přidat `CreateDXGIFactory1` (jen DX11, `0x1423a3e9e`).
5. **`launcher.rs:25-37`, `proxy.rs:3-16`:** opravit tabulku. `dinput8` je delay-load (flags 0). `steam_api64.dll` i `amd_ags_x64.dll` se z adresáře hry načtou **před** rendererem. `winmm` se načítá delay-loadem na začátku run loopu.
6. **Volitelný launcher-free režim** (§2.6 A/B). Inicializaci dělat ve forwarded exportu, ne v `DllMain` (loader lock, `lib.rs:357-368`).
7. **`bindings.rs:600-620` (registry map):** při pozdním startu registry seedovat z `dev+0x1018/0x1060/0x1080/0xff0` a `dev+0x1108/0x1110/0x10f8`. Bound CB VA může ležet v `mTempGpuConstantBuffer` (`+0x1080`), CPU obsah je na stejném offsetu v `+0x1018`.

## 4. UNKNOWN a runtime experimenty

| UNKNOWN | Experiment (Gargauth) | Co rozhodne |
|---|---|---|
| Je `#101` opravdu `D3D12CreateDevice`? | `hooks.rs` `hook_get_proc_address`: logovat `module_name` a ordinal, když `name ≤ 0xFFFF`, a porovnat `result` s `GetProcAddress(d3d12,"D3D12CreateDevice")` | rovnost adres |
| ~~`winmm`: System32, nebo kopie z adresáře hry?~~ | **Rozhodnuto (§2.7):** System32, načte ho Steam overlay 2 ms po startu | – |
| Hodnota `[dev+0xff0]` na NVIDIA | v `install` po vytvoření device: počkat na `0x1423a72e8` / první frame, přečíst `[[0x1451c4480]+0x78]+0xff0`, `+0x1060`, `+0x1080` | 0 ⇒ DEFAULT kopie (bound VA ≠ mapovaná VA) |
| ~~Je `amd_ags_x64.dll` v adresáři hry a volá se `agsInit` i na NVIDIA?~~ | **Rozhodnuto (§2.7):** ano, 1635 ms, po zkušebním a před skutečným `D3D12CreateDevice` | – |
| Funguje `slUpgradeInterface` na už vytvořené factory (varianta B)? | testovací proxy `amd_ags_x64.dll`: v `agsInit` upgradovat `[rcx-0x41ea8]` | swapchain je SL proxy (`slGetNativeInterface` ≠) |
| Ratio rebuild znovu vytváří RTV a resources | hook `CreateRenderTargetView`, zavolat setter `0x14229be60` s 0.67, pak 1.0 | nové RTV pro scénové targety |
| ~~Retail exe: SteamStub aktivní?~~ | **Rozhodnuto (§2.7):** ano, EP `0x14559f310` v `.bind`; při `CREATE_SUSPENDED` je `.text` šifrovaný (gate čeká na rozbalení, v logu ~1 s) | – |
| První start bez `graphics_option.ini`: DX11? | smazat ini (záloha), spustit a sledovat log „GraphicsOptionDX12: No/Yes“ přes hook `0x140a114a0` nebo `[load]` d3d11 | No ⇒ DX11 |
| ~~Timing `XAudio2_9.dll` vůči rendereru~~ | **Rozhodnuto (§2.7):** 5706 ms na audio vlákně, po swapchainu i NGX; proxy kandidát ne | – |

## 5. Mapa adres

| Adresa | Význam |
|---|---|
| `0x142e13020` / `0x142e13028` | IAT `GetPrivateProfileStringW` / `WritePrivateProfileStringW` |
| `0x142e13118` / `0x142e13120` / `0x142e13178` / `0x142e132f0` / `0x142e132f8` | IAT `GetProcAddress` / `LoadLibraryA` / `LoadLibraryExA` / `LoadLibraryExW` / `LoadLibraryW` |
| `0x142167b90` | `__delayLoadHelper2` (`LoadLibraryExA(…,0,0)` `0x142167ccc`) |
| `0x144c97a50` | delay IAT `d3d12!#101` (= `D3D12CreateDevice`), stub `0x14273de03` |
| `0x144c979f8` / `0x14273e288` | delay IAT / stub `amd_ags_x64!agsInit` |
| `0x144c97ae0` | delay IAT `steam_api64!SteamAPI_RestartAppIfNecessary` |
| `0x142742a48` | EP (CRT) |
| `0x1413a58d0` | WinMain |
| `0x1413a57b0` | app run (`0x14047fc00` ctor, `0x140480320` setup, `0x1422e4670` loop) |
| `0x1422e5010` / `0x1422e4ae0` | app ctor (CreateThread suspended) / start herního vlákna |
| `0x141aec7d0` | `sMhMain` ctor (DTI `0x14500e758`, velikost `0xb0530`) |
| `0x140441b90` / `0x1404416b0` | čtení DirectX12Enable / ini reader |
| `0x140444a50`, `0x140444ac0` | zápis DirectX12Enable |
| `0x143e89cf0` / `0x143e89fa8` | tabulka jmen klíčů / tabulka `Off,On` |
| `0x140fba3a0` → `0x142290880` | vytvoření rendereru (`0x7b710` B) / ctor |
| `0x14229a700` | `rendering_api_fallback.txt` (mode 0 read, 1 write, 2 delete) |
| `0x14229c570` | zápis fallbacku, pokud ještě není |
| `0x1422937c0` | smazání markeru |
| `r+0x7b414` | flag fallbacku |
| `0x1422994a0` | renderer init (alloc dev `0x42720`, `r+0x78`) |
| `0x1423a0950` | probe + DX volba |
| `0x1424e9990` / `0x1424e9630` / `0x1451cc080` | DX12 setter / getter / flag |
| `0x1423a48e0` | DX12 device init |
| `0x1423a3e20` | DX11 device init |
| `dev+0` factory, `+0x18` ID3D12Device, `+0x20` queue, `+0x1470` swapchain | DX12 objekt |
| `dev+0x240/0x258/0x260/0x1d8` | backbuffer resources / RTV heap / CPU base / inc |
| `dev+0x1018/0x1058/0x1060` | aréna CPU ptr / resource / VA (48 MiB) |
| `dev+0x1078/0x1080/0xff0` | GPU kopie CB / VA / mode |
| `dev+0x10f8/0x1108/0x1110` | `mTempDynamicBuffer` size / VA / CPU |
| `dev+0x13f8/0x1400` | D3D11 device / immediate context |
| `dev+0x41ea8` | `AGSContext*` |
| `0x1423a9ce0` | present + `GetDeviceRemovedReason` (`+0x128`) |
| `0x142553ff0` | HRESULT fatal handler |
| `0x1421fd060` | fatal (`0xdeadbeef` `0x1421fd0c9`) |
| `0x1423ac9e0` | recreate backbuffer RTV |
| `0x1423a22f0` | ResizeBuffers + HDR colorspace |
| `0x1423ac040` | adapter re-probe |
| `0x1422980b0` | per-frame ratio porovnání `+0x200`/`+0x204` |
| `0x14229be60` | ratio setter |
| `0x14229b020` | rebuild (flag `0x40`) |
| `0x1451d87d0..0x1451d8807` / `0x1451d8808..0x1451d883f` | NGX tabulka D3D11 / D3D12 (Init, Shutdown, GetScratch, GetParameters, Create, Evaluate, Release) |
| `0x14273edd0` / `0x14273f230` | `NVSDK_NGX_D3D11_Init` / `NVSDK_NGX_D3D12_Init` (exporty) |
| `0x14273fe90` | NGXLoadSigned (`nvngx.dll` vedle exe, pak driver store) |
| `0x142384120` / `0x1423842a0` | NGX init DX11 / DX12 |
| `0x142384420` | CreateFeatures (sdílené) |
| `0x1423837a0` | Shutdown (sdílené) |
| `0x142383ef0` | GetOptimalSettings (sdílené) |
| `0x142383830` / `0x142383af0` | DX11 EvalISP / EvalSS |
| `0x142383990` / `0x142383cf0` | DX12 EvalISP / EvalSS |
| `0x14354e108` / `0x14354e4b0` | vtable render contextu DX11 / DX12 (slot 25 = DLSS: `0x1425996d0` / `0x14259dae0`) |
| `0x1425985b0` / `0x14259b760` | ctor render contextu DX11 / DX12 |
| `0x14228f3b0`, `0x1423d0e70` | camera setup, matice (API-neutrální) |

### Odhad rozsahu DX11 portu

- **API-neutrální, převzít beze změny (~5 kLOC):** `hijack.rs` (ratio setter, config), `camera.rs`, `native.rs` (herní hooky), `gate.rs`, `config.rs`, `tune.rs`, `pathdump.rs`, `logger.rs`. Herní strana DLSS 1.x (CreateFeatures, OptimalSettings, ratio, rebuild) je společná.
- **Přepsat na D3D11 (~12–15 kLOC, jádro):** `gpu.rs` (3990 ř.: vtable hooky device/cmdlist → ID3D11Device/Context), `bindings.rs` (map registry → `ID3D11DeviceContext::Map`, CB přes `*SSetConstantBuffers`), `mv.rs`, `hudless.rs`, `sharpen.rs`, `states.rs`, `pipeline.rs`, `rootsig.rs` (v D3D11 odpadá), `dlss.rs`, `ngx.rs` a `ngxhost.rs` (D3D11 varianta: tabulka `0x1451d87d0`, `NVSDK_NGX_D3D11_*`, DX11 evaluate `0x142383af0`).
- **Odpadá:** `streamline.rs`, `fg.rs`, `fgmask.rs`, `swap.rs`. DLSS-G v D3D11 Streamline nepodporuje (INFERENCE ze SL dokumentace), takže DX11 port = **jen SR**.
- DX11 backend nahrává do immediate contextu (`dev+0x1400`), což zjednodušuje hookování: jeden context, žádné command listy.
