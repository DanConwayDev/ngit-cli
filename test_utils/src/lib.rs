use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use dialoguer::theme::{ColorfulTheme, Theme};
use futures::executor::block_on;
use git::GitTestRepo;
use nostr::{self, nips::nip65::RelayMetadata, Kind, Tag};
use nostr_database::{NostrDatabase, Order};
use nostr_sdk::{serde_json, Client, NostrSigner, TagStandard};
use nostr_sqlite::SQLiteDatabase;
use once_cell::sync::Lazy;
use rexpect::session::{Options, PtySession};
use strip_ansi_escapes::strip_str;
use tokio::runtime::Handle;

pub mod git;
pub mod relay;

pub static TEST_KEY_1_NSEC: &str =
    "nsec1ppsg5sm2aexq06juxmu9evtutr6jkwkhp98exxxvwamhru9lyx9s3rwseq";
pub static TEST_KEY_1_SK_HEX: &str =
    "08608a436aee4c07ea5c36f85cb17c58f52b3ad7094f9318cc777771f0bf218b";
pub static TEST_KEY_1_NPUB: &str =
    "npub175lyhnt6nn00qjw0v3navw9pxgv43txnku0tpxprl4h6mvpr6a5qlphudg";
pub static TEST_KEY_1_PUBKEY_HEX: &str =
    "f53e4bcd7a9cdef049cf6467d638a1321958acd3b71eb09823fd6fadb023d768";
pub static TEST_KEY_1_DISPLAY_NAME: &str = "bob";
pub static TEST_KEY_1_ENCRYPTED: &str = "ncryptsec1qgq77e3uftz8dh3jkjxwdms3v6gwqaqduxyzld82kskas8jcs5xup3sf2pc5tr0erqkqrtu0ptnjgjlgvx8lt7c0d7laryq2u7psfa6zm7mk7ln3ln58468shwatm7cx5wy5wvm7yk74ksrngygwxg74";
pub static TEST_KEY_1_ENCRYPTED_WEAK: &str = "ncryptsec1qg835almhlrmyxqtqeva44d5ugm9wk2ccmwspxrqv4wjsdpdlud9es5hsrvs0pas7dvsretm0mc26qwfc7v8986mqngnjshcplnqzj62lxf44a0kkdv788f6dh20x2eum96l2j8v37s5grrheu2hgrkf";
pub static TEST_KEY_1_KEYS: Lazy<nostr::Keys> =
    Lazy::new(|| nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap());

pub static TEST_KEY_1_SIGNER: Lazy<NostrSigner> =
    Lazy::new(|| NostrSigner::Keys(nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap()));

pub fn generate_test_key_1_signer() -> NostrSigner {
    NostrSigner::Keys(nostr::Keys::from_str(TEST_KEY_1_NSEC).unwrap())
}

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
            nostr::Tag::from_standardized(nostr::TagStandard::RelayMetadata {
                relay_url: nostr::Url::from_str("ws://localhost:8053").unwrap(),
                metadata: Some(RelayMetadata::Write),
            }),
            nostr::Tag::from_standardized(nostr::TagStandard::RelayMetadata {
                relay_url: nostr::Url::from_str("ws://localhost:8054").unwrap(),
                metadata: Some(RelayMetadata::Read),
            }),
            nostr::Tag::from_standardized(nostr::TagStandard::RelayMetadata {
                relay_url: nostr::Url::from_str("ws://localhost:8055").unwrap(),
                metadata: None,
            }),
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
            nostr::Tag::from_standardized(nostr::TagStandard::RelayMetadata {
                relay_url: nostr::Url::from_str("ws://localhost:8051").unwrap(),
                metadata: Some(RelayMetadata::Write),
            }),
            nostr::Tag::from_standardized(nostr::TagStandard::RelayMetadata {
                relay_url: nostr::Url::from_str("ws://localhost:8052").unwrap(),
                metadata: Some(RelayMetadata::Write),
            }),
        ],
    )
    .to_event(&TEST_KEY_1_KEYS)
    .unwrap()
}

