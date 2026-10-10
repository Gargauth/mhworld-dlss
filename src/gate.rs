//! One decision about whether this is the game build the mod knows.
//!
//! Everything this mod does to the engine rests on addresses in one build of
//! `MonsterHunterWorld.exe`: eight functions that are detoured, one that is
//! called (the engine's own setter for the render ratio, checked at both its
//! ends), three objects reached through the instructions that load them, two
//! instructions that say where the jitter lives, and one instruction whose
//! operand is rewritten for the mip bias. Checked one by one, where each is
//! used, a build that moved some of them would get some of the mod: a
//! feature creation answered with stand-in handles and an evaluation that
//! then handed those handles to the driver, or settings written at offsets
//! into an object that had changed shape.
//!
//! Here the question is asked once, of every site together:
//!
//! * while the executable is still being unpacked the sites are ciphertext,
//!   so a mismatch then means nothing and the question is asked again;
//! * once every site matches, everything is installed in one step, with the
//!   game's other threads suspended, or nothing is;
//! * once the game's own code is known to be running, a site that still does
//!   not match means a different build. Then nothing is installed, the
//!   game's NGX calls pass through as they came, no engine setting is
//!   written, and one line in the log says what was expected and what was
//!   found.
//!
//! The second half of this file is the other latch the rest of the mod asks:
//! whether DLSS can be used at all in this session. A different build is one
//! reason it cannot; a missing snippet, a driver that says no, and feature
//! creation that keeps failing are others (`ngx.rs`, `dlss.rs`).

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::native::{image, readable, CodeWrite, Detour, WriteError, MAX_WRITE};

/// A place in the game's code and the bytes that must be there.
pub(crate) struct Site {
    pub what: &'static str,
    pub rva: usize,
    pub bytes: &'static [u8],
    /// Which of `bytes` are not compared: an operand this mod itself
    /// rewrites, which must not make the site unrecognisable afterwards.
    pub wild: std::ops::Range<usize>,
}

impl Site {
    pub(crate) const fn new(what: &'static str, rva: usize, bytes: &'static [u8]) -> Site {
        Site {
            what,
            rva,
            bytes,
            wild: 0..0,
        }
    }
}

/// What was found at a site.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Probe {
    /// Not committed memory: outside the image, or not there yet.
    Unmapped,
    Matches,
    /// Readable and different, with what was read.
    Differs([u8; MAX_WRITE]),
}

/// Whether `found` is what `site` expects.
pub(crate) fn matches(site: &Site, found: &[u8]) -> bool {
    found.len() == site.bytes.len()
        && found
            .iter()
            .zip(site.bytes)
            .enumerate()
            .all(|(index, (found, wanted))| site.wild.contains(&index) || found == wanted)
}

unsafe fn probe(base: usize, site: &Site) -> Probe {
    let length = site.bytes.len();
    if base == 0 || !readable(base + site.rva, length) {
        return Probe::Unmapped;
    }
    let found = std::slice::from_raw_parts((base + site.rva) as *const u8, length);
    if matches(site, found) {
        Probe::Matches
    } else {
        let mut copy = [0u8; MAX_WRITE];
        copy[..length].copy_from_slice(found);
        Probe::Differs(copy)
    }
}

/// What to do about a set of probes.
#[derive(PartialEq, Debug)]
pub(crate) enum Decision {
    /// Not unpacked yet, as far as anything shows. Ask again.
    Wait,
    /// Every site matches. Install everything.
    Engage,
    /// Unpacked, and not the build this mod knows. Install nothing.
    StandDown,
}

/// The whole rule. `unpacked` is whether the game's own code is known to be
/// running, which is the only thing that tells ciphertext from a different
/// build: both are simply bytes that do not match.
pub(crate) fn decide(probes: &[Probe], unpacked: bool) -> Decision {
    if !probes.is_empty() && probes.iter().all(|probe| *probe == Probe::Matches) {
        Decision::Engage
    } else if unpacked {
        Decision::StandDown
    } else {
        Decision::Wait
    }
}

