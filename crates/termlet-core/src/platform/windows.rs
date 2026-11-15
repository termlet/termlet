//! Windows：用受保护的 DACL 把目录、文件限制为只有当前用户（与 SYSTEM）能访问。

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
    SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
    TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_TYPE, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_FLAGS, ACL, CONTAINER_INHERIT_ACE, CreateWellKnownSid,
    DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl, GetTokenInformation,
    INHERIT_ONLY_ACE, NO_INHERITANCE, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_MAX_SID_SIZE,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_GENERIC_WRITE, GetVolumeInformationByHandleW, WRITE_DAC,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, FILE_PERSISTENT_ACLS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_Contacts, FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads,
    FOLDERID_Favorites, FOLDERID_Links, FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow,
    FOLDERID_Music, FOLDERID_Objects3D, FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_ProgramData,
    FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, FOLDERID_Public, FOLDERID_RoamingAppData,
    FOLDERID_SavedGames, FOLDERID_SkyDrive, FOLDERID_UserProfiles, FOLDERID_Videos,
    FOLDERID_Windows, SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

/// 用户主目录中的已知文件夹（已知文件夹被重定向到别处时由 [`system_dirs`] 补上真实位置）。
pub(super) const HOME_FOLDERS: &[&str] = &[
    "Desktop",
    "Documents",
    "Downloads",
    "Music",
    "Pictures",
    "Videos",
    "OneDrive",
    "Favorites",
    "Links",
    "Contacts",
    "Saved Games",
    "Searches",
    "3D Objects",
    "AppData",
];

pub(super) fn system_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "PUBLIC",
        "ProgramData",
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ]
    .iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    .collect();
    let known: [GUID; 22] = [
        FOLDERID_Profile,
        FOLDERID_Desktop,
        FOLDERID_Documents,
        FOLDERID_Downloads,
        FOLDERID_Music,
        FOLDERID_Pictures,
        FOLDERID_Videos,
        FOLDERID_Favorites,
        FOLDERID_Links,
        FOLDERID_Contacts,
        FOLDERID_SavedGames,
        FOLDERID_Objects3D,
        FOLDERID_SkyDrive,
        FOLDERID_RoamingAppData,
        FOLDERID_LocalAppData,
        FOLDERID_LocalAppDataLow,
        FOLDERID_Public,
        FOLDERID_ProgramData,
        FOLDERID_Windows,
        FOLDERID_ProgramFiles,
        FOLDERID_ProgramFilesX86,
        FOLDERID_UserProfiles,
    ];
    dirs.extend(known.iter().filter_map(known_folder));
    dirs
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    let mut path = ptr::null_mut();
    // SAFETY: SHGetKnownFolderPath 写出一个以 0 结尾、需要 CoTaskMemFree 释放的字符串（失败时也要释放）
    unsafe {
        let hr = SHGetKnownFolderPath(id, 0, ptr::null_mut(), &mut path);
        let result = if hr >= 0 && !path.is_null() {
            let mut len = 0;
            while *path.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(path, len);
            Some(PathBuf::from(std::ffi::OsString::from_wide(slice)))
        } else {
            None
        };
        CoTaskMemFree(path as *const _);
        result
    }
}

/// 盘符根目录（`C:\`）或网络共享的根（`\\server\share`）。
pub(super) fn is_top_level(p: &Path) -> bool {
    p.parent().is_none()
}

fn wide(path: &Path) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain(Some(0)).collect()
}

/// 当前用户与 SYSTEM 的 SID。
struct PrivateSids {
    token_user: Vec<u8>,
    system: Vec<u8>,
}

impl PrivateSids {
    fn current() -> io::Result<Self> {
        // SAFETY: 标准的 Win32 调用序列；缓冲区大小由第一次调用返回
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0u32;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
            let mut token_user = vec![0u8; size as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                token_user.as_mut_ptr().cast(),
                size,
                &mut size,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut system = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
            let mut system_size = SECURITY_MAX_SID_SIZE;
            if CreateWellKnownSid(
                WinLocalSystemSid,
                ptr::null_mut(),
                system.as_mut_ptr().cast(),
                &mut system_size,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { token_user, system })
        }
    }

    fn user(&self) -> PSID {
        // SAFETY: token_user 是 GetTokenInformation(TokenUser) 写出的 TOKEN_USER
        unsafe { (*(self.token_user.as_ptr() as *const TOKEN_USER)).User.Sid }
    }

    fn system(&self) -> PSID {
        self.system.as_ptr() as PSID
    }
}

fn grant(sid: PSID, trustee_type: TRUSTEE_TYPE, inheritance: ACE_FLAGS) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: inheritance,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: trustee_type,
            ptstrName: sid as *mut u16,
        },
    }
}

/// 把 `entries` 设为 `path` 的受保护 DACL（不再继承上级目录的权限）。
fn set_protected_dacl(path: &Path, entries: &[EXPLICIT_ACCESS_W]) -> io::Result<()> {
    let name = wide(path);
    with_acl(entries, |acl| {
        // SAFETY: name 以 0 结尾，acl 在调用期间有效
        unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        }
    })
}

