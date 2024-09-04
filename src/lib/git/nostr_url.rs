use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use nostr::nips::nip01::Coordinate;
use nostr_sdk::{PublicKey, Url};

#[derive(Debug, PartialEq)]
pub enum ServerProtocol {
    Ssh,
    Https,
    Http,
    Git,
}

#[derive(Debug, PartialEq)]
pub struct NostrUrlDecoded {
    pub coordinates: HashSet<Coordinate>,
    pub protocol: Option<ServerProtocol>,
    pub user: Option<String>,
}

static INCORRECT_NOSTR_URL_FORMAT_ERROR: &str = "incorrect nostr git url format. try nostr://naddr123 or nostr://npub123/my-repo or nostr://ssh/npub123/relay.damus.io/my-repo";

impl std::str::FromStr for NostrUrlDecoded {
    type Err = anyhow::Error;

    fn from_str(url: &str) -> Result<Self> {
        let mut coordinates = HashSet::new();
        let mut protocol = None;
        let mut user = None;
        let mut relays = vec![];

        if !url.starts_with("nostr://") {
            bail!("nostr git url must start with nostr://");
        }
        // process get url parameters if present
        for (name, value) in Url::parse(url)?.query_pairs() {
            if name.contains("relay") {
                let mut decoded = urlencoding::decode(&value)
                    .context("could not parse relays in nostr git url")?
                    .to_string();
                if !decoded.starts_with("ws://") && !decoded.starts_with("wss://") {
                    decoded = format!("wss://{decoded}");
                }
                let url =
                    Url::parse(&decoded).context("could not parse relays in nostr git url")?;
                relays.push(url.to_string());
            } else if name == "protocol" {
                protocol = match value.as_ref() {
                    "ssh" => Some(ServerProtocol::Ssh),
                    "https" => Some(ServerProtocol::Https),
                    "http" => Some(ServerProtocol::Http),
                    "git" => Some(ServerProtocol::Git),
                    _ => None,
                };
            } else if name == "user" {
                user = Some(value.to_string());
            }
        }

        let mut parts: Vec<&str> = url[8..]
            .split('?')
            .next()
            .unwrap_or("")
            .split('/')
            .collect();

        // extract optional protocol
        if protocol.is_none() {
            let part = parts.first().context(INCORRECT_NOSTR_URL_FORMAT_ERROR)?;
            let protocol_str = if let Some(at_index) = part.find('@') {
                user = Some(part[..at_index].to_string());
                &part[at_index + 1..]
            } else {
                part
            };
            protocol = match protocol_str {
                "ssh" => Some(ServerProtocol::Ssh),
                "https" => Some(ServerProtocol::Https),
                "http" => Some(ServerProtocol::Http),
                "git" => Some(ServerProtocol::Git),
                _ => protocol,
            };
            if protocol.is_some() {
                parts.remove(0);
            }
        }
        // extract naddr npub/<optional-relays>/identifer
        let part = parts.first().context(INCORRECT_NOSTR_URL_FORMAT_ERROR)?;
        // naddr used
        if let Ok(coordinate) = Coordinate::parse(part) {
            if coordinate.kind.eq(&nostr_sdk::Kind::GitRepoAnnouncement) {
                coordinates.insert(coordinate);
            } else {
                bail!("naddr doesnt point to a git repository announcement");
            }
        // npub/<optional-relays>/identifer used
        } else if let Ok(public_key) = PublicKey::parse(part) {
            parts.remove(0);
            let identifier = parts
                .pop()
                .context("nostr url must have an identifier eg. nostr://npub123/repo-identifier")?
                .to_string();
            for relay in parts {
                let mut decoded = urlencoding::decode(relay)
                    .context("could not parse relays in nostr git url")?
                    .to_string();
                if !decoded.starts_with("ws://") && !decoded.starts_with("wss://") {
                    decoded = format!("wss://{decoded}");
                }
                let url =
                    Url::parse(&decoded).context("could not parse relays in nostr git url")?;
                relays.push(url.to_string());
            }
            coordinates.insert(Coordinate {
                identifier,
                public_key,
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays,
            });
        } else {
            bail!(INCORRECT_NOSTR_URL_FORMAT_ERROR);
        }

        Ok(Self {
            coordinates,
            protocol,
            user,
        })
    }
}

/** produce error when using local repo or custom protocols */
pub fn convert_clone_url_to_https(url: &str) -> Result<String> {
    // Strip credentials if present
    let stripped_url = strip_credentials(url);

    // Check if the URL is already in HTTPS format
    if stripped_url.starts_with("https://") {
        return Ok(stripped_url);
    }
    // Convert http:// to https://
    else if stripped_url.starts_with("http://") {
        return Ok(stripped_url.replace("http://", "https://"));
    }
    // Check if the URL starts with SSH
    else if stripped_url.starts_with("ssh://") {
        // Convert SSH to HTTPS
        let parts: Vec<&str> = stripped_url
            .trim_start_matches("ssh://")
            .split('/')
            .collect();
        if parts.len() >= 2 {
            // Construct the HTTPS URL
            return Ok(format!("https://{}/{}", parts[0], parts[1..].join("/")));
        }
        bail!("Invalid SSH URL format: {}", url);
    }
    // Convert ftp:// to https://
    else if stripped_url.starts_with("ftp://") {
        return Ok(stripped_url.replace("ftp://", "https://"));
    }
    // Convert git:// to https://
    else if stripped_url.starts_with("git://") {
        return Ok(stripped_url.replace("git://", "https://"));
    }

    // If the URL is neither HTTPS, SSH, nor git@, return an error
    bail!("Unsupported URL protocol: {}", url);
}