/// Every site, from the three modules that depend on one.
fn sites() -> &'static [&'static Site] {
    static ALL: std::sync::OnceLock<Vec<&'static Site>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        crate::hijack::SITES
            .iter()
            .chain(crate::native::SITES.iter())
            .chain(crate::tune::SITES.iter())
            .collect()
    })
}

/// The executable this mod was made against: Steam build 15539686.
///
/// From its PE header. A cheap first look and a line in the log, not the
/// test: the bytes at the sites are what decide, because they are what the
/// mod actually depends on. A header that differs with every site matching
/// is said and accepted; a header that matches with a site differing is a
/// stand-down.
const KNOWN_TIMESTAMP: u32 = 0x66d1_3cb6;
const KNOWN_SIZE_OF_IMAGE: u32 = 0x055d_3000;

/// `TimeDateStamp` and `SizeOfImage` of the loaded image.
unsafe fn header(base: usize) -> Option<(u32, u32)> {
    if base == 0 || !readable(base, 0x400) {
        return None;
    }
    // `e_lfanew`, then the signature, the file header and the optional
    // header: the timestamp is 8 bytes into the NT headers and SizeOfImage 80.
    let nt = (base as *const u8).add(0x3c).cast::<u32>().read_unaligned() as usize;
    if nt == 0 || nt > 0x300 {
        return None;
    }
    let nt = (base + nt) as *const u8;
    if nt.cast::<u32>().read_unaligned() != 0x0000_4550 {
        return None;
    }
    Some((
        nt.add(8).cast::<u32>().read_unaligned(),
        nt.add(80).cast::<u32>().read_unaligned(),
    ))
}

fn describe_header(base: usize) -> String {
    match unsafe { header(base) } {
        Some((timestamp, size)) if timestamp == KNOWN_TIMESTAMP && size == KNOWN_SIZE_OF_IMAGE => {
            format!(
                "the executable's header is the known build's (TimeDateStamp {timestamp:#010x}, \
                 SizeOfImage {size:#x})"
            )
        }
        Some((timestamp, size)) => format!(
            "the executable's header has TimeDateStamp {timestamp:#010x} and SizeOfImage \
             {size:#x}, where the known build has {KNOWN_TIMESTAMP:#010x} and \
             {KNOWN_SIZE_OF_IMAGE:#x}"
        ),
        None => "the executable's header could not be read".to_string(),
    }
}

const PENDING: u8 = 0;
const ENGAGED: u8 = 1;
const STOOD_DOWN: u8 = 2;
/// The host is the project's own test program and not the game. See
/// `not_the_game`.
const STAND_IN: u8 = 3;

static STATE: AtomicU8 = AtomicU8::new(PENDING);
/// One attempt at a time, whichever thread makes it.
static ATTEMPT: Mutex<()> = Mutex::new(());
/// Set once the game's own code has demonstrably run.
static RUNNING: AtomicBool = AtomicBool::new(false);
static RUNNING_BECAUSE: Mutex<&'static str> = Mutex::new("");
/// Polls that found a mismatch with the game already running.
static CONFIRMED: AtomicU32 = AtomicU32::new(0);
/// How many of those make a stand-down. The game's code is unpacked before
/// its entry point is reached, so one would do; a few cost fifty
/// milliseconds and cover a sign of life that came early.
const CONFIRMATIONS: u32 = 25;
/// Attempts that found a thread inside the code to be replaced.
static BUSY: AtomicU32 = AtomicU32::new(0);
const BUSY_LIMIT: u32 = 1000;

/// Whether the takeover is in: every site matched and every detour is
/// written. Nothing that touches the engine may run before this is true.
pub fn engaged() -> bool {
    STATE.load(Ordering::Relaxed) == ENGAGED
}

/// Whether the question has been answered with no.
pub fn stood_down() -> bool {
    STATE.load(Ordering::Relaxed) == STOOD_DOWN
}

