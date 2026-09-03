use std::{fs, process::Command};

const UNIX_TEMPLATE: &str = include_str!("../install/install.sh.in");
const WINDOWS_TEMPLATE: &str = include_str!("../install/install.ps1.in");

#[test]
fn installer_templates_have_one_deployment_contract() {
    for template in [UNIX_TEMPLATE, WINDOWS_TEMPLATE] {
        assert_eq!(template.matches("@@VERSION@@").count(), 1);
        assert_eq!(template.matches("@@ASSET_MANIFEST@@").count(), 1);
        assert!(!template.contains("github.com"));
        assert!(!template.to_ascii_lowercase().contains("/latest"));
        assert!(template.contains(".ngit-install-receipt.json"));
        assert!(template.contains("git-remote-nostr"));
    }
}

#[test]
fn unix_template_is_valid_shell_and_handles_nixos_with_musl() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("install.sh");
    fs::write(&path, UNIX_TEMPLATE).unwrap();
    let status = Command::new("bash").arg("-n").arg(&path).status().unwrap();
    assert!(status.success());
    assert!(UNIX_TEMPLATE.contains("if is_nixos || ldd --version"));
    assert!(UNIX_TEMPLATE.contains("linux-x86_64-musl"));
    assert!(UNIX_TEMPLATE.contains("\"$ngit_dir/ngit\" --version"));
    assert!(UNIX_TEMPLATE.contains("\"$ngit_dir/git-remote-nostr\" --version"));
}

#[test]
fn windows_template_requires_stable_https_zip_assets() {
    assert!(WINDOWS_TEMPLATE.contains("'^\\d+\\.\\d+\\.\\d+$'"));
    assert!(WINDOWS_TEMPLATE.contains("$uri.Scheme -ne \"https\""));
    assert!(WINDOWS_TEMPLATE.contains("$mime -ne \"application/zip\""));
    assert!(WINDOWS_TEMPLATE.contains("windows-x86_64"));
    assert!(WINDOWS_TEMPLATE.contains("Join-Path $installDirectory \"ngit.exe\""));
    assert!(WINDOWS_TEMPLATE.contains("Join-Path $installDirectory \"git-remote-nostr.exe\""));
}