pub static TEST_KEY_2_NSEC: &str =
    "nsec1ypglg6nj6ep0g2qmyfqcv2al502gje3jvpwye6mthmkvj93tqkesknv6qm";
pub static TEST_KEY_2_NPUB: &str =
    "npub1h2yz2eh0798nh25hvypenrz995nla9dktfuk565ljf3ghnkhdljsul834e";
pub static TEST_KEY_2_PUBKEY_HEX: &str =
    "ba882566eff14f3baa976103998c452d27fe95b65a796a6a9f92628bced76fe5";
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
    unsigned.id = Some(nostr::EventId::new(
        &keys.public_key(),
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    ));

    unsigned.sign(keys).unwrap()
}

pub fn generate_repo_ref_event() -> nostr::Event {
    generate_repo_ref_event_with_git_server(vec!["git:://123.gitexample.com/test".to_string()])
}

pub fn generate_repo_ref_event_with_git_server(git_servers: Vec<String>) -> nostr::Event {
    // taken from test git_repo
    // TODO - this may not be consistant across computers as it might take the
    // author and committer from global git config
    let root_commit = "9ee507fc4357d7ee16a5d8901bedcd103f23c17d";
    nostr::event::EventBuilder::new(
        nostr::Kind::GitRepoAnnouncement,
        "",
        [
            Tag::identifier(
                // root_commit.to_string()
                format!("{}-consider-it-random", root_commit),
            ),
            Tag::from_standardized(TagStandard::Reference(root_commit.to_string())),
            Tag::from_standardized(TagStandard::Name("example name".into())),
            Tag::from_standardized(TagStandard::Description("example description".into())),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("clone")),
                git_servers,
            ),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("web")),
                vec![
                    "https://exampleproject.xyz".to_string(),
                    "https://gitworkshop.dev/123".to_string(),
                ],
            ),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("relays")),
                vec![
                    "ws://localhost:8055".to_string(),
                    "ws://localhost:8056".to_string(),
                ],
            ),
            Tag::custom(
                nostr::TagKind::Custom(std::borrow::Cow::Borrowed("maintainers")),
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

/// enough to fool event_is_patch_set_root
pub fn get_pretend_proposal_root_event() -> nostr::Event {
    serde_json::from_str(r#"{"id":"431e58eb8e1b4e20292d1d5bbe81d5cfb042e1bc165de32eddfdd52245a4cce4","pubkey":"f53e4bcd7a9cdef049cf6467d638a1321958acd3b71eb09823fd6fadb023d768","created_at":1721404213,"kind":1617,"tags":[["a","30617:ba882566eff14f3baa976103998c452d27fe95b65a796a6a9f92628bced76fe5:9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random"],["a","30617:f53e4bcd7a9cdef049cf6467d638a1321958acd3b71eb09823fd6fadb023d768:9ee507fc4357d7ee16a5d8901bedcd103f23c17d-consider-it-random"],["r","9ee507fc4357d7ee16a5d8901bedcd103f23c17d"],["t","cover-letter"],["alt","git patch cover letter: exampletitle"],["t","root"],["e","8cb75aa4cda10a3a0f3242dc49d36159d30b3185bf63414cf6ce17f5c14a73b1","","mention"],["branch-name","feature"],["p","ba882566eff14f3baa976103998c452d27fe95b65a796a6a9f92628bced76fe5"],["p","f53e4bcd7a9cdef049cf6467d638a1321958acd3b71eb09823fd6fadb023d768"]],"content":"From fe973a840fba2a8ab37dd505c154854a69a6505c Mon Sep 17 00:00:00 2001\nSubject: [PATCH 0/2] exampletitle\n\nexampledescription","sig":"37d5b2338bf9fd9d598e6494ae88af9a8dbd52330cfe9d025ee55e35e2f3f55e931ba039d9f7fed8e6fc40206e47619a24f730f8eddc2a07ccfb3988a5005170"}"#).unwrap()
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
        match input {
            None => self.tester.send_line(""),
            Some(true) => self.tester.send("y"),
            Some(false) => self.tester.send("n"),
        }?;
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
            rexpect_session: rexpect_with(args, 4000).expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }
    pub fn new_from_dir<I, S>(dir: &PathBuf, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: rexpect_with_from_dir(dir, args, 4000)
                .expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }
    pub fn new_with_timeout_from_dir<I, S>(timeout_ms: u64, dir: &PathBuf, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: rexpect_with_from_dir(dir, args, timeout_ms)
                .expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }

    pub fn new_remote_helper_from_dir(dir: &PathBuf, nostr_remote_url: &str) -> Self {
        Self {
            rexpect_session: remote_helper_rexpect_with_from_dir(dir, nostr_remote_url, 4000)
                .expect("rexpect to spawn new process"),
            formatter: ColorfulTheme::default(),
        }
    }

    pub fn new_git_with_remote_helper_from_dir<I, S>(dir: &PathBuf, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            rexpect_session: git_with_remote_helper_rexpect_with_from_dir(dir, args, 4000)
                .expect("rexpect to spawn new process"),
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
        self.rexpect_session = rexpect_with(args, 4000).expect("rexpect to spawn new process");
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

    pub fn expect_eventually_and_print<S>(&mut self, message: S) -> Result<String>
    where
        S: Into<String>,
    {
        let message_string = message.into();
        let message = message_string.as_str();
        let before = self.exp_string(message).context("exp_string failed")?;
        println!("{before}");
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

    pub fn send_line(&mut self, line: &str) -> Result<()> {
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

pub fn remote_helper_rexpect_with_from_dir(
    dir: &PathBuf,
    nostr_remote_url: &str,
    timeout_ms: u64,
) -> Result<PtySession, rexpect::error::Error> {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("git-remote-nostr"));
    cmd.env("NGITTEST", "TRUE");
    cmd.env("GIT_DIR", dir);
    cmd.env("RUST_BACKTRACE", "0");
    cmd.current_dir(dir);
    cmd.args([dir.as_os_str().to_str().unwrap(), nostr_remote_url]);
    // using branch for PR https://github.com/rust-cli/rexpect/pull/103 to strip ansi escape codes
    rexpect::session::spawn_with_options(
        cmd,
        Options {
            timeout_ms: Some(timeout_ms),
            strip_ansi_escape_codes: true,
        },
    )
}

pub fn git_with_remote_helper_rexpect_with_from_dir<I, S>(
    dir: &PathBuf,
    args: I,
    timeout_ms: u64,
) -> Result<PtySession>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let git_exec_dir = dir.parent().unwrap().join("tmpgit-git-exec-path");
    if !git_exec_dir.exists() {
        std::fs::create_dir_all(&git_exec_dir)?;
        let src = PathBuf::from(
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .arg("--exec-path")
                    .output()?
                    .stdout,
            )
            .trim()
            .to_string(),
        );
        for entry in (std::fs::read_dir(src)?).flatten() {
            let src_path = entry.path();
            if let Some(name) = src_path.file_name() {
                let _ = std::fs::copy(&src_path, git_exec_dir.join(name));
            }
        }
    }
    std::fs::copy(
        assert_cmd::cargo::cargo_bin("git-remote-nostr"),
        git_exec_dir.join("git-remote-nostr"),
    )?;

    let mut cmd = std::process::Command::new("git");
    cmd.env("GIT_EXEC_PATH", git_exec_dir);
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
    .context("spawning failed")
}

/** copied from client.rs */
async fn get_local_cache_database(git_repo_path: &Path) -> Result<SQLiteDatabase> {
    SQLiteDatabase::open(git_repo_path.join(".git/nostr-cache.sqlite"))
        .await
        .context("cannot open or create nostr cache database at .git/nostr-cache.sqlite")
}

/** copied from client.rs */
pub async fn get_events_from_cache(
    git_repo_path: &Path,
    filters: Vec<nostr::Filter>,
) -> Result<Vec<nostr::Event>> {
    get_local_cache_database(git_repo_path)
        .await?
        .query(filters.clone(), Order::Asc)
        .await
        .context(
            "cannot execute query on opened git repo nostr cache database .git/nostr-cache.sqlite",
        )
}

pub fn get_proposal_branch_name(
    test_repo: &GitTestRepo,
    branch_name_in_event: &str,
) -> Result<String> {
    let events = block_on(get_events_from_cache(
        &test_repo.dir,
        vec![
            nostr::Filter::default()
                .kind(nostr_sdk::Kind::GitPatch)
                .hashtag("root"),
        ],
    ))?;
    get_proposal_branch_name_from_events(&events, branch_name_in_event)
}

pub fn get_proposal_branch_name_from_events(
    events: &Vec<nostr::Event>,
    branch_name_in_event: &str,
) -> Result<String> {
    for event in events {
        if event.tags().iter().any(|t| {
            !t.as_vec()[1].eq("revision-root")
                && event.tags().iter().any(|t| {
                    t.as_vec()[0].eq("branch-name") && t.as_vec()[1].eq(branch_name_in_event)
                })
        }) {
            return Ok(format!(
                "pr/{}({})",
                branch_name_in_event,
                &event.id.to_hex().as_str()[..8],
            ));
        }
    }
    bail!("cannot find proposal root with branch-name tag matching title")
}

pub static FEATURE_BRANCH_NAME_1: &str = "feature-example-t";
pub static FEATURE_BRANCH_NAME_2: &str = "feature-example-f";
pub static FEATURE_BRANCH_NAME_3: &str = "feature-example-c";
pub static FEATURE_BRANCH_NAME_4: &str = "feature-example-d";

pub static PROPOSAL_TITLE_1: &str = "proposal a";
pub static PROPOSAL_TITLE_2: &str = "proposal b";
pub static PROPOSAL_TITLE_3: &str = "proposal c";

pub fn cli_tester_create_proposals() -> Result<GitTestRepo> {
    let git_repo = GitTestRepo::default();
    git_repo.populate()?;
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_1,
        "a",
        Some((PROPOSAL_TITLE_1, "proposal a description")),
        None,
    )?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_2,
        "b",
        Some((PROPOSAL_TITLE_2, "proposal b description")),
        None,
    )?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    cli_tester_create_proposal(
        &git_repo,
        FEATURE_BRANCH_NAME_3,
        "c",
        Some((PROPOSAL_TITLE_3, "proposal c description")),
        None,
    )?;
    Ok(git_repo)
}

