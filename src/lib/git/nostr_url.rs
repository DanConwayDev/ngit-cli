use core::fmt;
use std::{collections::HashMap, str::FromStr};

use anyhow::{anyhow, bail, Context, Error, Result};
use nostr::nips::{nip01::Coordinate, nip05};
use nostr_sdk::{PublicKey, RelayUrl, ToBech32, Url};

use super::{get_git_config_item, save_git_config_item, Repo};

#[derive(Debug, PartialEq, Default, Clone)]
pub enum ServerProtocol {
    Ssh,
    Https,
    Http,
    Git,
    Ftp,
    Filesystem,
    #[default]
    Unspecified,
    UnauthHttps, // used for read to enable non-interactive failures over https
    UnauthHttp,  // used for read to enable non-interactive failures over https
}
impl fmt::Display for ServerProtocol {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ServerProtocol::Http => write!(f, "http"),
            ServerProtocol::Https => write!(f, "https"),
            ServerProtocol::Ftp => write!(f, "ftp"),
            ServerProtocol::Ssh => write!(f, "ssh"),
            ServerProtocol::Git => write!(f, "git"),
            ServerProtocol::Filesystem => write!(f, "filesystem"),
            ServerProtocol::Unspecified => write!(f, "unsepcified"),
            ServerProtocol::UnauthHttps => write!(f, "https (unauthenticated)"),
            ServerProtocol::UnauthHttp => write!(f, "http (unauthenticated)"),
        }
    }
}

impl FromStr for ServerProtocol {
    type Err = Error;

    // Method to convert a string to a ServerProtocol variant
    fn from_str(s: &str) -> Result<ServerProtocol> {
        match s {
            "http" => Ok(ServerProtocol::Http),
            "https" => Ok(ServerProtocol::Https),
            "ftp" => Ok(ServerProtocol::Ftp),
            "ssh" => Ok(ServerProtocol::Ssh),
            "git" => Ok(ServerProtocol::Git),
            "filesystem" => Ok(ServerProtocol::Filesystem),
            "http (unauthenticated)" => Ok(ServerProtocol::UnauthHttp),
            "https (unauthenticated)" => Ok(ServerProtocol::UnauthHttps),
            _ => bail!("not listed as a server protocol"),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct NostrUrlDecoded {
    pub original_string: String,
    pub coordinate: Coordinate,
    pub protocol: Option<ServerProtocol>,
    pub user: Option<String>,
    pub nip05: Option<String>,
}

impl fmt::Display for NostrUrlDecoded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "nostr://")?;
        if let Some(user) = &self.user {
            write!(f, "{}@", user)?;
        }
        if let Some(protocol) = &self.protocol {
            write!(f, "{}/", protocol)?;
        }
        write!(f, "{}/", self.coordinate.public_key.to_bech32().unwrap())?;
        if let Some(relay) = self.coordinate.relays.first() {
            write!(
                f,
                "{}/",
                urlencoding::encode(
                    relay
                        .as_str_without_trailing_slash()
                        .replace("wss://", "")
                        .as_str()
                )
            )?;
        }
        write!(f, "{}", self.coordinate.identifier)
    }
}

static INCORRECT_NOSTR_URL_FORMAT_ERROR: &str = "incorrect nostr git url format. try nostr://naddr123 or nostr://npub123/my-repo or nostr://ssh/npub123/relay.damus.io/my-repo";

