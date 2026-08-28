mod read;
mod support;
mod write;
mod write_app;

use anyhow::Result;
use serde_json::json;

use crate::{
    cli::{
        ReleaseAppCommands, ReleaseAssetCommands, ReleaseCommands, ReleaseSubCommandArgs,
        SignerParams,
    },
    cli_interactor::CliError,
    output,
};

pub async fn launch(
    args: &ReleaseSubCommandArgs,
    signer: SignerParams<'_>,
    json_output: bool,
) -> Result<()> {
    let command = command_name(&args.release_command);
    let result = match &args.release_command {
        ReleaseCommands::List(args) => read::release_list(args, signer).await,
        ReleaseCommands::View(args) => read::release_view(args, signer).await,
        ReleaseCommands::Publish(args) => write::release_publish(args, signer).await,
        ReleaseCommands::App(args) => match &args.app_command {
            ReleaseAppCommands::List(args) => read::app_list(args, signer).await,
            ReleaseAppCommands::View(args) => read::app_view(args, signer).await,
            ReleaseAppCommands::Init(args) => write::app_init(args, signer).await,
            ReleaseAppCommands::Link(args) => write::app_link(args, signer).await,
        },
        ReleaseCommands::Asset(args) => match &args.asset_command {
            ReleaseAssetCommands::List(args) => read::asset_list(args, signer).await,
            ReleaseAssetCommands::View(args) => read::asset_view(args, signer).await,
            ReleaseAssetCommands::Add(args) => write::asset_add(args, signer).await,
        },
    };

    match result {
        Ok(output) if json_output => {
            output::set_value(json!({
                "format_version": 1,
                "ok": true,
                "command": output.command,
                "repository": output.repository,
                "authority": output.authority,
                "warnings": output.warnings,
                "result": output.result,
            }));
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
            output::set_value(json!({
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
            }));
            Err(CliError.into())
        }
        Err(error) => Err(error),
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
            ReleaseAssetCommands::Add(_) => "release.asset.add",
        },
    }
}
