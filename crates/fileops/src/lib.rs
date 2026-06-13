//! Windows file operations for Treelens.
//!
//! All write paths route through the Shell's `IFileOperation` so they appear
//! identically to Explorer (recycle-bin restore, proper UAC, undo support).
//! v0.1 exposes: `recycle`, `open_in_explorer`, `open_in_terminal`,
//! `is_elevated`, and the super-skill helper `find_old_files`.

#![cfg(windows)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING,
    FOFX_ADDUNDORECORD, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem,
    SHCreateItemFromParsingName,
};
use windows::core::{HSTRING, PCWSTR};

#[derive(Debug, thiserror::Error)]
pub enum FileOpError {
    #[error("COM error: {0}")]
    Com(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("operation failed: {0}")]
    Failed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl From<windows::core::Error> for FileOpError {
    fn from(e: windows::core::Error) -> Self {
        FileOpError::Com(format!("{e}"))
    }
}

pub type Result<T> = std::result::Result<T, FileOpError>;

struct ComGuard;
impl ComGuard {
    fn new() -> Result<Self> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
            if hr.is_err() {
                return Err(FileOpError::Com(format!("CoInitializeEx failed: {hr:?}")));
            }
        }
        Ok(ComGuard)
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn shell_item_for(path: &Path) -> Result<IShellItem> {
    let abs = path.canonicalize().or_else(|_| Ok::<_, std::io::Error>(path.to_path_buf()))?;
    let s = abs.to_string_lossy().to_string();
    // Strip the Windows \\?\ verbatim prefix; SHCreateItemFromParsingName dislikes it.
    let s = s.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(s);
    let h = HSTRING::from(&s);
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(&h, None)? };
    Ok(item)
}

/// Move a path (file or directory) to the user's Recycle Bin.
pub fn recycle(path: impl AsRef<Path>) -> Result<()> {
    let _com = ComGuard::new()?;
    let item = shell_item_for(path.as_ref())?;
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        // Quiet, no confirmation, allow undo (recycle, not permanent).
        op.SetOperationFlags(recycle_flags())?;
        op.DeleteItem(&item, None)?;
        op.PerformOperations()?;
    }
    Ok(())
}

fn recycle_flags() -> FILEOPERATION_FLAGS {
    FILEOPERATION_FLAGS(
        FOF_ALLOWUNDO.0 as u32
            | FOF_NOCONFIRMATION.0 as u32
            | FOF_WANTNUKEWARNING.0 as u32
            | FOFX_ADDUNDORECORD.0 as u32
            | FOFX_RECYCLEONDELETE.0 as u32,
    )
}

/// Recycle multiple paths in one operation (faster + atomic from the user's POV).
pub fn recycle_many(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let _com = ComGuard::new()?;
    unsafe {
        let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        op.SetOperationFlags(recycle_flags())?;
        for p in paths {
            let item = shell_item_for(p)?;
            op.DeleteItem(&item, None)?;
        }
        op.PerformOperations()?;
    }
    Ok(())
}

/// Open Explorer with the given file pre-selected.
pub fn open_in_explorer(path: impl AsRef<Path>) -> Result<()> {
    use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, SHOpenFolderAndSelectItems};
    let abs = path
        .as_ref()
        .canonicalize()
        .or_else(|_| Ok::<_, std::io::Error>(path.as_ref().to_path_buf()))?;
    let s = abs.to_string_lossy().to_string();
    let s = s.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(s);
    let w = to_wide(&s);
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(w.as_ptr()));
        if pidl.is_null() {
            return Err(FileOpError::Failed("ILCreateFromPathW returned null".into()));
        }
        let res = SHOpenFolderAndSelectItems(pidl, None, 0);
        ILFree(Some(pidl));
        res?;
    }
    Ok(())
}

