use std::{
    ffi::{OsString, c_void},
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        io::OwnedHandle,
    },
    path::{Path, PathBuf},
    ptr, slice,
    sync::Arc,
};

use windows::{
    Win32::{
        Foundation::*,
        Globalization::{CSTR_EQUAL, CompareStringOrdinal},
        Security::{Authorization::*, *},
        Storage::FileSystem::*,
        System::{
            Com::CoTaskMemFree, Environment::*, Pipes::*, Registry::*, RemoteDesktop::*,
            SystemServices::*, Threading::*,
        },
        UI::{
            Shell::*,
            WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId},
        },
    },
    core::{GUID, PCWSTR, PWSTR},
};

use crate::win::{LocalAllocation, denied, is_error, os_error, own, raw, wide};

pub(crate) const CLIENT_ACCESS: u32 = 0x0010_0083;
const FILE_MAPPING: GENERIC_MAPPING = GENERIC_MAPPING {
    GenericRead: FILE_GENERIC_READ.0,
    GenericWrite: FILE_GENERIC_WRITE.0,
    GenericExecute: FILE_GENERIC_EXECUTE.0,
    GenericAll: FILE_ALL_ACCESS.0,
};
const MEDIUM: u32 = SECURITY_MANDATORY_MEDIUM_RID as u32;
const HIGH: u32 = SECURITY_MANDATORY_HIGH_RID as u32;
const ADMINS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
const TRUSTED_INSTALLER: &str = "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

struct Identity {
    user: String,
    logon: String,
    session: u32,
    integrity: u32,
    elevated: bool,
    administrator: bool,
}

impl Identity {
    fn read(token: HANDLE) -> io::Result<Self> {
        let app_container: u32 = token_value(token, TokenIsAppContainer)?;
        let app_container_sid: TOKEN_APPCONTAINER_INFORMATION =
            token_value(token, TokenAppContainerSid)?;
        let session = token_value(token, TokenSessionId)?;
        if app_container != 0
            || !app_container_sid.TokenAppContainer.0.is_null()
            || matches!(session, 0 | u32::MAX)
        {
            return Err(denied("token is not an interactive desktop identity"));
        }
        let user = token_buffer(token, TokenUser)?;
        let label = token_buffer(token, TokenIntegrityLevel)?;
        let groups = token_buffer(token, TokenGroups)?;
        let user = unsafe { sid_string((*user.as_ptr().cast::<TOKEN_USER>()).User.Sid)? };
        let label = unsafe { (*label.as_ptr().cast::<TOKEN_MANDATORY_LABEL>()).Label.Sid };
        let integrity = unsafe {
            if !IsValidSid(label).as_bool()
                || GetSidIdentifierAuthority(label).read().Value != [0, 0, 0, 0, 0, 16]
            {
                return Err(denied("invalid integrity SID"));
            }
            let count = *GetSidSubAuthorityCount(label);
            if count != 1 {
                return Err(denied("invalid integrity SID"));
            }
            *GetSidSubAuthority(label, 0)
        };
        let group_count = unsafe { *groups.as_ptr().cast::<u32>() } as usize;
        let offset = std::mem::offset_of!(TOKEN_GROUPS, Groups);
        if group_count > (size_of_val(groups.as_slice()) - offset) / size_of::<SID_AND_ATTRIBUTES>()
        {
            return Err(denied("invalid token groups"));
        }
        let mut logon = None;
        let mut administrator = false;
        for group in unsafe {
            slice::from_raw_parts(
                groups
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<SID_AND_ATTRIBUTES>(),
                group_count,
            )
        } {
            let flags = group.Attributes;
            let enabled = flags & SE_GROUP_ENABLED as u32 != 0
                && flags & SE_GROUP_USE_FOR_DENY_ONLY as u32 == 0;
            if enabled
                && flags & SE_GROUP_LOGON_ID as u32 == SE_GROUP_LOGON_ID as u32
                && logon.replace(sid_string(group.Sid)?).is_some()
            {
                return Err(denied("ambiguous token logon SID"));
            }
            if enabled
                && unsafe { IsWellKnownSid(group.Sid, WinBuiltinAdministratorsSid).as_bool() }
            {
                administrator = true;
            }
        }
        Ok(Self {
            user,
            logon: logon.ok_or_else(|| denied("missing token logon SID"))?,
            session,
            integrity,
            elevated: token_value::<u32>(token, TokenElevation)? != 0,
            administrator,
        })
    }

