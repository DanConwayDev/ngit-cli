use std::{ffi::OsStr, path::PathBuf, str::FromStr};

use anyhow::{bail, ensure, Context, Result};
use dialoguer::theme::{ColorfulTheme, Theme};
use directories::ProjectDirs;
use nostr::{self, Kind, Tag};
use once_cell::sync::Lazy;
use rexpect::session::{Options, PtySession};
use strip_ansi_escapes::strip_str;

pub mod git;
pub mod relay;

pub static PATCH_KIND: u64 = 1617;
pub static REPOSITORY_KIND: u64 = 30617;

pub static TEST_KEY_1_NSEC: &str =
    "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";
pub static TEST_KEY_1_SK_HEX: &str =
    "08608a436aee4c07ea5c36f85cb17c58f52b3ad7094f9318cc777771f0bf218b";
pub static TEST_KEY_1_NPUB: &str =
    "npub175lyhnt6nn00qjw0v3navw9pxgv43txnku0tpxprl4h6mvpr6a5qlphudg";
pub static TEST_KEY_1_PUBKEY_HEX: &str =
    "f53e4bcd7a9cdef049cf6467d638a1321958acd3b71eb09823fd6fadb023d768";
pub static TEST_KEY_1_DISPLAY_NAME: &str = "bob";
pub static TEST_KEY_1_ENCRYPTED: &str = "ncryptsec1qyq607h3cykxc3f2a44u89cdk336fptccn3fm5pf3nmf93d3c86qpunc7r6klwcn6lyszjy72wxwqq9aljg4pm6atvjrds9e248yhv76xfnt464265kgnjsvg8rlg06wg4sp9uljzfpu8zuaztcvfn2j8ggdrg8mldh850cy75efsyqqansert9wqmn4e6khpgvfz7h5le9";
pub static TEST_KEY_1_ENCRYPTED_WEAK: &str = "ncryptsec1qy8ke0tjqnn8wt3w6lnc86c27ry3qrptxctjfcgruryxy0at238kwyjwsswd7z88thysruzw3awlrsxjvw5uptcd7vt70ft9rtkx00m8cgy3khm4hxa5d2gfnc6athnfruy2eyl6pkas8k34jg85z7xjqqadzfzh9rp0fzxqtw0tvxksac3n8yc98uksvuf93e0lcvqy8j6";
pub static TEST_KEY_1_KEYS: Lazy<nostr::Keys> =
    Lazy::new(|| nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap());

pub fn generate_test_key_1_metadata_event(name: &str) -> nostr::Event {
    nostr::event::EventBuilder::metadata(&nostr::Metadata::new().name(name))
        .to_event(&TEST_KEY_1_KEYS)
        .unwrap()
}

pub fn generate_test_key_1_metadata_event_old(name: &str) -> nostr::Event {
    make_event_old_or_change_user(
        generate_test_key_1_metadata_event(name),
        &TEST_KEY_1_KEYS,
        10000,
    )
}

pub fn generate_test_key_1_kind_event(kind: Kind) -> nostr::Event {
    nostr::event::EventBuilder::new(kind, "", [])
        .to_event(&TEST_KEY_1_KEYS)
        .unwrap()
}

pub fn generate_test_key_1_relay_list_event() -> nostr::Event {
    nostr::event::EventBuilder::new(
        nostr::Kind::RelayList,
        "",
        [
            nostr::Tag::RelayMetadata(
                "ws://localhost:8053".into(),
                Some(nostr::RelayMetadata::Write),
            ),
            nostr::Tag::RelayMetadata(
                "ws://localhost:8054".into(),
                Some(nostr::RelayMetadata::Read),
            ),
            nostr::Tag::RelayMetadata("ws://localhost:8055".into(), None),
        ],
    )
    .to_event(&TEST_KEY_1_KEYS)
    .unwrap()
}

pub fn generate_test_key_1_relay_list_event_same_as_fallback() -> nostr::Event {
    nostr::event::EventBuilder::new(
        nostr::Kind::RelayList,
        "",
        [
            nostr::Tag::RelayMetadata(
                "ws://localhost:8051".into(),
                Some(nostr::RelayMetadata::Write),
            ),
            nostr::Tag::RelayMetadata(
                "ws://localhost:8052".into(),
                Some(nostr::RelayMetadata::Write),
            ),
        ],
    )
    .to_event(&TEST_KEY_1_KEYS)
    .unwrap()
}

