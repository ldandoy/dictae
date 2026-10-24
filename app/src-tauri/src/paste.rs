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