    fn medium(&self) -> bool {
        (MEDIUM..HIGH).contains(&self.integrity) && !self.elevated && !self.administrator
    }

    fn same_user_session(&self, other: &Self) -> bool {
        self.user == other.user && self.session == other.session
    }
}

fn token_value<T: Copy + Default>(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<T> {
    let mut value = T::default();
    let mut length = 0;
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some((&raw mut value).cast()),
            size_of::<T>() as u32,
            &mut length,
        )
    }
    .map_err(os_error)?;
    if length as usize != size_of::<T>() {
        return Err(denied("unexpected token information size"));
    }
    Ok(value)
}

fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> io::Result<Vec<usize>> {
    let mut length = 0;
    let error = unsafe { GetTokenInformation(token, class, None, 0, &mut length) }.err();
    if !error.is_some_and(|error| is_error(&error, ERROR_INSUFFICIENT_BUFFER))
        || !(8..=65536).contains(&length)
    {
        return Err(denied("unavailable token information"));
    }
    let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .map_err(os_error)?;
    Ok(buffer)
}

fn sid_string(sid: PSID) -> io::Result<String> {
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut text) }.map_err(os_error)?;
    let _allocation = LocalAllocation(text.0.cast());
    unsafe { text.to_string() }.map_err(io::Error::other)
}

fn process_token(process: HANDLE, access: TOKEN_ACCESS_MASK) -> io::Result<OwnedHandle> {
    let mut token = HANDLE::default();
    unsafe {
        OpenProcessToken(process, access, &mut token).map_err(os_error)?;
        Ok(own(token))
    }
}

pub(crate) struct Impersonation(std::marker::PhantomData<std::rc::Rc<()>>);

impl Impersonation {
    pub(crate) fn user(token: &OwnedHandle) -> io::Result<Self> {
        unsafe { ImpersonateLoggedOnUser(raw(token)) }.map_err(os_error)?;
        Ok(Self(std::marker::PhantomData))
    }