pub static TEST_KEY_2_NSEC: &str =
    "nsec1ypglg6nj6ep0g2qmyfqcv2al502gje3jvpwye6mthmkvj93tqkesknv6qm";
pub static TEST_KEY_2_NPUB: &str =
    "npub1h2yz2eh0798nh25hvypenrz995nla9dktfuk565ljf3ghnkhdljsul834e";

pub static TEST_KEY_2_DISPLAY_NAME: &str = "carole";
pub static TEST_KEY_2_ENCRYPTED: &str = "...2";
pub static TEST_KEY_2_KEYS: Lazy<nostr::Keys> =
    Lazy::new(|| nostr::Keys::from_str(TEST_KEY_2_NSEC).unwrap());

pub fn generate_test_key_2_metadata_event(name: &str) -> nostr::Event {
    nostr::event::EventBuilder::metadata(&nostr::Metadata::new().name(name))
        .to_event(&TEST_KEY_2_KEYS)
        .unwrap()
}

pub static TEST_INVALID_NSEC: &str = "nsec1ppsg5sm2aex";
pub static TEST_PASSWORD: &str = "769dfd£pwega8SHGv3!#Bsfd5t";
pub static TEST_INVALID_PASSWORD: &str = "INVALID769dfd£pwega8SHGv3!";
pub static TEST_WEAK_PASSWORD: &str = "fhaiuhfwe";
pub static TEST_RANDOM_TOKEN: &str = "lkjh2398HLKJ43hrweiJ6FaPfdssgtrg";

pub fn make_event_old_or_change_user(
    event: nostr::Event,
    keys: &nostr::Keys,
    how_old_in_secs: u64,
) -> nostr::Event {
    let mut unsigned =
        nostr::event::EventBuilder::new(event.kind, event.content.clone(), event.tags.clone())
            .to_unsigned_event(keys.public_key());

    unsigned.created_at =
        nostr::types::Timestamp::from(nostr::types::Timestamp::now().as_u64() - how_old_in_secs);
    unsigned.id = nostr::EventId::new(
        &keys.public_key(),
        unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );

    unsigned.sign(keys).unwrap()
}

pub fn generate_repo_ref_event() -> nostr::Event {
    // taken from test git_repo
    // TODO - this may not be consistant across computers as it might take the
    // author and committer from global git config
    let root_commit = "9ee507fc4357d7ee16a5d8901bedcd103f23c17d";
    nostr::event::EventBuilder::new(
        nostr::Kind::Custom(REPOSITORY_KIND),
        "",
        [
            Tag::Identifier(
                // root_commit.to_string()
                format!("{}-consider-it-random", root_commit),
            ),
            Tag::Reference(root_commit.into()),
            Tag::Name("example name".into()),
            Tag::Description("example description".into()),
            Tag::Generic(
                nostr::TagKind::Custom("clone".to_string()),
                vec!["git:://123.gitexample.com/test".to_string()],
            ),
            Tag::Generic(
                nostr::TagKind::Custom("web".to_string()),
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string(),
                ],
            ),
            Tag::Generic(
                nostr::TagKind::Custom("relays".to_string()),
                vec![
                    "ws://localhost:8055".to_string(),
                    "ws://localhost:8056".to_string(),
                ],
            ),
            Tag::Generic(
                nostr::TagKind::Custom("maintainers".to_string()),
                vec![
                    TEST_KEY_1_KEYS.public_key().to_string(),
                    TEST_KEY_2_KEYS.public_key().to_string(),
                ],
            ),
        ],
    )
    .to_event(&TEST_KEY_1_KEYS)
    .unwrap()
}

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

    pub fn expect_password(&mut self, prompt: &str) -> Result<CliTesterPasswordPrompt> {
        let mut i = CliTesterPasswordPrompt {
            tester: self,
            prompt: prompt.to_string(),
            confirmation_prompt: "".to_string(),
        };
        i.prompt().context("initial password prompt")?;
        Ok(i)
    }

    pub fn expect_confirm(
        &mut self,
        prompt: &str,
        default: Option<bool>,
    ) -> Result<CliTesterConfirmPrompt> {
        let mut i = CliTesterConfirmPrompt {
            tester: self,
            prompt: prompt.to_string(),
            default,
        };
        i.prompt(false, default).context("initial confirm prompt")?;
        Ok(i)
    }

    pub fn expect_confirm_eventually(
        &mut self,
        prompt: &str,
        default: Option<bool>,
    ) -> Result<CliTesterConfirmPrompt> {
        let mut i = CliTesterConfirmPrompt {
            tester: self,
            prompt: prompt.to_string(),
            default,
        };
        i.prompt(true, default).context("initial confirm prompt")?;
        Ok(i)
    }

    pub fn expect_choice(
        &mut self,
        prompt: &str,
        choices: Vec<String>,
    ) -> Result<CliTesterChoicePrompt> {
        let mut i = CliTesterChoicePrompt {
            tester: self,
            prompt: prompt.to_string(),
            choices,
        };
        i.prompt(false).context("initial confirm prompt")?;
        Ok(i)
    }

    pub fn expect_multi_select(
        &mut self,
        prompt: &str,
        choices: Vec<String>,
    ) -> Result<CliTesterMultiSelectPrompt> {
        let mut i = CliTesterMultiSelectPrompt {
            tester: self,
            prompt: prompt.to_string(),
            choices,
        };
        i.prompt(false).context("initial confirm prompt")?;
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

pub struct CliTesterPasswordPrompt<'a> {
    tester: &'a mut CliTester,
    prompt: String,
    confirmation_prompt: String,
}

