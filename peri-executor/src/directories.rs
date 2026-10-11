use std::path::Path;

use peri_acp_types::device_executor::{DirectoryEntry, DirectoryListing, DirectoryQuery};

use crate::{Error, Executor, Result};

impl Executor {
    pub async fn browse_directories(&self, query: DirectoryQuery) -> Result<DirectoryListing> {
        if query.path.len() > 4096
            || query.path.chars().any(char::is_control)
            || (!query.path.is_empty() && !Path::new(&query.path).is_absolute())
            || !(1..=100).contains(&query.limit)
            || query.offset > 100_000
        {
            return Err(Error::Invalid("invalid directory query".into()));
        }
        // An empty discovery path lists system roots without requiring a
        // previously saved directory to still exist.
        if query.path.is_empty() {
            return Ok(DirectoryListing {
                path: String::new(),
                parent: None,
                roots: roots(),
                directories: Vec::new(),
                next_offset: None,
            });
        }
        tokio::task::spawn_blocking(move || browse(query))
            .await
            .map_err(|_| Error::Invalid("directory discovery failed".into()))?
    }
}

#[cfg(test)]
#[path = "directories_test.rs"]
mod tests;

fn browse(query: DirectoryQuery) -> Result<DirectoryListing> {
    let path = std::fs::canonicalize(query.path)?;
    let current = display(&path)?;
    let mut children = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let Ok(entry) = entry else { continue };
        if !entry.path().is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.chars().any(char::is_control) {
            continue;
        }
        let Ok(child) = display(&entry.path()) else {
            continue;
        };
        children.push(DirectoryEntry { name, path: child });
        if children.len() > 100_000 {
            return Err(Error::Invalid(
                "directory has too many entries; specify a narrower path".into(),
            ));
        }
    }
    children.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.name.cmp(&b.name))
    });
    let next = query.offset.saturating_add(query.limit);
    let next_offset = (next < children.len()).then_some(next);
    Ok(DirectoryListing {
        path: current,
        parent: path.parent().map(display).transpose()?,
        roots: roots(),
        directories: children
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .collect(),
        next_offset,
    })
}

fn display(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .ok_or_else(|| Error::Invalid("directory must be UTF-8".into()))?;
    Ok(value.to_owned())
}

fn roots() -> Vec<String> {
    #[cfg(windows)]
    {
        // GetLogicalDrives reads the mounted drive mask without probing network paths.
        let mask = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
        (0..26)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| format!("{}:\\", char::from(b'A' + bit)))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec!["/".into()]
    }
}
