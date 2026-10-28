//! Running processes, from the kernel (what `ps` shows).

/// The program `pid` is running, as `ps -o comm` shows it: the name it was started as
/// (`argv[0]`, its last path part), so a program run through a link is named after the link
/// (`claude`, not the versioned file `2.1.310` it points to). Else the executable's name.
#[cfg(target_os = "macos")]
pub fn name(pid: i32) -> Option<String> {
    argv0(pid).map(|a| base_name(&a).to_string()).filter(|n| !n.is_empty()).or_else(|| executable_name(pid))
}

/// `pid`'s `argv[0]`, from `KERN_PROCARGS2` (any process of the same user).
#[cfg(target_os = "macos")]
fn argv0(pid: i32) -> Option<String> {
    // SAFETY: sysctl writes at most `size` bytes into `buf`, and `size` back.
    unsafe {
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        if libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let mut buf = vec![0u8; argmax.max(0) as usize];
        let mut size = buf.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, std::ptr::null_mut(), 0) != 0 {
            return None;
        }
        buf.truncate(size);
        procargs_argv0(&buf)
    }
}

/// `argv[0]` from a `KERN_PROCARGS2` buffer: argc (4 bytes), the executable's path, NUL padding,
/// then the arguments, each NUL-terminated.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn procargs_argv0(buf: &[u8]) -> Option<String> {
    let rest = buf.get(4..)?;
    let after_path = rest.iter().position(|b| *b == 0)?;
    let rest = &rest[after_path..];
    let start = rest.iter().position(|b| *b != 0)?;
    let rest = &rest[start..];
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    let a = String::from_utf8_lossy(&rest[..end]).into_owned();
    (!a.is_empty()).then_some(a)
}

/// The last part of a path (`/usr/bin/python3` → `python3`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn base_name(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or(path)
}

/// The executable file's name, from the kernel's process info.
#[cfg(target_os = "macos")]
fn executable_name(pid: i32) -> Option<String> {
    // SAFETY: proc_bsdinfo is plain old data; proc_pidinfo fills at most its size.
    let info = unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size);
        if n != size {
            return None;
        }
        info
    };
    // pbi_name is the long name; pbi_comm is cut to 16 bytes.
    let name = c_chars(&info.pbi_name);
    let name = if name.is_empty() { c_chars(&info.pbi_comm) } else { name };
    (!name.is_empty()).then_some(name)
}

#[cfg(target_os = "macos")]
fn c_chars(s: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = s.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The program `pid` is running (`/proc/<pid>/comm`).
#[cfg(not(target_os = "macos"))]
pub fn name(pid: i32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let comm = comm.trim_end_matches('\n');
    (!comm.is_empty()).then(|| comm.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_this_process() {
        let me = super::name(std::process::id() as i32).expect("this process has a name");
        // The test binary is named after the crate, with a hash: ctx_app-1a2b….
        assert!(me.starts_with("ctx_app") || me.starts_with("ctx-app"), "{me}");
        assert_eq!(super::name(-1), None);
    }

    #[test]
    fn argv0_from_procargs() {
        // argc, the executable's path, padding, then argv.
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/Users/me/.local/share/claude/versions/2.1.310\0\0\0claude\0--resume\0");
        assert_eq!(super::procargs_argv0(&buf).as_deref(), Some("claude"));
        assert_eq!(super::procargs_argv0(&buf[..2]), None);
        assert_eq!(super::base_name("/usr/bin/python3"), "python3");
        assert_eq!(super::base_name("claude"), "claude");
        assert_eq!(super::base_name("-zsh"), "-zsh");
    }

    /// A program started through a link is named after the link, as `claude` → `versions/2.1.310`.
    #[test]
    fn a_linked_program_has_the_links_name() {
        let dir = std::env::temp_dir().join(format!("ctx-app-process-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("my-sleeper");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/bin/sleep", &link).unwrap();
        let mut child = std::process::Command::new(&link).arg("5").spawn().unwrap();
        let mut name = None;
        for _ in 0..50 {
            name = super::name(child.id() as i32);
            if name.as_deref() == Some("my-sleeper") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(name.as_deref(), Some("my-sleeper"));
    }
}