    fn pipe(handle: HANDLE) -> io::Result<Self> {
        unsafe { ImpersonateNamedPipeClient(handle) }.map_err(os_error)?;
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        if unsafe { RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}

pub(crate) struct Security {
    server: Identity,
    client: Identity,
    pub(crate) interactive_token: Arc<OwnedHandle>,
    server_token: Option<OwnedHandle>,
    readonly_endpoint: bool,
    test_peer: Option<(u32, OwnedHandle)>,
    #[cfg(test)]
    test_process: bool,
}

impl Security {
    pub(crate) fn production() -> io::Result<Self> {
        let token = process_token(
            unsafe { GetCurrentProcess() },
            TOKEN_QUERY | TOKEN_DUPLICATE,
        )?;
        let server = Identity::read(raw(&token))?;
        if !server.elevated || !server.administrator || !(HIGH..16384).contains(&server.integrity) {
            return Err(denied(
                "daemon requires the elevated interactive user's token",
            ));
        }
        let linked: TOKEN_LINKED_TOKEN = token_value(raw(&token), TokenLinkedToken)?;
        let linked = unsafe { own(linked.LinkedToken) };
        let client = Identity::read(raw(&linked))?;
        if !server.same_user_session(&client) || !client.medium() {
            return Err(denied("linked token is not the same interactive user"));
        }
        // Without SeTcbPrivilege, Windows can return an identification-only linked
        // token. Use the shell's primary token only after matching its login to it.
        let interactive = if token_value::<u32>(raw(&linked), TokenType)? == TokenPrimary.0 as u32 {
            linked
        } else {
            let shell = unsafe { GetShellWindow() };
            let mut pid = 0;
            if shell.is_invalid() || unsafe { GetWindowThreadProcessId(shell, Some(&mut pid)) } == 0
            {
                return Err(denied("interactive Windows shell is unavailable"));
            }
            let process = unsafe {
                own(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(os_error)?)
            };
            let token = process_token(raw(&process), TOKEN_QUERY | TOKEN_DUPLICATE)?;
            let desktop = Identity::read(raw(&token))?;
            if !client.same_user_session(&desktop)
                || client.logon != desktop.logon
                || !desktop.medium()
            {
                return Err(denied(
                    "shell token does not match the linked desktop login",
                ));
            }
            let mut primary = HANDLE::default();
            unsafe {
                DuplicateTokenEx(
                    raw(&token),
                    TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE,
                    None,
                    SecurityImpersonation,
                    TokenPrimary,
                    &mut primary,
                )
                .map_err(os_error)?;
                own(primary)
            }
        };
        Ok(Self {
            server,
            client,
            interactive_token: Arc::new(interactive),
            server_token: Some(token),
            readonly_endpoint: false,
            test_peer: None,
            #[cfg(test)]
            test_process: false,
        })
    }

    /// Only used for explicit no-tray, read-only, non-production endpoints.
    pub(crate) fn readonly_endpoint(client_pid: Option<u32>) -> io::Result<Self> {
        let token = process_token(
            unsafe { GetCurrentProcess() },
            TOKEN_QUERY | TOKEN_DUPLICATE,
        )?;
        let identity = Identity::read(raw(&token))?;
        let mut security = if identity.medium() {
            Self {
                server: Identity::read(raw(&token))?,
                client: identity,
                interactive_token: Arc::new(token),
                server_token: None,
                readonly_endpoint: true,
                test_peer: None,
                #[cfg(test)]
                test_process: false,
            }
        } else {
            let mut security = Self::production()?;
            security.readonly_endpoint = true;
            security.server_token = None;
            security
        };
        if let Some(pid) = client_pid {
            let process = unsafe {
                own(OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    pid,
                )
                .map_err(os_error)?)
            };
            let token = process_token(raw(&process), TOKEN_QUERY)?;
            let peer = Identity::read(raw(&token))?;
            if !security.server.same_user_session(&peer)
                || peer.logon != security.server.logon
                || peer.integrity != security.server.integrity
                || unsafe { WaitForSingleObject(raw(&process), 0) } != WAIT_TIMEOUT
            {
                return Err(denied(
                    "diagnostic peer must be live in the same login and integrity",
                ));
            }
            security.test_peer = Some((pid, process));
        }
        Ok(security)
    }

    #[cfg(test)]
    pub(crate) fn isolated() -> io::Result<Self> {
        let token = process_token(
            unsafe { GetCurrentProcess() },
            TOKEN_QUERY | TOKEN_DUPLICATE,
        )?;
        Ok(Self {
            server: Identity::read(raw(&token))?,
            client: Identity::read(raw(&token))?,
            interactive_token: Arc::new(token),
            server_token: None,
            readonly_endpoint: false,
            test_peer: None,
            test_process: true,
        })
    }

    fn elevated_ace(&self) -> String {
        format!(
            "(XA;;GA;;;{};(Member_of {{SID(BA),SID({})}}))",
            self.server.user, self.server.logon
        )
    }

    pub(crate) fn pipe_descriptor(&self) -> io::Result<SecurityDescriptor> {
        if self.readonly_endpoint {
            return SecurityDescriptor::from_sddl(&format!(
                "O:{}D:P(A;;GA;;;{})S:(ML;;NWNR;;;ME)",
                self.server.user, self.server.logon
            ));
        }
        #[cfg(test)]
        if self.test_process {
            return SecurityDescriptor::from_sddl(&format!(
                "O:{}D:P(A;;GA;;;{})S:(ML;;NWNR;;;ME)",
                self.server.user, self.server.user
            ));
        }
        self.production_descriptor(false)
    }

    fn production_descriptor(&self, add_group: bool) -> io::Result<SecurityDescriptor> {
        let group = if add_group { "G:BA" } else { "" };
        SecurityDescriptor::from_sddl(&format!(
            "O:BA{group}D:P(A;;0x{CLIENT_ACCESS:08X};;;{}){}S:(ML;;NWNR;;;ME)",
            self.client.logon,
            self.elevated_ace()
        ))
    }

    pub(crate) fn verify_pipe(
        &self,
        descriptor: &SecurityDescriptor,
        handle: HANDLE,
        first: bool,
    ) -> io::Result<()> {
        let actual = descriptor.verify_assigned(handle)?;
        if first {
            if let Some(server) = &self.server_token {
                let ordinary = actual.maximum_access(&self.interactive_token)?;
                let elevated = actual.maximum_access(server)?;
                if ordinary != CLIENT_ACCESS || elevated & FILE_ALL_ACCESS.0 != FILE_ALL_ACCESS.0 {
                    return Err(denied(
                        "named-pipe access check differs from client/server policy",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn session_id(&self) -> u32 {
        self.client.session
    }

    pub(crate) fn session_unlocked(&self) -> io::Result<bool> {
        let mut info = PWSTR::null();
        let mut bytes = 0;
        unsafe {
            WTSQuerySessionInformationW(
                None,
                self.client.session,
                WTSSessionInfoEx,
                &mut info,
                &mut bytes,
            )
        }
        .map_err(os_error)?;
        let result = if !info.is_null() && bytes as usize >= size_of::<WTSINFOEXW>() {
            let info = unsafe { info.0.cast::<WTSINFOEXW>().read_unaligned() };
            if info.Level == 1 {
                let session = unsafe { info.Data.WTSInfoExLevel1 };
                if session.SessionId == self.client.session && session.SessionState == WTSActive {
                    Ok(session.SessionFlags as u32 == WTS_SESSIONSTATE_UNLOCK)
                } else {
                    Err(denied("the daemon's interactive session is not active"))
                }
            } else {
                Err(denied("unrecognized Windows session state"))
            }
        } else {
            Err(denied("missing Windows session state"))
        };
        unsafe {
            WTSFreeMemory(info.0.cast());
        }
        result
    }

    pub(crate) fn instance_mutex(&self) -> io::Result<OwnedHandle> {
        let descriptor = SecurityDescriptor::from_sddl(&format!(
            "O:BAD:P{}S:(ML;;NWNR;;;ME)",
            self.elevated_ace()
        ))?;
        let attrs = descriptor.attributes();
        let name = wide(format!(r"Local\MSIToolkit-Daemon-v1-{}", self.server.user));
        unsafe {
            SetLastError(ERROR_SUCCESS);
            let handle =
                CreateMutexW(Some(&attrs), false, PCWSTR(name.as_ptr())).map_err(os_error)?;
            let duplicate = GetLastError() == ERROR_ALREADY_EXISTS;
            let handle = own(handle);
            if duplicate {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "daemon already running in this session",
                ));
            }
            Ok(handle)
        }
    }

    /// Returns whether the proven peer is eligible for UI-only telemetry.
    pub(crate) fn authenticate(&self, pipe: HANDLE, wants_ui: bool) -> io::Result<bool> {
        let mut session = 0;
        unsafe { GetNamedPipeClientSessionId(pipe, &mut session) }.map_err(os_error)?;
        let peer = {
            let _impersonation = Impersonation::pipe(pipe)?;
            let mut token = HANDLE::default();
            unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) }
                .map_err(os_error)?;
            let token = unsafe { own(token) };
            let level: u32 = token_value(raw(&token), TokenImpersonationLevel)?;
            if level < SecurityIdentification.0 as u32 {
                return Err(denied("anonymous named-pipe peer"));
            }
            Identity::read(raw(&token))?
        };
        if !self.client.same_user_session(&peer) || peer.session != session {
            return Err(denied("named-pipe peer belongs to another user or session"));
        }
        if self.readonly_endpoint {
            if peer.logon != self.server.logon || peer.integrity != self.server.integrity {
                return Err(denied(
                    "diagnostic peer must use the same login and integrity",
                ));
            }
            return if let Some((expected, process)) = &self.test_peer {
                let mut pid = 0;
                unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.map_err(os_error)?;
                if pid != *expected
                    || unsafe { WaitForSingleObject(raw(process), 0) } != WAIT_TIMEOUT
                {
                    Err(denied("peer is outside the pinned diagnostic process"))
                } else {
                    Ok(wants_ui)
                }
            } else {
                Ok(false)
            };
        }
        #[cfg(test)]
        if self.test_process {
            let mut pid = 0;
            unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.map_err(os_error)?;
            return if peer.logon == self.client.logon
                && pid == std::process::id()
                && peer.integrity >= MEDIUM
            {
                Ok(wants_ui)
            } else {
                Err(denied("outside the isolated test process"))
            };
        }
        let medium = peer.medium() && peer.logon == self.client.logon;
        let elevated = peer.elevated
            && peer.administrator
            && (HIGH..16384).contains(&peer.integrity)
            && peer.logon == self.server.logon;
        if !medium && !elevated {
            return Err(denied(
                "named-pipe peer has an unexpected login or integrity level",
            ));
        }
        Ok(wants_ui && medium && self.verify_ui_process(pipe, &peer).is_ok())
    }

    fn verify_ui_process(&self, pipe: HANDLE, peer: &Identity) -> io::Result<()> {
        let mut pid = 0;
        unsafe { GetNamedPipeClientProcessId(pipe, &mut pid) }.map_err(os_error)?;
        let process = unsafe {
            own(OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .map_err(os_error)?)
        };
        let token = process_token(raw(&process), TOKEN_QUERY)?;
        let identity = Identity::read(raw(&token))?;
        if !identity.medium() || !identity.same_user_session(peer) || identity.logon != peer.logon {
            return Err(denied("UI process token does not match the connection"));
        }
        let mut image = vec![0u16; 32768];
        let mut len = image.len() as u32;
        unsafe {
            QueryFullProcessImageNameW(
                raw(&process),
                PROCESS_NAME_WIN32,
                PWSTR(image.as_mut_ptr()),
                &mut len,
            )
        }
        .map_err(os_error)?;
        let image = PathBuf::from(OsString::from_wide(&image[..len as usize]));
        let install = ProtectedInstall::open()?;
        let _image = install.image(&image, "msilt-ui.exe")?;
        let mut current_pid = 0;
        unsafe { GetNamedPipeClientProcessId(pipe, &mut current_pid) }.map_err(os_error)?;
        if current_pid != pid || unsafe { WaitForSingleObject(raw(&process), 0) } != WAIT_TIMEOUT {
            return Err(denied("UI process is no longer the connected client"));
        }
        Ok(())
    }
}

pub(crate) struct SecurityDescriptor(LocalAllocation);

impl SecurityDescriptor {
    fn from_sddl(sddl: &str) -> io::Result<Self> {
        let wide = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(os_error)?;
        Ok(Self(LocalAllocation(descriptor.0)))
    }

    fn query(
        handle: HANDLE,
        object: SE_OBJECT_TYPE,
        info: OBJECT_SECURITY_INFORMATION,
    ) -> io::Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            GetSecurityInfo(
                handle,
                object,
                info,
                None,
                None,
                None,
                None,
                Some(&mut descriptor),
            )
        }
        .ok()
        .map_err(os_error)?;
        Ok(Self(LocalAllocation(descriptor.0)))
    }

    fn raw(&self) -> PSECURITY_DESCRIPTOR {
        PSECURITY_DESCRIPTOR(self.0.0)
    }

    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }

