//! Start the agent at login through the per-user `Run` registry key.
//!
//! This is what tray applications conventionally use: no administrator rights,
//! and users can see and disable it in Task Manager → Startup apps. (The spec
//! suggested Task Scheduler; the agent's own idle wait already covers the start
//! delay Task Scheduler would add.)

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "Trindrome";

#[cfg(windows)]
mod imp {
    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    fn wide(s: &str) -> HSTRING {
        HSTRING::from(s)
    }

    fn check(err: WIN32_ERROR, what: &str) -> Result<()> {
        err.ok().with_context(|| what.to_string())
    }

    /// Sets `HKCU\<key>\<name>` to `command`.
    pub fn set(key: &str, name: &str, command: &str) -> Result<()> {
        let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `data` is a NUL-terminated UTF-16 string that outlives the call.
        let err = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &wide(key),
                &wide(name),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        check(err, "cannot write the startup registry value")
    }

    /// Reads `HKCU\<key>\<name>`, or `None` if it does not exist.
    pub fn get(key: &str, name: &str) -> Result<Option<String>> {
        let mut buffer = vec![0u16; 2048];
        let mut bytes = (buffer.len() * 2) as u32;
        // SAFETY: the buffer and its length describe valid writable memory.
        let err = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &wide(key),
                &wide(name),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        check(err, "cannot read the startup registry value")?;
        let len = (bytes as usize / 2).saturating_sub(1);
        Ok(Some(String::from_utf16_lossy(&buffer[..len])))
    }

    /// Removes `HKCU\<key>\<name>`; succeeds if it was already absent.
    pub fn remove(key: &str, name: &str) -> Result<()> {
        // SAFETY: plain registry call with valid strings.
        let err = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &wide(key), &wide(name)) };
        if err == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        check(err, "cannot remove the startup registry value")
    }
}

#[cfg(not(windows))]
mod imp {
    use anyhow::{Result, bail};
    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        bail!("startup at login is implemented for Windows only")
    }
    pub fn get(_: &str, _: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn remove(_: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

pub use imp::{get, remove, set};

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn registry_values_round_trip() {
        // A private key, never the real Run key.
        let key = r"Software\TrindromeTest";
        let name = format!("startup-test-{}", std::process::id());
        let command = r#""C:\Program Files\Trindrome\trindrome-agent.exe" --config "C:\x y\t.toml""#;
        set(key, &name, command).unwrap();
        assert_eq!(get(key, &name).unwrap().as_deref(), Some(command));
        remove(key, &name).unwrap();
        assert_eq!(get(key, &name).unwrap(), None);
        remove(key, &name).unwrap();
    }
}
