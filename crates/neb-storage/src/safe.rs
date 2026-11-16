use anyhow::{bail, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

/// Storage keys are portable relative paths, not filesystem paths or URLs.
pub fn validate_key(key: &str, allow_empty: bool) -> Result<()> {
    if key.is_empty() {
        if allow_empty {
            return Ok(());
        }
        bail!("Empty storage key");
    }
    if !allow_empty && key.ends_with('/') {
        bail!("File keys must not end with a separator");
    }
    if key.starts_with('/') || key.contains(['\\', ':', '\0', '?', '*', '"', '<', '>', '|']) {
        bail!("Invalid storage key");
    }
    for part in key.trim_end_matches('/').split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part.chars().any(char::is_control)
        {
            bail!("Unsafe storage key component");
        }
        let stem = part.split('.').next().unwrap_or("").to_uppercase();
        if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
            || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && matches!(stem.as_bytes()[3], b'1'..=b'9'))
        {
            bail!("Reserved filename");
        }
    }
    Ok(())
}

pub(crate) fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

/// Reject links, reparse points and components outside the selected root.
pub fn safe_join(root: &Path, key: &str, allow_empty: bool) -> Result<PathBuf> {
    validate_key(key, allow_empty)?;
    let root = fs::canonicalize(root).context("Storage root is unavailable")?;
    let mut current = root.clone();
    for part in Path::new(key).components() {
        match part {
            Component::Normal(name) => current.push(name),
            _ => bail!("Invalid relative path"),
        }
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            if metadata.file_type().is_symlink() || is_reparse(&metadata) {
                bail!("Links and reparse points are not followed");
            }
            if !fs::canonicalize(&current)?.starts_with(&root) {
                bail!("Path escaped the storage root");
            }
        }
    }
    Ok(current)
}

pub fn open_scoped(root: &Path, key: &str) -> Result<File> {
    safe_join(root, key, false)?;
    // Capability-relative open also confines a concurrently replaced ancestor.
    let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    Ok(dir.open(key)?.into_std())
}

pub fn hash_reader(reader: &mut impl Read) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut size = 0;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        size += count as u64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

/// Bounded sample fingerprint: SHA-256 of first 64 KiB + last 64 KiB + 8-byte LE file size.
/// Files up to 128 KiB are hashed in full. Reads at most 128 KiB regardless of file size.
pub fn quick_fingerprint_reader(reader: &mut (impl Read + Seek), size: u64) -> Result<String> {
    const CHUNK: u64 = 64 * 1024;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    if size <= CHUNK * 2 {
        let mut buf = Vec::with_capacity(size as usize);
        reader.take(CHUNK * 2 + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 != size {
            bail!("File size changed while sampling fingerprint");
        }
        hasher.update(&buf);
    } else {
        let mut head = vec![0u8; CHUNK as usize];
        reader.read_exact(&mut head)?;
        hasher.update(&head);
        reader.seek(SeekFrom::Start(size - CHUNK))?;
        let mut tail = vec![0u8; CHUNK as usize];
        reader.read_exact(&mut tail)?;
        hasher.update(&tail);
    }
    hasher.update(size.to_le_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// Pinned capability handle to a canonicalized import source root.
/// Opens relative keys via `O_NOFOLLOW` directory descriptors without per-file `canonicalize`.
pub struct ScopedSource {
    directory: Dir,
}

impl ScopedSource {
    pub fn open(root: &Path) -> Result<Self> {
        let directory = Dir::open_ambient_dir(root, cap_std::ambient_authority())
            .context("Import source is unavailable")?;
        Ok(Self { directory })
    }

    pub fn open_file(&self, key: &str) -> Result<File> {
        validate_key(key, false)?;
        let mut parts: Vec<_> = key.split('/').collect();
        let filename = parts.pop().context("Missing source filename")?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        if parts.is_empty() {
            let file = self
                .directory
                .open_with(filename, &options)
                .context("Links and reparse points are not followed")?
                .into_std();
            let meta = file.metadata()?;
            if is_reparse(&meta) {
                bail!("Links and reparse points are not followed");
            }
            return Ok(file);
        }
        let mut current = self
            .directory
            .open_dir_nofollow(parts[0])
            .context("Source directory is unavailable or is a link")?;
        for part in &parts[1..] {
            current = current
                .open_dir_nofollow(part)
                .context("Source directory is unavailable or is a link")?;
        }
        let file = current
            .open_with(filename, &options)
            .context("Links and reparse points are not followed")?
            .into_std();
        let meta = file.metadata()?;
        if is_reparse(&meta) {
            bail!("Links and reparse points are not followed");
        }
        Ok(file)
    }
}

/// Stream to an owned sibling temporary file, fsync, and atomically publish
/// without replacement in a single pass. Original sources are never removed.
pub fn publish_reader(
    reader: &mut dyn Read,
    destination: &Path,
    expected: Option<&str>,
) -> Result<(u64, String)> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .context("Destination must have a parent directory")?;
    if !parent.is_dir() {
        bail!("Destination parent does not exist");
    }
    let parent = fs::canonicalize(parent)?;
    let name = destination
        .file_name()
        .context("Destination filename is missing")?;
    validate_key(&name.to_string_lossy(), false)?;
    PreparedDestination::new(&parent)?.materialize()?.publish(
        reader,
        &name.to_string_lossy(),
        expected,
    )
}

/// Holds the nearest existing ancestor during review. No directories are made
/// until execution; each missing component is then created/opened without links.
pub struct PreparedDestination {
    ancestor: Dir,
    missing: Vec<String>,
}

impl PreparedDestination {
    pub fn new(path: &Path) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut anchor = PathBuf::new();
        let mut names = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(prefix) => anchor.push(prefix.as_os_str()),
                Component::RootDir => anchor.push(component.as_os_str()),
                Component::Normal(name) => {
                    let name = name.to_str().context("Destination must be valid Unicode")?;
                    validate_key(name, false)?;
                    names.push(name.to_string());
                }
                _ => bail!("Destination must not contain traversal components"),
            }
        }
        let mut ancestor = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?;
        for (index, name) in names.iter().enumerate() {
            match ancestor.open_dir_nofollow(name) {
                Ok(directory) => ancestor = directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Self {
                        ancestor,
                        missing: names[index..].to_vec(),
                    })
                }
                Err(error) => {
                    return Err(error).context("Destination ancestor is inaccessible or is a link")
                }
            }
        }
        Ok(Self {
            ancestor,
            missing: Vec::new(),
        })
    }

    pub fn materialize(&self) -> Result<ScopedDestination> {
        let mut directory = self.ancestor.try_clone()?;
        for name in &self.missing {
            match directory.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            directory = directory
                .open_dir_nofollow(name)
                .context("Destination directory was replaced by a link")?;
        }
        Ok(ScopedDestination { directory })
    }
}