/// 由 `entries` 建一个 ACL 交给 `apply`（返回 Win32 错误码），之后释放。
fn with_acl(entries: &[EXPLICIT_ACCESS_W], apply: impl FnOnce(*mut ACL) -> u32) -> io::Result<()> {
    // SAFETY: SetEntriesInAclW 分配的 ACL 用 LocalFree 释放
    unsafe {
        let mut acl: *mut ACL = ptr::null_mut();
        let err = SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            ptr::null(),
            &mut acl,
        );
        if err != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let err = apply(acl);
        LocalFree(acl.cast());
        if err != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        Ok(())
    }
}

/// 文件系统是否支持持久的 ACL（FAT32、exFAT 不支持，这时什么也不做）。读不到卷信息时按支持处理。
fn persistent_acls(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return true;
    };
    persistent_acls_of(&file)
}

fn persistent_acls_of(file: &std::fs::File) -> bool {
    let mut flags = 0u32;
    // SAFETY: 只读取卷信息，句柄在 file 的生命周期内有效
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut flags,
            ptr::null_mut(),
            0,
        )
    };
    ok == 0 || flags & FILE_PERSISTENT_ACLS != 0
}

pub(super) fn restrict_dir(dir: &Path) -> io::Result<()> {
    if !persistent_acls(dir) {
        return Ok(());
    }
    let sids = PrivateSids::current()?;
    set_protected_dacl(
        dir,
        &[
            grant(
                sids.user(),
                TRUSTEE_IS_USER,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
            grant(
                sids.system(),
                TRUSTEE_IS_WELL_KNOWN_GROUP,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            ),
        ],
    )
}

pub(super) fn restrict_file(path: &Path) -> io::Result<()> {
    if !persistent_acls(path) {
        return Ok(());
    }
    let sids = PrivateSids::current()?;
    set_protected_dacl(path, &[grant(sids.user(), TRUSTEE_IS_USER, NO_INHERITANCE)])
}

/// 新建只有当前用户能读写的文件（已存在时失败）：创建时独占（其他程序打不开），在写入内容之前就设好 DACL。
pub(super) fn create_private_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .access_mode(FILE_GENERIC_WRITE | WRITE_DAC)
        .share_mode(0)
        .open(path)?;
    if persistent_acls_of(&file) {
        let sids = PrivateSids::current()?;
        let handle = file.as_raw_handle() as HANDLE;
        with_acl(
            &[grant(sids.user(), TRUSTEE_IS_USER, NO_INHERITANCE)],
            |acl| {
                // SAFETY: handle 在 file 的生命周期内有效且带 WRITE_DAC，acl 在调用期间有效
                unsafe {
                    SetSecurityInfo(
                        handle,
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        acl,
                        ptr::null(),
                    )
                }
            },
        )?;
    }
    Ok(file)
}

/// 文件的 DACL 是否受保护、只给当前用户（测试用）。
#[cfg(test)]
pub(super) fn is_private_file(path: &Path) -> bool {
    let Ok(sids) = PrivateSids::current() else {
        return false;
    };
    let name = wide(path);
    // SAFETY: 同 is_private_dir
    unsafe {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        if GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let mut control = 0u16;
        let mut revision = 0u32;
        let mut private = GetSecurityDescriptorControl(sd, &mut control, &mut revision) != 0
            && control & SE_DACL_PROTECTED != 0
            && !dacl.is_null()
            && (*dacl).AceCount == 1;
        if private {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            private = GetAce(dacl, 0, &mut ace) != 0 && {
                let ace = ace as *const ACCESS_ALLOWED_ACE;
                EqualSid(ptr::addr_of!((*ace).SidStart) as PSID, sids.user()) != 0
            };
        }
        LocalFree(sd);
        private
    }
}

pub(super) fn is_private_dir(dir: &Path) -> bool {
    if !persistent_acls(dir) {
        return true;
    }
    let Ok(sids) = PrivateSids::current() else {
        return false;
    };
    let name = wide(dir);
    // SAFETY: GetNamedSecurityInfoW 分配的安全描述符用 LocalFree 释放；dacl 指向其内部
    unsafe {
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let err = GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut sd,
        );
        if err != ERROR_SUCCESS {
            return false;
        }
        let private = dacl_is_private(sd, dacl, &sids);
        LocalFree(sd);
        private
    }
}

/// DACL 受保护，只有继承到子项的"允许"项，且只给当前用户（完全控制）与 SYSTEM。
unsafe fn dacl_is_private(sd: PSECURITY_DESCRIPTOR, dacl: *mut ACL, sids: &PrivateSids) -> bool {
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: sd 与 dacl 来自 GetNamedSecurityInfoW，调用方保证有效
    unsafe {
        if GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0
            || control & SE_DACL_PROTECTED == 0
            || dacl.is_null()
        {
            return false;
        }
        let inherit = (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8;
        let mut has_user = false;
        for i in 0..(*dacl).AceCount as u32 {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            if GetAce(dacl, i, &mut ace) == 0 {
                return false;
            }
            let ace = ace as *const ACCESS_ALLOWED_ACE;
            let header = (*ace).Header;
            if header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE
                || header.AceFlags & (inherit | INHERIT_ONLY_ACE as u8) != inherit
            {
                return false;
            }
            let sid = ptr::addr_of!((*ace).SidStart) as PSID;
            if EqualSid(sid, sids.user()) != 0 {
                has_user = (*ace).Mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS;
            } else if EqualSid(sid, sids.system()) == 0 {
                return false;
            }
        }
        has_user
    }
}
