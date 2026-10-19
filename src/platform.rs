//! Small Windows helpers: elevation, HKLM registry (64-bit view), key permissions,
//! HTTPS download, and process control.
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsStr, io::Write, os::windows::{ffi::OsStrExt, process::CommandExt}, ptr::{null, null_mut}};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HLOCAL, LocalFree, WAIT_OBJECT_0},
    Networking::WinHttp::*,
    Security::{
        ACL, AdjustTokenPrivileges, CreateWellKnownSid, DACL_SECURITY_INFORMATION, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_PRIVILEGE_ENABLED, SECURITY_MAX_SID_SIZE, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, WinBuiltinAdministratorsSid,
        Authorization::{EXPLICIT_ACCESS_W, GetNamedSecurityInfoW, SE_REGISTRY_WOW64_64KEY, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
            TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W},
    },
    System::{
        Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS},
        Registry::*,
        Threading::{GetCurrentProcess, GetExitCodeProcess, INFINITE, OpenProcess, OpenProcessToken, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject},
    },
    UI::{Shell::{IsUserAnAdmin, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::SW_HIDE},
};

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> { s.as_ref().encode_wide().chain(Some(0)).collect() }

pub fn is_elevated() -> bool {
    // SAFETY: IsUserAnAdmin takes no arguments and only queries the current process token.
    unsafe { IsUserAnAdmin() != 0 }
}

/// Quotes one argument following the MSVC command-line parsing rules.
pub fn quote_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => { out.extend(std::iter::repeat_n('\\', slashes * 2 + 1)); slashes = 0; }
            _ => slashes = 0,
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', slashes));
    out.push('"');
    out
}

/// Runs this executable elevated with `args`, waits for it, and returns its exit code.
pub fn run_self_elevated(args: &[String]) -> Result<u32> {
    let exe = std::env::current_exe().context("Could not locate RoomTune.exe")?;
    let verb = wide("runas");
    let file = wide(&exe);
    let params = wide(args.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" "));
    // SAFETY: SHELLEXECUTEINFOW is a plain C struct; all-zero is its documented empty state.
    let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    info.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = params.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: every string pointer refers to a live, NUL-terminated buffer for the duration of the call.
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) { bail!("Administrator permission was declined. Nothing was changed"); }
        bail!("Could not start the elevated helper: {error}");
    }
    ensure!(!info.hProcess.is_null(), "The elevated helper did not report a process");
    let mut code = 1;
    // SAFETY: hProcess is a valid process handle owned by us (SEE_MASK_NOCLOSEPROCESS) and closed exactly once.
    unsafe {
        let waited = WaitForSingleObject(info.hProcess, INFINITE);
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        CloseHandle(info.hProcess);
        ensure!(waited == WAIT_OBJECT_0 && got != 0, "Lost track of the elevated helper");
    }
    Ok(code)
}

/// Reads a REG_SZ value from the 64-bit HKLM view.
pub fn registry_string(subkey: &str, value: &str) -> Option<String> {
    let (subkey, value) = (wide(subkey), wide(value));
    let mut buffer = vec![0u16; 2048];
    let mut bytes = (buffer.len() * 2) as u32;
    // SAFETY: the output buffer is `bytes` long and the key/value names are NUL-terminated.
    let status = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, subkey.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
            null_mut(), buffer.as_mut_ptr().cast(), &mut bytes)
    };
    if status != ERROR_SUCCESS { return None; }
    buffer.truncate(bytes as usize / 2);
    while buffer.last() == Some(&0) { buffer.pop(); }
    Some(String::from_utf16_lossy(&buffer))
}

/// An open HKLM key in the 64-bit registry view, closed on drop.
pub struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegOpenKeyExW/RegCreateKeyExW and is closed exactly once.
        unsafe { RegCloseKey(self.0); }
    }
}

fn reg_error(status: u32, what: &str, path: &str) -> anyhow::Error {
    anyhow::Error::new(std::io::Error::from_raw_os_error(status as i32)).context(format!("Registry {what} failed for HKLM\\{path}"))
}

impl Key {
    pub fn exists(path: &str) -> bool { Self::open(path, false).is_ok() }