    fn owner(&self) -> io::Result<String> {
        let mut owner = PSID::default();
        let mut defaulted = windows::core::BOOL::default();
        unsafe { GetSecurityDescriptorOwner(self.raw(), &mut owner, &mut defaulted) }
            .map_err(os_error)?;
        if owner.0.is_null() {
            return Err(denied("object has no explicit owner"));
        }
        sid_string(owner)
    }

    fn protected(&self) -> io::Result<bool> {
        let (mut control, mut revision) = (0, 0);
        unsafe { GetSecurityDescriptorControl(self.raw(), &mut control, &mut revision) }
            .map_err(os_error)?;
        Ok(control & SE_DACL_PROTECTED.0 != 0)
    }

    fn acl(&self, label: bool) -> io::Result<*const ACL> {
        let mut present = windows::core::BOOL::default();
        let mut defaulted = windows::core::BOOL::default();
        let mut acl = ptr::null_mut();
        let getter = if label {
            GetSecurityDescriptorSacl
        } else {
            GetSecurityDescriptorDacl
        };
        unsafe { getter(self.raw(), &mut present, &mut acl, &mut defaulted) }.map_err(os_error)?;
        if !present.as_bool() || acl.is_null() || !unsafe { IsValidAcl(acl).as_bool() } {
            return Err(denied("missing or invalid explicit ACL"));
        }
        Ok(acl)
    }

