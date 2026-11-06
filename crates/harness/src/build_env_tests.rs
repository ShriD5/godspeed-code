use super::*;

const AUTH: &str = "fifo:/tmp/godspeed-build-slots/slots.fifo";
const FLAGS: &str = "-j --jobserver-auth=fifo:/tmp/godspeed-build-slots/slots.fifo";

fn pool(make_reads_fifo: bool) -> BuildEnv {
    BuildEnv {
        jobserver: Some(AUTH.into()),
        make_reads_fifo,
        sccache: None,
    }
}

/// A Rust project with the given lockfile packages, plus an empty cargo home.
fn rust_project(packages: &[&str]) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
    let lock: String = packages
        .iter()
        .map(|name| format!("[[package]]\nname = \"{name}\"\nversion = \"1.0.0\"\n\n"))
        .collect();
    std::fs::write(repo.join("Cargo.lock"), format!("version = 4\n\n{lock}")).unwrap();
    let home = dir.path().join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    (dir, repo, home)
}

fn names(vars: &[(String, String)]) -> Vec<&str> {
    vars.iter().map(|(name, _)| name.as_str()).collect()
}

#[test]
fn nothing_without_a_published_pool_or_sccache() {
    let (_dir, repo, home) = rust_project(&["serde"]);
    assert!(vars(&BuildEnv::default(), Some(&repo), Some(&home), |_| false).is_empty());
}

#[test]
fn new_make_gets_makeflags_and_cargo_makeflags_anywhere() {
    let out = vars(&pool(true), None, None, |_| false);
    assert_eq!(
        out,
        vec![
            ("MAKEFLAGS".to_owned(), FLAGS.to_owned()),
            ("CARGO_MAKEFLAGS".to_owned(), FLAGS.to_owned()),
        ]
    );
}

#[test]
fn old_make_gets_only_cargo_makeflags_in_a_safe_rust_project() {
    let (_dir, repo, home) = rust_project(&["serde", "tokio"]);
    let out = vars(&pool(false), Some(&repo.join("src")), Some(&home), |_| {
        false
    });
    assert_eq!(out, vec![("CARGO_MAKEFLAGS".to_owned(), FLAGS.to_owned())]);
}

#[test]
fn old_make_skips_projects_whose_build_scripts_forward_makeflags() {
    for forwarder in ["cmake", "openssl-src", "tikv-jemalloc-sys"] {
        let (_dir, repo, home) = rust_project(&["serde", forwarder]);
        assert!(
            vars(&pool(false), Some(&repo), Some(&home), |_| false).is_empty(),
            "{forwarder}"
        );
        // A make that reads fifo jobservers handles them fine.
        assert_eq!(
            names(&vars(&pool(true), Some(&repo), Some(&home), |_| false)),
            ["MAKEFLAGS", "CARGO_MAKEFLAGS"]
        );
    }
    // A name that merely contains a forwarder is fine.
    let (_dir, repo, home) = rust_project(&["cmake-parser"]);
    assert_eq!(
        names(&vars(&pool(false), Some(&repo), Some(&home), |_| false)),
        ["CARGO_MAKEFLAGS"]
    );
}

#[test]
fn old_make_skips_dirs_without_a_lockfile() {
    let dir = tempfile::tempdir().unwrap();
    assert!(vars(&pool(false), Some(dir.path()), Some(dir.path()), |_| false).is_empty());
    assert!(vars(&pool(false), None, None, |_| false).is_empty());
}

#[test]
fn user_parallelism_choices_win() {
    let (_dir, repo, home) = rust_project(&["serde"]);
    for name in JOBS_OVERRIDES {
        let out = vars(&pool(true), Some(&repo), Some(&home), |n| n == *name);
        assert!(
            out.is_empty(),
            "{name} set by the user must keep the pool out"
        );
    }
}

#[test]
fn cargo_config_jobs_wins_in_the_repo_or_cargo_home() {
    let (_dir, repo, home) = rust_project(&["serde"]);
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    std::fs::write(repo.join(".cargo/config.toml"), "[build]\njobs = 2\n").unwrap();
    assert!(vars(&pool(true), Some(&repo.join("src")), Some(&home), |_| false).is_empty());

    let (_dir, repo, home) = rust_project(&["serde"]);
    std::fs::write(home.join("config.toml"), "build.jobs = 3\n").unwrap();
    assert!(vars(&pool(true), Some(&repo), Some(&home), |_| false).is_empty());

    // Unrelated build keys don't count.
    let (_dir, repo, home) = rust_project(&["serde"]);
    std::fs::write(
        home.join("config.toml"),
        "[build]\ntarget-dir = \"/tmp/t\"\n",
    )
    .unwrap();
    assert_eq!(
        names(&vars(&pool(true), Some(&repo), Some(&home), |_| false)),
        ["MAKEFLAGS", "CARGO_MAKEFLAGS"]
    );
}

#[test]
fn sccache_only_in_rust_projects_without_a_wrapper() {
    let env = BuildEnv {
        sccache: Some(PathBuf::from("/opt/homebrew/bin/sccache")),
        ..BuildEnv::default()
    };
    let (_dir, repo, home) = rust_project(&[]);
    assert_eq!(
        vars(&env, Some(&repo.join("src")), Some(&home), |_| false),
        vec![(
            "RUSTC_WRAPPER".to_owned(),
            "/opt/homebrew/bin/sccache".to_owned()
        )]
    );
    let plain = tempfile::tempdir().unwrap();
    assert!(vars(&env, Some(plain.path()), Some(&home), |_| false).is_empty());
    assert!(vars(&env, None, Some(&home), |_| false).is_empty());
    for name in WRAPPER_OVERRIDES {
        assert!(vars(&env, Some(&repo), Some(&home), |n| n == *name).is_empty());
    }
    std::fs::write(
        home.join("config.toml"),
        "[build]\nrustc-wrapper = \"/usr/bin/true\"\n",
    )
    .unwrap();
    assert!(vars(&env, Some(&repo), Some(&home), |_| false).is_empty());
}

#[test]
fn explicit_env_counts_as_a_user_choice() {
    let (_dir, repo, _home) = rust_project(&["serde"]);
    install(Some(pool(true)));
    let mut explicit = BTreeMap::new();
    explicit.insert("CARGO_BUILD_JOBS".to_owned(), "2".to_owned());
    assert!(vars_for(Some(&repo), |name| explicit.contains_key(name)).is_empty());
    install(None);
    assert!(vars_for(Some(&repo), |_| false).is_empty());
}

#[test]
fn parses_gnu_make_versions() {
    assert!(gnu_make_reads_fifo(
        "GNU Make 4.4.1\nBuilt for aarch64-apple-darwin"
    ));
    assert!(gnu_make_reads_fifo("GNU Make 4.4"));
    assert!(gnu_make_reads_fifo("GNU Make 5.0"));
    assert!(!gnu_make_reads_fifo("GNU Make 4.3"));
    assert!(!gnu_make_reads_fifo("GNU Make 3.81\nCopyright (C) 2006"));
    assert!(!gnu_make_reads_fifo("bmake 20240101"));
    assert!(!gnu_make_reads_fifo(""));
}
