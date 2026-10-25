//! Insertion du texte dans l'application active : presse-papiers + Ctrl+V simulé.
//! Version MVP ; la vérification UI Automation viendra ensuite.

use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};

/// Code de touche virtuel Windows de la lettre V, indépendant de la disposition clavier.
const VK_V: u32 = 0x56;

pub fn paste_text(text: &str, restore_clipboard: bool, delay_ms: u64) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("presse-papiers indisponible")?;
    let previous = if restore_clipboard {
        clipboard.get_text().ok()
    } else {
        None
    };

    clipboard
        .set_text(text.to_string())
        .context("écriture dans le presse-papiers")?;
    thread::sleep(Duration::from_millis(delay_ms));

    let sent = send_ctrl_v();

    if let Some(prev) = previous {
        // Laisser le temps à l'application cible de lire le presse-papiers.
        thread::sleep(Duration::from_millis(350));
        if let Err(e) = clipboard.set_text(prev) {
            log::warn!("restauration du presse-papiers impossible : {e}");
        }
    }

    sent
}

/// Copie la sélection de l'application active (Ctrl+C simulé) et la renvoie, en
/// remettant ensuite l'ancien contenu du presse-papiers. Vide si rien n'est sélectionné.
pub fn copy_selection() -> Result<String> {
    let mut clipboard = arboard::Clipboard::new().context("presse-papiers indisponible")?;
    let previous = clipboard.get_text().ok();
    // Marqueur : si l'application ne copie rien, le presse-papiers garde cette valeur.
    let marker = format!("\u{200B}dictae-{}", std::process::id());
    clipboard.set_text(marker.clone()).context("écriture dans le presse-papiers")?;
    send_key_with_control(VK_C)?;
    thread::sleep(Duration::from_millis(180));
    let copied = clipboard.get_text().unwrap_or_default();
    if let Some(prev) = previous {
        let _ = clipboard.set_text(prev);
    } else {
        let _ = clipboard.clear();
    }
    if copied == marker {
        Ok(String::new())
    } else {
        Ok(copied)
    }
}

/// Code de touche virtuel Windows de la lettre C.
const VK_C: u32 = 0x43;

/// Attend que Ctrl, Alt, Maj et Win soient physiquement relâchés (au plus `timeout`), pour
/// qu'un Ctrl+C simulé ne devienne pas Ctrl+Alt+C. Renvoie `false` si le délai est dépassé.
#[cfg(windows)]
pub fn wait_modifiers_released(timeout: Duration) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT};
    let started = std::time::Instant::now();
    loop {
        let down = [VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN]
            .iter()
            .any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0);
        if !down {
            return true;
        }
        if started.elapsed() >= timeout {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(not(windows))]
pub fn wait_modifiers_released(_timeout: Duration) -> bool {
    true
}

fn send_key_with_control(vk: u32) -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| anyhow!("simulation clavier : {e}"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| anyhow!("Ctrl : {e}"))?;
    let result = enigo
        .key(Key::Other(vk), Direction::Click)
        .map_err(|e| anyhow!("touche {vk:#x} : {e}"));
    let _ = enigo.key(Key::Control, Direction::Release);
    result
}

fn send_ctrl_v() -> Result<()> {
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| anyhow!("simulation clavier : {e}"))?;
    enigo
        .key(Key::Control, Direction::Press)
        .map_err(|e| anyhow!("Ctrl : {e}"))?;
    let result = enigo
        .key(Key::Other(VK_V), Direction::Click)
        .map_err(|e| anyhow!("V : {e}"));
    let _ = enigo.key(Key::Control, Direction::Release);
    result
}