impl CliTesterPasswordPrompt<'_> {
    fn prompt(&mut self) -> Result<&mut Self> {
        let p = match self.confirmation_prompt.is_empty() {
            true => self.prompt.as_str(),
            false => self.confirmation_prompt.as_str(),
        };

        let mut s = String::new();
        self.tester
            .formatter
            .format_password_prompt(&mut s, p)
            .expect("diagluer theme formatter should succeed");

        ensure!(s.contains(p), "dialoguer must be broken");

        self.tester
            .expect(format!("\r{}", sanatize(s)).as_str())
            .context("expect password input prompt")?;
        Ok(self)
    }

    pub fn with_confirmation(&mut self, prompt: &str) -> Result<&mut Self> {
        self.confirmation_prompt = prompt.to_string();
        Ok(self)
    }

    pub fn succeeds_with(&mut self, password: &str) -> Result<&mut Self> {
        self.tester.send_line(password)?;

        self.tester
            .expect("\r\n")
            .context("expect new lines after password input")?;

        if !self.confirmation_prompt.is_empty() {
            self.prompt()
                .context("expect password confirmation prompt")?;
            self.tester.send_line(password)?;
            self.tester
                .expect("\r\n\r")
                .context("expect new lines after password confirmation input")?;
        }

        let mut s = String::new();
        self.tester
            .formatter
            .format_password_prompt_selection(&mut s, self.prompt.as_str())
            .expect("diagluer theme formatter should succeed");

        ensure!(s.contains(self.prompt.as_str()), "dialoguer must be broken");

        self.tester
            .expect(format!("\r{}\r\n", sanatize(s)).as_str())
            .context("expect password prompt success")?;

        Ok(self)
    }
}

pub struct CliTesterConfirmPrompt<'a> {
    tester: &'a mut CliTester,
    prompt: String,
    default: Option<bool>,
}

impl CliTesterConfirmPrompt<'_> {
    fn prompt(&mut self, eventually: bool, default: Option<bool>) -> Result<&mut Self> {
        let mut s = String::new();
        self.tester
            .formatter
            .format_confirm_prompt(&mut s, self.prompt.as_str(), default)
            .expect("diagluer theme formatter should succeed");
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
                .context("expect confirm prompt")?;
        }

        Ok(self)
    }

    pub fn succeeds_with(&mut self, input: Option<bool>) -> Result<&mut Self> {
        self.tester.send_line(match input {
            None => "",
            Some(true) => "y",
            Some(false) => "n",
        })?;
        self.tester
            .expect("\r")
            .context("expect new line after confirm input to be printed")?;

        let mut s = String::new();
        self.tester
            .formatter
            .format_confirm_prompt_selection(
                &mut s,
                self.prompt.as_str(),
                match input {
                    None => self.default,
                    Some(_) => input,
                },
            )
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

pub struct CliTesterMultiSelectPrompt<'a> {
    tester: &'a mut CliTester,
    prompt: String,
    choices: Vec<String>,
}