/// Open the given directory in Windows Terminal (or PowerShell as a fallback).
pub fn open_in_terminal(dir: impl AsRef<Path>) -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let p = dir.as_ref();
    let p = if p.is_file() {
        p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| p.to_path_buf())
    } else {
        p.to_path_buf()
    };
    let dir_str = p.to_string_lossy().to_string();
    let dir_str = dir_str.strip_prefix(r"\\?\").map(|x| x.to_string()).unwrap_or(dir_str);
    let dir_w = to_wide(&dir_str);
    // Try Windows Terminal first.
    let wt_cmd_w = to_wide("wt.exe");
    let args_w = to_wide(&format!("-d \"{dir_str}\""));
    let open_verb_w = to_wide("open");
    unsafe {
        let h = ShellExecuteW(
            None,
            PCWSTR(open_verb_w.as_ptr()),
            PCWSTR(wt_cmd_w.as_ptr()),
            PCWSTR(args_w.as_ptr()),
            PCWSTR(dir_w.as_ptr()),
            SW_SHOWNORMAL,
        );
        // Returns > 32 on success; < 32 means error (often "no association").
        if (h.0 as isize) > 32 {
            return Ok(());
        }
    }
    // Fallback: PowerShell.
    let ps_w = to_wide("powershell.exe");
    let ps_args_w = to_wide("-NoExit -NoLogo");
    unsafe {
        let h = ShellExecuteW(
            None,
            PCWSTR(open_verb_w.as_ptr()),
            PCWSTR(ps_w.as_ptr()),
            PCWSTR(ps_args_w.as_ptr()),
            PCWSTR(dir_w.as_ptr()),
            SW_SHOWNORMAL,
        );
        if (h.0 as isize) <= 32 {
            return Err(FileOpError::Failed("ShellExecuteW failed".into()));
        }
    }
    Ok(())
}

/// Check whether the current process is running elevated (admin).
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size,
            &mut size,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// Relaunch the current executable elevated (runas). Returns Ok if the new process
/// started; the caller should typically exit the current process immediately after.
pub fn relaunch_as_admin() -> Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = std::env::current_exe()
        .map_err(|e| FileOpError::Failed(format!("current_exe: {e}")))?;
    let exe_str = exe.to_string_lossy().to_string();
    let exe_w = to_wide(&exe_str);
    let runas_w = to_wide("runas");
    unsafe {
        let h = ShellExecuteW(
            None,
            PCWSTR(runas_w.as_ptr()),
            PCWSTR(exe_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if (h.0 as isize) <= 32 {
            return Err(FileOpError::Failed("user declined UAC or runas failed".into()));
        }
    }
    Ok(())
}

// ---------- Super-skill helpers ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OldFile {
    pub path: PathBuf,
    pub size: u64,
    pub mtime: i64,
}

/// Visit a directory tree (no recursion into reparse points) and return files
/// whose mtime is older than `cutoff_unix_secs` and whose size is at least
/// `min_size`. Capped at `limit` entries — beyond that the caller can re-query.
pub fn find_old_files(
    root: impl AsRef<Path>,
    cutoff_unix_secs: i64,
    min_size: u64,
    limit: usize,
) -> Result<Vec<OldFile>> {
    use std::time::UNIX_EPOCH;
    let root = root.as_ref();
    let mut out: Vec<OldFile> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let Ok(iter) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in iter.flatten() {
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = ent.path();
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(meta) = ent.metadata() else { continue };
            if meta.len() < min_size {
                continue;
            }
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            if mtime > cutoff_unix_secs {
                continue;
            }
            out.push(OldFile {
                path,
                size: meta.len(),
                mtime,
            });
            if out.len() >= limit {
                break;
            }
        }
    }
    out.sort_unstable_by(|a, b| b.size.cmp(&a.size));
    Ok(out)
}

/// Find empty directories under `root` (recursive). Reparse points are not descended.
pub fn find_empty_dirs(root: impl AsRef<Path>, limit: usize) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.as_ref().to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let Ok(iter) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<std::fs::DirEntry> = iter.flatten().collect();
        // If any non-symlink-dir child exists, push subdirs and continue.
        let mut has_any_file = false;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for ent in children.drain(..) {
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_symlink() {
                has_any_file = true; // treat as "not empty"
                continue;
            }
            if ft.is_dir() {
                subdirs.push(ent.path());
            } else {
                has_any_file = true;
            }
        }
        for s in &subdirs {
            stack.push(s.clone());
        }
        if !has_any_file && subdirs.is_empty() {
            out.push(dir);
        }
    }
    Ok(out)
}