impl NostrUrlDecoded {
    pub async fn parse_and_resolve(url: &str, git_repo: &Option<&Repo>) -> Result<Self> {
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
                    RelayUrl::parse(&decoded).context("could not parse relays in nostr git url")?;
                relays.push(url);
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
            let protocol_str = if part.contains('.') {
                part
            } else if let Some(at_index) = part.find('@') {
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
                _ => None,
            };
            if protocol.is_some() {
                parts.remove(0);
            }
        }
        // extract naddr npub/<optional-relays>/identifer
        let part = parts.first().context(INCORRECT_NOSTR_URL_FORMAT_ERROR)?;
        let mut nip05 = None;
        // naddr used
        let coordinate = if let Ok(coordinate) = Coordinate::parse(part) {
            if coordinate.kind.eq(&nostr_sdk::Kind::GitRepoAnnouncement) {
                coordinate
            } else {
                bail!("naddr doesnt point to a git repository announcement");
            }
        // <npub|nip05_address>/<optional-relays>/identifer used
        } else {
            let npub_or_nip05 = part.to_owned();
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
                    RelayUrl::parse(&decoded).context("could not parse relays in nostr git url")?;
                relays.push(url);
            }
            let public_key = match PublicKey::parse(npub_or_nip05) {
                Ok(public_key) => public_key,
                Err(_) => {
                    nip05 = Some(npub_or_nip05.to_string());
                    if let Ok(public_key) =
                        resolve_nip05_from_git_config_cache(npub_or_nip05, git_repo)
                    {
                        public_key
                    } else {
                        let term = console::Term::stderr();
                        let domain = {
                            let s = npub_or_nip05.split('@').collect::<Vec<&str>>();
                            if s.len() == 2 { s[1] } else { s[0] }
                        };
                        term.write_line(&format!("fetching pubic key info from {domain}..."))?;
                        let res = nip05::profile(npub_or_nip05, None).await.context(format!(
                            "failed to get nostr public key for {npub_or_nip05} from {domain}"
                        ))?;
                        term.clear_last_lines(1)?;
                        nip05 = Some(npub_or_nip05.to_string());
                        let _ = save_nip05_to_git_config_cache(
                            npub_or_nip05,
                            &res.public_key,
                            git_repo,
                        );
                        if relays.is_empty() {
                            for r in res.relays {
                                relays.push(r);
                            }
                        }
                        res.public_key
                    }
                }
            };
            Coordinate {
                identifier,
                public_key,
                kind: nostr_sdk::Kind::GitRepoAnnouncement,
                relays,
            }
        };

        Ok(Self {
            original_string: url.to_string(),
            coordinate,
            protocol,
            user,
            nip05,
        })
    }
}

fn resolve_nip05_from_git_config_cache(nip05: &str, git_repo: &Option<&Repo>) -> Result<PublicKey> {
    if let Some(public_key) = load_nip_cache(git_repo)?.get(nip05) {
        Ok(*public_key)
    } else {
        bail!("nip05 not stored in local git config cache")
    }
}

pub fn use_nip05_git_config_cache_to_find_nip05_from_public_key(
    public_key: &PublicKey,
    git_repo: &Option<&Repo>,
) -> Result<Option<String>> {
    let h = load_nip_cache(git_repo)?;
    Ok(h.iter()
        .find_map(|(k, v)| if *v == *public_key { Some(k) } else { None })
        .cloned())
}

fn save_nip05_to_git_config_cache(
    nip05: &str,
    public_key: &PublicKey,
    git_repo: &Option<&Repo>,
) -> Result<()> {
    let mut h = load_nip_cache(git_repo)?;
    h.insert(nip05.to_string(), *public_key);

    let s = h
        .into_iter()
        .map(|(nip05, public_key)| format!("{nip05}:{}", public_key.to_hex()))
        .collect::<Vec<String>>()
        .join(",");

    save_git_config_item(git_repo, "nostr.nip05", s.as_str())
        .context("could not save nip05 cache in git config")
}

fn load_nip_cache(git_repo: &Option<&Repo>) -> Result<HashMap<String, PublicKey>> {
    let mut h = HashMap::new();
    let stored_value = get_git_config_item(git_repo, "nostr.nip05")?
        .context("no nip05s in local git config cache so retun empty cache")
        .unwrap_or_default();
    for pair in stored_value.split(',') {
        if let Some((cached_nip05, pubkey)) = pair.split_once(':') {
            if let Ok(public_key) = PublicKey::parse(pubkey) {
                h.insert(cached_nip05.to_string(), public_key);
            }
        }
    }
    Ok(h)
}

#[derive(Debug, PartialEq, Default)]
pub struct CloneUrl {
    original_string: String,
    host: String,
    path: String,
    parameters: Option<String>,
    protocol: ServerProtocol,
    user: Option<String>,
    port: Option<u16>,
    fragment: Option<String>,
}

