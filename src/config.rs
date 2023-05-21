use nostr::{secp256k1::SecretKey};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct MyConfig {
    version: u8,
    pub default_admin_group_event_serialized: Option<String>,
    pub default_relays:Vec<String>,
    pub private_key:Option<SecretKey>,
}

/// `MyConfig` implements `Default`
impl ::std::default::Default for MyConfig {
    fn default() -> Self { Self {
        version: 0,
        default_admin_group_event_serialized: None,
        default_relays:vec![],
        private_key: None,
    } }
}

pub fn load_config() -> MyConfig {
    confy::load("ngit-cli", None)
        .expect("load_config always to load confy custom config or defaults for ngit-cli")
}

pub fn save_conifg(cfg:&MyConfig) -> &MyConfig {
    confy::store("ngit-cli",None, &cfg)
        .expect("save_conifg always to save confy custom config or defaults for ngit-cli and return it");
    cfg
}