    fn verify_assigned(&self, handle: HANDLE) -> io::Result<Self> {
        let actual = Self::query(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | LABEL_SECURITY_INFORMATION,
        )?;
        if !actual.protected()? || self.owner()? != actual.owner()? {
            return Err(denied(
                "named-pipe owner or DACL protection differs from policy",
            ));
        }
        for label in [false, true] {
            let expected_acl = self.acl(label)?;
            let actual_acl = actual.acl(label)?;
            // The I/O manager may expand generic access bits when it assigns the DACL.
            unsafe {
                if (*expected_acl).AceCount != (*actual_acl).AceCount {
                    return Err(denied("named-pipe ACE count differs from policy"));
                }
                for index in 0..u32::from((*expected_acl).AceCount) {
                    let expected = ace(expected_acl, index)?;
                    let actual = ace(actual_acl, index)?;
                    if expected[..4] != actual[..4]
                        || expected[8..] != actual[8..]
                        || mapped_mask(expected, label) != mapped_mask(actual, label)
                    {
                        return Err(denied("named-pipe ACE differs from policy"));
                    }
                }
            }
        }
        Ok(actual)
    }

    fn maximum_access(&self, primary: &OwnedHandle) -> io::Result<u32> {
        let mut token = HANDLE::default();
        unsafe { DuplicateToken(raw(primary), SecurityIdentification, &mut token) }
            .map_err(os_error)?;
        let token = unsafe { own(token) };
        let mut privileges = [0usize; 16];
        let mut bytes = size_of_val(&privileges) as u32;
        let mut granted = 0;
        let mut allowed = windows::core::BOOL::default();
        unsafe {
            AccessCheck(
                self.raw(),
                raw(&token),
                MAXIMUM_ALLOWED,
                &FILE_MAPPING,
                Some(privileges.as_mut_ptr().cast()),
                &mut bytes,
                &mut granted,
                &mut allowed,
            )
        }
        .map_err(os_error)?;
        Ok(if allowed.as_bool() { granted } else { 0 })
    }
}

