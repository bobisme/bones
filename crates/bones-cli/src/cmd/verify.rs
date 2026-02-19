use std::path::Path;

use anyhow::Result;
use bones_core::verify::{ShardCheckStatus, verify_repository};

/// Run repository verification against `.bones/events` shards.
///
/// # Errors
///
/// Returns an error when verification checks fail.
pub fn run_verify(project_root: &Path, regenerate_missing: bool) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let report = verify_repository(&bones_dir, regenerate_missing)?;

    for shard in &report.shards {
        match &shard.status {
            ShardCheckStatus::Verified => {
                println!("OK   {}", shard.shard_name);
            }
            ShardCheckStatus::Regenerated => {
                println!("FIX  {} (regenerated missing manifest)", shard.shard_name);
            }
            ShardCheckStatus::Failed(reason) => {
                println!("FAIL {} ({reason})", shard.shard_name);
            }
        }
    }

    if report.active_shard_parse_ok {
        println!("OK   active shard parse sanity");
    } else {
        println!("FAIL active shard parse sanity");
    }

    if report.is_ok() {
        println!("verify: success");
        Ok(())
    } else {
        anyhow::bail!("verify: failed");
    }
}
