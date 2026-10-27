//! Release-artifact checks that spawn processes (python3, codesign). They
//! live in their own test binary so those spawns can't race the unit tests'
//! loopback servers.

use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};
use zeron_update::{Manifest, RELEASE_TEAM_ID, mac_app_artifact};

/// `scripts/release-manifest.py` generated this fixture; the updater must
/// parse it exactly.
#[test]
fn release_manifest_fixture_parses() {
    let manifest: Manifest = serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    assert_eq!(manifest.version, "0.5.8");
    let app = &manifest.files["godspeed-0.5.8-macos-arm64-app.tar.gz"];
    assert_eq!(
        app.sha256.as_deref(),
        Some("1143295a8db6805c33411a9c293506e9d1eaa08ff25f8feccd4506982418d74b")
    );
    assert_eq!(app.size, Some(20));
    assert_eq!(manifest.files.len(), 2);
}

/// Runs the real script (when python3 is around) and parses its output.
#[test]
fn release_manifest_script_output_parses() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/release-manifest.py");
    let tmp = tempfile::tempdir().unwrap();
    let app = mac_app_artifact("7.8.9");
    std::fs::write(tmp.path().join(&app), b"payload").unwrap();
    std::fs::write(tmp.path().join("godspeed-7.8.8-macos-arm64.dmg"), b"old").unwrap();
    let out = tmp.path().join("manifest.json");
    let Ok(status) = Command::new("python3")
        .arg("-I")
        .arg(&script)
        .args(["--version", "7.8.9", "--dir"])
        .arg(tmp.path())
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .status()
    else {
        return; // no python3 on this machine
    };
    assert!(status.success());
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(manifest.version, "7.8.9");
    assert_eq!(manifest.files.len(), 1, "only this version's artifacts");
    assert_eq!(
        manifest.files[&app].sha256.as_deref(),
        Some(format!("{:x}", Sha256::digest(b"payload")).as_str())
    );
    assert_eq!(manifest.files[&app].size, Some(7));
}

/// An ad-hoc signed bundle (what anyone can produce) must never pass.
#[cfg(target_os = "macos")]
#[test]
fn ad_hoc_signed_bundles_are_refused() {
    use zeron_update::verify_mac_app_signature;
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("Godspeed.app");
    std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    std::fs::copy("/bin/echo", app.join("Contents/MacOS/godspeed")).unwrap();
    std::fs::write(
        app.join("Contents/Info.plist"),
        include_str!("../../../dist/macos/Info.plist").replace("__VERSION__", "999.0.0"),
    )
    .unwrap();
    let signed = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", "-"])
        .arg(&app)
        .status()
        .unwrap();
    assert!(signed.success());
    for running in [None, Some(RELEASE_TEAM_ID)] {
        assert!(verify_mac_app_signature(&app, running, false).is_err());
    }
    // Tampering is caught too.
    std::fs::write(app.join("Contents/MacOS/extra"), b"x").unwrap();
    assert!(verify_mac_app_signature(&app, None, false).is_err());
}
