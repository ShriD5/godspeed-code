use super::*;

#[test]
fn defaults_share_slots_automatically_and_use_sccache() {
    let dir = tempfile::tempdir().unwrap();
    let slots = BuildSlots::open_in(dir.path(), dir.path());
    let settings = slots.settings();
    assert!(settings.share_slots);
    assert_eq!(settings.slots, 0);
    assert!(settings.use_sccache);
    assert_eq!(settings.default_slots, default_slots());
    assert_eq!(settings.effective_slots(), default_slots());
    assert_eq!(settings.supported, cfg!(unix));
    // Nothing runs before start.
    assert!(!settings.active);
}

#[test]
fn partial_files_keep_the_defaults() {
    let stored: Stored = serde_json::from_str(r#"{"slots": 6}"#).unwrap();
    assert_eq!(
        stored,
        Stored {
            share_slots: true,
            slots: 6,
            use_sccache: true
        }
    );
    let stored: Stored = serde_json::from_str("{}").unwrap();
    assert_eq!(stored, Stored::default());
}

#[test]
fn updates_save_and_clamp() {
    let dir = tempfile::tempdir().unwrap();
    let slots = BuildSlots::open_in(dir.path(), dir.path());
    let saved = slots
        .update(BuildSlotsSettingsUpdate {
            share_slots: Some(false),
            slots: Some(1000),
            use_sccache: Some(false),
        })
        .unwrap();
    assert!(!saved.share_slots);
    assert_eq!(saved.slots, MAX_BUILD_SLOTS);
    assert!(!saved.use_sccache);
    // Reopened from disk.
    let reopened = BuildSlots::open_in(dir.path(), dir.path()).settings();
    assert_eq!(
        (reopened.share_slots, reopened.slots, reopened.use_sccache),
        (false, MAX_BUILD_SLOTS, false)
    );
    // Back to automatic.
    let saved = slots
        .update(BuildSlotsSettingsUpdate {
            slots: Some(0),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(saved.effective_slots(), default_slots());
}

#[test]
fn rebalance_waits_for_two_quiet_ticks() {
    let mut quiet = 0;
    // Full pool: nothing to do.
    assert_eq!(rebalance(7, 7, false, &mut quiet), Rebalance::Nothing);
    // Slots out while builds run: theirs.
    assert_eq!(rebalance(2, 7, true, &mut quiet), Rebalance::Nothing);
    assert_eq!(quiet, 0);
    // Missing with nothing running: wait one tick, then refill.
    assert_eq!(rebalance(5, 7, false, &mut quiet), Rebalance::Nothing);
    assert_eq!(rebalance(5, 7, false, &mut quiet), Rebalance::Refill(2));
    assert_eq!(quiet, 0);
    // A build in between resets the wait.
    assert_eq!(rebalance(5, 7, false, &mut quiet), Rebalance::Nothing);
    assert_eq!(rebalance(5, 7, true, &mut quiet), Rebalance::Nothing);
    assert_eq!(rebalance(5, 7, false, &mut quiet), Rebalance::Nothing);
    assert_eq!(rebalance(5, 7, false, &mut quiet), Rebalance::Refill(2));
    // Surplus drains the same way.
    assert_eq!(rebalance(9, 7, false, &mut quiet), Rebalance::Nothing);
    assert_eq!(rebalance(9, 7, false, &mut quiet), Rebalance::Drain(2));
}

#[test]
fn spots_build_tools_in_ps_output() {
    assert!(any_build_tool(
        "/usr/sbin/cfprefsd\n/Users/me/.rustup/toolchains/stable/bin/rustc\n"
    ));
    assert!(any_build_tool("  cargo\n"));
    assert!(any_build_tool("/usr/bin/make\n"));
    assert!(!any_build_tool(
        "/usr/local/bin/sccache\n/bin/zsh\ncargo-watch\nrust-analyzer\n"
    ));
    assert!(!any_build_tool(""));
}

#[test]
fn pool_dirs_are_stable_and_per_data_dir() {
    let temp = Path::new("/tmp");
    let a = pool_dir(temp, Path::new("/Users/me/.godspeed"));
    assert_eq!(a, pool_dir(temp, Path::new("/Users/me/.godspeed")));
    assert_ne!(a, pool_dir(temp, Path::new("/Users/me/.godspeed-dev")));
    assert!(a.starts_with(temp));
}

#[cfg(unix)]
mod unix {
    use super::super::pool::SlotPool;
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
    use std::time::{Duration, Instant};

    /// Tests that publish the process-wide build environment.
    static GLOBAL: Mutex<()> = Mutex::new(());

    #[test]
    fn pool_counts_takes_and_returns_slots() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        let pool = SlotPool::create(&pool_dir, 5).unwrap();
        let fifo = pool_dir.join("slots.fifo");
        let meta = std::fs::symlink_metadata(&fifo).unwrap();
        assert!(meta.file_type().is_fifo());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&pool_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(pool.auth(), format!("fifo:{}", fifo.display()));
        assert_eq!(pool.available().unwrap(), 5);
        assert_eq!(pool.take(2).unwrap(), 2);
        assert_eq!(pool.available().unwrap(), 3);
        // Never more than there is, and never blocks.
        assert_eq!(pool.take(10).unwrap(), 3);
        assert_eq!(pool.available().unwrap(), 0);
        pool.add(5).unwrap();
        assert_eq!(pool.available().unwrap(), 5);
        assert!(pool.is_intact());
        drop(pool);
        assert!(!fifo.exists(), "the FIFO is removed with the pool");
        assert!(!pool_dir.exists());
    }

    #[test]
    fn stale_fifos_are_replaced_and_foreign_dirs_refused() {
        let dir = tempfile::tempdir().unwrap();
        let pool_dir = dir.path().join("pool");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&pool_dir)
            .unwrap();
        // A stale pool left behind by a crashed engine (a plain file here).
        std::fs::write(pool_dir.join("slots.fifo"), "stale").unwrap();
        let pool = SlotPool::create(&pool_dir, 3).unwrap();
        assert_eq!(pool.available().unwrap(), 3);
        drop(pool);

        let open = dir.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            SlotPool::create(&open, 3).is_err(),
            "group/other-readable dirs are refused"
        );

        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&pool_dir, &link).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&pool_dir)
            .unwrap();
        assert!(
            SlotPool::create(&link, 3).is_err(),
            "symlinked dirs are refused"
        );
    }

    #[test]
    fn deleted_fifos_are_noticed() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SlotPool::create(&dir.path().join("pool"), 2).unwrap();
        std::fs::remove_file(dir.path().join("pool/slots.fifo")).unwrap();
        assert!(!pool.is_intact());
        // A replacement at the same path isn't ours either.
        let other = SlotPool::create(&dir.path().join("pool"), 2).unwrap();
        assert!(!pool.is_intact());
        assert!(other.is_intact());
        // Dropping the stale one leaves the replacement alone.
        drop(pool);
        assert!(other.is_intact());
    }

    #[test]
    fn service_runs_one_pool_of_slots_minus_one_and_publishes_it() {
        let _global = GLOBAL.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir(&data).unwrap();
        let slots = BuildSlots::open_in(&data, dir.path());
        slots
            .update(BuildSlotsSettingsUpdate {
                slots: Some(4),
                ..Default::default()
            })
            .unwrap();
        assert!(slots.auth().is_none(), "no pool before the host is probed");
        slots.set_probe_for_test(true, Some(PathBuf::from("/opt/sccache")));
        let auth = slots.auth().expect("pool running");
        assert!(slots.settings().active);
        let path = PathBuf::from(auth.strip_prefix("fifo:").unwrap());
        assert!(path.starts_with(dir.path()));
        let published = build_env::current().expect("published");
        assert_eq!(published.jobserver.as_deref(), Some(auth.as_str()));
        assert!(published.make_reads_fifo);
        assert_eq!(
            published.sccache.as_deref(),
            Some(Path::new("/opt/sccache"))
        );
        {
            let state = lock(&slots.inner.state);
            let pool = state.pool.as_ref().unwrap();
            assert_eq!(
                pool.tokens(),
                3,
                "4 slots: 3 in the pool + each build's implicit one"
            );
            assert_eq!(pool.available().unwrap(), 3);
        }

        // A sweep deletes the FIFO: the next check recreates it.
        std::fs::remove_file(&path).unwrap();
        slots.check();
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_fifo()
        );

        // sccache off: still published for the pool, without the wrapper.
        slots
            .update(BuildSlotsSettingsUpdate {
                use_sccache: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(build_env::current().unwrap().sccache, None);

        // Sharing off: the pool goes away.
        let off = slots
            .update(BuildSlotsSettingsUpdate {
                share_slots: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert!(!off.active);
        assert!(slots.auth().is_none());
        assert!(!path.exists());
        assert!(build_env::current().is_none(), "nothing left to inject");
        build_env::install(None);
    }

    const CHILD_ENV: &str = "GODSPEED_TEST_BUILD_SLOTS_FAKE_BUILD";
    const LOG_ENV: &str = "GODSPEED_TEST_BUILD_SLOTS_LOG";

    /// A fake build: when re-run as a child with `CHILD_ENV`, six workers
    /// each take a slot from the inherited jobserver (the same `jobserver`
    /// client cargo uses, reading `CARGO_MAKEFLAGS`), log it, work briefly
    /// and give it back. Without `CHILD_ENV` it runs two of them at once
    /// against one pool and checks they never held more than the pool has.
    #[test]
    fn two_fake_builds_never_exceed_the_pool() {
        if std::env::var_os(CHILD_ENV).is_some() {
            fake_build();
            return;
        }
        const TOKENS: usize = 3;
        let dir = tempfile::tempdir().unwrap();
        let pool = SlotPool::create(&dir.path().join("pool"), TOKENS).unwrap();
        let log = dir.path().join("log");
        std::fs::write(&log, "").unwrap();
        let children: Vec<_> = (0..2)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "build_slots::tests::unix::two_fake_builds_never_exceed_the_pool",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env(CHILD_ENV, "1")
                    .env(LOG_ENV, &log)
                    .env(
                        "CARGO_MAKEFLAGS",
                        format!("-j --jobserver-auth={}", pool.auth()),
                    )
                    .env_remove("MAKEFLAGS")
                    .env_remove("MFLAGS")
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let deadline = Instant::now() + Duration::from_secs(60);
        for mut child in children {
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "fake build failed: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("fake build hung");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let text = std::fs::read_to_string(&log).unwrap();
        let (mut held, mut peak, mut acquired) = (0i64, 0i64, 0);
        for line in text.lines() {
            match line {
                "+" => {
                    held += 1;
                    acquired += 1;
                }
                "-" => held -= 1,
                other => panic!("bad log line {other:?}"),
            }
            peak = peak.max(held);
        }
        assert_eq!(acquired, 2 * 6 * 4, "every worker ran");
        assert!(
            peak <= TOKENS as i64,
            "held {peak} slots from a pool of {TOKENS}"
        );
        assert!(
            peak >= 2,
            "the builds should have shared the pool (peak {peak})"
        );
        assert_eq!(pool.available().unwrap(), TOKENS, "every slot came back");
    }

    fn fake_build() {
        // SAFETY: called once, before any other descriptor juggling.
        let client = unsafe { jobserver::Client::from_env() }.expect("inherited jobserver");
        let log = std::env::var_os(LOG_ENV).unwrap();
        let workers: Vec<_> = (0..6)
            .map(|_| {
                let client = client.clone();
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
                    for _ in 0..4 {
                        let slot = client.acquire().unwrap();
                        // Logged inside the hold, so the log never overstates it.
                        file.write_all(b"+\n").unwrap();
                        std::thread::sleep(Duration::from_millis(15));
                        file.write_all(b"-\n").unwrap();
                        drop(slot);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