impl CliTesterMultiSelectPrompt<'_> {
    fn prompt(&mut self, eventually: bool) -> Result<&mut Self> {
        if eventually {
            self.tester
                .expect_eventually(format!("{}:\r\n", self.prompt))
                .context("expect multi-select prompt eventually")?;
        } else {
            self.tester
                .expect(format!("{}:\r\n", self.prompt))
                .context("expect multi-select prompt")?;
        }
        Ok(self)
    }

    pub fn succeeds_with(
        &mut self,
        chosen_indexes: Vec<usize>,
        report: bool,
        default_indexes: Vec<usize>,
    ) -> Result<&mut Self> {
        if report {
            bail!("TODO: add support for report")
        }

        fn show_options(
            tester: &mut CliTester,
            choices: &[String],
            active_index: usize,
            selected_indexes: &[usize],
        ) -> Result<()> {
            for (index, item) in choices.iter().enumerate() {
                tester.expect(format!(
                    "{}{}{}\r\n",
                    if active_index.eq(&index) { "> " } else { "  " },
                    if selected_indexes.iter().any(|i| i.eq(&index)) {
                        "[x] "
                    } else {
                        "[ ] "
                    },
                    item,
                ))?;
            }
            Ok(())
        }

        show_options(self.tester, &self.choices, 0, &default_indexes)?;

        if default_indexes.eq(&chosen_indexes) {
            self.tester.send("\r\n")?;
        } else {
            bail!("TODO: add support changing options");
        }

        for _ in self.choices.iter() {
            self.tester.expect("\r")?;
        }
        // one for removing prompt maybe?
        self.tester.expect("\r")?;

        Ok(self)
    }
}

pub struct CliTesterChoicePrompt<'a> {
    tester: &'a mut CliTester,
    prompt: String,
    choices: Vec<String>,
}

