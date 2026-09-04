use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{MaybeUninit, size_of};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS, ERROR_INSUFFICIENT_BUFFER,
    ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACL, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
    GetLengthSid, GetSecurityDescriptorDacl, GetTokenInformation, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FileDispositionInfoEx, FileIdInfo, FileRenameInfoEx, FileStandardInfo,
    GetFileInformationByHandleEx, SetFileInformationByHandle,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::WindowsProgramming::{
    FILE_RENAME_FLAG_POSIX_SEMANTICS, FILE_RENAME_FLAG_REPLACE_IF_EXISTS,
};

use crate::private_fs::{DatabaseMode, EntryStat, FileStat};
use crate::{CacheError, Result};

#[path = "platform/cleanup.rs"]
mod cleanup;
pub(super) use cleanup::clean;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const SHARE_PINNED: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE;
const SHARE_PAYLOAD: u32 = SHARE_PINNED | FILE_SHARE_DELETE;
const OPEN_NO_REPARSE: u32 = FILE_FLAG_OPEN_REPARSE_POINT;
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
const PUBLICATION_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    id: [u8; 16],
}

#[derive(Clone, Copy)]
struct HandleStat {
    identity: FileIdentity,
    size: u64,
    allocated: u64,
    modified_ns: u64,
    links: u32,
    directory: bool,
    reparse: bool,
}

pub(super) struct Directory {
    // Every ancestor stays open without FILE_SHARE_DELETE. Namespace changes
    // cannot redirect a later operation away from this validated chain.
    chain: Arc<Vec<Arc<File>>>,
    path: PathBuf,
}

