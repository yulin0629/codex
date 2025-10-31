macro_rules! windows_modules {
    ($($name:ident),+ $(,)?) => {
        $(#[cfg(target_os = "windows")] mod $name;)+
    };
}

windows_modules!(acl, allow, audit, cap, env, logging, policy, token, winutil);

#[cfg(target_os = "windows")]
pub use windows_impl::preflight_audit_everyone_writable;
#[cfg(target_os = "windows")]
pub use windows_impl::run_windows_sandbox_capture;
#[cfg(target_os = "windows")]
pub use windows_impl::CaptureResult;

#[cfg(not(target_os = "windows"))]
pub use stub::preflight_audit_everyone_writable;
#[cfg(not(target_os = "windows"))]
pub use stub::run_windows_sandbox_capture;
#[cfg(not(target_os = "windows"))]
pub use stub::CaptureResult;

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::acl::add_allow_ace;
    use super::acl::allow_null_device;
    use super::acl::revoke_ace;
    use super::allow::compute_allow_paths;
    use super::audit;
    use super::cap::cap_sid_file;
    use super::cap::load_or_create_cap_sids;
    use super::env::apply_no_network_to_env;
    use super::env::ensure_non_interactive_pager;
    use super::env::normalize_null_device_env;
    use super::logging::debug_log;
    use super::logging::log_failure;
    use super::logging::log_start;
    use super::logging::log_success;
    use super::policy::SandboxMode;
    use super::policy::SandboxPolicy;
    use super::token::convert_string_sid_to_sid;
    use super::winutil::format_last_error;
    use super::winutil::to_wide;
    use anyhow::Result;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::fs;
    use std::io;
    use std::path::Path;
    use std::path::PathBuf;
    use std::ptr;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::Foundation::SetHandleInformation;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Foundation::HANDLE_FLAG_INHERIT;
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::CreateProcessAsUserW;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
    use windows_sys::Win32::System::Threading::INFINITE;
    use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
    use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
    use windows_sys::Win32::System::Threading::STARTUPINFOW;

    type PipeHandles = ((HANDLE, HANDLE), (HANDLE, HANDLE), (HANDLE, HANDLE));

    fn ensure_dir(p: &Path) -> Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }

    fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
        let mut items: Vec<(String, String)> =
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        items.sort_by(|a, b| {
            a.0.to_uppercase()
                .cmp(&b.0.to_uppercase())
                .then(a.0.cmp(&b.0))
        });
        let mut w: Vec<u16> = Vec::new();
        for (k, v) in items {
            let mut s = to_wide(format!("{}={}", k, v));
            s.pop();
            w.extend_from_slice(&s);
            w.push(0);
        }
        w.push(0);
        w
    }

    // Quote a single Windows command-line argument following the rules used by
    // CommandLineToArgvW/CRT so that spaces, quotes, and backslashes are preserved.
    // Reference behavior matches Rust std::process::Command on Windows.
    fn quote_windows_arg(arg: &str) -> String {
        let needs_quotes = arg.is_empty()
            || arg
                .chars()
                .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"'));
        if !needs_quotes {
            return arg.to_string();
        }

        let mut quoted = String::with_capacity(arg.len() + 2);
        quoted.push('"');
        let mut backslashes = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => {
                    backslashes += 1;
                }
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    if backslashes > 0 {
                        quoted.push_str(&"\\".repeat(backslashes));
                        backslashes = 0;
                    }
                    quoted.push(ch);
                }
            }
        }
        if backslashes > 0 {
            quoted.push_str(&"\\".repeat(backslashes * 2));
        }
        quoted.push('"');
        quoted
    }

    unsafe fn setup_stdio_pipes() -> io::Result<PipeHandles> {
        let mut in_r: HANDLE = 0;
        let mut in_w: HANDLE = 0;
        let mut out_r: HANDLE = 0;
        let mut out_w: HANDLE = 0;
        let mut err_r: HANDLE = 0;
        let mut err_w: HANDLE = 0;
        if CreatePipe(&mut in_r, &mut in_w, ptr::null_mut(), 0) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if CreatePipe(&mut out_r, &mut out_w, ptr::null_mut(), 0) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if CreatePipe(&mut err_r, &mut err_w, ptr::null_mut(), 0) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if SetHandleInformation(in_r, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if SetHandleInformation(out_w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        if SetHandleInformation(err_w, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(io::Error::from_raw_os_error(GetLastError() as i32));
        }
        Ok(((in_r, in_w), (out_r, out_w), (err_r, err_w)))
    }

    pub struct CaptureResult {
        pub exit_code: i32,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
        pub timed_out: bool,
    }

    pub fn preflight_audit_everyone_writable(
        cwd: &Path,
        env_map: &HashMap<String, String>,
    ) -> Result<()> {
        audit::audit_everyone_writable(cwd, env_map)
    }

    pub fn run_windows_sandbox_capture(
        policy_json_or_preset: &str,
        sandbox_policy_cwd: &Path,
        command: Vec<String>,
        cwd: &Path,
        mut env_map: HashMap<String, String>,
        timeout_ms: Option<u64>,
    ) -> Result<CaptureResult> {
        let policy = SandboxPolicy::parse(policy_json_or_preset)?;
        normalize_null_device_env(&mut env_map);
        ensure_non_interactive_pager(&mut env_map);
        apply_no_network_to_env(&mut env_map)?;

        let current_dir = cwd.to_path_buf();
        // for now, don't fail if we detect world-writable directories
        // audit::audit_everyone_writable(&current_dir, &env_map)?;
        log_start(&command);
        let (h_token, psid_to_use): (HANDLE, *mut c_void) = unsafe {
            match &policy.0 {
                SandboxMode::ReadOnly => {
                    let caps = load_or_create_cap_sids(sandbox_policy_cwd);
                    ensure_dir(&cap_sid_file(sandbox_policy_cwd))?;
                    fs::write(
                        cap_sid_file(sandbox_policy_cwd),
                        serde_json::to_string(&caps)?,
                    )?;
                    let psid = convert_string_sid_to_sid(&caps.readonly).unwrap();
                    super::token::create_readonly_token_with_cap(psid)?
                }
                SandboxMode::WorkspaceWrite => {
                    let caps = load_or_create_cap_sids(sandbox_policy_cwd);
                    ensure_dir(&cap_sid_file(sandbox_policy_cwd))?;
                    fs::write(
                        cap_sid_file(sandbox_policy_cwd),
                        serde_json::to_string(&caps)?,
                    )?;
                    let psid = convert_string_sid_to_sid(&caps.workspace).unwrap();
                    super::token::create_workspace_write_token_with_cap(psid)?
                }
            }
        };

        unsafe {
            if matches!(policy.0, SandboxMode::WorkspaceWrite) {
                if let Ok(base) = super::token::get_current_token_for_restriction() {
                    if let Ok(bytes) = super::token::get_logon_sid_bytes(base) {
                        let mut tmp = bytes.clone();
                        let psid2 = tmp.as_mut_ptr() as *mut c_void;
                        allow_null_device(psid2);
                    }
                    windows_sys::Win32::Foundation::CloseHandle(base);
                }
            }
        }

        let persist_aces = matches!(policy.0, SandboxMode::WorkspaceWrite);
        let allow = compute_allow_paths(&policy, sandbox_policy_cwd, &current_dir, &env_map);
        let mut guards: Vec<(PathBuf, *mut c_void)> = Vec::new();
        unsafe {
            for p in &allow {
                if let Ok(added) = add_allow_ace(p, psid_to_use) {
                    if added {
                        if persist_aces {
                            if p.is_dir() {
                                // best-effort seeding omitted intentionally
                            }
                        } else {
                            guards.push((p.clone(), psid_to_use));
                        }
                    }
                }
            }
            allow_null_device(psid_to_use);
        }

        let (stdin_pair, stdout_pair, stderr_pair) = unsafe { setup_stdio_pipes()? };
        let ((in_r, in_w), (out_r, out_w), (err_r, err_w)) = (stdin_pair, stdout_pair, stderr_pair);
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.dwFlags |= STARTF_USESTDHANDLES;
        si.hStdInput = in_r;
        si.hStdOutput = out_w;
        si.hStdError = err_w;

        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let cmdline_str = command
            .iter()
            .map(|a| quote_windows_arg(a))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmdline: Vec<u16> = to_wide(&cmdline_str);
        let env_block = make_env_block(&env_map);
        let desktop = to_wide("Winsta0\\Default");
        si.lpDesktop = desktop.as_ptr() as *mut u16;
        let spawn_res = unsafe {
            CreateProcessAsUserW(
                h_token,
                ptr::null(),
                cmdline.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                1,
                CREATE_UNICODE_ENVIRONMENT,
                env_block.as_ptr() as *mut c_void,
                to_wide(cwd).as_ptr(),
                &si,
                &mut pi,
            )
        };
        if spawn_res == 0 {
            let err = unsafe { GetLastError() } as i32;
            let dbg = format!(
                "CreateProcessAsUserW failed: {} ({}) | cwd={} | cmd={} | env_u16_len={} | si_flags={}",
                err,
                format_last_error(err),
                cwd.display(),
                cmdline_str,
                env_block.len(),
                si.dwFlags,
            );
            debug_log(&dbg);
            unsafe {
                CloseHandle(in_r);
                CloseHandle(in_w);
                CloseHandle(out_r);
                CloseHandle(out_w);
                CloseHandle(err_r);
                CloseHandle(err_w);
                CloseHandle(h_token);
            }
            return Err(anyhow::anyhow!("CreateProcessAsUserW failed: {}", err));
        }

        unsafe {
            CloseHandle(in_r);
            // Close the parent's stdin write end so the child sees EOF immediately.
            CloseHandle(in_w);
            CloseHandle(out_w);
            CloseHandle(err_w);
        }

        let (tx_out, rx_out) = std::sync::mpsc::channel::<Vec<u8>>();
        let (tx_err, rx_err) = std::sync::mpsc::channel::<Vec<u8>>();
        let t_out = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let mut read_bytes: u32 = 0;
                let ok = unsafe {
                    windows_sys::Win32::Storage::FileSystem::ReadFile(
                        out_r,
                        tmp.as_mut_ptr(),
                        tmp.len() as u32,
                        &mut read_bytes,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read_bytes == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..read_bytes as usize]);
            }
            let _ = tx_out.send(buf);
        });
        let t_err = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            loop {
                let mut read_bytes: u32 = 0;
                let ok = unsafe {
                    windows_sys::Win32::Storage::FileSystem::ReadFile(
                        err_r,
                        tmp.as_mut_ptr(),
                        tmp.len() as u32,
                        &mut read_bytes,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || read_bytes == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..read_bytes as usize]);
            }
            let _ = tx_err.send(buf);
        });

        let timeout = timeout_ms.map(|ms| ms as u32).unwrap_or(INFINITE);
        let res = unsafe { WaitForSingleObject(pi.hProcess, timeout) };
        let timed_out = res == 0x0000_0102;
        let mut exit_code_u32: u32 = 1;
        if !timed_out {
            unsafe {
                GetExitCodeProcess(pi.hProcess, &mut exit_code_u32);
            }
        } else {
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
            }
        }

        unsafe {
            if pi.hThread != 0 {
                CloseHandle(pi.hThread);
            }
            if pi.hProcess != 0 {
                CloseHandle(pi.hProcess);
            }
            CloseHandle(h_token);
        }
        let _ = t_out.join();
        let _ = t_err.join();
        let stdout = rx_out.recv().unwrap_or_default();
        let stderr = rx_err.recv().unwrap_or_default();
        let exit_code = if timed_out {
            128 + 64
        } else {
            exit_code_u32 as i32
        };

        if exit_code == 0 {
            log_success(&command);
        } else {
            log_failure(&command, &format!("exit code {}", exit_code));
        }

        if !persist_aces {
            unsafe {
                for (p, sid) in guards {
                    revoke_ace(&p, sid);
                }
            }
        }

        Ok(CaptureResult {
            exit_code,
            stdout,
            stderr,
            timed_out,
        })
    }
}

#[cfg(not(target_os = "windows"))]
mod stub {
    use anyhow::bail;
    use anyhow::Result;
    use std::collections::HashMap;
    use std::path::Path;

    #[derive(Debug, Default)]
    pub struct CaptureResult {
        pub exit_code: i32,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
        pub timed_out: bool,
    }

    pub fn preflight_audit_everyone_writable(
        _cwd: &Path,
        _env_map: &HashMap<String, String>,
    ) -> Result<()> {
        bail!("Windows sandbox is only available on Windows")
    }

    pub fn run_windows_sandbox_capture(
        _policy_json_or_preset: &str,
        _sandbox_policy_cwd: &Path,
        _command: Vec<String>,
        _cwd: &Path,
        _env_map: HashMap<String, String>,
        _timeout_ms: Option<u64>,
    ) -> Result<CaptureResult> {
        bail!("Windows sandbox is only available on Windows")
    }
}