/// Whether this mod's NGX is to be used: in the game with the takeover in,
/// and in the test stand-in, which has no engine to take over.
///
/// Not a substitute for `engaged`. Anything that reads or writes the
/// engine's code or data asks `engaged`, which the stand-in never satisfies.
pub fn ngx_is_ours() -> bool {
    matches!(STATE.load(Ordering::Relaxed), ENGAGED | STAND_IN)
}

/// Note that the game's own code is running, which means its executable has
/// been unpacked. One relaxed load once it is known.
pub fn note_game_running(because: &'static str) {
    if RUNNING.load(Ordering::Relaxed) {
        return;
    }
    *RUNNING_BECAUSE.lock() = because;
    RUNNING.store(true, Ordering::SeqCst);
}

/// Start asking, from a thread, until there is an answer.
///
/// Early matters: the engine allocates the dense motion target only if its
/// DLSS features already exist when a render node is configured, so the
/// detours go in at the first moment the code is readable, which is neither
/// at load nor at any hook this mod owns.
pub fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| {
        let began = std::time::Instant::now();
        let mut slow = false;
        loop {
            attempt(None);
            if STATE.load(Ordering::SeqCst) != PENDING {
                return;
            }
            if !slow && began.elapsed() > std::time::Duration::from_secs(120) {
                slow = true;
                crate::log!(
                    "[bld] after 120 s the game's code is still not readable at every site \
                     this mod checks, and the game has not started its renderer. Still \
                     waiting, less often; nothing is installed until all of them match"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(if slow { 100 } else { 2 }));
        }
    });
}

/// Answer now, because the caller is the game itself and cannot be told
/// "later": whatever does not match at this point is not going to.
///
/// Returns whether this mod's NGX is to be used from here on.
pub fn decide_now(because: &'static str) -> bool {
    note_game_running(because);
    attempt(Some(because));
    ngx_is_ours()
}

/// The name of the project's test program, `tools/standin`.
const STAND_IN_NAME: &str = "standin.exe";

/// The host's image is too small to contain the sites, so it is not the
/// game, whatever build. Decided at once: there is nothing to wait for.
///
/// One host is let through to a mode of its own: `tools/standin`, which
/// exists to run this mod's render hooks and its NGX path without the game.
/// It is recognised by its file name and only after the test above, so no
/// name can make the game itself be treated as one. In that mode nothing of
/// an engine is touched -- there is none -- and everything else runs.
fn not_the_game(image_size: usize, furthest: usize) {
    let host = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    if host.eq_ignore_ascii_case(STAND_IN_NAME) {
        STATE.store(STAND_IN, Ordering::SeqCst);
        crate::log!(
            "[bld] the host is {host}, this project's test stand-in, and not the game: its \
             image is {image_size:#x} bytes and the game's code sites reach {furthest:#x}. No \
             engine detour is installed and no engine setting is written; the render hooks \
             and NGX run as they do in the game"
        );
        return;
    }
    stand_down(&format!(
        "the host executable {host} is not Monster Hunter: World: its image is \
         {image_size:#x} bytes and the code this mod checks lies up to {furthest:#x} into \
         the game's"
    ));
}

/// The diagnostic that shows a stand-down without a different build.
fn deliberately_wrong() -> bool {
    static ASKED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASKED.get_or_init(|| crate::config::get().diag_unknown_build)
}