impl FromStr for CloneUrl {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        // Check if the input is a local path
        if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") {
            return Ok(Self {
                original_string: s.to_string(),
                protocol: ServerProtocol::Filesystem,
                ..CloneUrl::default()
            });
        }
        let url_str = if s.contains("://") {
            s.to_string() // Use the original string
        } else {
            let protocol = // Check for the SSH format user@host:path and convert to ssh://
                if s.contains('@') && s
                .split('@')
                .nth(0)
                .map_or(false, |part| !part.contains('/')) {
                    "ssh"
                }
                // otherwise assume unspecified
                else {
                    "unspecified"
                };
            format!(
                "{protocol}://{}",
                if contains_port(s) {
                    s.to_string()
                } else {
                    s.replace(":/", "/").replace(':', "/")
                }
            )
        };

        let url = Url::parse(&url_str).context("Failed to parse URL")?;

        let protocol = match url.scheme() {
            "ssh" => ServerProtocol::Ssh,
            "https" => ServerProtocol::Https,
            "http" => ServerProtocol::Http,
            "git" => ServerProtocol::Git,
            "ftp" => ServerProtocol::Ftp,
            "unspecified" => ServerProtocol::Unspecified,
            _ => return Err(anyhow::anyhow!("Unsupported protocol: {}", url.scheme())),
        };

        let host = url.host_str().context("Missing host")?.to_string();
        let path = url.path().to_string();
        let parameters = url.query().map(|s| s.to_string());
        let port = url.port();

        let fragment = url.fragment().map(|s| s.to_string());

        let user = if url.username().is_empty() {
            None
        } else {
            Some(url.username().to_string())
        };

        Ok(CloneUrl {
            original_string: s.to_string(),
            host,
            path,
            parameters,
            protocol,
            user,
            port,
            fragment,
        })
    }
}

fn contains_port(s: &str) -> bool {
    if let Some(after_host) = s.split('@').nth(1).unwrap_or(s).split(':').nth(1) {
        if let Some(port) = after_host.split('/').next() {
            if port.parse::<u16>().is_ok() {
                return true;
            }
        }
    }
    false
}

impl CloneUrl {
    pub fn format_as(&self, protocol: &ServerProtocol, user: &Option<String>) -> Result<String> {
        // Check for incompatible protocol conversions
        if *protocol == ServerProtocol::Filesystem {
            if self.protocol == ServerProtocol::Filesystem {
                // If converting from Filesystem to Filesystem, return the original string
                return Ok(self.original_string.clone());
            } else {
                // If converting to Filesystem from any other protocol, return an error
                bail!(
                    "Cannot convert to Filesystem protocol from {:?}",
                    self.protocol
                );
            }
        }

        let mut url = Url::parse(&format!(
            "{}{}",
            match protocol {
                ServerProtocol::Https => "https://",
                ServerProtocol::UnauthHttps => "https://",
                ServerProtocol::Http => "http://",
                ServerProtocol::UnauthHttp => "http://",
                ServerProtocol::Git => "git://",
                ServerProtocol::Ftp => "ftp://",
                ServerProtocol::Ssh => "ssh://",
                ServerProtocol::Unspecified => "https://",
                _ => bail!("unsupported protocol"),
            },
            &self.host
        ))
        .context("Failed to parse base URL")?; // Start with the specified scheme

        url.set_path(&self.path);

        // Set the port if present
        if let Some(port) = self.port {
            url.set_port(Some(port))
                .map_err(|_| anyhow!("failed to add port"))?;
        }

        // Set the query parameters if present
        if let Some(ref parameters) = self.parameters {
            url.set_query(Some(parameters));
        }

        // Set the fragment if present
        if let Some(ref fragment) = self.fragment {
            url.set_fragment(Some(fragment));
        }

        let mut formatted_url = url.to_string();

        if *protocol == ServerProtocol::Ssh {
            formatted_url = formatted_url.replace(
                "ssh://",
                format!("{}@", user.as_deref().unwrap_or("git")).as_str(),
            );
            if !contains_port(&formatted_url) {
                formatted_url = replace_first_occurrence(&formatted_url, '/', ':');
            }
        } else if *protocol == ServerProtocol::Unspecified {
            formatted_url = formatted_url.replace("https://", "");
        }

        Ok(strip_trailing_slash(&formatted_url))
    }
    pub fn domain(&self) -> String {
        self.host.to_string()
    }
    pub fn protocol(&self) -> ServerProtocol {
        self.protocol.clone()
    }

    pub fn short_name(&self) -> String {
        let domain = self.domain();
        if domain.is_empty() {
            self.original_string.to_string()
        } else {
            format!("{domain}{}", self.path)
        }
    }
}

fn replace_first_occurrence(s: &str, target: char, replacement: char) -> String {
    let mut result = s.to_string();
    if let Some(index) = result.find(target) {
        result.replace_range(index..index + 1, &replacement.to_string());
    }
    result
}

