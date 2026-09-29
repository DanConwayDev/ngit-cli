use anyhow::Result;
use serde_json::Value;
use test_harness::Harness;

#[tokio::test]
async fn command_paths_preserve_repository_lookup_evidence() -> Result<()> {
    let harness = Harness::builder(
        env!("CARGO_BIN_EXE_ngit"),
        env!("CARGO_BIN_EXE_git-remote-nostr"),
    )
    .with_relay("default")
    .build()
    .await?;
    let (repo, state) = harness.arrange_init_state_b_coordinate_only().await?;
    for command in [
        vec!["issue", "list"],
        vec!["pr", "list"],
        vec!["ci", "status"],
        vec!["repo", "edit", "--description", "new"],
        vec!["repo", "accept"],
        vec!["repo", "leave"],
        vec!["repo", "follow-lead"],
        vec!["init"],
        vec!["release", "list"],
        vec!["repo"],
    ] {
        let output = repo
            .ngit([vec!["--quiet", "--json"], command.clone()].concat())
            .output()
            .await?;
        let doc: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            output.status.success(),
            command == ["repo"],
            "{command:?}: {doc}"
        );
        let diagnostic = if command == ["repo"] {
            &doc["lookup"]
        } else if command[0] == "release" {
            &doc["error"]
        } else {
            &doc
        };
        let category = diagnostic
            .get("category")
            .or_else(|| diagnostic.get("code"));
        assert_eq!(
            category,
            Some(&Value::from("repository_not_found")),
            "{command:?}: {doc}"
        );
        assert_eq!(
            diagnostic["details"]["value"], state.coordinate_bech32,
            "{doc}"
        );
        assert_eq!(diagnostic["details"]["network_searched"], true);
        assert_eq!(
            diagnostic["details"]["relays"][0]["outcome"], "not_found",
            "{doc}"
        );
    }
    for command in [
        vec!["issue", "list"],
        vec!["pr", "list"],
        vec!["ci", "status"],
        vec!["release", "list"],
        vec!["repo"],
    ] {
        let output = repo
            .ngit(
                [
                    vec!["--quiet", "--json"],
                    command.clone(),
                    vec!["--offline"],
                ]
                .concat(),
            )
            .output()
            .await?;
        let doc: Value = serde_json::from_slice(&output.stdout)?;
        let diagnostic = if command == ["repo"] {
            &doc["lookup"]
        } else if command[0] == "release" {
            &doc["error"]
        } else {
            &doc
        };
        let category = diagnostic
            .get("category")
            .or_else(|| diagnostic.get("code"));
        assert_eq!(
            category,
            Some(&Value::from("repository_not_cached")),
            "{command:?}: {doc}"
        );
        assert_eq!(diagnostic["details"]["network_searched"], false);
        assert_eq!(diagnostic["details"]["relays"], serde_json::json!([]));
    }
    Ok(())
}