fn attempt(decisive: Option<&'static str>) {
    let _one_at_a_time = ATTEMPT.lock();
    if STATE.load(Ordering::SeqCst) != PENDING {
        return;
    }
    let base = image();
    let sites = sites();
    // The cheap first look: an image that ends before the sites do is not
    // the game. Memory at those addresses would be somebody else's.
    let furthest = sites
        .iter()
        .map(|site| site.rva + site.bytes.len())
        .max()
        .unwrap_or(0);
    if let Some((_, size)) = unsafe { header(base) } {
        if (size as usize) < furthest {
            not_the_game(size as usize, furthest);
            return;
        }
    }
    let mut probes: Vec<Probe> = sites
        .iter()
        .map(|site| unsafe { probe(base, site) })
        .collect();
    if deliberately_wrong() {
        // Only once the real answer is in, so the line that follows shows a
        // genuine read with one byte of the expectation changed.
        if let (Some(Probe::Matches), Some(site)) = (probes.first().copied(), sites.first()) {
            let mut found = [0u8; MAX_WRITE];
            found[..site.bytes.len()].copy_from_slice(site.bytes);
            found[0] ^= 0xff;
            probes[0] = Probe::Differs(found);
        }
    }

    let all_match = probes.iter().all(|probe| *probe == Probe::Matches);
    let running = RUNNING.load(Ordering::SeqCst);
    let unpacked = decisive.is_some()
        || (running
            && !all_match
            && CONFIRMED.fetch_add(1, Ordering::Relaxed) + 1 >= CONFIRMATIONS);

    match decide(&probes, unpacked) {
        Decision::Wait => say_progress(&probes),
        Decision::StandDown => {
            let because = decisive.unwrap_or(*RUNNING_BECAUSE.lock());
            stand_down(&describe_mismatch(base, sites, &probes, because));
        }
        Decision::Engage => unsafe { engage(base, decisive.is_some()) },
    }
}

