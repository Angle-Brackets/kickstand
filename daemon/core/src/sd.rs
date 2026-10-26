// SPDX-License-Identifier: GPL-3.0-only
//! SD card access for the session. Paths here are already normalized and policy-checked SD-root paths
//! (`/switch/...`); this module adds the `sdmc:` mount name and maps fs results to protocol errors.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use nx::fs::{self, DirectoryOpenMode, FileAccessor, FileOpenOption, SeekFrom};
use nx::ipc::sf::fsp::{DirectoryEntryType, rc as fsp_rc};
use nx::result::{ResultBase, ResultCode};
use proto::{DirEntry, Error, ErrorKind};

pub const MOUNT: &str = "sdmc";

pub fn mount() -> nx::result::Result<()> {
    fs::initialize_fspsrv_session()?;
    fs::mount_sd_card(MOUNT)
}

pub fn full(path: &str) -> String {
    format!("{MOUNT}:{path}")
}

pub fn io_error(what: &str, path: &str, rc: ResultCode) -> Error {
    let kind = if fsp_rc::ResultPathNotFound::matches(rc) { ErrorKind::NotFound } else { ErrorKind::Io };
    Error { kind, message: format!("{what} {path}: {rc}"), result: Some(rc.get_value()) }
}

pub fn list_dir(path: &str, max: usize) -> Result<(Vec<DirEntry>, bool), Error> {
    let mode = DirectoryOpenMode::ReadDirectories() | DirectoryOpenMode::ReadFiles();
    let mut dir = fs::open_directory(&full(path), mode).map_err(|rc| io_error("open dir", path, rc))?;
    let mut entries = Vec::new();
    while let Some(entry) = dir.read_next().map_err(|rc| io_error("read dir", path, rc))? {
        if entries.len() == max {
            return Ok((entries, true));
        }
        entries.push(DirEntry {
            name: String::from(entry.name.get_str().unwrap_or("?")),
            is_dir: entry.entry_type == DirectoryEntryType::Directory,
            size: entry.file_size as u64,
        });
    }
    Ok((entries, false))
}

pub fn stat(path: &str) -> Result<DirEntry, Error> {
    let name = String::from(&path[path.rfind('/').map_or(0, |i| i + 1)..]);
    let kind = fs::get_entry_type(&full(path)).map_err(|rc| io_error("stat", path, rc))?;
    if kind == DirectoryEntryType::Directory {
        return Ok(DirEntry { name, is_dir: true, size: 0 });
    }
    let mut file = fs::open_file(&full(path), FileOpenOption::Read()).map_err(|rc| io_error("open", path, rc))?;
    let size = file.get_size().map_err(|rc| io_error("size", path, rc))? as u64;
    Ok(DirEntry { name, is_dir: false, size })
}

/// Reads up to `len` bytes at `offset`; returns the data and the file's size.
pub fn read(path: &str, offset: u64, len: usize) -> Result<(Vec<u8>, u64), Error> {
    let mut file = fs::open_file(&full(path), FileOpenOption::Read()).map_err(|rc| io_error("open", path, rc))?;
    let size = file.get_size().map_err(|rc| io_error("size", path, rc))? as u64;
    let len = len.min(size.saturating_sub(offset) as usize);
    let mut data = vec![0u8; len];
    if len > 0 {
        file.seek(SeekFrom::Start(offset)).map_err(|rc| io_error("seek", path, rc))?;
        let mut done = 0;
        while done < len {
            let n = file.read_array(&mut data[done..]).map_err(|rc| io_error("read", path, rc))?;
            if n == 0 {
                break;
            }
            done += n;
        }
        data.truncate(done);
    }
    Ok((data, size))
}

/// Creates every missing directory leading up to and including `dir`.
pub fn create_dirs(dir: &str) -> Result<(), Error> {
    let mut at = String::new();
    for part in dir.split('/').filter(|p| !p.is_empty()) {
        at.push('/');
        at.push_str(part);
        if let Err(rc) = fs::create_directory(&full(&at))
            && !fsp_rc::ResultPathAlreadyExists::matches(rc)
        {
            return Err(io_error("mkdir", &at, rc));
        }
    }
    Ok(())
}

/// Creates `path` at its final size (so writes don't need append mode) and opens it for writing.
pub fn create_for_write(path: &str, size: u64) -> Result<FileAccessor, Error> {
    remove_if_exists(path)?;
    fs::create_file(&full(path), size as usize, Default::default()).map_err(|rc| io_error("create", path, rc))?;
    fs::open_file(&full(path), FileOpenOption::Write()).map_err(|rc| io_error("open", path, rc))
}

pub fn remove_if_exists(path: &str) -> Result<(), Error> {
    match fs::remove_file(&full(path)) {
        Err(rc) if !fsp_rc::ResultPathNotFound::matches(rc) => Err(io_error("remove", path, rc)),
        _ => Ok(()),
    }
}

pub fn rename(from: &str, to: &str) -> Result<(), Error> {
    fs::rename_file(&full(from), &full(to)).map_err(|rc| io_error("rename", from, rc))
}