impl CliTesterChoicePrompt<'_> {
    fn prompt(&mut self, eventually: bool) -> Result<&mut Self> {
        let mut s = String::new();
        self.tester
            .formatter
            .format_select_prompt(&mut s, self.prompt.as_str())
            .expect("diagluer theme formatter should succeed");
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
                .context("expect confirm prompt")?;
        }

        Ok(self)
    }

    pub fn succeeds_with(
        &mut self,
        chosen_index: u64,
        report: bool,
        default_index: Option<u64>,
    ) -> Result<&mut Self> {
        if default_index.is_some() {
            println!("TODO: add support for default choice")
        }

        fn show_options(
            tester: &mut CliTester,
            choices: &[String],
            selected_index: Option<usize>,
        ) -> Result<()> {
            if selected_index.is_some() {
                for _ in 0..choices.len() {
                    tester.expect("\r").context("expect new line per choice")?;
                }
            } else {
                tester
                    .expect("\r\n")
                    .context("expect new line before choices")?;
            }

            for (index, item) in choices.iter().enumerate() {
                let mut s = String::new();
                tester
                    .formatter
                    .format_select_prompt_item(
                        &mut s,
                        item.as_str(),
                        if let Some(i) = selected_index {
                            index == i
                        } else {
                            false
                        },
                    )
                    .expect("diagluer theme formatter should succeed");
                ensure!(
                    s.contains(item.as_str()),
                    "dialoguer must be broken as formatted prompt success doesnt contain prompt"
                );
                tester.expect(sanatize(s)).context("expect choice item")?;

                tester
                    .expect(if choices.len() == index {
                        "\r\r"
                    } else {
                        "\r\n"
                    })
                    .context("expect new line after choice item")?;
            }
            Ok(())
        }
        fn show_selected(
            tester: &mut CliTester,
            prompt: &str,
            choices: &[String],
            selected_index: u64,
        ) -> Result<()> {
            let mut s = String::new();

            let selected = choices[usize::try_from(selected_index)?].clone();
            tester
                .formatter
                .format_select_prompt_selection(&mut s, prompt, selected.as_str())
                .expect("diagluer theme formatter should succeed");
            ensure!(
                s.contains(selected.as_str()),
                "dialoguer must be broken as formatted prompt success doesnt contain prompt"
            );
            tester.expect(sanatize(s)).context("expect choice item")?;
            Ok(())
        }

        show_options(self.tester, &self.choices, None)?;

        for _ in 0..(chosen_index + 1) {
            self.tester.send("j")?;
        }

        self.tester.send(" ")?;

        for index in 0..(chosen_index + 1) {
            show_options(self.tester, &self.choices, Some(usize::try_from(index)?))?;
        }

        for _ in 0..self.choices.len() {
            self.tester
                .expect("\r")
                .context("expect new line per option")?;
        }

        self.tester
            .expect("\r")
            .context("expect new line after options")?;

        if report {
            show_selected(self.tester, &self.prompt, &self.choices, chosen_index)?;
            self.tester
                .expect("\r\n")
                .context("expect new line at end")?;
        }

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
            rexpect_session: rexpect_with(args, 2000).expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }
    pub fn new_from_dir<I, S>(dir: &PathBuf, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: rexpect_with_from_dir(dir, args, 2000)
                .expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }
    pub fn new_with_timeout<I, S>(timeout_ms: u64, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: rexpect_with(args, timeout_ms).expect("rexpect to spawn new process"),
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
        self.rexpect_session = rexpect_with(args, 2000).expect("rexpect to spawn new process");
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

    fn exp_string(&mut self, message: &str) -> Result<String> {
        match self
            .rexpect_session
            .exp_string(message)
            .context("expected immediate end but got timed out")
        {
            Ok(before) => Ok(before),
            Err(e) => {
                for p in [51, 52, 53, 55, 56, 57] {
                    let _ = relay::shutdown_relay(8000 + p);
                }
                Err(e)
            }
        }
    }

    /// returns what came before expected message
    pub fn expect_eventually<S>(&mut self, message: S) -> Result<String>
    where
        S: Into<String>,
    {
        let message_string = message.into();
        let message = message_string.as_str();
        let before = self.exp_string(message).context("exp_string failed")?;
        Ok(before)
    }

    pub fn expect_after_whitespace<S>(&mut self, message: S) -> Result<&mut Self>
    where
        S: Into<String>,
    {
        assert_eq!("", self.expect_eventually(message)?.trim());
        Ok(self)
    }

    pub fn expect<S>(&mut self, message: S) -> Result<&mut Self>
    where
        S: Into<String>,
    {
        let message_string = message.into();
        let message = message_string.as_str();
        let before = self.expect_eventually(message)?;
        if !before.is_empty() {
            std::fs::write("test-cli-expect-output.txt", before.clone())?;

            // let mut output = std::fs::File::create("aaaaaaaaaaa.txt")?;
            // write!(output, "{}", *before);
        }
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

    fn exp_eof(&mut self) -> Result<String> {
        match self
            .rexpect_session
            .exp_eof()
            .context("expected end but got timed out")
        {
            Ok(before) => Ok(before),
            Err(e) => {
                for p in [51, 52, 53, 55, 56, 57] {
                    let _ = relay::shutdown_relay(8000 + p);
                }
                Err(e)
            }
        }
    }

    pub fn expect_end(&mut self) -> Result<()> {
        let before = self
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
            .exp_eof()
            .context("expected immediate end but got timed out")?;
        assert_eq!(before, message);
        Ok(())
    }

    pub fn expect_end_eventually_and_print(&mut self) -> Result<()> {
        let before = self.exp_eof().context("expected end but got timed out")?;
        println!("ended eventually with:");
        println!("{}", &before);
        Ok(())
    }

    pub fn expect_end_with_whitespace(&mut self) -> Result<()> {
        let before = self
            .exp_eof()
            .context("expected immediate end but got timed out")?;
        assert_eq!(before.trim(), "");
        Ok(())
    }

    pub fn expect_end_eventually(&mut self) -> Result<String> {
        self.exp_eof()
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

    fn send(&mut self, s: &str) -> Result<()> {
        self.rexpect_session.send(s).context("send failed")?;
        self.rexpect_session.flush()?;
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

pub fn rexpect_with<I, S>(args: I, timeout_ms: u64) -> Result<PtySession, rexpect::error::Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("ngit"));
    cmd.env("NGITTEST", "TRUE");
    cmd.env("RUST_BACKTRACE", "0");
    cmd.args(args);
    // using branch for PR https://github.com/rust-cli/rexpect/pull/103 to strip ansi escape codes
    rexpect::session::spawn_with_options(
        cmd,
        Options {
            timeout_ms: Some(timeout_ms),
            strip_ansi_escape_codes: true,
        },
    )
}

pub fn rexpect_with_from_dir<I, S>(
    dir: &PathBuf,
    args: I,
    timeout_ms: u64,
) -> Result<PtySession, rexpect::error::Error>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("ngit"));
    cmd.env("NGITTEST", "TRUE");
    cmd.env("RUST_BACKTRACE", "0");
    cmd.current_dir(dir);
    cmd.args(args);
    // using branch for PR https://github.com/rust-cli/rexpect/pull/103 to strip ansi escape codes
    rexpect::session::spawn_with_options(
        cmd,
        Options {
            timeout_ms: Some(timeout_ms),
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