/// How many sites match, said when the number changes.
fn say_progress(probes: &[Probe]) {
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    let matching = probes
        .iter()
        .filter(|probe| **probe == Probe::Matches)
        .count() as u64;
    if LAST.swap(matching, Ordering::Relaxed) != matching {
        crate::log!(
            "[bld] {matching} of {} code sites are readable and as expected; waiting for the \
             rest to be unpacked",
            probes.len()
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The one line that says what was expected and what was found.
fn describe_mismatch(
    base: usize,
    sites: &[&'static Site],
    probes: &[Probe],
    because: &str,
) -> String {
    let wrong: Vec<String> = sites
        .iter()
        .zip(probes)
        .filter_map(|(site, probe)| match probe {
            Probe::Matches => None,
            Probe::Unmapped => Some(format!(
                "{} at +{:#x} is not mapped memory",
                site.what, site.rva
            )),
            Probe::Differs(found) => Some(format!(
                "{} at +{:#x}: expected {}, found {}",
                site.what,
                site.rva,
                hex(site.bytes),
                hex(&found[..site.bytes.len()])
            )),
        })
        .collect();
    format!(
        "this is not the game build this mod knows ({} of {} code sites differ, checked when \
         {because}; {}){}: {}",
        wrong.len(),
        sites.len(),
        describe_header(base),
        if deliberately_wrong() {
            " -- diag_unknown_build = 1 is set, which makes the first site read as wrong on \
             purpose"
        } else {
            ""
        },
        wrong.join("; ")
    )
}

/// Install everything, in one step, or nothing.
unsafe fn engage(base: usize, decisive: bool) {
    let settings = crate::config::get();
    // The mip bias first, because it can still say "not yet": the constant it
    // is built from lives in data that may not be in place when the code is.
    let bias = crate::tune::prepare_exact_bias(
        base,
        settings.mip_bias,
        settings.render_scale,
        settings.mip_bias_extra,
    );
    // Taken before the write, because it is what the bias line reports: was
    // the game's own code already running when the instruction was changed.
    let running = RUNNING.load(Ordering::SeqCst);
    if matches!(bias, crate::tune::Bias::NotReady) && !decisive && !running {
        static SAID: AtomicBool = AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[bld] every code site matches; waiting for the data the mip bias is built \
                 from before installing anything"
            );
        }
        return;
    }

    let detours: Vec<Detour> = crate::hijack::detours()
        .into_iter()
        .chain(crate::native::detours())
        .collect();
    let mut writes: Vec<CodeWrite> = Vec::with_capacity(detours.len() + 1);
    for detour in &detours {
        match crate::native::prepare_detour(base, detour) {
            Some(write) => writes.push(write),
            None => {
                discard(&detours);
                stand_down(&format!(
                    "no memory could be allocated for the trampoline of {}",
                    detour.site.what
                ));
                return;
            }
        }
    }
    if let crate::tune::Bias::Ready(plan) = &bias {
        writes.push(plan.write);
    }

    let mut tries = 0;
    let outcome = loop {
        match crate::native::write_code(&writes) {
            Err(WriteError::Busy) if decisive && tries < 50 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            other => break other,
        }
    };

    let what = |index: usize| writes.get(index).map(|write| write.what).unwrap_or("?");
    match outcome {
        Ok(suspended) => {
            STATE.store(ENGAGED, Ordering::SeqCst);
            for (detour, write) in detours.iter().zip(&writes) {
                crate::log!(
                    "[eng] {} at {:#x} now runs through us (trampoline at {:#x})",
                    detour.site.what,
                    write.address,
                    detour.trampoline.load(Ordering::Relaxed)
                );
            }
            crate::log!(
                "[bld] all {} code sites match the game build this mod knows; {}. {} detours \
                 were written in one step with {suspended} other threads suspended",
                sites().len(),
                describe_header(base),
                detours.len()
            );
            crate::log!("[hij] the game's DLSS path is ours");
            crate::tune::bias_written(&bias, running);
        }
        Err(WriteError::Busy) => {
            discard(&detours);
            if decisive || BUSY.fetch_add(1, Ordering::Relaxed) + 1 >= BUSY_LIMIT {
                stand_down(
                    "every code site matches the known build, but a thread of the game stayed \
                     inside the code to be replaced, so nothing was written",
                );
            }
        }
        Err(WriteError::NotCode { index, protection }) => {
            discard(&detours);
            // Readable, matching, and not executable: the unpacker has not
            // finished with the page. Unless the game is already running, in
            // which case this is not a state to write into.
            if decisive {
                stand_down(&format!(
                    "{} matches the known build but its page is not executable code \
                     (protection {protection:#x}), so nothing was written",
                    what(index)
                ));
            }
        }
        Err(WriteError::Protect { index }) => {
            discard(&detours);
            stand_down(&format!(
                "{} matches the known build but its page could not be made writable, so \
                 nothing was written",
                what(index)
            ));
        }
        Err(WriteError::Changed { index, found }) => {
            discard(&detours);
            let length = writes.get(index).map(|write| write.length).unwrap_or(0);
            stand_down(&format!(
                "{} matched the known build when it was checked and read {} when it came to \
                 be written; what had been written before it was put back, so nothing is \
                 installed",
                what(index),
                hex(&found[..length])
            ));
        }
    }
}

unsafe fn discard(detours: &[Detour]) {
    for detour in detours {
        crate::native::discard_detour(detour);
    }
}

/// Answer no, once, and say so in one line.
fn stand_down(why: &str) {
    if STATE.swap(STOOD_DOWN, Ordering::SeqCst) == STOOD_DOWN {
        return;
    }
    crate::log!(
        "[bld] standing down: {why}. Nothing in the game's code or settings is changed, its \
         NGX calls pass through as it made them, its temporal pass is left alone and its \
         swapchain is DXGI's own. What stays is this library's hooks on the import table and \
         on Direct3D's and DXGI's vtables, which pass every call through"
    );
    go_idle();
    // Frame generation was started at load, before this could be known.
    crate::streamline::stand_down();
}

// ---------------------------------------------------------------------------
// Whether DLSS can be used at all
// ---------------------------------------------------------------------------

static UNUSABLE: AtomicBool = AtomicBool::new(false);
/// Set once NGX has been initialised by this mod, its entry points are bound
/// and the driver has said DLSS is available.
static READY: AtomicBool = AtomicBool::new(false);

/// Whether DLSS is ready to be used: initialised, bound, available, and not
/// since found unusable.
///
/// Everything this mod does to the picture waits for this. Until it is true
/// the engine's temporal pass is left to run, the camera is not offset and
/// no engine setting is written, so a session in which NGX never comes up --
/// another vendor's GPU, a driver without it, DirectX 11 -- is the game as
/// it is without the mod. One relaxed load of each of two flags.
pub fn dlss_ready() -> bool {
    READY.load(Ordering::Relaxed) && !UNUSABLE.load(Ordering::Relaxed)
}

/// Whether DLSS was ever ready in this session, usable still or not: that
/// is, whether the engine has been set on its DLSS path at all.
pub fn dlss_was_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

/// NGX is up and the driver says DLSS is available: start using it.
pub fn declare_ready(what: &str) {
    if UNUSABLE.load(Ordering::SeqCst) || READY.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::log!("[dls] DLSS can be used: {what}");
    crate::gpu::activate_temporal_pipelines();
}

/// The engine is rendering through Direct3D 12 and DLSS is not ready. Called
/// once per submission while that lasts, and not at all once it is.
///
/// NGX is initialised while the renderer starts, before the first frame. A
/// game that has been submitting work for a minute without it is not going
/// to, and the log should say so rather than say nothing.
pub fn waiting_for_dlss() {
    if UNUSABLE.load(Ordering::Relaxed) {
        return;
    }
    note_game_running("the renderer submitted its first work");
    if STATE.load(Ordering::Relaxed) != ENGAGED {
        return;
    }
    static SINCE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    if SINCE.get_or_init(std::time::Instant::now).elapsed() > WAIT_FOR_NGX {
        declare_unusable(
            "the game has been rendering through Direct3D 12 for a minute and has not \
             initialised NGX through this mod, and without NGX there is no DLSS. A GPU that is \
             not NVIDIA's, or a driver without NGX, is the usual reason",
        );
    }
}

const WAIT_FOR_NGX: std::time::Duration = std::time::Duration::from_secs(60);

/// What the game's own settings file says about DirectX 12, if it says.
///
/// `graphics_option.ini` in the game folder carries `DirectX12Enable=On` or
/// `Off`, and the game picks its renderer from it at start-up.
pub(crate) fn directx12_enabled(graphics_options: &str) -> Option<bool> {
    graphics_options.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if !key.trim().eq_ignore_ascii_case("DirectX12Enable") {
            return None;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("On") {
            Some(true)
        } else if value.eq_ignore_ascii_case("Off") {
            Some(false)
        } else {
            None
        }
    })
}

/// Whether the game is set to start on DirectX 11, read from its settings
/// file at load. `false` when the file is missing or does not say.
pub fn configured_for_directx11(game_folder: &std::path::Path) -> bool {
    let path = game_folder.join("graphics_option.ini");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| directx12_enabled(&text))
        == Some(false)
}

