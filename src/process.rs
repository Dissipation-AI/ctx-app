//! Running processes, from the kernel (what `ps` shows).

/// The program `pid` is running: the executable's name, as `ps -o comm` and Activity Monitor
/// show it.
#[cfg(target_os = "macos")]
pub fn name(pid: i32) -> Option<String> {
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
}
