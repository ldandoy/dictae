//! Coupe le son des autres applications pendant la dictée (sessions audio Windows
//! Core Audio du périphérique de sortie par défaut), puis le rétablit.
//!
//! Seules les sessions que nous avons coupées nous-mêmes sont rétablies ; une session
//! déjà muette avant la dictée reste muette.

#[cfg(windows)]
mod win {
    use anyhow::{Context, Result};
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eMultimedia, eRender, AudioSessionStateExpired, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
        ISimpleAudioVolume, MMDeviceEnumerator,
    };
    use windows::Win32::Foundation::S_OK;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED};

    struct Session {
        id: String,
        pid: u32,
        system_sounds: bool,
        expired: bool,
        volume: ISimpleAudioVolume,
    }

    fn with_com<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(anyhow::anyhow!("CoInitializeEx : {hr}"));
        }
        let result = f();
        unsafe { CoUninitialize() };
        result
    }

    fn sessions() -> Result<Vec<Session>> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).context("MMDeviceEnumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .context("périphérique de sortie par défaut")?;
            let manager: IAudioSessionManager2 = device.Activate(CLSCTX_ALL, None).context("IAudioSessionManager2")?;
            let list = manager.GetSessionEnumerator().context("énumération des sessions")?;
            let count = list.GetCount().unwrap_or(0);
            let mut out = Vec::new();
            for i in 0..count {
                let Ok(control) = list.GetSession(i) else { continue };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else { continue };
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else { continue };
                let id = match control2.GetSessionInstanceIdentifier() {
                    Ok(p) if !p.is_null() => {
                        let s = p.to_string().unwrap_or_default();
                        CoTaskMemFree(Some(p.as_ptr() as *const _));
                        s
                    }
                    _ => continue,
                };
                out.push(Session {
                    id,
                    pid: control2.GetProcessId().unwrap_or(0),
                    // S_OK = session des sons système ; S_FALSE (aussi « réussi ») = session ordinaire.
                    system_sounds: control2.IsSystemSoundsSession() == S_OK,
                    expired: control.GetState().map(|s| s == AudioSessionStateExpired).unwrap_or(true),
                    volume,
                });
            }
            Ok(out)
        }
    }

    /// Description lisible des sessions (diagnostic).
    pub fn describe() -> Result<Vec<String>> {
        with_com(|| {
            Ok(sessions()?
                .iter()
                .map(|s| {
                    let muted = unsafe { s.volume.GetMute() }.map(|b| b.as_bool()).unwrap_or(false);
                    format!(
                        "pid={} system={} expired={} muted={} id={}",
                        s.pid,
                        s.system_sounds,
                        s.expired,
                        muted,
                        s.id.chars().take(60).collect::<String>()
                    )
                })
                .collect())
        })
    }

    /// Coupe les sessions des autres processus ; renvoie les identifiants coupés.
    pub fn mute_others() -> Result<Vec<String>> {
        with_com(|| {
            let me = std::process::id();
            let mut muted = Vec::new();
            for s in sessions()? {
                if s.pid == me || s.system_sounds || s.expired {
                    continue;
                }
                let already = unsafe { s.volume.GetMute() }.map(|b| b.as_bool()).unwrap_or(true);
                if already {
                    continue;
                }
                if unsafe { s.volume.SetMute(true, std::ptr::null()) }.is_ok() {
                    muted.push(s.id);
                }
            }
            Ok(muted)
        })
    }

    /// Rétablit le son des sessions coupées par `mute_others`.
    pub fn restore(ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        with_com(|| {
            let mut restored = 0;
            for s in sessions()? {
                if ids.iter().any(|id| id == &s.id) && unsafe { s.volume.SetMute(false, std::ptr::null()) }.is_ok() {
                    restored += 1;
                }
            }
            Ok(restored)
        })
    }
}

#[cfg(windows)]
pub use win::{mute_others, restore};
#[cfg(all(windows, test))]
pub use win::describe;

#[cfg(not(windows))]
pub fn mute_others() -> anyhow::Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(not(windows))]
pub fn restore(_ids: &[String]) -> anyhow::Result<usize> {
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Coupe puis rétablit les sessions réelles : à lancer à la main avec de la musique
    /// (`cargo test -- --ignored ducking`).
    #[test]
    #[ignore]
    fn mute_and_restore_live() {
        #[cfg(windows)]
        for line in describe().unwrap() {
            eprintln!("session : {line}");
        }
        let ids = mute_others().unwrap();
        eprintln!("{} session(s) coupée(s) : {ids:?}", ids.len());
        std::thread::sleep(std::time::Duration::from_secs(2));
        let n = restore(&ids).unwrap();
        eprintln!("{n} rétablie(s)");
        assert_eq!(n, ids.len());
    }
}
