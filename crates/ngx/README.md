# mhw-ngx

NGX for mhw-dlss: the `NVSDK_NGX_Parameter` ABI, the NGX session the game
opens (intercepted, re-issued with API 21, host detection, availability
answer) and the DLSS super-resolution feature lifecycle. Contract:
`docs/ARCHITECTURE.md` § mhw-ngx; RE facts: `docs/re/ReDlss1Path.md`.

Depends on `mhw-core` and `mhw-win` only. Log targets: `ngx` (session),
`dlss` (feature).

## Modules

| module | pure | what |
|---|---|---|
| `param` | yes¹ | vtable `Slot`s in MSVC order, typed `Value`/`Write`, parameter `names`, `Parameters` (typed get/set through the vtable, `verify_slots`) |
| `dlss` | yes | `FeatureDesc` (sizes, `Quality`, `RenderPreset`, `CreateFlags`, output subrects) → `create_writes`; `EvalParams` + `RawInputs` → `eval_writes`; validation; stand-in handles |
| `init` | yes | `GameInit`, `InitOptions`/`Identity`/`InitTail`/`InitAction`, `plan` (entry point per host), `FeatureCommonInfo`, `Availability`, `availability_writes` |
| `host` | yes | driver vs foreign classification, exports a foreign host needs |
| `retire` | yes | `FrameClock` (distinct frames), `RetireQueue` (`RETIRE_FRAMES` = 16) |
| `budget` | yes | `CreateBudget`: one creation attempt per frame, 5 per description |
| `version` | yes | `VS_VERSIONINFO` of a loaded image or a file (`read_file_version` reads headers + `.rsrc` only) |
| `session` | Windows | the seven intercepted entry points, `Init` redirect, binding, capability, spoof |
| `feature` | Windows | create / evaluate / recreate / retire, introspection |

¹ `Parameters` is x86-64 only; its calls use `extern "win64"` so the host
tests run them against a stand-in object.

## Wiring (what `mod` does)

1. **GetProcAddress.** `mod` owns the `KERNEL32!GetProcAddress` IAT hook.
   For every name: call the real `GetProcAddress(module, name)`, then
   `session::set_original(name, module, address)`; if that returns `true`,
   give the game `session::intercept(name).unwrap()` instead. The game's
   static NGX library resolves its seven table entries inside its `Init`
   wrapper before calling `Init`; the resolve of `NVSDK_NGX_D3D12_Init`
   decides the host. Optionally feed library loads to
   `session::note_core_load(requested, module)` for the host log line.
   Intercepted: `Init`, `Shutdown`, `GetScratchBufferSize`,
   `GetParameters`, `CreateFeature`, `EvaluateFeature`, `ReleaseFeature`
   (`session::Entry`). D3D11 names are never intercepted.
2. **Init policy.** Before the game creates its device, register
   `session::set_init_policy(fn(&GameInit) -> InitAction)`. It is called
   once, on the game's thread, inside the game's `Init`, without locks held:
   * `InitAction::Redirect(InitOptions::new(mod_folder))` — API 21
     (`init::API_VERSION`), the game's app id `0x5f8a85f`, zero tail.
     Dev A/B: `identity: Identity::ProjectId { .. }` (core
     `Init_ProjectID`) or `Identity::AppId(_)`; `tail: InitTail::Game`
     forwards the adapter LUID the game's stack holds (0.2 behaviour, RE
     E7); `timeout: Some(..)` when Streamline already faulted in this
     session.
   * `InitAction::PassThrough` — the mod stands down (a foreign host is
     answered `FeatureNotSupported` instead: its `Init` misreads the game's
     arguments).
   * `InitAction::Refuse`.
   The policy can check `mod_folder/nvngx_dlss.dll` (`init::SNIPPET`,
   `version::read_file_version`).
3. **Readiness.** `session::ready()` is true once the redirected `Init`
   succeeded, the feature entry points are bound and NGX's capability block
   says `SuperSampling.Available = 1`; `session::refusal()` says why not.
4. **Availability answer.** Once the engine's DLSS 1 path is taken over:
   `session::set_spoof_availability(true)`. The game's `GetParameters` then
   reports SuperSampling **and** ImageSignalProcessing available — only
   while `ready()` and NGX's own block says SuperSampling is available.
5. **Stand-in handles.** The engine's `CreateFeatures` stand-in gives the
   game `dlss::STAND_IN_SUPER_SAMPLING` (ctx+0x38) and
   `dlss::STAND_IN_IMAGE_SIGNAL` (ctx+0x40); the session keeps them away
   from NGX's `ReleaseFeature`/`EvaluateFeature`.
6. **Per frame** (render thread, upscale slot):
   ```rust
   let desc = FeatureDesc { render, display, quality: Quality::for_sizes(render, display),
       preset: settings.upscaling.preset.into(),
       flags: CreateFlags { is_hdr, mv_low_res: CreateFlags::mv_low_res_for(render, display),
                            mv_jittered: false, depth_inverted, auto_exposure: true },
       output_subrects: false };
   let params = EvalParams { jitter: mhw_core::math::dlss_jitter(px), mv_scale,
       subrects: Subrects::full(render), reset };
   unsafe { feature::evaluate(&list, &desc, &DlssInputs { colour, motion, depth, output,
       exposure: None }, &params, frame) }
   ```
   and `feature::tick(frame)` every frame (retired items age only on
   distinct frame numbers). `feature::release(frame)` when DLSS is turned
   off; `feature::retire_resource(resource, frame)` for an input replaced
   while a list may still use it; `feature::retry_creation()` after
   `EvalError::GaveUp` if something changed.
   The list must be on the same side of any proxy layer as the device NGX
   was initialised with (the game's device as the game holds it — with
   ReShade, ReShade's proxy list).

## Introspection

Always: `session::ready/refusal/availability/host`, `feature::live`,
`feature::snippets` (loaded `nvngx_dlss.dll` paths and versions, read at
each creation). With feature `dev`: `session::snapshot()` (host, game
`Init` call, route, bound module, capability, spoof state, per-entry call
counts) and `feature::snapshot()` (live feature, retire queue, creation
failures, counters, last evaluation inputs/params/status, snippets).
`mhw-dlss`'s `dev` feature must enable `mhw-ngx/dev`.

## Facts this crate relies on

* `NVSDK_NGX_Parameter` slots: Set `void*, D3D12, D3D11, i32, u32, f64,
  f32, u64` = 0..7, Get the same 8..15, `Reset` 16 (game wrappers
  `0x14273e370..0x14273e8f0`).
* Game `Init`: `(0x5f8a85f, L"DLSS", device, 0x12)`; stack slots 5/6 hold
  the adapter LUID and 0. Table `0x1451d8808`: Init, Shutdown, Scratch,
  GetParameters, Create, Evaluate, Release.
* The game needs features 1 (SS) and 9 (ISP) available, or it hides DLSS.
* `nvsdk_ngx_defs.h` (310.x) for names, enums, `FeatureCommonInfo` (40 B);
  OptiScaler's `NVNGX_DLSS_Dx12.cpp` for the core export signatures
  (`Init_Ext`, `Init_ProjectID`).

## Open questions (runtime)

* E4: whether NGX applies an OTA library to app id `0x5f8a85f` instead of
  the mod's `nvngx_dlss.dll` — compare `feature::snippets()` with
  `Identity::ProjectId`.
* E7: whether the legacy `Init` at version 21 reads the stack tail —
  compare `InitTail::Zero` with `InitTail::Game`.
* Whether the driver-store `nvngx.dll` exports the full modern surface;
  binding prefers `_nvngx.dll` (0.2) and logs the module used.
