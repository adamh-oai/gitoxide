//! Recursive removal of a linked worktree and its administrative directory.

use std::{
    convert::Infallible,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
};

use gix_features::{
    parallel::{self, Reduce},
    progress::{NestedProgress, Progress},
};
/// A failure while scanning or deleting one of the removal roots.
#[derive(Debug, thiserror::Error)]
#[error("Could not remove '{path}': {source}")]
pub struct DirectoryError {
    /// The path whose scan or deletion failed.
    pub path: PathBuf,
    /// The underlying filesystem error.
    #[source]
    pub source: io::Error,
}

/// The error returned by [`remove()`].
///
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The checkout could not be fully removed, but its private Git directory was removed.
    #[error("Could not fully remove the linked-worktree checkout")]
    Worktree(#[source] DirectoryError),
    /// The private Git directory could not be fully removed, but its checkout was removed.
    #[error("Could not fully remove the linked-worktree administration")]
    GitDir(#[source] DirectoryError),
    /// Neither the checkout nor its private Git directory could be fully removed.
    #[error("Could not fully remove the checkout ({worktree}) or its administration ({git_dir})")]
    Both {
        /// The first checkout-removal error.
        worktree: DirectoryError,
        /// The first administrative-removal error.
        git_dir: DirectoryError,
    },
}

/// Recursively remove `work_dir` and its private `git_dir` without following symbolic links.
///
/// Traversal and leaf deletion use all available threads when the `parallel` feature is enabled.
/// The private Git directory is removed even if removing the checkout fails. A missing root is
/// considered removed successfully, and an empty parent `worktrees` directory is removed as well.
pub fn remove(
    work_dir: impl AsRef<Path>,
    git_dir: impl AsRef<Path>,
    mut progress: impl NestedProgress,
) -> Result<(), Error> {
    let work_dir = work_dir.as_ref();
    let git_dir = git_dir.as_ref();
    let worktree = remove_root(
        work_dir,
        progress.add_child("scan worktree"),
        progress.add_child("remove worktree"),
    )
    .err();
    let git_dir_error = remove_root(
        git_dir,
        progress.add_child("scan administration"),
        progress.add_child("remove administration"),
    )
    .err();

    if git_dir_error.is_none()
        && let Some(worktrees_dir) = git_dir.parent()
        && worktrees_dir.file_name().is_some_and(|name| name == "worktrees")
    {
        // This is intentionally best-effort, like Git: a non-empty directory merely means other
        // linked worktrees remain.
        let _ = fs::remove_dir(worktrees_dir);
    }

    match (worktree, git_dir_error) {
        (None, None) => Ok(()),
        (Some(err), None) => Err(Error::Worktree(err)),
        (None, Some(err)) => Err(Error::GitDir(err)),
        (Some(worktree), Some(git_dir)) => Err(Error::Both { worktree, git_dir }),
    }
}

fn remove_root(root: &Path, mut scan: impl Progress, mut remove: impl Progress) -> Result<(), DirectoryError> {
    let root = absolute_root(root)?;
    let root = root.as_path();
    scan.init(None, gix_features::progress::count("entries"));
    let mut leaves = Vec::new();
    let mut directories = Vec::new();
    #[cfg(unix)]
    let mut retained_mounts = Vec::new();
    let mut first_error = None;
    #[cfg(unix)]
    let containing_device = root
        .parent()
        .and_then(|parent| fs::metadata(parent).ok())
        .map(|metadata| std::os::unix::fs::MetadataExt::dev(&metadata));
    #[cfg(unix)]
    let root_is_filesystem_root = root.parent().is_none();
    #[cfg(unix)]
    let descend = move |entry: &dua_core::Entry| may_descend(containing_device, root_is_filesystem_root, entry);
    #[cfg(not(unix))]
    let descend = |_: &dua_core::Entry| true;
    for entry in dua_core::walk(root, parallel::num_threads(None), dua_core::Order::Completion, descend) {
        scan.inc();
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if entry.file_type.is_dir() {
                    #[cfg(unix)]
                    if !may_descend(containing_device, root_is_filesystem_root, &entry) {
                        retained_mounts.push(path.clone());
                    }
                    directories.push((entry.depth, path));
                } else {
                    leaves.push((path, entry.file_type.is_symlink()));
                }
            }
            Err(source) if gix_fs::io_err::is_not_found(source.kind(), source.raw_os_error()) => {}
            Err(source) => {
                first_error.get_or_insert_with(|| DirectoryError {
                    path: root.to_owned(),
                    source,
                });
            }
        }
    }

    remove.init(
        Some(leaves.len() + directories.len()),
        gix_features::progress::count("entries"),
    );
    let counter = remove.counter();
    let leaf_error = parallel::in_parallel(
        leaves.into_iter(),
        None,
        |_| (),
        {
            let counter = counter.clone();
            move |(path, is_symlink), _| {
                let result = remove_leaf(&path, is_symlink);
                counter.fetch_add(1, Ordering::Relaxed);
                result
                    .err()
                    .filter(|err| !gix_fs::io_err::is_not_found(err.kind(), err.raw_os_error()))
                    .map(|source| DirectoryError { path, source })
            }
        },
        FirstError::default(),
    )
    .unwrap_or_else(|never| match never {});
    if first_error.is_none() {
        first_error = leaf_error;
    }

    directories.sort_unstable_by_key(|(depth, _)| std::cmp::Reverse(*depth));
    for (_, path) in directories {
        #[cfg(unix)]
        if let Some(mount) = retained_mount(&path, &retained_mounts) {
            // ponytail: mount points are rare; index their ancestors if this scan ever becomes measurable.
            if first_error.is_none() && mount == path {
                first_error = Some(DirectoryError {
                    path: path.clone(),
                    source: io::Error::other("refusing to remove a mounted filesystem"),
                });
            }
            remove.inc();
            continue;
        }
        let result = fs::remove_dir(&path);
        remove.inc();
        if let Err(source) = result
            && !gix_fs::io_err::is_not_found(source.kind(), source.raw_os_error())
            && first_error.is_none()
        {
            first_error = Some(DirectoryError { path, source });
        }
    }

    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn absolute_root(root: &Path) -> Result<PathBuf, DirectoryError> {
    std::path::absolute(root).map_err(|source| DirectoryError {
        path: root.to_owned(),
        source,
    })
}

