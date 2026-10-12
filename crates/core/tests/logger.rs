//! The installed logger end to end: one process, one global logger, so this
//! lives in its own test binary with a single test.

use mhw_core::log::{self, LevelFilter, LogConfig};

#[test]
fn installed_logger_writes_filtered_rate_limited_lines() {
    let dir = std::env::temp_dir().join(format!("mhw-core-it-logger-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = log::init(LogConfig {
        dirs: vec![dir.clone()],
        level: LevelFilter::Info,
        targets: log::parse_targets("perf=debug").unwrap(),
        profile: "/home/jane".into(),
        ..LogConfig::default()
    })
    .expect("first init")
    .expect("a log file");
    assert!(
        log::init(LogConfig::default()).is_err(),
        "second init refused"
    );

    ::log::info!(target: "boot", version = "0.3.0", dir = "/home/jane/mod"; "started");
    ::log::debug!(target: "boot", "hidden: boot is at info");
    ::log::debug!(target: "perf", "shown: perf is at debug");
    for frame in 0..5u32 {
        mhw_core::log_once!(Warn, "render", "no-depth", "no depth view (frame {frame})");
        mhw_core::log_every!(2, Info, "perf", "every other frame {frame}");
        mhw_core::log_changed!(
            Info,
            "render",
            "size",
            frame / 2,
            "size bucket {}",
            frame / 2
        );
    }
    // Raising the level while running.
    log::set_filter(LevelFilter::Trace, Vec::new());
    ::log::trace!(target: "boot", "trace now visible");

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().map(|line| &line[24..]).collect();
    assert_eq!(
        lines,
        [
            "I [boot] started version=0.3.0 dir=%USERPROFILE%/mod",
            "D [perf] shown: perf is at debug",
            "W [render] no depth view (frame 0)",
            "I [perf] every other frame 0",
            "I [render] size bucket 0",
            "I [perf] every other frame 2",
            "I [render] size bucket 1",
            "I [perf] every other frame 4",
            "I [render] size bucket 2",
            "T [boot] trace now visible",
        ]
    );
    let _ = std::fs::remove_dir_all(&dir);
}