fn strip_trailing_slash(s: &str) -> String {
    s.strip_suffix('/').unwrap_or(s).to_string()
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

    mod clone_url_from_str_format_as {
        use super::*;

        mod when_user_specified {
            use super::*;

            mod but_not_in_original_url {
                use super::*;

                #[test]
                fn https_to_https_ignores_user() {
                    let result = "https://github.com/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &Some("user1".to_string()))
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }
                #[test]
                fn https_to_ssh_uses_specified_user() {
                    let result = "https://github.com/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Ssh, &Some("user1".to_string()))
                        .unwrap();
                    assert_eq!(result, "user1@github.com:user/repo.git");
                }
            }
            mod and_a_different_user_in_original_url {
                use super::*;

                #[test]
                fn ssh_uses_specified_user() {
                    let result = "user2@github.com/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Ssh, &Some("user1".to_string()))
                        .unwrap();
                    assert_eq!(result, "user1@github.com:user/repo.git");
                }
            }
        }

        #[test]
        fn format_as_ssh_defaults_to_git_user() {
            let result = "https://github.com/user/repo.git"
                .parse::<CloneUrl>()
                .unwrap()
                .format_as(&ServerProtocol::Ssh, &None)
                .unwrap();
            assert_eq!(result, "git@github.com:user/repo.git");
        }

        #[test]
        fn format_as_ssh_includes_port() {
            let result = "https://github.com:1000/user/repo.git"
                .parse::<CloneUrl>()
                .unwrap()
                .format_as(&ServerProtocol::Ssh, &None)
                .unwrap();
            assert_eq!(result, "git@github.com:1000/user/repo.git");
        }

        #[test]
        fn format_as_unspecified_ommits_prefix() {
            let result = "https://github.com/user/repo.git"
                .parse::<CloneUrl>()
                .unwrap()
                .format_as(&ServerProtocol::Unspecified, &None)
                .unwrap();
            assert_eq!(result, "github.com/user/repo.git");
        }

        mod input_all_formats_to_from_str_and_correctly_format_as_https {
            use super::*;

            #[test]
            fn test_https_url() {
                let result = "https://github.com/user/repo.git"
                    .parse::<CloneUrl>()
                    .unwrap()
                    .format_as(&ServerProtocol::Https, &None)
                    .unwrap();
                assert_eq!(result, "https://github.com/user/repo.git");
            }

            mod with_unspecified_and_additional_features {
                use super::*;

                #[test]
                fn port() {
                    let result = "github.com:1000/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com:1000/user/repo.git");
                }

                #[test]
                fn colon() {
                    let result = "github.com:user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }

                #[test]
                fn path_with_fragment() {
                    let result = "github.com/user/repo.git#readme"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git#readme");
                }

                #[test]
                fn path_with_parameters() {
                    let result = "github.com/user/repo.git?ref=main"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git?ref=main");
                }

                #[test]
                fn port_with_parameters_and_fragment() {
                    let result = "github.com:2222/repo.git?version=1.0#section1"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(
                        result,
                        "https://github.com:2222/repo.git?version=1.0#section1"
                    );
                }
            }

            mod with_https_and_additional_features {
                use super::*;

                #[test]
                fn credentials_and_they_are_stripped() {
                    let result = "https://username:password@github.com/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }

                #[test]
                fn port() {
                    let result = "https://github.com:1000/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com:1000/user/repo.git");
                }

                #[test]
                fn path_with_fragment() {
                    let result = "https://github.com/user/repo.git#readme"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git#readme");
                }

                #[test]
                fn path_with_parameters() {
                    let result = "https://github.com/user/repo.git?ref=main"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git?ref=main");
                }

                #[test]
                fn port_with_parameters_and_fragment() {
                    let result = "https://github.com:2222/repo.git?version=1.0#section1"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(
                        result,
                        "https://github.com:2222/repo.git?version=1.0#section1"
                    );
                }
            }

            #[test]
            fn test_http_url() {
                let result = "http://github.com/user/repo.git"
                    .parse::<CloneUrl>()
                    .unwrap()
                    .format_as(&ServerProtocol::Https, &None)
                    .unwrap();
                assert_eq!(result, "https://github.com/user/repo.git");
            }

            mod ssh_input {
                use super::*;

                #[test]
                fn test_git_at_url() {
                    let result = "git@github.com:user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }

                #[test]
                fn test_user_at_url() {
                    let result = "user1@github.com:user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }
                #[test]
                fn path_has_colon_slash_prefix() {
                    let result = "user1@github.com:/user/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git");
                }

                #[test]
                fn port_specified_with_path() {
                    let result = "user@github.com:2222/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com:2222/repo.git");
                }

                #[test]
                fn port_specified_without_path() {
                    let result = "user@github.com:2222"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com:2222");
                }

                #[test]
                fn path_with_fragment() {
                    let result = "user1@github.com:/user/repo.git#readme"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git#readme");
                }

                #[test]
                fn path_with_parameters() {
                    let result = "user@github.com:/user/repo.git?ref=main"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(result, "https://github.com/user/repo.git?ref=main");
                }

                #[test]
                fn port_with_parameters_and_fragment() {
                    let result = "user@github.com:2222/repo.git?version=1.0#section1"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None)
                        .unwrap();
                    assert_eq!(
                        result,
                        "https://github.com:2222/repo.git?version=1.0#section1"
                    );
                }
            }

            #[test]
            fn test_ftp_url() {
                let result = "ftp://example.com/repo.git"
                    .parse::<CloneUrl>()
                    .unwrap()
                    .format_as(&ServerProtocol::Https, &None)
                    .unwrap();
                assert_eq!(result, "https://example.com/repo.git");
            }

            #[test]
            fn test_git_protocol_url() {
                let result = "git://example.com/repo.git"
                    .parse::<CloneUrl>()
                    .unwrap()
                    .format_as(&ServerProtocol::Https, &None)
                    .unwrap();
                assert_eq!(result, "https://example.com/repo.git");
            }

            #[test]
            fn test_invalid_url() {
                let clone_url_result = "unsupported://example.com/repo.git".parse::<CloneUrl>();
                assert!(clone_url_result.is_err());
            }
            mod local_addresses_should_return_error {
                use super::*;
                #[test]
                fn test_absolute_local_path() {
                    let result = "/path/to/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None);
                    assert!(result.is_err()); // Expecting an error when converting to HTTPS
                }

                #[test]
                fn test_relative_local_path() {
                    let result = "./path/to/repo.git"
                        .parse::<CloneUrl>()
                        .unwrap()
                        .format_as(&ServerProtocol::Https, &None);
                    assert!(result.is_err()); // Expecting an error when converting to HTTPS
                }
            }
        }
    }
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
    mod nostr_git_url_format {
        use nostr::nips::nip01::Coordinate;
        use nostr_sdk::PublicKey;

        use super::*;
        use crate::git::nostr_url::NostrUrlDecoded;

        #[test]
        fn standard() -> Result<()> {
            assert_eq!(
                format!(
                    "{}",
                    NostrUrlDecoded {
                        original_string: String::new(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![RelayUrl::parse("wss://nos.lol").unwrap()],
                        },
                        protocol: None,
                        user: None,
                        nip05: None,
                    }
                ),
                "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit",
            );
            Ok(())
        }

        #[test]
        fn no_relay() -> Result<()> {
            assert_eq!(
                format!(
                    "{}",
                    NostrUrlDecoded {
                        original_string: String::new(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![],
                        },
                        protocol: None,
                        user: None,
                        nip05: None,
                    }
                ),
                "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit",
            );
            Ok(())
        }

        #[test]
        fn with_protocol() -> Result<()> {
            assert_eq!(
                format!(
                    "{}",
                    NostrUrlDecoded {
                        original_string: String::new(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![RelayUrl::parse("wss://nos.lol").unwrap()],
                        },
                        protocol: Some(ServerProtocol::Ssh),
                        user: None,
                        nip05: None,
                    }
                ),
                "nostr://ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit",
            );
            Ok(())
        }

        #[test]
        fn with_protocol_and_user() -> Result<()> {
            assert_eq!(
                format!(
                    "{}",
                    NostrUrlDecoded {
                        original_string: String::new(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![RelayUrl::parse("wss://nos.lol").unwrap()],
                        },
                        protocol: Some(ServerProtocol::Ssh),
                        user: Some("bla".to_string()),
                        nip05: None,
                    }
                ),
                "nostr://bla@ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit",
            );
            Ok(())
        }
    }

    mod nostr_url_decoded_paramemters_from_str {
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
                    vec![RelayUrl::parse("wss://nos.lol").unwrap()]
                } else {
                    vec![]
                },
            }
        }

        #[tokio::test]
        async fn from_naddr() -> Result<()> {
            let url = "nostr://naddr1qqzxuemfwsqs6amnwvaz7tmwdaejumr0dspzpgqgmmc409hm4xsdd74sf68a2uyf9pwel4g9mfdg8l5244t6x4jdqvzqqqrhnym0k2qj".to_string();
            assert_eq!(
                NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                NostrUrlDecoded {
                    original_string: url.clone(),
                    coordinate: Coordinate {
                        identifier: "ngit".to_string(),
                        public_key: PublicKey::parse(
                            "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                        )
                        .unwrap(),
                        kind: nostr_sdk::Kind::GitRepoAnnouncement,
                        relays: vec![RelayUrl::parse("wss://nos.lol").unwrap()], /* wont add the
                                                                                  * slash */
                    },
                    protocol: None,
                    user: None,
                    nip05: None,
                },
            );
            Ok(())
        }

        mod from_npub_slash_identifier {
            use super::*;

            #[tokio::test]
            async fn without_relay() -> Result<()> {
                let url =
                    "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit"
                        .to_string();
                assert_eq!(
                    NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                    NostrUrlDecoded {
                        original_string: url.clone(),
                        coordinate: get_model_coordinate(false),
                        protocol: None,
                        user: None,
                        nip05: None,
                    },
                );
                Ok(())
            }

            mod with_url_parameters {
                use super::*;

                #[tokio::test]
                async fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    let url = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay=nos.lol".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(true),
                            protocol: None,
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_encoded_relay() -> Result<()> {
                    let url = format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}",
                        urlencoding::encode("wss://nos.lol")
                    );
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(true),
                            protocol: None,
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_multiple_encoded_relays() -> Result<()> {
                    let url = format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?relay={}&relay1={}",
                        urlencoding::encode("wss://nos.lol/"),
                        urlencoding::encode("wss://relay.damus.io/"),
                    );
                    assert_eq!(
                    NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                    NostrUrlDecoded {
                        original_string: url.clone(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![
                                RelayUrl::parse("wss://nos.lol/").unwrap(),
                                RelayUrl::parse("wss://relay.damus.io/").unwrap(),
                            ],
                        },
                        protocol: None,
                        user: None,
                        nip05: None,
                    },
                );
                    Ok(())
                }

                #[tokio::test]
                async fn with_server_protocol() -> Result<()> {
                    let url = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(false),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_server_protocol_and_user() -> Result<()> {
                    let url = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit?protocol=ssh&user=fred".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(false),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                            nip05: None,
                        },
                    );
                    Ok(())
                }
            }

            mod with_parameters_embedded_with_slashes {
                use super::*;

                #[tokio::test]
                async fn with_relay_without_scheme_defaults_to_wss() -> Result<()> {
                    let url = "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/nos.lol/ngit".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(true),
                            protocol: None,
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_encoded_relay() -> Result<()> {
                    let url = format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/ngit",
                        urlencoding::encode("wss://nos.lol")
                    );
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(true),
                            protocol: None,
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_multiple_encoded_relays() -> Result<()> {
                    let url = format!(
                        "nostr://npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/{}/{}/ngit",
                        urlencoding::encode("wss://nos.lol/"),
                        urlencoding::encode("wss://relay.damus.io/"),
                    );
                    assert_eq!(
                    NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                    NostrUrlDecoded {
                        original_string: url.clone(),
                        coordinate: Coordinate {
                            identifier: "ngit".to_string(),
                            public_key: PublicKey::parse(
                                "npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr",
                            )
                            .unwrap(),
                            kind: nostr_sdk::Kind::GitRepoAnnouncement,
                            relays: vec![
                                RelayUrl::parse("wss://nos.lol/").unwrap(),
                                RelayUrl::parse("wss://relay.damus.io/").unwrap(),
                            ],
                        },
                        protocol: None,
                        user: None,
                        nip05: None,
                    },
                );
                    Ok(())
                }

                #[tokio::test]
                async fn with_server_protocol() -> Result<()> {
                    let url = "nostr://ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(false),
                            protocol: Some(ServerProtocol::Ssh),
                            user: None,
                            nip05: None,
                        },
                    );
                    Ok(())
                }

                #[tokio::test]
                async fn with_server_protocol_and_user() -> Result<()> {
                    let url = "nostr://fred@ssh/npub15qydau2hjma6ngxkl2cyar74wzyjshvl65za5k5rl69264ar2exs5cyejr/ngit".to_string();
                    assert_eq!(
                        NostrUrlDecoded::parse_and_resolve(&url, &None).await?,
                        NostrUrlDecoded {
                            original_string: url.clone(),
                            coordinate: get_model_coordinate(false),
                            protocol: Some(ServerProtocol::Ssh),
                            user: Some("fred".to_string()),
                            nip05: None,
                        },
                    );
                    Ok(())
                }
            }
        }
    }
}