impl Directory {
    pub(super) fn root(path: &Path, create: bool) -> Result<Self> {
        let absolute = std::path::absolute(path)?;
        let mut current = PathBuf::new();
        let mut chain = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(_) => current.push(component.as_os_str()),
                Component::RootDir => {
                    current.push(component.as_os_str());
                    chain.push(Arc::new(open_validated_directory(&current)?));
                }
                Component::Normal(name) => {
                    current.push(name);
                    let file = match open_validated_directory(&current) {
                        Ok(file) => file,
                        Err(CacheError::Io(error))
                            if create && error.kind() == io::ErrorKind::NotFound =>
                        {
                            match create_private_directory(&current) {
                                Ok(()) => {}
                                Err(CacheError::Io(error))
                                    if error.kind() == io::ErrorKind::AlreadyExists => {}
                                Err(error) => return Err(error),
                            }
                            open_validated_directory(&current)?
                        }
                        Err(error) => return Err(error),
                    };
                    chain.push(Arc::new(file));
                }
                Component::CurDir | Component::ParentDir => {
                    return Err(unsafe_path(
                        &absolute,
                        "cache root contains an unsafe path component",
                    ));
                }
            }
        }
        let file = chain
            .last()
            .ok_or_else(|| unsafe_path(&absolute, "cache root has no directory component"))?;
        validate_private_acl(file, &absolute)?;
        Ok(Self {
            chain: Arc::new(chain),
            path: absolute,
        })
    }

    #[cfg(feature = "xet-chunk-cache")]
    pub(super) fn root_with_private_parent(path: &Path) -> Result<(Self, Option<Self>)> {
        let root = Self::root(path, false)?;
        let parent = path
            .parent()
            .and_then(|parent| Self::root(parent, false).ok());
        Ok((root, parent))
    }

    pub(super) fn child(&self, name: &OsStr, create: bool) -> Result<Self> {
        component_name(name)?;
        let path = self.path.join(name);
        if create {
            match create_private_directory(&path) {
                Ok(()) => {}
                Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let file = open_validated_directory(&path)?;
        validate_private_acl(&file, &path)?;
        let mut chain = self.chain.as_ref().clone();
        chain.push(Arc::new(file));
        Ok(Self {
            chain: Arc::new(chain),
            path,
        })
    }

    fn descendant_parent(&self, relative: &Path, create: bool) -> Result<(Self, OsString)> {
        let path = self.path.join(relative);
        let mut directory = Self {
            chain: Arc::clone(&self.chain),
            path: self.path.clone(),
        };
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                return Err(unsafe_path(
                    &path,
                    "entry contains an unsafe path component",
                ));
            };
            if components.peek().is_none() {
                component_name(name)?;
                return Ok((directory, name.to_owned()));
            }
            directory = directory.child(name, create)?;
        }
        Err(unsafe_path(&path, "entry has no filename"))
    }

    pub(super) fn open_read(&self, relative: &Path) -> Result<File> {
        let (directory, name) = self.descendant_parent(relative, false)?;
        let path = directory.path.join(name);
        let file = open_file(&path, false, false, SHARE_PAYLOAD)?;
        validate_handle(&file, &path, false)?;
        validate_private_acl(&file, &path)?;
        if !fs4::fs_std::FileExt::try_lock_shared(&file)? {
            return Err(busy("cache entry is being maintained"));
        }
        Ok(file)
    }

    pub(super) fn open_lock(&self, relative: &Path) -> Result<File> {
        let (directory, name) = self.descendant_parent(relative, true)?;
        let path = directory.path.join(name);
        let file = open_file(&path, true, false, SHARE_PINNED)?;
        validate_handle(&file, &path, false)?;
        validate_private_acl(&file, &path)?;
        Ok(file)
    }

    pub(super) fn remove_read_file(&self, relative: &Path, original: &File) -> Result<Option<u64>> {
        fs4::fs_std::FileExt::unlock(original)?;
        let identity = handle_stat(original)?.identity;
        self.remove_relative_if(relative, false, &mut |candidate| {
            Ok(handle_stat(candidate)?.identity == identity)
        })
    }

    pub(super) fn remove_relative(&self, relative: &Path) -> Result<u64> {
        self.remove_relative_if(relative, false, &mut |_| Ok(true))?
            .ok_or_else(|| io::Error::other("unconditional cache removal was skipped").into())
    }

    fn remove_payload(&self, name: &OsStr, dry_run: bool) -> Result<u64> {
        component_name(name)?;
        self.remove_relative_if(Path::new(name), dry_run, &mut |_| Ok(true))?
            .ok_or_else(|| io::Error::other("unconditional cache removal was skipped").into())
    }

    pub(super) fn remove_relative_if(
        &self,
        relative: &Path,
        dry_run: bool,
        should_remove: &mut dyn FnMut(&mut File) -> Result<bool>,
    ) -> Result<Option<u64>> {
        let (directory, name) = self.descendant_parent(relative, false)?;
        let path = directory.path.join(name);
        let mut file = open_for_delete(&path)?;
        let stat = handle_stat(&file)?;
        validate_handle(&file, &path, false)?;
        validate_private_acl(&file, &path)?;
        if !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
            return Err(busy("cache entry has an active reader"));
        }
        if !should_remove(&mut file)? {
            return Ok(None);
        }
        if !dry_run {
            delete_handle(&file)?;
        }
        Ok(Some(stat.size))
    }

    fn entry_names(&self) -> Result<Vec<OsString>> {
        std::fs::read_dir(&self.path)?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name())
                    .map_err(CacheError::from)
            })
            .collect()
    }

    pub(super) fn visit_files(
        &self,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        self.visit_selected_files(&|_| Ok(true), visitor)
    }

    pub(super) fn visit_selected_files(
        &self,
        select: &dyn Fn(&Path) -> Result<bool>,
        visitor: &mut dyn FnMut(&Path, FileStat) -> Result<()>,
    ) -> Result<()> {
        visit_directory(self, Path::new(""), 0, select, &mut |path, entry| {
            let entry = entry?;
            if !entry.is_directory {
                visitor(path, entry.file)?;
            }
            Ok(())
        })
    }

    pub(super) fn inspect_entries(
        &self,
        visitor: &mut dyn FnMut(&Path, Result<EntryStat>) -> Result<()>,
    ) -> Result<()> {
        visit_directory(self, Path::new(""), 0, &|_| Ok(true), visitor)
    }
}

const MAX_SCAN_DEPTH: usize = 32;

