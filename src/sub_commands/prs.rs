

use clap::Args;
use dialoguer::{Select, Confirm, theme::ColorfulTheme};
use nostr::{Event};

use crate::{fetch_pull_push::fetch_pull_push, ngit_tag::{tag_extract_value, tag_is_branch_merged_from, tag_is_branch}};

#[derive(Args)]
pub struct PrsSubCommand {
}

pub fn prs(
    _sub_command_args: &PrsSubCommand,
) {

    let branch_refs = fetch_pull_push(
        None,
        false,
        false,
        None,
        false,
        None,
        None,
    );
    fn pull_request_merged(pull_request: &Event, merges: &Vec<Event>) -> bool {
        let branch_from = tag_extract_value(
            pull_request.tags.iter().find(|tag|tag_is_branch_merged_from(tag))
                .expect("pull_request will always have branch merge from tag")
        );
        let branch_to = tag_extract_value(
            pull_request.tags.iter().find(|tag|tag_is_branch(tag))
                .expect("pull_request will always have branch tag")
        );

        merges.iter().any(|m|
            tag_extract_value(
                m.tags.iter().find(|tag|tag_is_branch_merged_from(tag))
                    .expect("merge will always have branch merge from tag")
            ) == branch_from.clone()
            // && tag_extract_value(
            //     m.tags.iter().find(|tag|tag_is_branch(tag))
            //         .expect("merge will always have branch merge from tag")
            // ) == branch_to.clone()
        )
    }
    // list PRs against branches that have not been merged
    let outstanding_prs: Vec<&Event> = branch_refs.pull_requests.iter().filter(|pr|
        !pull_request_merged(pr, &branch_refs.merges)
    ).collect();

    if outstanding_prs.is_empty() {
        return println!("There are no open pull requests for this repository on selected relays");
    }

    fn extract_summary(pr:&Event) -> String {
        let split_string:Vec<String> = pr.content.split("\n").map(|s| s.to_string()).collect();
        split_string.get(1)
            .expect("PR content will always have a second line which is the title")
            .clone()
    }
    let summaries:Vec<String> = outstanding_prs.iter().map(|pr|extract_summary(pr)).collect();
    
    loop {

        // select pr to review
        let i = Select::new()
            .with_prompt("Select PR to review")
            .items(&summaries)
            .report(false)
            .interact()
            .unwrap();

        // display summary
        println!(
            "{}\n raised: {}",
            outstanding_prs[i].content,
            outstanding_prs[i].created_at.to_human_datetime(),
        );

        let branch_id = tag_extract_value(
            outstanding_prs[i].tags.iter()
                .find(|tag|tag_is_branch_merged_from(tag))
                .expect("pr will always have a branch merged from tag")
        );

        let _branch_title = branch_refs
            .branch_as_repo(Some(&branch_id)).name;
        // pull branch
        if Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(
                format!(
                    "pull branch for '{}'?",
                    extract_summary(outstanding_prs[i])
                )
            )
            .default(true)
            .interact()
            .unwrap()
        {
            fetch_pull_push(
                None,
                true,
                false,
                Some(branch_id),
                false,
                None,
                None,
            );
            break
        }
    }
}