/// All target filesystem operations are relative to pinned directory handles.
pub struct ScopedDestination {
    directory: Dir,
}

impl ScopedDestination {
    fn parent(&self, key: &str, create: bool) -> Result<(Dir, String)> {
        validate_key(key, false)?;
        let mut parts: Vec<_> = key.split('/').collect();
        let filename = parts.pop().context("Missing target filename")?.to_string();
        let mut directory = self.directory.try_clone()?;
        for part in parts {
            if create {
                match directory.create_dir(part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            directory = directory
                .open_dir_nofollow(part)
                .context("Target directory is unavailable or was replaced by a link")?;
        }
        Ok((directory, filename))
    }

    pub fn exists(&self, key: &str) -> Result<bool> {
        match self
            .parent(key, false)
            .and_then(|(directory, name)| Ok(directory.symlink_metadata(name)?))
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub fn reader(&self, key: &str) -> Result<File> {
        let (directory, name) = self.parent(key, false)?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        Ok(directory.open_with(name, &options)?.into_std())
    }

    pub fn publish(
        &self,
        reader: &mut dyn Read,
        key: &str,
        expected: Option<&str>,
    ) -> Result<(u64, String)> {
        self.publish_checked(reader, key, expected, |_, _| Ok(()))
    }

    pub fn publish_checked<F>(
        &self,
        reader: &mut dyn Read,
        key: &str,
        expected: Option<&str>,
        verify_source: F,
    ) -> Result<(u64, String)>
    where
        F: FnOnce(u64, &str) -> Result<()>,
    {
        let (parent, name) = self.parent(key, true)?;
        if parent.symlink_metadata(&name).is_ok() {
            bail!("Destination already exists; replacement is forbidden");
        }
        let temporary_name = format!(".nebulume-{}.partial", uuid::Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut temporary = Some(parent.open_with(&temporary_name, &options)?.into_std());
        let outcome = (|| -> Result<(u64, String)> {
            let mut hasher = Sha256::new();
            let mut size = 0;
            let mut buffer = vec![0u8; 256 * 1024];
            {
                let file = temporary.as_mut().expect("temporary file is open");
                loop {
                    let count = reader.read(&mut buffer)?;
                    if count == 0 {
                        break;
                    }
                    file.write_all(&buffer[..count])?;
                    hasher.update(&buffer[..count]);
                    size += count as u64;
                }
                file.sync_all()?;
            }
            // Close the temporary descriptor before linking/renaming (required on Windows/SMB).
            drop(temporary.take());
            let hash = hex::encode(hasher.finalize());
            if expected.is_some_and(|expected| expected != hash) {
                bail!("Source content changed after planning");
            }
            verify_source(size, &hash)?;
            publish_temp_no_overwrite(&parent, &temporary_name, &name)?;
            Ok((size, hash))
        })();
        drop(temporary);
        // Only this operation's randomized temporary name is removed, through
        // the same pinned parent capability; no ambient or discovered paths.
        let _ = parent.remove_file(&temporary_name);
        outcome
    }
}

fn hard_link_unsupported(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidInput
    ) {
        return true;
    }
    #[cfg(unix)]
    if let Some(raw) = error.raw_os_error() {
        // ENOTSUP (45 on macOS, 95 on Linux), EOPNOTSUPP, EXDEV, EPERM, ENOSYS, EINVAL
        if matches!(raw, 1 | 18 | 22 | 38 | 45 | 95 | 102) {
            return true;
        }
    }
    false
}

fn publish_temp_no_overwrite(parent: &Dir, temporary_name: &str, name: &str) -> Result<()> {
    match parent.hard_link(temporary_name, parent, name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("Destination already exists; replacement is forbidden")
        }
        // Fallback for exFAT / FAT32 / SMB / mounted WebDAV volumes that reject hard links.
        Err(error) if hard_link_unsupported(&error) => {
            if parent.symlink_metadata(name).is_ok() {
                bail!("Destination already exists; replacement is forbidden");
            }
            parent
                .rename(temporary_name, parent, name)
                .context("Safe publication failed while committing temporary file")
        }
        Err(error) => Err(error).context("Safe publication failed while linking temporary file"),
    }
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("JSON destination needs a parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
