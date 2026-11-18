//! Linux-only, handle-relative cleanup of failed dependency installs.

use std::collections::BTreeSet;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone)]
struct Directory {
    name: OsString,
    device: u64,
    inode: u64,
}

impl Directory {
    fn new(name: OsString, metadata: &Metadata) -> Self {
        Self {
            name,
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    fn matches(&self, file: &File) -> bool {
        file.metadata()
            .is_ok_and(|meta| meta.dev() == self.device && meta.ino() == self.inode)
    }
}

fn open_directory(parent: &File, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: parent owns a live fd and name is NUL-terminated. A successful
    // openat returns a new fd, whose sole owner is the File below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

/// Reopen one component at a time, checking every directory's identity. Keeping
/// only the root and current handles avoids one fd per level in deep trees.
fn reopen(root: &File, path: &[Directory]) -> io::Result<File> {
    let mut current = root.try_clone()?;
    for directory in path {
        let next = open_directory(&current, &directory.name)?;
        if !directory.matches(&next) {
            return Err(io::Error::other("cleanup directory was replaced"));
        }
        current = next;
    }
    Ok(current)
}

fn names(directory: &File) -> io::Result<Vec<OsString>> {
    // Linux procfs resolves this held handle, even when the directory's former
    // pathname has been renamed. No repository pathname is resolved here.
    let mut names = fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

fn unlink(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: parent owns a live fd and name is NUL-terminated. unlinkat never
    // follows the final symlink, and all names come from individual entries.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), flags) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Empty the held directory, reopening descendants relative to it. Iterative
/// postorder traversal bounds both call-stack and fd use independently of depth.
fn empty(root: &File) -> io::Result<()> {
    let mut pending = vec![(Vec::new(), false)];
    while let Some((path, visited)) = pending.pop() {
        let directory = reopen(root, &path)?;
        if visited {
            let (child, ancestors) = path.split_last().expect("only descendants are revisited");
            let parent = reopen(root, ancestors)?;
            if !child.matches(&open_directory(&parent, &child.name)?) {
                return Err(io::Error::other("cleanup directory was replaced"));
            }
            unlink(&parent, &child.name, libc::AT_REMOVEDIR)?;
            continue;
        }
        if !path.is_empty() {
            pending.push((path.clone(), true));
        }
        for name in names(&directory)? {
            match open_directory(&directory, &name) {
                Ok(child) => {
                    let mut path = path.clone();
                    path.push(Directory::new(name, &child.metadata()?));
                    pending.push((path, false));
                }
                Err(error) if matches!(error.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP)) => {
                    // A symlink is unlinked as an entry, never traversed. If a
                    // file becomes a directory, unlinkat fails rather than recursing.
                    unlink(&directory, &name, 0)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

pub(super) fn remove(repo: &Path, before: &BTreeSet<PathBuf>) -> usize {
    remove_with_hook(repo, before, &mut |_| {})
}

fn remove_with_hook(
    repo: &Path,
    before: &BTreeSet<PathBuf>,
    collected: &mut impl FnMut(&Path),
) -> usize {
    // O_NOFOLLOW applies to the checkout itself, not its trusted ancestors:
    // callers may legitimately reach the checkout through a symlinked parent.
    let Ok(root) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(repo)
    else {
        return 0;
    };
    let Ok(metadata) = root.metadata() else {
        return 0;
    };
    let root_identity = Directory::new(OsString::new(), &metadata);
    let mut pending = vec![(Vec::new(), repo.to_path_buf())];
    let mut removed = 0;
    while let Some((ancestors, path)) = pending.pop() {
        let Ok(parent) = reopen(&root, &ancestors) else {
            continue;
        };
        let Ok(entries) = names(&parent) else {
            continue;
        };
        for name in entries {
            if ancestors.is_empty() && name == ".git" {
                continue;
            }
            let Ok(child) = open_directory(&parent, &name) else {
                continue;
            };
            let Ok(metadata) = child.metadata() else {
                continue;
            };
            let identity = Directory::new(name.clone(), &metadata);
            let path = path.join(&name);
            if name != "node_modules" {
                let mut ancestors = ancestors.clone();
                ancestors.push(identity);
                pending.push((ancestors, path));
                continue;
            }
            if before.contains(&path) {
                continue;
            }
            // Collect and remove one candidate at a time, holding both handles
            // throughout. Collecting every candidate's fd would exhaust RLIMIT_NOFILE
            // in a wide workspace just as recursive handles would in a deep one.
            collected(&path);
            let unchanged = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(repo)
                .is_ok_and(|file| root_identity.matches(&file))
                && reopen(&root, &ancestors).is_ok()
                && open_directory(&parent, &name).is_ok_and(|file| identity.matches(&file));
            if unchanged
                && empty(&child).is_ok()
                && open_directory(&parent, &name).is_ok_and(|file| identity.matches(&file))
                && unlink(&parent, &name, libc::AT_REMOVEDIR).is_ok()
            {
                removed += 1;
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn replaced_candidates_and_ancestors_are_skipped() {
        for replaced_level in 0..3 {
            for with_symlink in [false, true] {
                let temporary = tempfile::tempdir().unwrap();
                let repo = temporary.path().join("repo");
                let modules = repo.join("packages/a/node_modules");
                fs::create_dir_all(&modules).unwrap();
                fs::write(modules.join("original"), "original").unwrap();
                let outside = temporary.path().join("outside");
                fs::create_dir_all(outside.join("node_modules")).unwrap();
                fs::write(outside.join("sentinel"), "outside").unwrap();
                fs::write(outside.join("node_modules/sentinel"), "outside").unwrap();
                let replaced = match replaced_level {
                    0 => modules.clone(),
                    1 => modules.parent().unwrap().to_path_buf(),
                    _ => repo.clone(),
                };
                let moved = temporary.path().join("moved");
                let mut calls = 0;
                let removed = remove_with_hook(&repo, &BTreeSet::new(), &mut |candidate| {
                    assert_eq!(candidate, modules);
                    calls += 1;
                    fs::rename(&replaced, &moved).unwrap();
                    if with_symlink {
                        symlink(&outside, &replaced).unwrap();
                    } else {
                        fs::create_dir(&replaced).unwrap();
                        fs::write(replaced.join("replacement"), "replacement").unwrap();
                    }
                });
                assert_eq!(calls, 1);
                assert_eq!(removed, 0);
                assert!(outside.join("sentinel").exists());
                assert!(outside.join("node_modules/sentinel").exists());
                let original = match replaced_level {
                    0 => moved.join("original"),
                    1 => moved.join("node_modules/original"),
                    _ => moved.join("packages/a/node_modules/original"),
                };
                assert!(original.exists());
                if with_symlink {
                    assert!(fs::symlink_metadata(&replaced).unwrap().is_symlink());
                } else {
                    assert!(replaced.join("replacement").exists());
                }
            }
        }
    }

    #[test]
    fn cleanup_accepts_a_symlinked_parent_and_unlinks_inner_symlinks() {
        let temporary = tempfile::tempdir().unwrap();
        let parent = temporary.path().join("parent");
        let repo = parent.join("repo");
        let modules = repo.join("node_modules");
        fs::create_dir_all(modules.join("dependency/nested")).unwrap();
        fs::write(modules.join("dependency/nested/index.js"), "").unwrap();
        let outside = temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "outside").unwrap();
        symlink(&outside, modules.join("link")).unwrap();
        symlink(&outside, repo.join("external")).unwrap();
        let alias = temporary.path().join("alias");
        symlink(&parent, &alias).unwrap();
        assert_eq!(remove(&alias.join("repo"), &BTreeSet::new()), 1);
        assert!(!modules.exists());
        assert!(outside.join("sentinel").exists());
        assert!(repo.join("external").exists());
    }

    #[test]
    fn traversal_keeps_a_bounded_number_of_descriptors() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path();
        let mut parent = repo.to_path_buf();
        // Exercise deep discovery and deep removal, plus many siblings. A
        // handle per pending directory would retain all these siblings at once.
        for _ in 0..160 {
            parent.push("d");
        }
        fs::create_dir_all(parent.join("node_modules")).unwrap();
        let mut nested = parent.join("node_modules");
        for _ in 0..160 {
            nested.push("d");
        }
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("leaf"), "").unwrap();
        for i in 0..160 {
            fs::create_dir_all(repo.join(format!("sibling-{i}/node_modules"))).unwrap();
        }
        let mut calls = 0;
        assert_eq!(
            remove_with_hook(repo, &BTreeSet::new(), &mut |_| {
                calls += 1;
                // Other tests run concurrently; count only handles into this
                // test's private tree rather than imposing a process-wide limit.
                let held = fs::read_dir("/proc/self/fd")
                    .unwrap()
                    .flatten()
                    .filter_map(|entry| fs::read_link(entry.path()).ok())
                    .filter(|path| path.starts_with(repo))
                    .count();
                assert!(held <= 4, "{held} checkout descriptors");
            }),
            161
        );
        assert_eq!(calls, 161);
        assert!(!parent.join("node_modules").exists());
    }
}
