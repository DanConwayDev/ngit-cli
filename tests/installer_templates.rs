use std::{fs, process::Command};

const UNIX_TEMPLATE: &str = include_str!("../install/install.sh.in");
const WINDOWS_TEMPLATE: &str = include_str!("../install/install.ps1.in");
const README: &str = include_str!("../README.md");

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
fn unix_template_is_valid_shell_and_prefers_nix_on_nixos() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("install.sh");
    fs::write(&path, UNIX_TEMPLATE).unwrap();
    let status = Command::new("bash").arg("-n").arg(&path).status().unwrap();
    assert!(status.success());
    assert!(UNIX_TEMPLATE.contains("if is_nixos || ldd --version"));
    assert!(UNIX_TEMPLATE.contains("linux-x86_64-musl"));
    assert!(UNIX_TEMPLATE.contains("nix profile add 'git+https://ngit.dev/ngit.git?ref=stable'"));
    assert!(!UNIX_TEMPLATE.contains("refs/tags/"));
    assert!(!UNIX_TEMPLATE.contains("github:DanConwayDev/ngit-cli"));
    assert!(UNIX_TEMPLATE.contains("bash -s -- --standalone"));
    assert!(UNIX_TEMPLATE.contains("if is_nixos && [ \"$FORCE_STANDALONE\" -ne 1 ]"));
    assert!(UNIX_TEMPLATE.contains("is_nixos && return 1"));
    assert!(UNIX_TEMPLATE.contains("\"$ngit_dir/ngit\" --version"));
    assert!(UNIX_TEMPLATE.contains("\"$ngit_dir/git-remote-nostr\" --version"));
}

#[test]
fn readme_nix_install_tracks_the_stable_branch() {
    assert!(README.contains("nix profile add 'git+https://ngit.dev/ngit.git?ref=stable'"));
    assert!(!README.contains("refs/tags/"));
    assert!(!README.contains("github:DanConwayDev/ngit-cli"));
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

#[cfg(unix)]
#[test]
fn unix_installer_only_delegates_to_supported_receipted_versions() {
    use std::os::unix::fs::PermissionsExt;

    for (version, receipt, supports_update, delegated) in [
        ("ngit 1.6.0", true, true, false),
        ("ngit 2.6.3", true, true, false),
        ("unknown", true, true, false),
        ("ngit 3.0.1", true, false, false),
        ("ngit 3.0.1", false, true, false),
        ("ngit 3.0.1", true, true, true),
        ("ngit 3.0.0-rc.3", true, true, true),
        ("ngit 10.0.0", true, true, true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("ngit");
        fs::write(
            &executable,
            format!(
                "#!/usr/bin/env bash\ncase \"$*\" in\n  --version) echo '{version}' ;;\n  'update --help') touch \"$PROBED\"; exit {} ;;\n  update) touch \"$DELEGATED\" ;;\n  *) exit 1 ;;\nesac\n",
                if supports_update { 0 } else { 1 },
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        if receipt {
            fs::write(directory.path().join(".ngit-install-receipt.json"), "{}").unwrap();
        }
        let script = directory.path().join("test.sh");
        fs::write(
            &script,
            format!(
                "{}\ndelegate_existing_install || touch \"$FALLBACK\"\n",
                UNIX_TEMPLATE.strip_suffix("main \"$@\"\n").unwrap(),
            ),
        )
        .unwrap();
        let output = Command::new("bash")
            .arg(&script)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    directory.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("PROBED", directory.path().join("probed"))
            .env("DELEGATED", directory.path().join("delegated"))
            .env("FALLBACK", directory.path().join("fallback"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{version}: {output:?}");
        assert_eq!(
            directory.path().join("delegated").exists(),
            delegated,
            "{version}"
        );
        assert_eq!(
            directory.path().join("fallback").exists(),
            !delegated,
            "{version}"
        );
        if version == "ngit 1.6.0" || version == "ngit 2.6.3" || version == "unknown" {
            assert!(!directory.path().join("probed").exists(), "{version}");
        }
    }
}

#[cfg(unix)]
#[test]
fn unix_installer_fallback_reuses_receipted_directory() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let bin = directory.path().join("custom/bin");
    fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("ngit");
    fs::write(&executable, "#!/usr/bin/env bash\nexit 1\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    for receipt in [false, true] {
        if receipt {
            fs::write(bin.join(".ngit-install-receipt.json"), "{}").unwrap();
        }
        let script = directory.path().join("test.sh");
        fs::write(
            &script,
            format!(
                "{}\nchosen=$(find_install_dir)\n[ \"$chosen\" = \"$EXPECTED\" ]\n",
                UNIX_TEMPLATE.strip_suffix("main \"$@\"\n").unwrap(),
            ),
        )
        .unwrap();
        let output = Command::new("bash")
            .arg(&script)
            .env("HOME", directory.path())
            .env(
                "PATH",
                format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
            )
            .env(
                "EXPECTED",
                if receipt {
                    bin.clone()
                } else {
                    directory.path().join(".local/bin")
                },
            )
            .output()
            .unwrap();
        assert!(output.status.success(), "receipt={receipt}: {output:?}");
    }
}
