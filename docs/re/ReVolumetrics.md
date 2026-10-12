# ReVolumetrics: volumetrická mlha a zubaté hrany v mlze

Cíl: `MonsterHunterWorld.exe` build 15539686 (PE ts `0x66d13cb6`). Zdroje:
Ghidra (projekt `C:\Users\Ondra\dev\ghidra\proj`), dump shaderů z modu
(`[Dev] DumpShaders`), disassembly přes `D3DDisassemble`, dva snímky
z RenderDocu (nativní hra bez modu, 4K, herní TAA) a čtení paměti běžící hry.
OBSERVED = viděno v kódu, snímku nebo paměti; INFERENCE = odvozeno.

## 1. Příznak

Hrany geometrie v mlze (kmen a větve proti zamlžené obloze) mají schody
velké ~2 px výstupu. S jitterem se nehýbou, takže je nevyhladí herní TAA
ani DLSS: zůstávají i v průměru 32 jitterovaných snímků. Stejné je to bez
modu s herním TAA, v 0.2 i v rewritu (OBSERVED, F12 screenshoty).
S vypnutou volumetrikou zmizí.

## 2. Řetěz průchodů (RenderDoc, nativní 4K)

| event | průchod | výstup |
|---|---|---|
| 21975… | integrace froxelů (`rwFroxels`), `CBVRCompute` | 3D |
| 22098… | `rwLuminance`/`rwTransmittance` po dlaždicích | poloviční |
| 23721–23732 | `CSVRCheckers` (+`_RECOMPUTE`), `tileDepth` | poloviční |
| 23738 | `rwCheckerDepth`/`rwCheckerLuminance`/`rwCheckerTransmittance` | 1920×1080 |
| 23744 | **`CSVRUpsampleBilateral`** → `rwFinalL`/`rwFinalT` | 3840×2160 |
| 23749 | near-depth průchod přes `ditherList` | 3840×2160 |
| 24087 | `CBVRFilter`: zmenšení, jeden vzorek na blok 2×2 (pevná pozice) | 1920×1080 |
| 24095, 24101 | `CBVRGaussian`: rozmazání podle hloubky, H a V | 1920×1080 |
| 24111 | luminance/tonemap PS: `scéna × T + L`, L/T ze 24101 | 3840×2160 |
| 24118 | herní TAA (`CB_TemporalAA2`) | 3840×2160 |

Schody vznikají na dvou místech (OBSERVED v exportech textur):

1. **Upsample (23744)** rozhoduje po blocích 2×2: v rovné dlaždici bere
   bilineární průměr, v hranové váhu 1/|Δz| a jinak nejbližší vzorek. Už
   výstup ve 4K má schody v polovičním rozlišení.
2. **Zmenšení a rozmazání (24087–24101)** vrátí mlhu do 1080p a průchod
   24111 ji natáhne bez ohledu na hloubku.

## 3. Přepínač `mDownsampleVolume`

Vlastnost objektu tonemap/luminance (stejná třída jako `mToneMapType`,
`mEdgeSharpness`, `mpL`, `mpT`). Registrace vlastností `FUN_1424cf2e0`,
vtable `0x143538400` (slot 3), konstruktor `0x1424ce130`, destruktor
`0x1424ce380`.

| pole | offset |
|---|---|
| `mEdgeSharpness` (float, 0.0003) | `+0x198` |
| `mDownsampleVolume` (bool, výchozí 1) | `+0x210` |
| `mpL` / `mpT` | `+0x218` / ? |
| `mpLacc` / `mpTacc` | `+0x228` / ? |

Aktivní instance je v globálu **`0x1451cbc08`**. Destruktor ho pod critical
section `0x1451cbc48` nuluje, pokud ukazuje na sebe (OBSERVED v kódu, za
běhu ukazoval na instanci nalezenou podle vtable).

Hra ho čte v každém snímku (OBSERVED, druhý snímek z RenderDocu po zápisu
0 do běžící hry): průchody 24087–24101 zmizí a průchod 24111 čte přímo
`rwFinalL`/`rwFinalT` ve 4K.

## 4. Experiment

| varianta | výsledek |
|---|---|
| náhradní upsample, downsample zapnutý | beze změny (zmenšení vrátí schody) |
| `mDownsampleVolume = 0`, herní upsample (bez modu, TAA) | skoro beze změny (schody z upsamplu) |
| `mDownsampleVolume = 0`, herní upsample (mod, DLSS Quality) | ve hře pořád zubaté |
| náhradní upsample + `mDownsampleVolume = 0` (mod, DLSS Quality) | obrys kmene hladký |

V modu je to `[Upscaling] DownsampleVolume = false` (restart): render
(`crates/render/src/sys/volumetric.rs`) postaví herní upsample z náhradního
shaderu, engine (`tune.rs`) drží `mDownsampleVolume` vypnuté pod herním
zámkem, ale až když je náhradní upsample opravdu postavený.

Pozor na vrstvy: ReShade s add-onem, který poslouchá události pipeline
(RenoDX), přestaví `CreateComputePipelineState` na
`ID3D12Device2::CreatePipelineState` se streamem. Mod ten stream dřív jen
četl, takže s RenoDX zůstal herní upsample a `mDownsampleVolume = 0`
pustil do obrazu nerozmazanou mlhu s časovým šumem z ditheringu (OBSERVED:
šum ve vstupu DLSS, korelace mezi snímky 0,002). Teď se náhrada dělá
i ve streamu a přepínač se drží jen s postaveným upsamplem.

Náhradní upsample (`crates/render/src/sys/vr_upsample.hlsl`) počítá
každý pixel zvlášť: bilineárně ze 4 vzorků v polovičním rozlišení s váhou
podle podobnosti s plnou (jitterovanou) hloubkou. Když se nic neshoduje,
sáhne do sousedních dlaždic, nakonec vezme vzorek s nejbližší hloubkou.
Okraj mlhy tak sleduje jitterovanou geometrii. Nahrazuje tři varianty podle
velikosti dlaždice, hash FNV-1a bytecode:

| hash | `TILE_SHIFT` |
|---|---|
| `d764231394df273e` | 2 |
| `c7ec8d17b7d29f74` | 3 |
| `d98ea60b1e806cf2` | 4 |

Purpurová diagnostika potvrdila, že upsample píše ~66 % obrazu včetně
stromu (zbytek jsou blízké objekty a dlaždice near-depth průchodu).

## 5. `sVolumeRenderer` (pro úplnost)

Globál `0x1451c6c30`, vtable `0x14351a750`. Vlastnosti: `mEnabled` `+0x38`,
`mQualityPreset` `+0xe0`, `mDepthThreshold`/`mAngleThreshold` `+0x140`/`+0x144`,
`mJitterType` `+0x354` (3), `mUseNearDepth` `+0x358`, bitové přepínače
`+0xec`, hodnoty z menu `+0x3c0..0x3c8`. Změny prahů a typu jitteru schody
neodstranily. **Zápis do těchto vlastností za běhu jednou shodil hru** při
výběru varianty shaderu (`+0x263db05`, čtení z 0x58), proto na ně nesahat.