unsafe fn ace<'a>(acl: *const ACL, index: u32) -> io::Result<&'a [u8]> {
    let mut ptr: *mut c_void = ptr::null_mut();
    unsafe {
        GetAce(acl, index, &mut ptr).map_err(os_error)?;
        let size = (*ptr.cast::<ACE_HEADER>()).AceSize as usize;
        if size < 8 {
            return Err(denied("unexpected ACE size"));
        }
        Ok(slice::from_raw_parts(ptr.cast::<u8>(), size))
    }
}

fn mapped_mask(ace: &[u8], label: bool) -> u32 {
    let mut mask = u32::from_le_bytes([ace[4], ace[5], ace[6], ace[7]]);
    if !label {
        unsafe {
            MapGenericMask(&mut mask, &FILE_MAPPING);
        }
    }
    mask
}

pub(crate) fn known_folder(id: &GUID, token: Option<HANDLE>) -> io::Result<PathBuf> {
    let text =
        unsafe { SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), token) }.map_err(os_error)?;
    struct CoPath(PWSTR);
    impl Drop for CoPath {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0.0.cast()));
            }
        }
    }
    let text = CoPath(text);
    Ok(PathBuf::from(OsString::from_wide(unsafe {
        text.0.as_wide()
    })))
}

struct ProtectedInstall {
    directory: PathBuf,
    _ancestors: Vec<OwnedHandle>,
    _directory: OwnedHandle,
}

impl ProtectedInstall {
    fn open() -> io::Result<Self> {
        let directory = configured_install_dir()?;
        let ancestors = lock_non_reparse_ancestors(&directory)?;
        let dir_handle = protected_object(&directory, true)?;
        Ok(Self {
            _ancestors: ancestors,
            _directory: dir_handle,
            directory,
        })
    }

    fn image(&self, actual: &Path, name: &str) -> io::Result<OwnedHandle> {
        let expected = self.directory.join(name);
        // The installed path is read from admin-only HKLM / OS folders, not inferred from the peer.
        if !path_equal(actual, &expected) {
            return Err(denied(
                "image is outside the protected MSI-Toolkit installation",
            ));
        }
        protected_object(&expected, false)
    }
}

