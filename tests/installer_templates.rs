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
    assert!(WINDOWS_TEMPLATE.contains("Install-Binaries $InstallDirectory $sources"));
}

#[test]
fn powershell_installer_transactions_when_powershell_is_available() {
    if Command::new("pwsh")
        .args(["-NoProfile", "-Command", "exit 0"])
        .output()
        .is_err()
    {
        eprintln!(
            "PowerShell not installed; run this test in nix shell nixpkgs#powershell to exercise the Windows installer"
        );
        return;
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new("pwsh");
    command
        .args(["-NoProfile", "-File"])
        .arg(root.join("tests/installer_windows.ps1"))
        .arg("-TemplatePath")
        .arg(root.join("install/install.ps1.in"));
    let output = assert_cmd::Command::from_std(command)
        .timeout(std::time::Duration::from_secs(30))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[cfg(unix)]
mod shell {
    use std::{
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        time::Duration,
    };

    use bitcoin_hashes::sha256;

    use super::*;

    struct Installer {
        _temp: tempfile::TempDir,
        home: PathBuf,
        tools: PathBuf,
        active: PathBuf,
        destination: PathBuf,
        archive: PathBuf,
        script: PathBuf,
    }

    fn executable(path: &Path, body: &str) {
        fs::write(path, format!("#!/usr/bin/env bash\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn receipt(dir: &Path) {
        fs::write(
            dir.join(".ngit-install-receipt.json"),
            r#"{"schema":1,"method":"standalone","version":"1.6.0"}"#,
        )
        .unwrap();
    }

    impl Installer {
        fn new(helper: bool) -> Self {
            Self::with_release_version(helper, "3.0.1")
        }

        fn with_release_version(helper: bool, release_version: &str) -> Self {
            Self::with_version_check_failure(helper, release_version, None)
        }

        fn with_version_check_failure(
            helper: bool,
            release_version: &str,
            failure: Option<(&str, bool)>,
        ) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().canonicalize().unwrap().join("home with spaces");
            let tools = temp.path().join("tools");
            let active = home.join(".cargo/bin");
            let destination = home.join(".local/bin");
            for dir in [&home, &tools, &active, &destination] {
                fs::create_dir_all(dir).unwrap();
            }
            // Expose only known tools: never discover or execute the host's ngit/Cargo.
            for tool in [
                "bash", "cat", "gzip", "awk", "basename", "dirname", "readlink", "cp", "chmod",
                "mv", "find", "tar", "mkdir", "rm", "rmdir", "mktemp", "touch",
            ] {
                let output = Command::new("bash")
                    .args(["-c", "command -v \"$1\"", "tools", tool])
                    .output()
                    .unwrap();
                assert!(output.status.success(), "missing test tool {tool}");
                symlink(
                    String::from_utf8(output.stdout).unwrap().trim(),
                    tools.join(tool),
                )
                .unwrap();
            }
            let archive = temp.path().join("release.tar.gz");
            let encoder = flate2::write::GzEncoder::new(
                fs::File::create(&archive).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(encoder);
            for (name, version) in [
                ("ngit", format!("ngit {release_version}")),
                ("git-remote-nostr", format!("v{release_version}")),
            ] {
                if name == "git-remote-nostr" && !helper {
                    continue;
                }
                let mut body = format!("#!/usr/bin/env bash\nprintf '%s\\n' '{version}'\n");
                if let Some((failing_binary, installed_only)) = failure {
                    if name == failing_binary {
                        if installed_only {
                            body.push_str("case \"$0\" in */new/*) exit 0 ;; esac\n");
                        }
                        body.push_str(
                            "printf '%s' \"$0\" > \"$HOME/failed-version-check\"\nexit 17\n",
                        );
                    }
                }
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                tar.append_data(&mut header, name, body.as_bytes()).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
            let hash = sha256::Hash::hash(&fs::read(&archive).unwrap()).to_string();
            // Hash implementations differ between macOS and Linux.
            for tool in ["sha256sum", "shasum"] {
                let output = Command::new("bash")
                    .args(["-c", "command -v \"$1\"", "tools", tool])
                    .output()
                    .unwrap();
                if output.status.success() {
                    symlink(
                        String::from_utf8(output.stdout).unwrap().trim(),
                        tools.join(tool),
                    )
                    .unwrap();
                    break;
                }
            }
            let source = UNIX_TEMPLATE.strip_suffix("main \"$@\"\n").unwrap()
                .replace("@@VERSION@@", "3.0.1")
                .replace("@@ASSET_MANIFEST@@", &format!("linux-x86_64-gnu|https://example.invalid/release|{hash}|release.tar.gz|application/gzip"));
            let script = temp.path().join("install.sh");
            fs::write(&script, format!("{source}\nis_nixos() {{ return 1; }}\ndetect_target() {{ echo linux-x86_64-gnu; }}\ndownload() {{ cp \"$TEST_ARCHIVE\" \"$2\"; }}\nmain \"$@\"\n")).unwrap();
            Self {
                _temp: temp,
                home,
                tools,
                active,
                destination,
                archive,
                script,
            }
        }

        fn run(&self, args: &[&str]) -> std::process::Output {
            let mut command = Command::new("bash");
            command
                .arg(&self.script)
                .args(args)
                .env("HOME", &self.home)
                .env("CARGO_HOME", self.home.join(".cargo"))
                .env(
                    "PATH",
                    format!(
                        "{}:{}:{}",
                        self.active.display(),
                        self.destination.display(),
                        self.tools.display()
                    ),
                )
                .env("TEST_ARCHIVE", &self.archive);
            assert_cmd::Command::from_std(command)
                .timeout(Duration::from_secs(10))
                .output()
                .unwrap()
        }

        fn old_standalone(&self, version: &str) {
            executable(
                &self.destination.join("ngit"),
                &format!("echo 'ngit {version}'"),
            );
            executable(
                &self.destination.join("git-remote-nostr"),
                "echo old-helper",
            );
            receipt(&self.destination);
        }
    }

    #[test]
    fn installs_and_repairs_without_invoking_an_old_updater() {
        let fixture = Installer::new(true);
        fixture.old_standalone("1.6.0");
        fs::remove_file(fixture.destination.join("git-remote-nostr")).unwrap();
        let output = fixture.run(&[]);
        assert!(output.status.success(), "{output:?}");
        assert!(
            fs::read_to_string(fixture.destination.join("ngit"))
                .unwrap()
                .contains("3.0.1")
        );
        assert!(
            fs::read_to_string(fixture.destination.join("git-remote-nostr"))
                .unwrap()
                .contains("3.0.1")
        );
        assert!(!fixture.destination.join(".ngit-install-lock").exists());
    }

    #[test]
    fn standalone_migration_never_takes_over_cargo_files() {
        let fixture = Installer::new(true);
        executable(&fixture.active.join("ngit"), "echo 'ngit 1.6.0'");
        let before = fs::read(fixture.active.join("ngit")).unwrap();
        assert!(!fixture.run(&[]).status.success());
        assert!(fixture.run(&["--method", "standalone"]).status.success());
        assert_eq!(fs::read(fixture.active.join("ngit")).unwrap(), before);
        assert!(!fixture.active.join(".ngit-install-receipt.json").exists());
        assert!(
            !fixture
                .run(&[
                    "--method",
                    "standalone",
                    "--install-dir",
                    fixture.active.to_str().unwrap()
                ])
                .status
                .success()
        );
        assert_eq!(fs::read(fixture.active.join("ngit")).unwrap(), before);
    }

    #[test]
    fn unavailable_default_directory_does_not_fall_back_into_cargo() {
        let fixture = Installer::new(true);
        executable(&fixture.active.join("ngit"), "echo 'ngit 1.6.0'");
        fs::remove_dir(&fixture.destination).unwrap();
        fs::write(&fixture.destination, "blocked").unwrap();
        assert!(!fixture.run(&["--method", "standalone"]).status.success());
        assert!(!fixture.active.join(".ngit-install-receipt.json").exists());
        assert!(
            fs::read_to_string(fixture.active.join("ngit"))
                .unwrap()
                .contains("1.6.0")
        );
    }

    #[test]
    fn automatic_and_explicit_cargo_updates_preserve_the_release_and_root() {
        for custom_root in [false, true] {
            let mut fixture = Installer::new(true);
            if custom_root {
                fixture.active = fixture.home.join("custom Cargo root/bin");
                fs::create_dir_all(&fixture.active).unwrap();
                fs::write(fixture.active.parent().unwrap().join(".crates2.json"), "{}").unwrap();
            }
            executable(&fixture.active.join("ngit"), "echo 'ngit 1.6.0'");
            executable(
                &fixture.tools.join("cargo"),
                "printf '%s\\n' \"$@\" > \"$HOME/cargo-args\"",
            );
            for args in [Vec::new(), vec!["--method", "cargo"]] {
                let output = fixture.run(&args);
                assert!(output.status.success(), "{output:?}");
                let args = fs::read_to_string(fixture.home.join("cargo-args")).unwrap();
                assert_eq!(
                    args.lines().collect::<Vec<_>>(),
                    vec![
                        "install",
                        "ngit",
                        "--locked",
                        "--version",
                        "3.0.1",
                        "--root",
                        fixture.active.parent().unwrap().to_str().unwrap()
                    ]
                );
                assert!(!fixture.destination.join("ngit").exists());
            }
        }
    }

    #[test]
    fn automatic_cargo_failure_does_not_fall_back_to_standalone() {
        let fixture = Installer::new(true);
        executable(&fixture.active.join("ngit"), "echo 'ngit 1.6.0'");
        executable(
            &fixture.tools.join("cargo"),
            "touch \"$HOME/cargo-called\"\nexit 17",
        );
        let before = fs::read(fixture.active.join("ngit")).unwrap();
        assert!(!fixture.run(&[]).status.success());
        assert!(fixture.home.join("cargo-called").exists());
        assert_eq!(fs::read(fixture.active.join("ngit")).unwrap(), before);
        assert!(!fixture.destination.join("ngit").exists());
        assert!(!fixture.active.join(".ngit-install-receipt.json").exists());
    }

    #[test]
    fn standalone_receipt_takes_precedence_over_a_cargo_directory() {
        let fixture = Installer::new(true);
        executable(&fixture.active.join("ngit"), "echo 'ngit 1.6.0'");
        receipt(&fixture.active);
        executable(
            &fixture.tools.join("cargo"),
            "touch \"$HOME/cargo-called\"\nexit 17",
        );
        let output = fixture.run(&[]);
        assert!(output.status.success(), "{output:?}");
        assert!(!fixture.home.join("cargo-called").exists());
        assert!(
            fs::read_to_string(fixture.active.join("ngit"))
                .unwrap()
                .contains("3.0.1")
        );
        assert!(!fixture.destination.join("ngit").exists());
    }

    #[test]
    fn incomplete_archive_preserves_existing_binaries_and_receipt() {
        let fixture = Installer::new(false);
        fixture.old_standalone("1.6.0");
        let names = ["ngit", "git-remote-nostr", ".ngit-install-receipt.json"];
        let before: Vec<_> = names
            .iter()
            .map(|name| fs::read(fixture.destination.join(name)).unwrap())
            .collect();
        assert!(!fixture.run(&[]).status.success());
        for (name, before) in names.iter().zip(before) {
            assert_eq!(fs::read(fixture.destination.join(name)).unwrap(), before);
        }
    }

    #[test]
    fn wrong_binary_version_and_bad_hash_do_not_replace_existing_files() {
        for bad_hash in [false, true] {
            let fixture = Installer::with_release_version(true, "2.6.3");
            fixture.old_standalone("1.6.0");
            if bad_hash {
                fs::write(&fixture.archive, "corrupt download").unwrap();
            }
            let before = fs::read(fixture.destination.join("ngit")).unwrap();
            let output = fixture.run(&[]);
            assert!(!output.status.success(), "{output:?}");
            assert_eq!(fs::read(fixture.destination.join("ngit")).unwrap(), before);
        }
    }

    #[test]
    fn matching_version_output_with_failure_preserves_the_previous_installation() {
        for binary in ["ngit", "git-remote-nostr"] {
            for installed_only in [false, true] {
                let fixture = Installer::with_version_check_failure(
                    true,
                    "3.0.1",
                    Some((binary, installed_only)),
                );
                fixture.old_standalone("1.6.0");
                let names = ["ngit", "git-remote-nostr", ".ngit-install-receipt.json"];
                let before: Vec<_> = names
                    .iter()
                    .map(|name| fs::read(fixture.destination.join(name)).unwrap())
                    .collect();
                let output = fixture.run(&[]);
                assert!(
                    !output.status.success(),
                    "{binary}, installed={installed_only}: {output:?}"
                );
                let failed = fs::read_to_string(fixture.home.join("failed-version-check")).unwrap();
                if installed_only {
                    assert_eq!(Path::new(&failed), fixture.destination.join(binary));
                } else {
                    assert!(Path::new(&failed).parent().unwrap().ends_with("new"));
                }
                for (name, before) in names.iter().zip(before) {
                    assert_eq!(fs::read(fixture.destination.join(name)).unwrap(), before);
                }
                assert!(!fixture.destination.join(".ngit-install-lock").exists());
            }
        }
    }

    #[test]
    fn second_replacement_failure_restores_both_binaries_and_receipt() {
        let fixture = Installer::new(true);
        fixture.old_standalone("1.6.0");
        let real_mv = fs::read_link(fixture.tools.join("mv")).unwrap();
        fs::remove_file(fixture.tools.join("mv")).unwrap();
        symlink(real_mv, fixture.tools.join("real-mv")).unwrap();
        executable(
            &fixture.tools.join("mv"),
            "case \"$*\" in *new/git-remote-nostr*) exit 1 ;; esac\nexec real-mv \"$@\"",
        );
        let names = ["ngit", "git-remote-nostr", ".ngit-install-receipt.json"];
        let before: Vec<_> = names
            .iter()
            .map(|name| fs::read(fixture.destination.join(name)).unwrap())
            .collect();
        let output = fixture.run(&[]);
        assert!(!output.status.success(), "{output:?}");
        for (name, before) in names.iter().zip(before) {
            assert_eq!(fs::read(fixture.destination.join(name)).unwrap(), before);
        }
    }

    #[test]
    fn downgrade_and_invalid_receipt_require_explicit_flags() {
        let fixture = Installer::new(true);
        fixture.old_standalone("4.0.0-rc.1");
        assert!(!fixture.run(&[]).status.success());
        assert!(fixture.run(&["--allow-downgrade"]).status.success());
        fs::write(
            fixture.destination.join(".ngit-install-receipt.json"),
            "broken",
        )
        .unwrap();
        assert!(!fixture.run(&[]).status.success());
        let output = fixture.run(&["--repair"]);
        assert!(output.status.success(), "{output:?}");
    }

    #[test]
    fn symlinked_command_repairs_its_canonical_standalone_directory() {
        let fixture = Installer::new(true);
        fixture.old_standalone("1.6.0");
        symlink(
            fixture.destination.join("ngit"),
            fixture.active.join("ngit"),
        )
        .unwrap();
        let output = fixture.run(&[]);
        assert!(output.status.success(), "{output:?}");
        assert!(fixture.active.join("ngit").is_symlink());
        assert!(
            fs::read_to_string(fixture.destination.join("ngit"))
                .unwrap()
                .contains("3.0.1")
        );
    }
}