pub fn cli_tester_create_proposal_branches_ready_to_send() -> Result<GitTestRepo> {
    let git_repo = GitTestRepo::default();
    git_repo.populate()?;
    create_and_populate_branch(&git_repo, FEATURE_BRANCH_NAME_1, "a", false)?;
    create_and_populate_branch(&git_repo, FEATURE_BRANCH_NAME_2, "b", false)?;
    create_and_populate_branch(&git_repo, FEATURE_BRANCH_NAME_3, "c", false)?;
    Ok(git_repo)
}

pub fn create_and_populate_branch(
    test_repo: &GitTestRepo,
    branch_name: &str,
    prefix: &str,
    only_one_commit: bool,
) -> Result<()> {
    test_repo.checkout("main")?;
    test_repo.create_branch(branch_name)?;
    test_repo.checkout(branch_name)?;
    std::fs::write(
        test_repo.dir.join(format!("{}3.md", prefix)),
        "some content",
    )?;
    test_repo.stage_and_commit(format!("add {}3.md", prefix).as_str())?;
    if !only_one_commit {
        std::fs::write(
            test_repo.dir.join(format!("{}4.md", prefix)),
            "some content",
        )?;
        test_repo.stage_and_commit(format!("add {}4.md", prefix).as_str())?;
    }
    Ok(())
}