fn visit_directory(
    directory: &Directory,
    relative: &Path,
    depth: usize,
    select: &dyn Fn(&Path) -> Result<bool>,
    visitor: &mut dyn FnMut(&Path, Result<EntryStat>) -> Result<()>,
) -> Result<()> {
    if depth > MAX_SCAN_DEPTH {
        return visitor(
            relative,
            Err(io::Error::new(io::ErrorKind::InvalidData, "cache inventory is too deep").into()),
        );
    }
    let handle = directory
        .chain
        .last()
        .ok_or_else(|| CacheError::Internal("Windows cache directory lost its handle".into()))?;
    visitor(relative, entry_stat(handle, &directory.path))?;
    for name in directory.entry_names()? {
        let child_relative = relative.join(&name);
        if !select(&child_relative)? {
            continue;
        }
        let path = directory.path.join(&name);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            visitor(
                &child_relative,
                Err(unsafe_path(&path, "cache entry is a reparse point")),
            )?;
            continue;
        }
        if metadata.is_dir() {
            match directory.child(&name, false) {
                Ok(child) => visit_directory(&child, &child_relative, depth + 1, select, visitor)?,
                Err(error) => visitor(&child_relative, Err(error))?,
            }
            continue;
        }
        let file = match open_file(&path, false, false, SHARE_PAYLOAD) {
            Ok(file) => file,
            Err(error) => {
                visitor(&child_relative, Err(error))?;
                continue;
            }
        };
        visitor(&child_relative, entry_stat(&file, &path))?;
    }
    Ok(())
}

fn entry_stat(file: &File, path: &Path) -> Result<EntryStat> {
    validate_private_acl(file, path)?;
    let stat = handle_stat(file)?;
    if stat.reparse || (!stat.directory && stat.links != 1) {
        return Err(unsafe_path(
            path,
            "entry is a reparse point or has another hard link",
        ));
    }
    Ok(EntryStat {
        file: FileStat {
            size: stat.size,
            modified_ns: stat.modified_ns,
        },
        allocated_bytes: stat.allocated,
        is_directory: stat.directory,
    })
}

fn component_name(name: &OsStr) -> Result<OsString> {
    let path = Path::new(name);
    if path.components().count() == 1
        && matches!(path.components().next(), Some(Component::Normal(_)))
    {
        Ok(name.to_owned())
    } else {
        Err(unsafe_path(path, "expected one normal path component"))
    }
}

fn unsafe_path(path: &Path, reason: &str) -> CacheError {
    CacheError::UnsafeRoot {
        path: path.display().to_string(),
        reason: reason.to_owned(),
    }
}

fn busy(message: &str) -> CacheError {
    io::Error::new(io::ErrorKind::WouldBlock, message).into()
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn open_directory(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .share_mode(SHARE_PINNED)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | OPEN_NO_REPARSE);
    Ok(options.open(path)?)
}

fn open_validated_directory(path: &Path) -> Result<File> {
    let file = open_directory(path)?;
    validate_handle(&file, path, true)?;
    Ok(file)
}

fn open_file(path: &Path, create: bool, exclusive: bool, share: u32) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .share_mode(share)
        .custom_flags(OPEN_NO_REPARSE);
    if exclusive {
        options.create_new(true);
    } else if create {
        options.create(true);
    }
    Ok(options.open(path)?)
}

fn open_for_delete(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .access_mode(GENERIC_READ | DELETE)
        .share_mode(SHARE_PINNED)
        .custom_flags(OPEN_NO_REPARSE);
    match options.open(path) {
        Ok(file) => Ok(file),
        Err(error) if error.raw_os_error() == Some(ERROR_SHARING_VIOLATION as i32) => {
            Err(busy("cache entry has an active publisher"))
        }
        Err(error) => Err(error.into()),
    }
}

fn open_temporary(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
        .share_mode(SHARE_PINNED)
        .custom_flags(OPEN_NO_REPARSE)
        .create_new(true);
    Ok(options.open(path)?)
}

