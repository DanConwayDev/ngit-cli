use anyhow::Result;

// https://git-scm.com/docs/gitremote-helpers#_capabilities
pub async fn launch() -> Result<()> {
    // blank line indicates end of capabilities
    println("");
    Ok(())
}