pub fn cli_tester_create_proposal(
    test_repo: &GitTestRepo,
    branch_name: &str,
    prefix: &str,
    cover_letter_title_and_description: Option<(&str, &str)>,
    in_reply_to: Option<String>,
) -> Result<()> {
    create_and_populate_branch(test_repo, branch_name, prefix, false)?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    if let Some(in_reply_to) = in_reply_to {
        let mut p = CliTester::new_from_dir(
            &test_repo.dir,
            [
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
                "--disable-cli-spinners",
                "send",
                "HEAD~2",
                "--no-cover-letter",
                "--in-reply-to",
                in_reply_to.as_str(),
            ],
        );
        p.expect_end_eventually()?;
    } else if let Some((title, description)) = cover_letter_title_and_description {
        let mut p = CliTester::new_from_dir(
            &test_repo.dir,
            [
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
                "--disable-cli-spinners",
                "send",
                "HEAD~2",
                "--title",
                format!("\"{title}\"").as_str(),
                "--description",
                format!("\"{description}\"").as_str(),
            ],
        );
        p.expect_end_eventually()?;
    } else {
        let mut p = CliTester::new_from_dir(
            &test_repo.dir,
            [
                "--nsec",
                TEST_KEY_1_NSEC,
                "--password",
                TEST_PASSWORD,
                "--disable-cli-spinners",
                "send",
                "HEAD~2",
                "--no-cover-letter",
            ],
        );
        p.expect_end_eventually()?;
    }
    Ok(())
}

