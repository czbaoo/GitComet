use crate::model::RepoId;

/// Fetch operations share authentication, cancellation, and transfer reporting.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FetchMsg {
    All {
        repo_id: RepoId,
    },
    Refspecs {
        repo_id: RepoId,
        remote: String,
        refspecs: Vec<String>,
    },
    /// Fetch a single branch from a remote, mirroring GitComet's CLI fetch
    /// (`git fetch --prune --tags <remote> <branch>:<branch>`).
    Branch {
        repo_id: RepoId,
        remote: String,
        branch: String,
    },
}