fn handle_stat(file: &File) -> Result<HandleStat> {
    let handle = file.as_raw_handle() as HANDLE;
    let mut id = MaybeUninit::<FILE_ID_INFO>::uninit();
    let mut standard = MaybeUninit::<FILE_STANDARD_INFO>::uninit();
    // SAFETY: both calls receive a live handle and correctly sized outputs.
    unsafe {
        if GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            id.as_mut_ptr().cast(),
            size_of::<FILE_ID_INFO>() as u32,
        ) == 0
            || GetFileInformationByHandleEx(
                handle,
                FileStandardInfo,
                standard.as_mut_ptr().cast(),
                size_of::<FILE_STANDARD_INFO>() as u32,
            ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    // SAFETY: successful calls initialized both outputs.
    let id = unsafe { id.assume_init() };
    let standard = unsafe { standard.assume_init() };
    let metadata = file.metadata()?;
    Ok(HandleStat {
        identity: FileIdentity {
            volume: id.VolumeSerialNumber,
            id: id.FileId.Identifier,
        },
        size: standard.EndOfFile as u64,
        allocated: standard.AllocationSize as u64,
        // Windows FILETIME counts 100 ns intervals since 1601. Catalog order
        // uses Unix nanoseconds and SQLite integers are signed 64-bit values.
        modified_ns: metadata
            .last_write_time()
            .saturating_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
            .saturating_mul(100),
        links: standard.NumberOfLinks,
        directory: standard.Directory,
        reparse: metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    })
}

fn validate_handle(file: &File, path: &Path, directory: bool) -> Result<()> {
    let stat = handle_stat(file)?;
    if stat.reparse || stat.directory != directory || (!directory && stat.links != 1) {
        return Err(unsafe_path(
            path,
            "entry is a reparse point, special file, or has another hard link",
        ));
    }
    Ok(())
}

fn delete_handle(file: &File) -> Result<()> {
    let disposition = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: the disposition value is valid for this live DELETE handle.
    let result = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            ptr::from_ref(&disposition).cast(),
            size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this owner closes its token exactly once.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

fn current_user_sid() -> Result<Vec<u8>> {
    let mut token = ptr::null_mut();
    // SAFETY: output receives one owned process-token handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let token = OwnedHandle(token);
    let mut bytes = 0;
    // SAFETY: null buffer requests the required size.
    unsafe { GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut bytes) };
    if io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        return Err(io::Error::last_os_error().into());
    }
    let mut buffer = vec![0u8; bytes as usize];
    // SAFETY: the sized buffer receives TOKEN_USER and its referenced SID.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: successful TokenUser output starts with this structure.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    // SAFETY: Windows validated the SID returned by GetTokenInformation.
    let length = unsafe { GetLengthSid(sid) } as usize;
    let mut copy = vec![0u8; length];
    // SAFETY: both SID extents are exactly length bytes and do not overlap.
    unsafe { ptr::copy_nonoverlapping(sid.cast::<u8>(), copy.as_mut_ptr(), length) };
    Ok(copy)
}

fn well_known_sid(kind: i32) -> Result<Vec<u8>> {
    let mut bytes = windows_sys::Win32::Security::SECURITY_MAX_SID_SIZE as u32;
    let mut sid = vec![0u8; bytes as usize];
    // SAFETY: the sized output receives a well-known local SID.
    if unsafe { CreateWellKnownSid(kind, ptr::null_mut(), sid.as_mut_ptr().cast(), &mut bytes) }
        == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    sid.truncate(bytes as usize);
    Ok(sid)
}

fn sid_allowed(sid: PSID, allowed: &[Vec<u8>]) -> bool {
    allowed
        .iter()
        // SAFETY: ACL parsing and owned SID construction supply valid SIDs.
        .any(|candidate| unsafe { EqualSid(sid, candidate.as_ptr().cast_mut().cast()) } != 0)
}

fn validate_private_acl(file: &File, path: &Path) -> Result<()> {
    let mut descriptor = ptr::null_mut();
    let mut owner = ptr::null_mut();
    let code = unsafe {
        windows_sys::Win32::Security::Authorization::GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code as i32).into());
    }
    let descriptor = LocalDescriptor(descriptor);
    let user = current_user_sid()?;
    let system = well_known_sid(WinLocalSystemSid)?;
    let administrators = well_known_sid(WinBuiltinAdministratorsSid)?;
    let allowed = [user, system, administrators];
    // SAFETY: GetSecurityInfo returned owner and a self-relative descriptor.
    if owner.is_null() || !sid_allowed(owner, &allowed) {
        return Err(unsafe_path(
            path,
            "cache entry is not owned by the current user or a trusted system principal",
        ));
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut acl: *mut ACL = ptr::null_mut();
    // SAFETY: descriptor remains live and outputs receive its DACL metadata.
    if unsafe { GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) }
        == 0
        || present == 0
        || acl.is_null()
    {
        return Err(unsafe_path(path, "cache entry has no private DACL"));
    }
    // SAFETY: the ACL header is part of the live validated descriptor.
    let count = unsafe { (*acl).AceCount };
    for index in 0..u32::from(count) {
        let mut raw = ptr::null_mut();
        // SAFETY: index is bounded by AceCount and receives an ACE pointer.
        if unsafe { GetAce(acl, index, &mut raw) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: every ACE begins with ACE_HEADER.
        let header = unsafe { &*raw.cast::<windows_sys::Win32::Security::ACE_HEADER>() };
        if u32::from(header.AceType) == ACCESS_ALLOWED_ACE_TYPE {
            // SAFETY: this tag identifies ACCESS_ALLOWED_ACE and SidStart.
            let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
            if !sid_allowed(ptr::from_ref(&ace.SidStart).cast_mut().cast(), &allowed) {
                return Err(unsafe_path(
                    path,
                    "cache entry grants access to another principal",
                ));
            }
        } else if matches!(header.AceType, 4 | 5 | 9 | 11) {
            return Err(unsafe_path(
                path,
                "cache entry contains an unsupported allow ACE",
            ));
        }
    }
    Ok(())
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        // SAFETY: LocalAlloc created the security descriptor returned here.
        unsafe { LocalFree(self.0.cast()) };
    }
}