/// returns (originating_repo, test_repo)
pub fn create_proposals_and_repo_with_proposal_pulled_and_checkedout(
    proposal_number: u16,
) -> Result<(GitTestRepo, GitTestRepo)> {
    Ok((
        cli_tester_create_proposals()?,
        create_repo_with_proposal_branch_pulled_and_checkedout(proposal_number)?,
    ))
}

pub fn create_repo_with_proposal_branch_pulled_and_checkedout(
    proposal_number: u16,
) -> Result<GitTestRepo> {
    let test_repo = GitTestRepo::default();
    test_repo.populate()?;
    use_ngit_list_to_download_and_checkout_proposal_branch(&test_repo, proposal_number)?;
    Ok(test_repo)
}

pub fn use_ngit_list_to_download_and_checkout_proposal_branch(
    test_repo: &GitTestRepo,
    proposal_number: u16,
) -> Result<()> {
    let mut p = CliTester::new_from_dir(&test_repo.dir, ["list"]);
    p.expect("fetching updates...\r\n")?;
    p.expect_eventually("\r\n")?; // some updates listed here
    let mut c = p.expect_choice(
        "all proposals",
        vec![
            format!("\"{PROPOSAL_TITLE_3}\""),
            format!("\"{PROPOSAL_TITLE_2}\""),
            format!("\"{PROPOSAL_TITLE_1}\""),
        ],
    )?;
    c.succeeds_with(
        if proposal_number == 3 {
            0
        } else if proposal_number == 2 {
            1
        } else {
            2
        },
        true,
        None,
    )?;
    let mut c = p.expect_choice(
        "",
        vec![
            format!("create and checkout proposal branch (2 ahead 0 behind 'main')"),
            format!("apply to current branch with `git am`"),
            format!("download to ./patches"),
            format!("back"),
        ],
    )?;
    c.succeeds_with(0, false, Some(0))?;
    p.expect_end_eventually()?;
    Ok(())
}

