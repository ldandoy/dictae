//! Garde-fous autour des processus enfants (llama-server) sous Windows.
//!
//! Un installeur ou un plantage peut tuer Dictae sans lui laisser arrêter ses enfants :
//! chaque llama-server orphelin garde alors 3 Go de RAM et autant de mémoire vidéo, et
//! les suivants sont lents. Deux parades : (1) rattacher l'enfant à un *job object*
//! « kill on close », fermé automatiquement avec le processus parent ; (2) au démarrage,
//! terminer les llama-server encore vivants lancés depuis notre dossier de ressources.

use std::path::Path;
use std::process::Child;

#[cfg(not(windows))]
use anyhow::Result;

#[cfg(windows)]
mod win {
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use std::process::Child;

    use anyhow::{Context, Result};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE,
    };
    use windows::core::PWSTR;

    /// Job object dont la fermeture tue les processus rattachés.
    pub struct JobGuard(HANDLE);

    unsafe impl Send for JobGuard {}
    unsafe impl Sync for JobGuard {}

    impl JobGuard {
        pub fn new() -> Result<Self> {
            unsafe {
                let job = CreateJobObjectW(None, None).context("création du job object")?;
                let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
                .context("configuration du job object")?;
                Ok(Self(job))
            }
        }

        pub fn assign(&self, child: &Child) -> Result<()> {
            unsafe { AssignProcessToJobObject(self.0, HANDLE(child.as_raw_handle() as _)) }
                .context("rattachement de l'enfant au job object")
        }
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    fn wide_to_string(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// Chemin normalisé pour comparaison : sans préfixe `\\?\`, en minuscules.
    fn norm(path: &str) -> String {
        path.trim_start_matches(r"\\?\").to_lowercase()
    }

    /// Termine les processus dont l'image est exactement `exe` (sauf le nôtre). Renvoie
    /// le nombre de processus terminés.
    pub fn kill_processes_at(exe: &Path) -> Result<usize> {
        let wanted_name = exe
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let wanted_path = norm(&exe.to_string_lossy());
        let me = std::process::id();
        let mut killed = 0;
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).context("liste des processus")?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
            while ok {
                let pid = entry.th32ProcessID;
                let name = wide_to_string(&entry.szExeFile).to_lowercase();
                if pid != me && name == wanted_name {
                    if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, false, pid) {
                        let mut buf = [0u16; 1024];
                        let mut len = buf.len() as u32;
                        let path = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                            .ok()
                            .map(|_| wide_to_string(&buf[..len as usize]));
                        if path.as_deref().map(norm) == Some(wanted_path.clone()) {
                            match TerminateProcess(handle, 1) {
                                Ok(()) => {
                                    log::info!("processus orphelin terminé : {name} (pid {pid})");
                                    killed += 1;
                                }
                                Err(e) => log::warn!("impossible de terminer {name} (pid {pid}) : {e}"),
                            }
                        }
                        let _ = CloseHandle(handle);
                    }
                }
                ok = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }
        Ok(killed)
    }
}

#[cfg(windows)]
pub use win::JobGuard;

#[cfg(not(windows))]
pub struct JobGuard;

#[cfg(not(windows))]
impl JobGuard {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
    pub fn assign(&self, _child: &Child) -> Result<()> {
        Ok(())
    }
}

/// Termine les processus orphelins dont l'exécutable est exactement `exe`.
pub fn kill_orphans(exe: &Path) -> usize {
    #[cfg(windows)]
    {
        match win::kill_processes_at(exe) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("recherche de processus orphelins impossible : {e:#}");
                0
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = exe;
        0
    }
}

/// Rattache `child` à un job « kill on close » ; le `JobGuard` renvoyé doit vivre aussi
/// longtemps que l'enfant. `None` si le rattachement échoue (l'enfant tourne quand même).
pub fn guard_child(child: &Child) -> Option<JobGuard> {
    match JobGuard::new().and_then(|job| job.assign(child).map(|_| job)) {
        Ok(job) => Some(job),
        Err(e) => {
            log::warn!("job object indisponible, l'enfant pourrait survivre à Dictae : {e:#}");
            None
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// Copie de ping.exe dans un dossier temporaire : un chemin que personne d'autre n'utilise.
    fn private_ping(dir: &Path) -> std::path::PathBuf {
        let target = dir.join("dictae-test-ping.exe");
        std::fs::copy(r"C:\Windows\System32\ping.exe", &target).unwrap();
        target
    }

    fn spawn_sleeper(exe: &Path) -> Child {
        Command::new(exe)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn exited(child: &mut Child) -> bool {
        for _ in 0..50 {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn job_guard_kills_child_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = private_ping(dir.path());
        let mut child = spawn_sleeper(&exe);
        let guard = guard_child(&child).expect("job object");
        assert!(child.try_wait().unwrap().is_none(), "l'enfant tourne tant que le job existe");
        drop(guard);
        assert!(exited(&mut child), "l'enfant doit mourir à la fermeture du job");
    }

    #[test]
    fn kill_orphans_terminates_only_processes_at_that_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = private_ping(dir.path());
        let other = dir.path().join("dictae-test-other.exe");
        std::fs::copy(&exe, &other).unwrap();
        let mut orphan = spawn_sleeper(&exe);
        let mut bystander = spawn_sleeper(&other);
        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(kill_orphans(&exe), 1);
        assert!(exited(&mut orphan));
        assert!(bystander.try_wait().unwrap().is_none(), "un autre exécutable n'est pas touché");
        let _ = bystander.kill();
        let _ = bystander.wait();
        let _ = orphan.wait();
        assert_eq!(kill_orphans(&exe), 0);
    }
}
