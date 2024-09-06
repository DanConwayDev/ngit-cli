use std::path::Path;

use directories::UserDirs;

pub fn check_ssh_keys() -> bool {
    // Get the user's home directory using the directories crate
    if let Some(user_dirs) = UserDirs::new() {
        let ssh_dir = user_dirs.home_dir().join(".ssh");
        let key_files = vec![
            "id_rsa",
            "id_ecdsa",
            "id_ed25519",
            "id_rsa.pub",
            "id_ecdsa.pub",
            "id_ed25519.pub",
        ];

        for key in key_files {
            if Path::new(&ssh_dir.join(key)).exists() {
                return true; // At least one key exists
            }
        }
    }
    false // No keys found
}