pub fn remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main(
    test_repo: &GitTestRepo,
) -> Result<String> {
    let branch_name = test_repo.get_checked_out_branch_name()?;
    test_repo.checkout("main")?;
    test_repo.git_repo.branch(
        &branch_name,
        &test_repo
            .git_repo
            .find_commit(test_repo.get_tip_of_local_branch(&branch_name)?)?
            .parent(0)?,
        true,
    )?;
    Ok(branch_name)
}

pub fn amend_last_commit(test_repo: &GitTestRepo, commit_msg: &str) -> Result<String> {
    let branch_name =
        remove_latest_commit_so_proposal_branch_is_behind_and_checkout_main(test_repo)?;
    // add another commit (so we have an ammened local branch)
    test_repo.checkout(&branch_name)?;
    std::fs::write(test_repo.dir.join("ammended-commit.md"), commit_msg)?;
    test_repo.stage_and_commit(commit_msg)?;
    Ok(branch_name)
}

pub fn create_proposals_with_first_rebased_and_repo_with_latest_main_and_unrebased_proposal()
-> Result<(GitTestRepo, GitTestRepo)> {
    let (_, test_repo) = create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

    // recreate proposal 1 on top of a another commit (like a rebase on top
    // of one extra commit)
    let second_originating_repo = GitTestRepo::default();
    second_originating_repo.populate()?;
    std::fs::write(
        second_originating_repo.dir.join("amazing.md"),
        "some content",
    )?;
    second_originating_repo.stage_and_commit("commit for rebasing on top of")?;
    cli_tester_create_proposal(
        &second_originating_repo,
        FEATURE_BRANCH_NAME_1,
        "a",
        Some((PROPOSAL_TITLE_1, "proposal a description")),
        Some(get_first_proposal_event_id()?.to_string()),
    )?;

    // pretend we have pulled the updated main branch
    let branch_name = test_repo.get_checked_out_branch_name()?;
    test_repo.checkout("main")?;
    std::fs::write(test_repo.dir.join("amazing.md"), "some content")?;
    test_repo.stage_and_commit("commit for rebasing on top of")?;
    test_repo.checkout(&branch_name)?;
    Ok((second_originating_repo, test_repo))
}

fn get_first_proposal_event_id() -> Result<nostr::EventId> {
    // get proposal id of first
    let client = Client::default();
    Handle::current().block_on(client.add_relay("ws://localhost:8055"))?;
    Handle::current().block_on(client.connect_relay("ws://localhost:8055"))?;
    let proposals = Handle::current().block_on(client.get_events_of(
        vec![
        nostr::Filter::default()
            .kind(nostr::Kind::GitPatch)
            .custom_tag(
                nostr::SingleLetterTag::lowercase(nostr::Alphabet::T),
                vec!["root"],
            ),
    ],
        nostr_sdk::EventSource::relays(Some(Duration::from_millis(500))),
    ))?;
    Handle::current().block_on(client.disconnect())?;

    let proposal_1_id = proposals
        .iter()
        .find(|e| {
            e.tags
                .iter()
                .any(|t| t.as_vec()[1].eq(&FEATURE_BRANCH_NAME_1))
        })
        .unwrap()
        .id;
    Ok(proposal_1_id)
}

pub fn create_proposals_with_first_revised_and_repo_with_unrevised_proposal_checkedout()
-> Result<(GitTestRepo, GitTestRepo)> {
    let (originating_repo, test_repo) =
        create_proposals_and_repo_with_proposal_pulled_and_checkedout(1)?;

    use_ngit_list_to_download_and_checkout_proposal_branch(&originating_repo, 1)?;

    amend_last_commit(&originating_repo, "add some ammended-commit.md")?;

    let mut p = CliTester::new_from_dir(
        &originating_repo.dir,
        [
            "--nsec",
            TEST_KEY_1_NSEC,
            "--password",
            TEST_PASSWORD,
            "--disable-cli-spinners",
            "push",
            "--force",
        ],
    );
    p.expect_end_eventually()?;

    Ok((originating_repo, test_repo))
}
