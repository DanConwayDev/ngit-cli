use std::ffi::OsStr;

use anyhow::{ensure, Context, Result};
use dialoguer::theme::{ColorfulTheme, Theme};
use directories::ProjectDirs;
use rexpect::session::{Options, PtySession};
use strip_ansi_escapes::strip_str;

pub static TEST_KEY_1_NSEC: &str =
    "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";

pub static TEST_KEY_2_NSEC: &str =
    "nsec1ypglg6nj6ep0g2qmyfqcv2al502gje3jvpwye6mthmkvj93tqkesknv6qm";

/// wrapper for a cli testing tool - currently wraps rexpect and dialoguer
///
/// 1. allow more accurate articulation of expected behaviour
/// 2. provide flexibility to swap rexpect for a tool that better maps to
///    expected behaviour
/// 3. provides flexability to swap dialoguer with another cli interaction tool
pub struct CliTester {
    rexpect_session: PtySession,
    formatter: ColorfulTheme,
}

impl CliTester {
    pub fn expect_input(&mut self, prompt: &str) -> Result<CliTesterInputPrompt> {
        let mut i = CliTesterInputPrompt {
            tester: self,
            prompt: prompt.to_string(),
        };
        i.prompt(false).context("initial input prompt")?;
        Ok(i)
    }

    pub fn expect_input_eventually(&mut self, prompt: &str) -> Result<CliTesterInputPrompt> {
        let mut i = CliTesterInputPrompt {
            tester: self,
            prompt: prompt.to_string(),
        };
        i.prompt(true).context("initial input prompt")?;
        Ok(i)
    }
}

pub struct CliTesterInputPrompt<'a> {
    tester: &'a mut CliTester,
    prompt: String,
}

impl CliTesterInputPrompt<'_> {
    fn prompt(&mut self, eventually: bool) -> Result<&mut Self> {
        let mut s = String::new();
        self.tester
            .formatter
            .format_prompt(&mut s, self.prompt.as_str())
            .expect("diagluer theme formatter should succeed");
        s.push(' ');

        ensure!(
            s.contains(self.prompt.as_str()),
            "dialoguer must be broken as formatted prompt success doesnt contain prompt"
        );

        if eventually {
            self.tester
                .expect_eventually(sanatize(s).as_str())
                .context("expect input prompt eventually")?;
        } else {
            self.tester
                .expect(sanatize(s).as_str())
                .context("expect input prompt")?;
        }

        Ok(self)
    }

    pub fn succeeds_with(&mut self, input: &str) -> Result<&mut Self> {
        self.tester.send_line(input)?;
        self.tester
            .expect(input)
            .context("expect input to be printed")?;
        self.tester
            .expect("\r")
            .context("expect new line after input to be printed")?;

        let mut s = String::new();
        self.tester
            .formatter
            .format_input_prompt_selection(&mut s, self.prompt.as_str(), input)
            .expect("diagluer theme formatter should succeed");
        if !s.contains(self.prompt.as_str()) {
            panic!("dialoguer must be broken as formatted prompt success doesnt contain prompt");
        }
        let formatted_success = format!("{}\r\n", sanatize(s));

        self.tester
            .expect(formatted_success.as_str())
            .context("expect immediate prompt success")?;
        Ok(self)
    }
}

impl CliTester {
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: rexpect_with(args).expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }

    pub fn restart_with<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.rexpect_session
            .process
            .exit()
            .expect("process to exit");
        self.rexpect_session = rexpect_with(args).expect("rexpect to spawn new process");
        self
    }

    pub fn exit(&mut self) -> Result<()> {
        match self
            .rexpect_session
            .process
            .exit()
            .context("expect proccess to exit")
        {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// returns what came before expected message
    pub fn expect_eventually(&mut self, message: &str) -> Result<String> {
        let before = self
            .rexpect_session
            .exp_string(message)
            .context("exp_string failed")?;
        Ok(before)
    }

    pub fn expect(&mut self, message: &str) -> Result<&mut Self> {
        let before = self.expect_eventually(message)?;
        ensure!(
            before.is_empty(),
            format!(
                "expected message \"{}\". but got \"{}\" first.",
                message.replace('\n', "\\n").replace('\r', "\\r"),
                before.replace('\n', "\\n").replace('\r', "\\r"),
            ),
        );
        Ok(self)
    }

    pub fn expect_end(&mut self) -> Result<()> {
        let before = self
            .rexpect_session
            .exp_eof()
            .context("expected immediate end but got timed out")?;
        ensure!(
            before.is_empty(),
            format!(
                "expected immediate end but got '{}' first.",
                before.replace('\n', "\\n").replace('\r', "\\r"),
            ),
        );
        Ok(())
    }

    pub fn expect_end_with(&mut self, message: &str) -> Result<()> {
        let before = self
            .rexpect_session
            .exp_eof()
            .context("expected immediate end but got timed out")?;
        assert_eq!(before, message);
        Ok(())
    }
    pub fn expect_end_eventually(&mut self) -> Result<String> {
        self.rexpect_session
            .exp_eof()
            .context("expected end eventually but got timed out")
    }

    pub fn expect_end_eventually_with(&mut self, message: &str) -> Result<()> {
        self.expect_eventually(message)?;
        self.expect_end()
    }

    fn send_line(&mut self, line: &str) -> Result<()> {
        self.rexpect_session
            .send_line(line)
            .context("send_line failed")?;
        Ok(())
    }
}

/// sanatize unicode string for rexpect
fn sanatize(s: String) -> String {
    // remove ansi codes as they don't work with rexpect
    strip_str(s)
        // sanatize unicode rexpect issue 105 is resolved https://github.com/rust-cli/rexpect/issues/105
        .as_bytes()
        .iter()
        .map(|c| *c as char)
        .collect::<String>()
}

pub fn rexpect_with<I, S>(args: I) -> Result<PtySession, rexpect::error::Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("ngit"));
    cmd.args(args);
    // using branch for PR https://github.com/rust-cli/rexpect/pull/103 to strip ansi escape codes
    rexpect::session::spawn_with_options(
        cmd,
        Options {
            timeout_ms: Some(2000),
            strip_ansi_escape_codes: true,
        },
    )
}

/// backup and remove application config and data
pub fn before() -> Result<()> {
    backup_existing_config()
}

/// restore backuped application config and data
pub fn after() -> Result<()> {
    restore_config_backup()
}

/// run func between before and after scripts which backup, reset and restore
/// application config
///
/// TODO: fix issue: if func panics, after() is not run.
pub fn with_fresh_config<F>(func: F) -> Result<()>
where
    F: Fn() -> Result<()>,
{
    before()?;
    func()?;
    after()
}

fn backup_existing_config() -> Result<()> {
    let config_path = get_dirs().config_dir().join("config.json");
    let backup_config_path = get_dirs().config_dir().join("config-backup.json");
    if config_path.exists() {
        std::fs::rename(config_path, backup_config_path)?;
    }
    Ok(())
}

fn restore_config_backup() -> Result<()> {
    let config_path = get_dirs().config_dir().join("config.json");
    let backup_config_path = get_dirs().config_dir().join("config-backup.json");
    if config_path.exists() {
        std::fs::remove_file(&config_path)?;
    }
    if backup_config_path.exists() {
        std::fs::rename(backup_config_path, config_path)?;
    }
    Ok(())
}

fn get_dirs() -> ProjectDirs {
    ProjectDirs::from("", "CodeCollaboration", "ngit")
        .expect("rust directories crate should return ProjectDirs")
}