fn private_descriptor() -> Result<LocalDescriptor> {
    let user = current_user_sid()?;
    let mut sid_string = ptr::null_mut();
    // SAFETY: the copied current-user SID is valid and output is LocalAlloc-owned.
    if unsafe { ConvertSidToStringSidW(user.as_ptr().cast_mut().cast(), &mut sid_string) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    let sid_string = LocalWide(sid_string);
    let length = (0..)
        .find(|index| unsafe { *sid_string.0.add(*index) } == 0)
        .ok_or_else(|| io::Error::other("Windows SID string is not terminated"))?;
    // SAFETY: length stops before the terminator in the LocalAlloc buffer.
    let sid = String::from_utf16(unsafe { std::slice::from_raw_parts(sid_string.0, length) })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let sddl = format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = ptr::null_mut();
    // SAFETY: SDDL is terminated and output is LocalAlloc-owned.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(LocalDescriptor(descriptor))
}

fn create_private_directory(path: &Path) -> Result<()> {
    let descriptor = private_descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path = wide(path);
    // SAFETY: path, attributes, and the protected descriptor remain live for
    // the synchronous creation call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(())
    }
}

struct LocalWide(*mut u16);

impl Drop for LocalWide {
    fn drop(&mut self) {
        // SAFETY: ConvertSidToStringSidW returns a LocalAlloc-owned buffer.
        unsafe { LocalFree(self.0.cast()) };
    }
}

pub(crate) struct Database {
    connection: rusqlite::Connection,
    _root: Directory,
    _main: File,
    identity: DatabaseIdentity,
}

impl std::ops::Deref for Database {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl Database {
    pub(crate) fn transaction(&mut self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection.transaction()
    }

    pub(crate) fn transaction_with_behavior(
        &mut self,
        behavior: rusqlite::TransactionBehavior,
    ) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.connection.transaction_with_behavior(behavior)
    }

