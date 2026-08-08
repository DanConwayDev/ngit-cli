mod read;
mod support;
mod write;
mod write_app;

use anyhow::Result;
use serde_json::json;

use crate::{
    cli::{Cli, ReleaseAppCommands, ReleaseAssetCommands, ReleaseCommands, ReleaseSubCommandArgs},
    cli_interactor::CliError,
};

pub async fn launch(cli: &Cli, args: &ReleaseSubCommandArgs) -> Result<()> {
    let json_output = wants_json(&args.release_command);
    let command = command_name(&args.release_command);
    let result = match &args.release_command {
        ReleaseCommands::List(args) => read::release_list(cli, args).await,
        ReleaseCommands::View(args) => read::release_view(cli, args).await,
        ReleaseCommands::Publish(args) => write::release_publish(cli, args).await,
        ReleaseCommands::App(args) => match &args.app_command {
            ReleaseAppCommands::List(args) => read::app_list(cli, args).await,
            ReleaseAppCommands::View(args) => read::app_view(cli, args).await,
            ReleaseAppCommands::Init(args) => write::app_init(cli, args).await,
            ReleaseAppCommands::Link(args) => write::app_link(cli, args).await,
        },
        ReleaseCommands::Asset(args) => match &args.asset_command {
            ReleaseAssetCommands::List(args) => read::asset_list(cli, args).await,
            ReleaseAssetCommands::View(args) => read::asset_view(cli, args).await,
        },
    };

    match result {
        Ok(output) if json_output => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "format_version": 1,
                    "ok": true,
                    "command": output.command,
                    "repository": output.repository,
                    "authority": output.authority,
                    "warnings": output.warnings,
                    "result": output.result,
                }))?
            );
            Ok(())
        }
        Ok(output) => {
            for warning in output.warnings {
                eprintln!("warning: {}", warning.message);
            }
            println!("{}", output.human);
            Ok(())
        }
        Err(error) if json_output => {
            let (code, message, details) =
                error.downcast_ref::<support::ReleaseError>().map_or_else(
                    || {
                        (
                            "operation_failed",
                            format!("{error:#}"),
                            serde_json::Value::Object(serde_json::Map::new()),
                        )
                    },
                    |error| (error.code, error.message.clone(), error.details.clone()),
                );
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "format_version": 1,
                    "ok": false,
                    "command": command,
                    "repository": null,
                    "authority": null,
                    "warnings": [],
                    "result": null,
                    "error": {
                        "code": code,
                        "message": message,
                        "details": details,
                    }
                }))?
            );
            Err(CliError.into())
        }
        Err(error) => Err(error),
    }
}

fn wants_json(command: &ReleaseCommands) -> bool {
    match command {
        ReleaseCommands::List(args) => args.json,
        ReleaseCommands::View(args) => args.json,
        ReleaseCommands::Publish(args) => args.json,
        ReleaseCommands::App(args) => match &args.app_command {
            ReleaseAppCommands::List(args) => args.json,
            ReleaseAppCommands::View(args) => args.json,
            ReleaseAppCommands::Init(args) => args.json,
            ReleaseAppCommands::Link(args) => args.json,
        },
        ReleaseCommands::Asset(args) => match &args.asset_command {
            ReleaseAssetCommands::List(args) => args.json,
            ReleaseAssetCommands::View(args) => args.json,
        },
    }
}

fn command_name(command: &ReleaseCommands) -> &'static str {
    match command {
        ReleaseCommands::List(_) => "release.list",
        ReleaseCommands::View(_) => "release.view",
        ReleaseCommands::Publish(_) => "release.publish",
        ReleaseCommands::App(args) => match &args.app_command {
            ReleaseAppCommands::List(_) => "release.app.list",
            ReleaseAppCommands::View(_) => "release.app.view",
            ReleaseAppCommands::Init(_) => "release.app.init",
            ReleaseAppCommands::Link(_) => "release.app.link",
        },
        ReleaseCommands::Asset(args) => match &args.asset_command {
            ReleaseAssetCommands::List(_) => "release.asset.list",
            ReleaseAssetCommands::View(_) => "release.asset.view",
        },
    }
}