fn configured_install_dir() -> io::Result<PathBuf> {
    for (subkey, value) in [
        (r"SOFTWARE\MSI-Toolkit", "InstallDir"),
        (
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit_is1",
            "InstallLocation",
        ),
        (
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\MSI-Toolkit",
            "InstallLocation",
        ),
    ] {
        if let Some(raw) = read_hklm_string(subkey, value) {
            let trimmed = raw.trim().trim_end_matches(['\\', '/']);
            if let Some(valid) = validate_local_install_dir(Path::new(trimmed)) {
                return Ok(valid);
            }
        }
    }
    Ok(known_folder(&FOLDERID_ProgramFiles, None)?.join("MSI-Toolkit"))
}

fn read_hklm_string(subkey: &str, value: &str) -> Option<String> {
    let subkey_w = wide(subkey);
    let value_w = wide(value);
    let mut bytes = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS || !(2..=65_536).contains(&bytes) {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let text = String::from_utf16_lossy(&buf[..len]);
    (!text.is_empty()).then_some(text)
}

fn validate_local_install_dir(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    let mut comps = path.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return None;
    };
    if !matches!(prefix.kind(), Prefix::Disk(_)) || comps.next() != Some(Component::RootDir) {
        return None;
    }
    let mut depth = 0usize;
    for part in comps {
        match part {
            Component::Normal(name) => {
                let text = name.to_str()?;
                if text.is_empty()
                    || text.ends_with([' ', '.'])
                    || text.contains([':', '"', '/', '\\'])
                    || text.chars().any(char::is_control)
                {
                    return None;
                }
                depth += 1;
            }
            _ => return None,
        }
    }
    (depth >= 1).then(|| path.to_path_buf())
}

fn lock_non_reparse_ancestors(directory: &Path) -> io::Result<Vec<OwnedHandle>> {
    use std::path::Component;
    let mut comps = directory.components();
    let Some(Component::Prefix(prefix)) = comps.next() else {
        return Err(denied("installation path must be an absolute local path"));
    };
    if comps.next() != Some(Component::RootDir) {
        return Err(denied("installation path must have a root directory"));
    }
    let mut current = PathBuf::from(prefix.as_os_str());
    current.push(r"\");
    let mut handles = Vec::new();
    let mut rest = comps.peekable();
    while let Some(Component::Normal(part)) = rest.next() {
        if rest.peek().is_none() {
            break;
        }
        current.push(part);
        handles.push(open_non_reparse_dir(&current)?);
    }
    Ok(handles)
}

fn open_non_reparse_dir(path: &Path) -> io::Result<OwnedHandle> {
    let name = wide(path);
    let handle = unsafe {
        own(CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .map_err(os_error)?)
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            raw(&handle),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
        )
    }
    .map_err(os_error)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
    {
        return Err(denied(
            "installation ancestor contains a reparse point or non-directory",
        ));
    }
    Ok(handle)
}

fn path_equal(a: &Path, b: &Path) -> bool {
    let (a, b): (Vec<u16>, Vec<u16>) = (
        a.as_os_str().encode_wide().collect(),
        b.as_os_str().encode_wide().collect(),
    );
    // CompareStringOrdinal uses the same invariant case folding as NTFS path lookup.
    unsafe { CompareStringOrdinal(a.as_slice(), b.as_slice(), true) == CSTR_EQUAL }
}

fn trusted_owner(sid: &str) -> bool {
    matches!(sid, ADMINS | SYSTEM | TRUSTED_INSTALLER)
}

