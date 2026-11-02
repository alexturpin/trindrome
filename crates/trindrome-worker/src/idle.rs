//! User activity: keyboard/mouse idle time and excluded (e.g. game) processes.
//!
//! `GetLastInputInfo` reports input for the calling session only, which is why
//! the worker runs in the user's session (agent or terminal), never as a service.
//! Keyboard/mouse idle time misses controllers; excluded processes cover games.

use std::time::Duration;

/// How long since the last keyboard or mouse input in this session.
#[cfg(windows)]
pub fn user_idle() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: `info` is a properly sized LASTINPUTINFO owned by this frame.
    if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        return None;
    }
    // Both are 32-bit millisecond tick counts; wrapping subtraction survives rollover.
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(now.wrapping_sub(info.dwTime) as u64))
}

#[cfg(not(windows))]
pub fn user_idle() -> Option<Duration> {
    None
}

/// A running process: executable file name and a way to look up its full path.
pub struct ProcessInfo<'a> {
    pub exe: String,
    pub full_path: Box<dyn Fn() -> Option<String> + 'a>,
}

fn normalize(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

/// Which configured exclusions match running processes. A pattern containing a
/// path separator must match the full image path; otherwise the file name.
/// Matching is case-insensitive. Full paths are only looked up for name matches.
pub fn match_exclusions<'a>(processes: impl IntoIterator<Item = ProcessInfo<'a>>, patterns: &[String]) -> Vec<String> {
    let patterns: Vec<(String, String, bool)> = patterns
        .iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| {
            let norm = normalize(p.trim());
            let file = norm.rsplit('\\').next().unwrap_or(&norm).to_string();
            let is_path = norm.contains('\\');
            (norm, file, is_path)
        })
        .collect();
    let mut found: Vec<String> = Vec::new();
    for process in processes {
        let exe = process.exe.to_lowercase();
        for (pattern, file, is_path) in &patterns {
            if *file != exe || found.contains(pattern) {
                continue;
            }
            if !is_path || (process.full_path)().is_some_and(|p| normalize(&p) == *pattern) {
                found.push(pattern.clone());
            }
        }
    }
    found
}

/// Distinct executable names of running processes, sorted case-insensitively
/// (for picking applications to exclude).
#[cfg(windows)]
pub fn running_process_names() -> Vec<String> {
    let mut names: Vec<String> = snapshot()
        .map(|p| p.into_iter().map(|(exe, _)| exe).collect())
        .unwrap_or_default();
    names.sort_by_key(|n| n.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    names
}

#[cfg(not(windows))]
pub fn running_process_names() -> Vec<String> {
    Vec::new()
}

/// Executable name and process id of every running process.
#[cfg(windows)]
fn snapshot() -> Result<Vec<(String, u32)>, String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };

    // SAFETY: plain Win32 calls; the snapshot handle is closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(|e| e.to_string())?;
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    // SAFETY: `entry` is sized correctly and the snapshot handle is valid.
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while ok {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        processes.push((String::from_utf16_lossy(&entry.szExeFile[..len]), entry.th32ProcessID));
        ok = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snapshot) };
    Ok(processes)
}

/// Excluded applications currently running, by snapshotting the process list.
#[cfg(windows)]
pub fn running_exclusions(patterns: &[String]) -> Result<Vec<String>, String> {
    if patterns.iter().all(|p| p.trim().is_empty()) {
        return Ok(Vec::new());
    }
    let processes = snapshot()?;
    Ok(match_exclusions(
        processes.into_iter().map(|(exe, pid)| ProcessInfo {
            exe,
            full_path: Box::new(move || process_image_path(pid)),
        }),
        patterns,
    ))
}

#[cfg(windows)]
fn process_image_path(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    // SAFETY: the handle is closed before returning; the buffer outlives the call.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buffer.as_mut_ptr()), &mut len);
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

#[cfg(not(windows))]
pub fn running_exclusions(patterns: &[String]) -> Result<Vec<String>, String> {
    if patterns.iter().all(|p| p.trim().is_empty()) {
        Ok(Vec::new())
    } else {
        Err("process exclusions are implemented for Windows only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn procs(list: &[(&'static str, Option<&'static str>)]) -> Vec<ProcessInfo<'static>> {
        list.iter()
            .map(|&(exe, path)| ProcessInfo {
                exe: exe.to_string(),
                full_path: Box::new(move || path.map(str::to_string)),
            })
            .collect()
    }

    #[test]
    fn names_match_case_insensitively_and_paths_exactly() {
        let running = || {
            procs(&[
                (
                    "FlightSimulator2024.exe",
                    Some(r"C:\Games\MSFS\FlightSimulator2024.exe"),
                ),
                ("game.exe", Some(r"D:\Steam\Other\game.exe")),
                ("explorer.exe", None),
            ])
        };
        assert_eq!(
            match_exclusions(running(), &["flightsimulator2024.EXE".into()]),
            ["flightsimulator2024.exe"]
        );
        assert!(match_exclusions(running(), &[r"C:\Games\Wrong\game.exe".into()]).is_empty());
        assert_eq!(
            match_exclusions(running(), &["d:/steam/other/GAME.exe".into()]),
            [r"d:\steam\other\game.exe"]
        );
        assert!(match_exclusions(running(), &["notepad.exe".into(), "  ".into()]).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn live_sensors_respond() {
        assert!(user_idle().is_some(), "GetLastInputInfo failed");
        let me = std::env::current_exe().unwrap();
        let name = me.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            running_exclusions(std::slice::from_ref(&name)).unwrap(),
            [name.to_lowercase()]
        );
        let full = me.to_string_lossy().to_string();
        assert_eq!(
            running_exclusions(&[full]).unwrap().len(),
            1,
            "full-path match of this test process"
        );
    }
}
