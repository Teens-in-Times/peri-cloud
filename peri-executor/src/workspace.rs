use std::path::Path;

use peri_process::filesystem::directory_identity;

use crate::{Error, Result};
use peri_acp_types::device_executor::{SessionBinding, WorkspaceIdentity};

pub(crate) async fn identity(path: &str) -> Result<WorkspaceIdentity> {
    let path = path.to_owned();
    let evidence =
        tokio::task::spawn_blocking(move || directory_identity(Path::new(&path))).await??;
    Ok(WorkspaceIdentity {
        device: evidence.device,
        inode: evidence.inode,
    })
}

pub(crate) async fn verify(binding: &SessionBinding) -> Result<()> {
    let actual = identity(&binding.workspace)
        .await
        .map_err(|_| Error::WorkspaceChanged)?;
    if actual != binding.workspace_identity {
        return Err(Error::WorkspaceChanged);
    }
    Ok(())
}