    pub fn open(path: &str, write: bool) -> Result<Self> {
        let mut key = null_mut();
        let access = KEY_WOW64_64KEY | if write { KEY_READ | KEY_SET_VALUE } else { KEY_READ };
        // SAFETY: the path is NUL-terminated and `key` receives a handle we own.
        let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(path).as_ptr(), 0, access, &mut key) };
        if status != ERROR_SUCCESS { return Err(reg_error(status, "open", path)); }
        Ok(Self(key))
    }

    pub fn create(path: &str) -> Result<Self> {
        let mut key = null_mut();
        // SAFETY: the path is NUL-terminated; optional class, security, and disposition pointers are null.
        let status = unsafe {
            RegCreateKeyExW(HKEY_LOCAL_MACHINE, wide(path).as_ptr(), 0, null(), REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY, null(), &mut key, null_mut())
        };
        if status != ERROR_SUCCESS { return Err(reg_error(status, "create", path)); }
        Ok(Self(key))
    }

    pub fn has(&self, name: &str) -> bool {
        // SAFETY: querying only the existence of a value; all output pointers are null.
        unsafe { RegQueryValueExW(self.0, wide(name).as_ptr(), null(), null_mut(), null_mut(), null_mut()) == ERROR_SUCCESS }
    }

    pub fn string(&self, name: &str) -> Option<String> {
        let mut buffer = vec![0u16; 1024];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: the buffer is `bytes` long; RRF_RT_REG_SZ guarantees string data.
        let status = unsafe { RegGetValueW(self.0, null(), wide(name).as_ptr(), RRF_RT_REG_SZ, null_mut(), buffer.as_mut_ptr().cast(), &mut bytes) };
        if status != ERROR_SUCCESS { return None; }
        buffer.truncate(bytes as usize / 2);
        while buffer.last() == Some(&0) { buffer.pop(); }
        Some(String::from_utf16_lossy(&buffer))
    }

    fn set(&self, name: &str, kind: REG_VALUE_TYPE, data: &[u16]) -> Result<()> {
        // SAFETY: `data` is a valid UTF-16 buffer whose byte length is passed.
        let status = unsafe { RegSetValueExW(self.0, wide(name).as_ptr(), 0, kind, data.as_ptr().cast(), (data.len() * 2) as u32) };
        if status != ERROR_SUCCESS { return Err(reg_error(status, "write", name)); }
        Ok(())
    }

    pub fn set_string(&self, name: &str, value: &str) -> Result<()> { self.set(name, REG_SZ, &wide(value)) }

    pub fn set_multi(&self, name: &str, value: &str) -> Result<()> {
        let mut data = wide(value); data.push(0);
        self.set(name, REG_MULTI_SZ, &data)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        // SAFETY: the value name is NUL-terminated.
        let status = unsafe { RegDeleteValueW(self.0, wide(name).as_ptr()) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND { return Err(reg_error(status, "delete", name)); }
        Ok(())
    }
}

pub fn is_access_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io|
        io.kind() == std::io::ErrorKind::PermissionDenied || io.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)))
}

fn enable_privilege(name: &str) -> Result<()> {
    let mut token = null_mut();
    // SAFETY: standard token adjustment; the token handle is closed below.
    unsafe {
        ensure!(OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token) != 0, "Could not open process token");
        let mut privileges = TOKEN_PRIVILEGES { PrivilegeCount: 1, Privileges: [LUID_AND_ATTRIBUTES { Luid: std::mem::zeroed(), Attributes: SE_PRIVILEGE_ENABLED }] };
        let ok = LookupPrivilegeValueW(null(), wide(name).as_ptr(), &mut privileges.Privileges[0].Luid) != 0
            && AdjustTokenPrivileges(token, 0, &privileges, 0, null_mut(), null_mut()) != 0;
        CloseHandle(token);
        ensure!(ok, "Could not enable {name}");
    }
    Ok(())
}

/// Makes the built-in Administrators group owner of an HKLM key and grants it full control,
/// as Equalizer APO's Device Selector does for protected endpoint keys. Requires elevation.
pub fn make_key_writable(path: &str) -> Result<()> {
    enable_privilege("SeTakeOwnershipPrivilege")?;
    let mut sid = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut sid_len = sid.len() as u32;
    let name = wide(format!(r"MACHINE\{path}"));
    // SAFETY: `sid` is large enough for any SID; the name is NUL-terminated; buffers returned by
    // GetNamedSecurityInfoW/SetEntriesInAclW are released with LocalFree.
    unsafe {
        ensure!(CreateWellKnownSid(WinBuiltinAdministratorsSid, null_mut(), sid.as_mut_ptr().cast(), &mut sid_len) != 0, "Could not build Administrators SID");
        let psid = sid.as_mut_ptr().cast();
        let status = SetNamedSecurityInfoW(name.as_ptr(), SE_REGISTRY_WOW64_64KEY, OWNER_SECURITY_INFORMATION, psid, null_mut(), null(), null());
        ensure!(status == ERROR_SUCCESS, reg_error(status, "take ownership", path));
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let status = GetNamedSecurityInfoW(name.as_ptr(), SE_REGISTRY_WOW64_64KEY, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), &mut dacl, null_mut(), &mut descriptor);
        ensure!(status == ERROR_SUCCESS, reg_error(status, "read permissions", path));
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: KEY_ALL_ACCESS, grfAccessMode: SET_ACCESS, grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
            Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: 0, TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_GROUP, ptstrName: psid as _ },
        };
        let mut new_dacl: *mut ACL = null_mut();
        let status = SetEntriesInAclW(1, &access, dacl, &mut new_dacl);
        LocalFree(descriptor as HLOCAL);
        ensure!(status == ERROR_SUCCESS, reg_error(status, "build permissions", path));
        let status = SetNamedSecurityInfoW(name.as_ptr(), SE_REGISTRY_WOW64_64KEY, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), new_dacl, null());
        LocalFree(new_dacl as HLOCAL);
        ensure!(status == ERROR_SUCCESS, reg_error(status, "write permissions", path));
    }
    Ok(())
}