    pub(super) fn identity(&self) -> DatabaseIdentity {
        self.identity
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DatabaseIdentity(FileIdentity);

#[cfg(any(feature = "local-cache", test))]
pub(super) fn open_database(
    root: &Path,
    path: &Path,
    mode: DatabaseMode,
    busy_timeout: std::time::Duration,
) -> Result<Database> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
    let root = Directory::root(root, mode == DatabaseMode::Create)?;
    open_database_at(&root, relative, mode, busy_timeout)
}

pub(super) fn open_database_at(
    root: &Directory,
    relative: &Path,
    mode: DatabaseMode,
    busy_timeout: std::time::Duration,
) -> Result<Database> {
    let (directory, name) = root.descendant_parent(relative, mode == DatabaseMode::Create)?;
    let path = directory.path.join(name);
    if mode == DatabaseMode::Create {
        match open_file(&path, true, true, SHARE_PINNED) {
            Ok(file) => {
                validate_handle(&file, &path, false)?;
                validate_private_acl(&file, &path)?;
            }
            Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let main = open_file(&path, false, false, SHARE_PINNED)?;
    validate_handle(&main, &path, false)?;
    validate_private_acl(&main, &path)?;
    let flags = match mode {
        DatabaseMode::ReadOnly => rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        DatabaseMode::ReadWrite | DatabaseMode::Create => {
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
        }
    } | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX
        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = rusqlite::Connection::open_with_flags(&path, flags).map_err(|source| {
        CacheError::Index {
            path: path.display().to_string(),
            source,
        }
    })?;
    connection
        .busy_timeout(busy_timeout)
        .map_err(|source| CacheError::Index {
            path: path.display().to_string(),
            source,
        })?;
    if handle_stat(&main)?.identity
        != handle_stat(&open_file(&path, false, false, SHARE_PINNED)?)?.identity
    {
        return Err(unsafe_path(
            &path,
            "database changed while SQLite opened it",
        ));
    }
    let identity = DatabaseIdentity(handle_stat(&main)?.identity);
    Ok(Database {
        connection,
        _root: directory,
        _main: main,
        identity,
    })
}

pub(super) fn open_database_leased(
    root: &Directory,
    relative: &Path,
    identity: DatabaseIdentity,
    busy_timeout: std::time::Duration,
) -> Result<Database> {
    let database = open_database_at(root, relative, DatabaseMode::ReadWrite, busy_timeout)?;
    if database.identity().0 != identity.0 {
        return Err(unsafe_path(
            &root.path.join(relative),
            "database generation changed",
        ));
    }
    Ok(database)
}

pub(super) fn validate_database_generation(
    root: &Directory,
    relative: &Path,
    identity: DatabaseIdentity,
) -> Result<()> {
    let (directory, name) = root.descendant_parent(relative, false)?;
    let path = directory.path.join(name);
    let file = open_file(&path, false, false, SHARE_PINNED)?;
    if handle_stat(&file)?.identity == identity.0 {
        Ok(())
    } else {
        Err(unsafe_path(&path, "database generation changed"))
    }
}

pub(super) fn check_directory(path: &Path, create: bool) -> Result<()> {
    Directory::root(path, create).map(|_| ())
}

#[cfg(any(feature = "local-cache", test))]
pub(super) fn open_read(root: &Path, path: &Path) -> Result<File> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
    Directory::root(root, false)?.open_read(relative)
}

#[cfg(feature = "xet-chunk-cache")]
pub(super) fn entry_names(root: &Path, path: &Path, limit: usize) -> Result<Vec<OsString>> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
    let pinned = Directory::root(root, false)?;
    let directory =
        relative
            .components()
            .try_fold(pinned, |directory, component| match component {
                Component::Normal(name) => directory.child(name, false),
                _ => Err(unsafe_path(path, "entry contains an unsafe path component")),
            })?;
    let mut names = directory.entry_names()?;
    if names.len() > limit {
        return Err(CacheError::CorruptObject {
            path: path.display().to_string(),
            reason: format!("directory exceeds the {limit} entry safety limit"),
        });
    }
    names.sort();
    Ok(names)
}

pub(super) struct TemporaryFile {
    _directory: Directory,
    temporary_path: PathBuf,
    destination_path: PathBuf,
    file: File,
    published: bool,
}

impl TemporaryFile {
    #[cfg(test)]
    pub(super) fn new(root: &Path, path: &Path) -> Result<Self> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| unsafe_path(path, "entry is outside cache root"))?;
        Self::new_at(&Directory::root(root, true)?, relative)
    }

    pub(super) fn new_at(root: &Directory, relative: &Path) -> Result<Self> {
        let (directory, destination) = root.descendant_parent(relative, true)?;
        let destination_path = directory.path.join(destination);
        for _ in 0..128 {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary_path = directory
                .path
                .join(format!(".tmp-{}-{sequence}", std::process::id()));
            match open_temporary(&temporary_path) {
                Ok(file) => {
                    validate_handle(&file, &temporary_path, false)?;
                    validate_private_acl(&file, &temporary_path)?;
                    return Ok(Self {
                        _directory: directory,
                        temporary_path,
                        destination_path,
                        file,
                        published: false,
                    });
                }
                Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "temporary cache names exhausted",
        )
        .into())
    }

    pub(super) fn file(&self) -> &File {
        &self.file
    }

    pub(super) fn lease(&self) -> Result<File> {
        // This duplicate retains the same no-delete Windows file object. After
        // handle-based publication it pins the destination until registration.
        Ok(self.file.try_clone()?)
    }

    #[cfg(all(feature = "remote-client", feature = "local-cache"))]
    pub(super) fn into_unlinked_file(mut self) -> Result<File> {
        delete_handle(&self.file)?;
        self.published = true;
        Ok(self.file.try_clone()?)
    }

    pub(super) fn commit(mut self) -> Result<()> {
        let started = Instant::now();
        loop {
            match rename_handle(&self.file, &self.destination_path) {
                Ok(()) => break,
                Err(CacheError::Io(error))
                    if publication_busy(&error) && started.elapsed() < PUBLICATION_BUSY_TIMEOUT =>
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
        self.published = true;
        Ok(())
    }
}

fn publication_busy(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        [
            ERROR_ACCESS_DENIED,
            ERROR_ALREADY_EXISTS,
            ERROR_FILE_EXISTS,
            ERROR_SHARING_VIOLATION,
        ]
        .contains(&(code as u32))
    })
}