// Function to strip username and password from the URL
fn strip_credentials(url: &str) -> String {
    if let Some(pos) = url.find("://") {
        let (protocol, rest) = url.split_at(pos + 3); // Split at "://"
        let rest_parts: Vec<&str> = rest.split('@').collect();
        if rest_parts.len() > 1 {
            // If there are credentials, return the URL without them
            return format!("{}{}", protocol, rest_parts[1]);
        }
    } else if let Some(at_pos) = url.find('@') {
        // Handle user@host:path format
        let (_, rest) = url.split_at(at_pos);
        // This is a git@ syntax
        let host_and_repo = &rest[1..]; // Skip the ':'
        return format!("ssh://{}", host_and_repo.replace(':', "/"));
    }
    url.to_string() // Return the original URL if no credentials are found
}

#[cfg(test)]
mod tests {
    use super::*;
    mod convert_clone_url_to_https {
        use super::*;

        #[test]
        fn test_https_url() {
            let url = "https://github.com/user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_http_url() {
            let url = "http://github.com/user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_http_url_with_credentials() {
            let url = "http://username:password@github.com/user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_git_at_url() {
            let url = "git@github.com:user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_user_at_url() {
            let url = "user1@github.com:user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_ssh_url() {
            let url = "ssh://github.com/user/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://github.com/user/repo.git");
        }

        #[test]
        fn test_ftp_url() {
            let url = "ftp://example.com/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://example.com/repo.git");
        }

        #[test]
        fn test_git_protocol_url() {
            let url = "git://example.com/repo.git";
            let result = convert_clone_url_to_https(url).unwrap();
            assert_eq!(result, "https://example.com/repo.git");
        }

        #[test]
        fn test_invalid_url() {
            let url = "unsupported://example.com/repo.git";
            let result = convert_clone_url_to_https(url);
            assert!(result.is_err());
        }
    }

    mod nostr_git_url_paramemters_from_str {
        use std::str::FromStr;

        use super::*;

        fn get_model_coordinate(relays: bool) -> Coordinate {
            Coordinate {
                identifier: "ngit".to_string(),
                public_key: PublicKey::parse(
                    "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                )
                .unwrap(),
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays: if relays {
                    vec!["wss://nos.lol/".to_string()]
                } else {
                    vec![]
                },
            }
        }

        #[test]
        fn from_naddr() -> Result<()> {
            assert_eq!(
                NostrUrlDecoded::from_str(
                    "nostr://naddr1qqzxuemfwsqs6amnwvaz7tmwdaejumr0dspzpgqgmmc409hm4xsdd74sf68a2uyf9pwel4g9mfdg8l5244t6x4jdqvzqqqrhnym0k2qj"
                )?,
                NostrUrlDecoded {
                    coordinates: HashSet::from([Coordinate {
                        identifier: "ngit".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .unwrap(),
                        kind: nostr_sdk::Kind::GitRepoAnnouncement,
                        relays: vec!["wss://nos.lol".to_string()], // wont add the slash
                    }]),
                    protocol: None,
                    user: None,
                },
            );
            Ok(())
        }
        mod from_npub_slash_identifier {
            use super::*;

            #[test]
            fn without_relay() -> Result<()> {
                assert_eq!(
                    NostrUrlDecoded::from_str(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                    )?,
                    NostrUrlDecoded {
                        coordinates: HashSet::from([get_model_coordinate(false)]),
                        protocol: None,
                        user: None,
                    },
                );
                Ok(())
            }

            mod with_url_parameters {

                use super::*;

                #[test]
                fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay=nos.lol"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_encoded_relay() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}",
                            urlencoding::encode("wss://nos.lol")
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_multiple_encoded_relays() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}&relay1={}",
                            urlencoding::encode("wss://nos.lol"),
                            urlencoding::encode("wss://relay.damus.io"),
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([Coordinate {
                                identifier: "ngit".to_string(),
                                public_key: PublicKey::parse(
                                    "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                                )
                                .unwrap(),
                                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                                relays: vec![
                                    "wss://nos.lol/".to_string(),
                                    "wss://relay.damus.io/".to_string(),
                                ],
                            }]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_server_protocol() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_server_protocol_and_user() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh&user=fred"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                        },
                    );
                    Ok(())
                }
            }
            mod with_parameters_embedded_with_slashes {
                use super::*;

                #[test]
                fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_encoded_relay() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/ngit",
                            urlencoding::encode("wss://nos.lol")
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(true)]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_multiple_encoded_relays() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(&format!(
                            "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/{}/ngit",
                            urlencoding::encode("wss://nos.lol"),
                            urlencoding::encode("wss://relay.damus.io"),
                        ))?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([Coordinate {
                                identifier: "ngit".to_string(),
                                public_key: PublicKey::parse(
                                    "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                                )
                                .unwrap(),
                                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                                relays: vec![
                                    "wss://nos.lol/".to_string(),
                                    "wss://relay.damus.io/".to_string(),
                                ],
                            }]),
                            protocol: None,
                            user: None,
                        },
                    );
                    Ok(())
                }

                #[test]
                fn with_server_protocol() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                        },
                    );
                    Ok(())
                }
                #[test]
                fn with_server_protocol_and_user() -> Result<()> {
                    assert_eq!(
                        NostrUrlDecoded::from_str(
                            "nostr://fred@ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                        )?,
                        NostrUrlDecoded {
                            coordinates: HashSet::from([get_model_coordinate(false)]),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                        },
                    );
                    Ok(())
                }
            }
        }
    }
}