fn protected_object(path: &Path, directory: bool) -> io::Result<OwnedHandle> {
    let name = wide(path);
    let handle = unsafe {
        own(CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .map_err(os_error)?)
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            raw(&handle),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            size_of_val(&info) as u32,
        )
    }
    .map_err(os_error)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
        || (info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0) != directory
    {
        return Err(denied(
            "installation path contains a reparse point or unexpected object",
        ));
    }
    let sd = SecurityDescriptor::query(
        raw(&handle),
        SE_FILE_OBJECT,
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
    )?;
    if !trusted_owner(&sd.owner()?) {
        return Err(denied("installation object has an untrusted owner"));
    }
    let acl = sd.acl(false)?;
    let mutation = FILE_WRITE_DATA.0
        | FILE_APPEND_DATA.0
        | FILE_WRITE_ATTRIBUTES.0
        | FILE_WRITE_EA.0
        | FILE_DELETE_CHILD.0
        | DELETE.0
        | WRITE_DAC.0
        | WRITE_OWNER.0;
    for index in 0..u32::from(unsafe { (*acl).AceCount }) {
        let entry = unsafe { ace(acl, index)? };
        if entry[1] & INHERIT_ONLY_ACE.0 as u8 != 0 || mapped_mask(entry, false) & mutation == 0 {
            continue;
        }
        match u32::from(entry[0]) {
            ACCESS_DENIED_ACE_TYPE
            | ACCESS_DENIED_OBJECT_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_ACE_TYPE
            | ACCESS_DENIED_CALLBACK_OBJECT_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE | ACCESS_ALLOWED_CALLBACK_ACE_TYPE
                if entry.len() >= 16
                    && trusted_owner(&sid_string(PSID(entry[8..].as_ptr().cast_mut().cast()))?) => {
            }
            _ => {
                return Err(denied(
                    "installation object permits modification by an untrusted principal",
                ));
            }
        }
    }
    Ok(handle)
}

pub(crate) fn launch_ui(token: &OwnedHandle) -> io::Result<()> {
    let install = ProtectedInstall::open()?;
    let _daemon = install.image(&std::env::current_exe()?, "msilt-daemon.exe")?;
    let ui = install.directory.join("msilt-ui.exe");
    let _ui = install.image(&ui, "msilt-ui.exe")?;
    let exe = wide(&ui);
    let dir = wide(&install.directory);
    let mut environment = ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut environment, Some(raw(token)), false) }
        .map_err(os_error)?;
    struct Environment(*mut c_void);
    impl Drop for Environment {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyEnvironmentBlock(self.0);
            }
        }
    }
    let environment = Environment(environment);
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessWithTokenW(
            raw(token),
            CREATE_PROCESS_LOGON_FLAGS(0),
            PCWSTR(exe.as_ptr()),
            None,
            CREATE_UNICODE_ENVIRONMENT,
            Some(environment.0),
            PCWSTR(dir.as_ptr()),
            &startup,
            &mut process,
        )
        .map_err(os_error)?;
        drop(own(process.hThread));
        drop(own(process.hProcess));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_access_check_confirms_the_medium_initiator_mask() {
        let security = if Identity::read(raw(&process_token(
            unsafe { GetCurrentProcess() },
            TOKEN_QUERY,
        )
        .unwrap()))
        .unwrap()
        .elevated
        {
            Security::production().unwrap()
        } else {
            Security::isolated().unwrap()
        };
        let sd = security.production_descriptor(true).unwrap();
        assert_eq!(sd.owner().unwrap(), ADMINS);
        assert!(sd.protected().unwrap());
        let dacl = sd.acl(false).unwrap();
        let label = sd.acl(true).unwrap();
        unsafe {
            assert_eq!((*dacl).AceCount, 2);
            assert_eq!(mapped_mask(ace(dacl, 0).unwrap(), false), CLIENT_ACCESS);
            assert_eq!(
                u32::from(ace(dacl, 1).unwrap()[0]),
                ACCESS_ALLOWED_CALLBACK_ACE_TYPE
            );
            assert_eq!((*label).AceCount, 1);
            let entry = ace(label, 0).unwrap();
            assert_eq!(u32::from(entry[0]), SYSTEM_MANDATORY_LABEL_ACE_TYPE);
            assert_eq!(
                mapped_mask(entry, true),
                SYSTEM_MANDATORY_LABEL_NO_READ_UP | SYSTEM_MANDATORY_LABEL_NO_WRITE_UP
            );
            assert_eq!(
                sid_string(PSID(entry[8..].as_ptr().cast_mut().cast())).unwrap(),
                "S-1-16-8192"
            );
        }
        assert_eq!(
            sd.maximum_access(&security.interactive_token).unwrap(),
            CLIENT_ACCESS
        );
        assert_eq!(CLIENT_ACCESS & FILE_APPEND_DATA.0, 0);
    }
}