fn rename_handle(file: &File, destination: &Path) -> Result<()> {
    let destination = wide(destination);
    let name_bytes = destination
        .len()
        .checked_sub(1)
        .and_then(|units| units.checked_mul(size_of::<u16>()))
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache path is too long"))?;
    let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let buffer_bytes = header
        .checked_add(name_bytes as usize)
        .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "cache path is too long"))?;
    let buffer_bytes = u32::try_from(buffer_bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let mut buffer = vec![0usize; (buffer_bytes as usize).div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: the usize buffer is aligned for FILE_RENAME_INFO and has room
    // for its fixed header followed by every UTF-16 destination code unit.
    unsafe {
        ptr::write(
            info,
            FILE_RENAME_INFO {
                Anonymous: FILE_RENAME_INFO_0 {
                    Flags: FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS,
                },
                RootDirectory: ptr::null_mut(),
                FileNameLength: name_bytes,
                FileName: [0],
            },
        );
        ptr::copy_nonoverlapping(
            destination.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            destination.len(),
        );
        if SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileRenameInfoEx,
            info.cast(),
            buffer_bytes,
        ) == 0
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    Ok(())
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = delete_handle(&self.file);
        }
    }
}

pub(super) struct DirectoryStream(std::vec::IntoIter<OsString>);

impl DirectoryStream {
    pub(super) fn new(directory: &Directory) -> Result<Self> {
        Ok(Self(directory.entry_names()?.into_iter()))
    }

    pub(super) fn next_name(&mut self) -> Result<Option<OsString>> {
        Ok(self.0.next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn private_root_roundtrips_and_removes_a_payload() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = root.join("objects/value");
        let mut pending = TemporaryFile::new(&root, &path).unwrap();
        pending.file.write_all(b"value").unwrap();
        pending.commit().unwrap();

        let pinned = Directory::root(&root, false).unwrap();
        let mut bytes = Vec::new();
        pinned
            .open_read(Path::new("objects/value"))
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"value");
        assert_eq!(
            pinned.remove_relative(Path::new("objects/value")).unwrap(),
            5
        );
        assert!(!path.exists());
    }

    #[test]
    fn active_reader_prevents_payload_removal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = root.join("objects/value");
        let mut pending = TemporaryFile::new(&root, &path).unwrap();
        pending.file.write_all(b"value").unwrap();
        pending.commit().unwrap();
        let pinned = Directory::root(&root, false).unwrap();
        let reader = pinned.open_read(Path::new("objects/value")).unwrap();

        assert!(matches!(
            pinned.remove_relative(Path::new("objects/value")),
            Err(CacheError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock
        ));
        drop(reader);
        assert_eq!(
            pinned.remove_relative(Path::new("objects/value")).unwrap(),
            5
        );
    }

    #[test]
    fn pinned_root_cannot_be_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let pinned = Directory::root(&root, true).unwrap();

        assert!(std::fs::rename(&root, temp.path().join("replacement")).is_err());
        drop(pinned);
        std::fs::rename(&root, temp.path().join("replacement")).unwrap();
    }

    #[test]
    fn pinned_database_cannot_be_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let pinned = Directory::root(&root, true).unwrap();
        let database = open_database_at(
            &pinned,
            Path::new("catalog.sqlite"),
            DatabaseMode::Create,
            std::time::Duration::ZERO,
        )
        .unwrap();
        let path = root.join("catalog.sqlite");
        let replacement = root.join("replacement.sqlite");

        assert!(std::fs::rename(&path, &replacement).is_err());
        drop(database);
        std::fs::rename(&path, &replacement).unwrap();
    }

    #[test]
    fn hard_linked_payload_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let path = root.join("objects/value");
        let mut pending = TemporaryFile::new(&root, &path).unwrap();
        pending.file.write_all(b"value").unwrap();
        pending.commit().unwrap();
        std::fs::hard_link(&path, root.join("second-link")).unwrap();

        assert!(matches!(
            Directory::root(&root, false)
                .unwrap()
                .open_read(Path::new("objects/value")),
            Err(CacheError::UnsafeRoot { .. })
        ));
    }
}
