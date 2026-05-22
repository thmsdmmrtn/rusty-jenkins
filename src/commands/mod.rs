pub mod build;
pub mod config;
pub mod config_sweep;
pub mod inspect;
pub mod list;
pub mod list_tag;
pub mod logs;
pub mod patch_tag;
pub mod sweep;

use crate::cli::JobTarget;
use crate::client::{encode_job_path, JenkinsClient};
use anyhow::Result;
use serde::Deserialize;

/// Resolve the set of job paths from a [`JobTarget`].
/// `--path` expands to all direct (non-folder) job children.
/// `--job-name` entries are used as-is.
/// Both flags can be combined.
pub async fn resolve_jobs(client: &JenkinsClient, target: &JobTarget) -> Result<Vec<String>> {
    let mut jobs: Vec<String> = target.job_names.clone();

    if let Some(folder) = &target.path {
        #[derive(Deserialize)]
        struct FolderInfo {
            #[serde(default)]
            jobs: Vec<JobEntry>,
        }
        #[derive(Deserialize)]
        struct JobEntry {
            name: String,
            #[serde(rename = "_class")]
            class: String,
        }

        let path = format!(
            "job/{}/api/json?tree=jobs[name,_class]",
            encode_job_path(folder)
        );
        let resp = client.get(&path).await?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "could not list folder '{}': HTTP {}",
                folder,
                resp.status()
            );
        }
        let info: FolderInfo = resp
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("parsing folder JSON: {e}"))?;

        for entry in info.jobs {
            let is_folder = entry.class.contains("Folder") || entry.class.contains("folder");
            if !is_folder {
                jobs.push(format!("{folder}/{}", entry.name));
            }
        }
    }

    if jobs.is_empty() {
        anyhow::bail!("no jobs targeted — provide --path or at least one --job-name");
    }
    Ok(jobs)
}
