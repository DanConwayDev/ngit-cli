/* !

# Authentication and Key Management Requirements

## User Experience

For a smooth UX:
1. a private key should only need to be imported once
2. authentication to sign events should persist at least across multiple calls
to the cli tool within a single terminal session.

## Security

1. key material must be encrypted with a salted passphrase when stored on disk.
2. the passphase should only be accessable
 a) by this specific cli tool; or alternatively
 b) only within the terminal session


# Implementation

Every private key entired into the tool is encrypted with a salted user
provided passphrase and stored on disk in the tool's configuration file
alongside display_name and public key for identification.

The private key of the current logged-in user is encrypted with a salted
randomly generated token and stored on disk in the configuration file alongside
the public key for identification. The token is stored in the OS's keyring
using a rust crate called 'keyring'. On Linux this expires after a few days
whilst on Windows and MacOS it never expires.

Should the token be cycled? cycling the token would prevent an attacker who had
access to only the token or the encrypted key from returning after the token
had been cycled. This isn't worth it. An attacker is much more likely to have
access to both simultainiously.

logout should delete the key encrypted with the token and the token. It should
give the option to clear encrypted key material for the current user or all
users.

*/

init

initialize repoisiotr 


replaceable event

commit id

search by initial commit / initial 5 commits
name



initialising a reposistory 


git nostr init
    > intialise repo 


git nostr init - request patches / PRs, issues, 
    features to support
    -- branch
    -- patches / PRs
    -- issues

    -- override git push to also push to nostr.

    settings
    --git-repos - one or more git repositories where the latest commits can be pulled from
    --name
    --description


git push nostr main