/// The game is set to DirectX 11, where this mod has nothing to do: decide
/// now, before anything is installed, and say so.
pub fn stand_down_for_directx11() {
    stand_down(
        "the game's graphics_option.ini has DirectX12Enable=Off, so the game starts on \
         DirectX 11, and this mod only works with DirectX 12. To use the mod, turn DirectX 12 \
         on in the game's display options and restart the game",
    );
}

/// The game looked up a Direct3D 11 entry point by name.
///
/// NGX's Direct3D 11 entry points are only asked for by the Direct3D 11
/// renderer, so that lookup settles it and is said as a fact.
/// `D3D11CreateDevice` on its own is said for what it is, a lookup, and not
/// acted on.
pub fn note_directx11_lookup(symbol: &str) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if symbol.starts_with("NVSDK_NGX_D3D11_") {
        declare_unusable(&format!(
            "the game looked up {symbol}, which only its DirectX 11 renderer does: the game \
             is running on DirectX 11, and this mod only works with DirectX 12. Turn DirectX \
             12 on in the game's display options and restart the game"
        ));
    } else if !SAID.swap(true, Ordering::Relaxed) {
        crate::log!(
            "[dx11] the game looked up {symbol}. If it is running on DirectX 11, this mod does \
             nothing: it only works with DirectX 12, which is turned on in the game's display \
             options"
        );
    }
}