fn remove_leaf(path: &Path, is_symlink: bool) -> io::Result<()> {
    let result = if is_symlink {
        gix_fs::symlink::remove(path)
    } else {
        fs::remove_file(path)
    };
    #[cfg(windows)]
    if !is_symlink
        && result
            .as_ref()
            .is_err_and(|err| err.kind() == io::ErrorKind::PermissionDenied)
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
        return fs::remove_file(path);
    }
    result
}

#[cfg(unix)]
fn may_descend(containing_device: Option<u64>, root_is_filesystem_root: bool, entry: &dua_core::Entry) -> bool {
    !root_is_filesystem_root
        && containing_device
            .zip(entry.metadata.as_ref().ok().map(std::os::unix::fs::MetadataExt::dev))
            .is_none_or(|(root, entry)| root == entry)
}

#[cfg(unix)]
fn retained_mount<'a>(path: &Path, mounts: &'a [PathBuf]) -> Option<&'a Path> {
    mounts
        .iter()
        .find(|mount| mount.starts_with(path))
        .map(PathBuf::as_path)
}

#[derive(Default)]
struct FirstError(Option<DirectoryError>);

impl Reduce for FirstError {
    type Input = Option<DirectoryError>;
    type FeedProduce = ();
    type Output = Option<DirectoryError>;
    type Error = Infallible;

    fn feed(&mut self, error: Self::Input) -> Result<Self::FeedProduce, Self::Error> {
        if self.0.is_none() {
            self.0 = error;
        }
        Ok(())
    }

    fn finalize(self) -> Result<Self::Output, Self::Error> {
        Ok(self.0)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn relative_roots_are_normalized_before_finding_the_containing_device() {
        let root = super::absolute_root(std::path::Path::new("one-component"))
            .expect("the current directory can be made absolute");
        assert!(root.is_absolute());
        assert!(
            root.parent()
                .and_then(|parent| std::fs::metadata(parent).ok())
                .is_some(),
            "a relative root still has a containing filesystem"
        );
    }

    #[test]
    fn mount_boundaries_are_not_descended() -> std::io::Result<()> {
        let tmp = gix_testtools::tempfile::tempdir()?;
        let entry = dua_core::Entry::from_path(tmp.path())?;
        let device = entry.metadata.as_ref().expect("temporary directory has metadata").dev();
        assert!(super::may_descend(Some(device), false, &entry));
        assert!(!super::may_descend(Some(device.wrapping_add(1)), false, &entry));
        assert!(!super::may_descend(Some(device), true, &entry));
        let mount = tmp.path().join("checkout/mount");
        assert_eq!(
            super::retained_mount(&mount, std::slice::from_ref(&mount)),
            Some(mount.as_path()),
            "the mount itself records the removal error"
        );
        assert_eq!(
            super::retained_mount(tmp.path(), std::slice::from_ref(&mount)),
            Some(mount.as_path()),
            "mount ancestors are retained too"
        );
        Ok(())
    }
}