struct Internet(*mut core::ffi::c_void);

impl Drop for Internet {
    fn drop(&mut self) {
        // SAFETY: a non-null WinHTTP handle we own, closed once.
        if !self.0.is_null() { unsafe { WinHttpCloseHandle(self.0); } }
    }
}

fn http_error(what: &str) -> anyhow::Error { anyhow::anyhow!("Download failed ({what}): {}", std::io::Error::last_os_error()) }

/// Downloads `https://{host}{path}` (following redirects) into `out`, reporting progress 0..1.
pub fn download(host: &str, path: &str, out: &mut impl Write, max_bytes: u64, progress: &dyn Fn(f32), cancelled: &dyn Fn() -> bool) -> Result<u64> {
    // SAFETY: all strings are NUL-terminated and outlive the calls; handles are owned by `Internet` guards.
    unsafe {
        let session = Internet(WinHttpOpen(wide("RoomTune").as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        ensure!(!session.0.is_null(), http_error("session"));
        let connection = Internet(WinHttpConnect(session.0, wide(host).as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0));
        ensure!(!connection.0.is_null(), http_error("connect"));
        let request = Internet(WinHttpOpenRequest(connection.0, wide("GET").as_ptr(), wide(path).as_ptr(), null(), null(), null(), WINHTTP_FLAG_SECURE));
        ensure!(!request.0.is_null(), http_error("request"));
        ensure!(WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) != 0, http_error("send"));
        ensure!(WinHttpReceiveResponse(request.0, null_mut()) != 0, http_error("response"));
        let header = |query: u32| -> Option<u32> {
            let (mut value, mut size) = (0u32, 4u32);
            (WinHttpQueryHeaders(request.0, query | WINHTTP_QUERY_FLAG_NUMBER, null(), (&mut value as *mut u32).cast(), &mut size, null_mut()) != 0).then_some(value)
        };
        let status = header(WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
        ensure!(status == 200, "Download failed: the server answered HTTP {status}");
        let total = header(WINHTTP_QUERY_CONTENT_LENGTH).map(u64::from).filter(|&t| t > 0);
        let mut buffer = vec![0u8; 64 * 1024];
        let mut received = 0u64;
        loop {
            ensure!(!cancelled(), "Download cancelled");
            let mut read = 0u32;
            ensure!(WinHttpReadData(request.0, buffer.as_mut_ptr().cast(), buffer.len() as u32, &mut read) != 0, http_error("read"));
            if read == 0 { break; }
            received += read as u64;
            ensure!(received <= max_bytes, "Download is larger than expected");
            out.write_all(&buffer[..read as usize])?;
            if let Some(total) = total { progress((received as f32 / total as f32).min(1.0)); }
        }
        Ok(received)
    }
}

/// Terminates every running process whose executable name matches (case-insensitive).
pub fn kill_processes(exe_name: &str) -> usize {
    let mut killed = 0;
    // SAFETY: toolhelp snapshot iteration with a correctly sized PROCESSENTRY32W; handles are closed.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot as isize == -1 { return 0; }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry) != 0;
        while more {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case(exe_name) {
                let process = OpenProcess(PROCESS_TERMINATE, 0, entry.th32ProcessID);
                if !process.is_null() {
                    if TerminateProcess(process, 1) != 0 { killed += 1; }
                    CloseHandle(process);
                }
            }
            more = Process32NextW(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
    }
    killed
}

/// Runs a console tool without flashing a window and returns whether it succeeded.
pub fn run_hidden(program: &str, args: &[&str]) -> bool {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new(program).args(args).creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
}