/// Whether DLSS cannot be used in this session. Latched: once true it stays
/// true until the game is restarted.
///
/// One relaxed load, for anything that would otherwise go on asking the
/// engine to render small for a resolve that is not going to happen.
pub fn dlss_unusable() -> bool {
    UNUSABLE.load(Ordering::Relaxed)
}

/// Latch DLSS as unusable and say why, once.
///
/// From here on this mod leaves the engine's temporal pass alone, stops
/// offsetting the camera, and stops answering the game's question about DLSS
/// with yes. What the engine has already been told is not taken back from
/// here: see `hijack::wanted_ratio`.
pub fn declare_unusable(why: &str) {
    if UNUSABLE.swap(true, Ordering::SeqCst) {
        return;
    }
    crate::log!(
        "[dls] DLSS is not used in this session: {why}. The mod leaves the engine's own \
         anti-aliasing in place"
    );
    go_idle_quietly();
}

fn go_idle() {
    UNUSABLE.store(true, Ordering::SeqCst);
    go_idle_quietly();
}

fn go_idle_quietly() {
    crate::native::stop_jitter();
    crate::gpu::retire_temporal_pipelines();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: Site = Site::new("a test site", 0x10, &[0x48, 0x8b, 0x05, 0x11, 0x22]);
    const PATCHED: Site = Site {
        what: "a site this mod rewrites",
        rva: 0x20,
        bytes: &[0xf3, 0x0f, 0x10, 0x05, 1, 2, 3, 4, 0x44],
        wild: 4..8,
    };

    fn differs() -> Probe {
        Probe::Differs([0u8; MAX_WRITE])
    }

    #[test]
    fn a_signature_matches_only_itself() {
        assert!(matches(&SITE, &[0x48, 0x8b, 0x05, 0x11, 0x22]));
        // The opcode alone is not a match.
        assert!(!matches(&SITE, &[0x48, 0x8b, 0x05, 0x11, 0x23]));
        assert!(!matches(&SITE, &[0x48, 0x8b, 0x05, 0x11]));
        assert!(!matches(&SITE, &[]));
    }

    #[test]
    fn bytes_this_mod_rewrites_are_not_compared_and_the_rest_are() {
        assert!(matches(
            &PATCHED,
            &[0xf3, 0x0f, 0x10, 0x05, 9, 9, 9, 9, 0x44]
        ));
        assert!(!matches(
            &PATCHED,
            &[0xf3, 0x0f, 0x10, 0x05, 1, 2, 3, 4, 0x45]
        ));
        assert!(!matches(
            &PATCHED,
            &[0xf2, 0x0f, 0x10, 0x05, 1, 2, 3, 4, 0x44]
        ));
    }

    #[test]
    fn everything_matching_engages_whether_or_not_the_game_is_running() {
        let probes = [Probe::Matches, Probe::Matches];
        assert_eq!(decide(&probes, false), Decision::Engage);
        assert_eq!(decide(&probes, true), Decision::Engage);
    }

    #[test]
    fn a_mismatch_before_the_game_runs_is_waited_out() {
        assert_eq!(decide(&[Probe::Matches, differs()], false), Decision::Wait);
        assert_eq!(
            decide(&[Probe::Unmapped, Probe::Unmapped], false),
            Decision::Wait
        );
    }

    #[test]
    fn one_mismatch_with_the_game_running_stands_the_whole_mod_down() {
        // Seven of eight is not a partial install.
        let mut probes = vec![Probe::Matches; 8];
        probes[5] = differs();
        assert_eq!(decide(&probes, true), Decision::StandDown);
        probes[5] = Probe::Unmapped;
        assert_eq!(decide(&probes, true), Decision::StandDown);
    }

    #[test]
    fn the_games_settings_file_says_which_renderer() {
        let on = "[GraphicsOption]\nScreenMode=HDRMode\nDirectX12Enable=On\nDisplayIndex=0\n";
        assert_eq!(directx12_enabled(on), Some(true));
        assert_eq!(directx12_enabled(&on.replace("=On", "=Off")), Some(false));
        assert_eq!(directx12_enabled("directx12enable = off\r\n"), Some(false));
        // Missing, or something this does not understand: not a reason to
        // stand down.
        assert_eq!(directx12_enabled("[GraphicsOption]\nV-Sync=Off\n"), None);
        assert_eq!(directx12_enabled("DirectX12Enable=Maybe\n"), None);
        assert_eq!(directx12_enabled(""), None);
    }

    #[test]
    fn no_sites_is_not_a_match() {
        assert_eq!(decide(&[], false), Decision::Wait);
        assert_eq!(decide(&[], true), Decision::StandDown);
    }

    #[test]
    fn every_site_fits_in_a_write_and_none_is_checked_by_an_opcode_alone() {
        for site in sites() {
            assert!(site.bytes.len() <= MAX_WRITE, "{} is too long", site.what);
            assert!(
                site.bytes.len() - site.wild.len() >= 10,
                "{} is checked by only {} bytes",
                site.what,
                site.bytes.len() - site.wild.len()
            );
        }
        // Eight functions detoured, three object loads, two reads, one
        // operand, and the two ends of the one function that is called (the
        // engine's ratio setter). The configuration load is shared by two
        // modules and listed once.
        assert_eq!(sites().len(), 16);
    }

    #[test]
    fn the_mismatch_line_names_the_site_and_both_byte_strings() {
        let mut found = [0u8; MAX_WRITE];
        found[..5].copy_from_slice(&[0x48, 0x8b, 0x0d, 0x11, 0x22]);
        let line = describe_mismatch(
            0,
            &[&SITE, &PATCHED],
            &[Probe::Differs(found), Probe::Matches],
            "the game initialised NGX",
        );
        assert!(line.contains("1 of 2 code sites differ"), "{line}");
        assert!(
            line.contains("a test site at +0x10: expected 488b051122, found 488b0d1122"),
            "{line}"
        );
        assert!(line.contains("the game initialised NGX"), "{line}");
        assert!(!line.contains("a site this mod rewrites"), "{line}");
    }

    /// Check every signature against an unpacked image of the game, when one
    /// is to hand: `MHW_DLSS_UNPACKED_EXE=<path> cargo test -- --ignored`.
    ///
    /// The image is not something this repository can carry, so this does not
    /// run by default. It is the test to run after changing any site.
    #[test]
    #[ignore]
    fn the_signatures_are_the_known_builds() {
        let path = std::env::var("MHW_DLSS_UNPACKED_EXE")
            .expect("set MHW_DLSS_UNPACKED_EXE to an unpacked MonsterHunterWorld.exe");
        let image = std::fs::read(path).unwrap();
        let u32_at = |offset: usize| {
            u32::from_le_bytes(image[offset..offset + 4].try_into().unwrap()) as usize
        };
        let nt = u32_at(0x3c);
        assert_eq!(u32_at(nt + 8) as u32, KNOWN_TIMESTAMP);
        assert_eq!(u32_at(nt + 80) as u32, KNOWN_SIZE_OF_IMAGE);
        let sections = u16::from_le_bytes(image[nt + 6..nt + 8].try_into().unwrap()) as usize;
        let optional = u16::from_le_bytes(image[nt + 20..nt + 22].try_into().unwrap()) as usize;
        let table = nt + 24 + optional;
        let file_offset = |rva: usize| {
            (0..sections).find_map(|index| {
                let entry = table + index * 40;
                let (size, address, raw) =
                    (u32_at(entry + 8), u32_at(entry + 12), u32_at(entry + 20));
                (rva >= address && rva < address + size).then(|| rva - address + raw)
            })
        };
        for site in sites() {
            let offset = file_offset(site.rva).expect("the site is in a section");
            let found = &image[offset..offset + site.bytes.len()];
            assert!(
                matches(site, found),
                "{} at +{:#x}: expected {}, found {}",
                site.what,
                site.rva,
                hex(site.bytes),
                hex(found)
            );
        }
    }
}
