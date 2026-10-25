//! Contexte d'insertion : application active, champ de saisie focalisé, texte autour du
//! curseur, URL du navigateur, et vérification du collage. Windows UI Automation.
//!
//! Tout ce qui est lu ici provient de l'écran de l'utilisateur ; c'est du contexte pour la
//! mise en forme, jamais des instructions.

use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertionContext {
    /// Nom lisible de l'application (« Google Chrome », « Code »…).
    pub app: String,
    /// Nom de l'exécutable sans extension, en minuscules (« chrome », « code »).
    pub exe: String,
    pub window_title: String,
    pub url: Option<String>,
    /// Texte juste avant le curseur (ou avant la sélection), tronqué au début.
    pub before: String,
    /// Texte juste après le curseur (ou après la sélection), tronqué à la fin.
    pub after: String,
    pub selected: String,
    /// Le champ focalisé est modifiable.
    pub editable: bool,
    /// Du texte a pu être lu dans le champ focalisé.
    pub readable: bool,
    /// La position du curseur est connue (sinon `before` est la fin du champ).
    pub caret_known: bool,
    pub password: bool,
    pub capture_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// Le texte collé a été relu dans le champ.
    Verified,
    /// Le champ est lisible mais le texte n'y est pas.
    Missing,
    /// Impossible de relire le champ (terminal, application sans UI Automation…).
    Unavailable,
}

const BEFORE_MAX: i32 = 400;
const AFTER_MAX: i32 = 120;
/// La sélection est lue en entier (réécriture) ; au-delà on considère qu'il n'y en a pas d'exploitable.
const SELECTED_MAX: i32 = 20_000;

/// Nom lisible pour les exécutables courants.
pub fn friendly_app_name(exe: &str) -> String {
    match exe {
        "chrome" => "Google Chrome",
        "msedge" => "Microsoft Edge",
        "firefox" => "Firefox",
        "brave" => "Brave",
        "opera" | "opera_gx" => "Opera",
        "vivaldi" => "Vivaldi",
        "arc" => "Arc",
        "code" => "Visual Studio Code",
        "cursor" => "Cursor",
        "windowsterminal" => "Terminal Windows",
        "powershell" | "pwsh" => "PowerShell",
        "cmd" => "Invite de commandes",
        "winword" => "Word",
        "excel" => "Excel",
        "powerpnt" => "PowerPoint",
        "outlook" | "olk" => "Outlook",
        "onenote" => "OneNote",
        "notepad" => "Bloc-notes",
        "notepad++" => "Notepad++",
        "slack" => "Slack",
        "discord" => "Discord",
        "teams" | "ms-teams" => "Microsoft Teams",
        "telegram" => "Telegram",
        "whatsapp" => "WhatsApp",
        "signal" => "Signal",
        "thunderbird" => "Thunderbird",
        "obsidian" => "Obsidian",
        "notion" => "Notion",
        "claude" => "Claude",
        "chatgpt" => "ChatGPT",
        "dictae" => "Dictae",
        _ => exe,
    }
    .to_string()
}

fn is_browser(exe: &str) -> bool {
    matches!(exe, "chrome" | "msedge" | "firefox" | "brave" | "opera" | "opera_gx" | "vivaldi" | "arc")
}

/// Heuristique : la valeur de la barre d'adresse ressemble-t-elle à une URL ?
fn looks_like_url(value: &str) -> bool {
    let v = value.trim();
    !v.is_empty()
        && !v.contains(char::is_whitespace)
        && (v.starts_with("http://") || v.starts_with("https://") || v.contains('.') || v.starts_with("localhost"))
        && v.len() < 2048
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Ajuste espaces et majuscule initiale du texte à insérer selon ce qui l'entoure.
pub fn smart_spacing(text: &str, ctx: &InsertionContext) -> String {
    let text = text.trim();
    if text.is_empty() || !ctx.caret_known {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    let mut body = text.to_string();

    if let Some(last) = ctx.before.chars().last() {
        // Au milieu d'une phrase : pas de majuscule initiale (sauf sigle) ni d'espace double.
        let mid_sentence = last.is_alphanumeric() || matches!(last, ',' | ';' | ':');
        if mid_sentence {
            let mut chars = body.chars();
            if let (Some(first), Some(second)) = (chars.next(), chars.next()) {
                if first.is_uppercase() && second.is_lowercase() {
                    body = first.to_lowercase().collect::<String>() + &body[first.len_utf8()..];
                }
            }
        }
        if !last.is_whitespace() && !matches!(last, '(' | '[' | '{' | '«' | '"' | '\'') {
            out.push(' ');
        }
    }
    out.push_str(&body);
    if let Some(next) = ctx.after.chars().next() {
        if next.is_alphanumeric() {
            out.push(' ');
        }
    }
    out
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::time::Instant;

    use anyhow::{anyhow, Context as _, Result};
    use windows::core::{Interface, BOOL, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HWND};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, IUIAutomationTextPattern2,
        IUIAutomationTextRange, IUIAutomationValuePattern, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
        TextUnit_Character, TreeScope_Descendants, UIA_ControlTypePropertyId, UIA_EditControlTypeId, UIA_TextPattern2Id,
        UIA_TextPatternId, UIA_ValuePatternId,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};

    /// Initialise COM (MTA) sur le thread courant pour la durée de la closure.
    fn with_com<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
        // S_FALSE (déjà initialisé) est aussi un succès.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(anyhow!("CoInitializeEx : {hr}"));
        }
        let result = f();
        unsafe { CoUninitialize() };
        result
    }

    fn automation() -> Result<IUIAutomation> {
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }.context("CUIAutomation")
    }

    fn foreground_window() -> (HWND, String, String) {
        unsafe {
            let hwnd = GetForegroundWindow();
            let mut title_buf = [0u16; 512];
            let n = GetWindowTextW(hwnd, &mut title_buf);
            let title = String::from_utf16_lossy(&title_buf[..n.max(0) as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            let exe = process_exe(pid).unwrap_or_default();
            (hwnd, title, exe)
        }
    }

    fn process_exe(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut buf = [0u16; 1024];
            let mut size = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size).is_ok();
            let _ = CloseHandle(handle);
            if !ok {
                return None;
            }
            let path = String::from_utf16_lossy(&buf[..size as usize]);
            let file = path.rsplit(['\\', '/']).next()?;
            let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            Some(stem.to_ascii_lowercase())
        }
    }

    fn range_text(range: &IUIAutomationTextRange, max: i32) -> String {
        unsafe { range.GetText(max) }.map(|b| b.to_string()).unwrap_or_default()
    }

    /// Texte avant / après / sélectionné à partir d'un pattern texte et d'une plage « curseur ».
    fn around(caret: &IUIAutomationTextRange) -> Result<(String, String, String)> {
        unsafe {
            let before = caret.Clone()?;
            before.MoveEndpointByRange(TextPatternRangeEndpoint_End, caret, TextPatternRangeEndpoint_Start)?;
            let _ = before.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -BEFORE_MAX);
            let after = caret.Clone()?;
            after.MoveEndpointByRange(TextPatternRangeEndpoint_Start, caret, TextPatternRangeEndpoint_End)?;
            let _ = after.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, AFTER_MAX);
            Ok((range_text(&before, -1), range_text(&after, -1), range_text(caret, SELECTED_MAX)))
        }
    }

    /// Plage du curseur : la sélection courante si elle existe, sinon le caret (TextPattern2).
    fn caret_range(element: &IUIAutomationElement) -> Option<(IUIAutomationTextRange, IUIAutomationTextPattern)> {
        unsafe {
            let text: IUIAutomationTextPattern = element.GetCurrentPattern(UIA_TextPatternId).ok()?.cast().ok()?;
            if let Ok(selection) = text.GetSelection() {
                if selection.Length().unwrap_or(0) > 0 {
                    if let Ok(range) = selection.GetElement(0) {
                        return Some((range, text));
                    }
                }
            }
            let text2: IUIAutomationTextPattern2 = element.GetCurrentPattern(UIA_TextPattern2Id).ok()?.cast().ok()?;
            let mut active = BOOL(0);
            let caret = text2.GetCaretRange(&mut active).ok()?;
            Some((caret, text))
        }
    }

    fn read_focused(ctx: &mut InsertionContext) -> Result<()> {
        let uia = automation()?;
        let element = unsafe { uia.GetFocusedElement() }.context("élément focalisé")?;
        unsafe {
            ctx.password = element.CurrentIsPassword().map(|b| b.as_bool()).unwrap_or(false);
            if ctx.password {
                return Ok(());
            }
            if let Ok(value) = element.GetCurrentPattern(UIA_ValuePatternId).and_then(|p| p.cast::<IUIAutomationValuePattern>()) {
                ctx.editable = !value.CurrentIsReadOnly().map(|b| b.as_bool()).unwrap_or(true);
            }
            if let Some((caret, text)) = caret_range(&element) {
                let (before, after, selected) = around(&caret)?;
                ctx.before = before;
                ctx.after = after;
                ctx.selected = selected;
                ctx.readable = true;
                ctx.caret_known = true;
                ctx.editable = ctx.editable || true;
                let _ = text;
                return Ok(());
            }
            if let Ok(text) = element.GetCurrentPattern(UIA_TextPatternId).and_then(|p| p.cast::<IUIAutomationTextPattern>()) {
                // Pattern texte sans caret connu : on suppose le curseur en fin de document.
                let doc = text.DocumentRange()?;
                let tail = doc.Clone()?;
                tail.MoveEndpointByRange(TextPatternRangeEndpoint_Start, &doc, TextPatternRangeEndpoint_End)?;
                let _ = tail.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -BEFORE_MAX);
                ctx.before = range_text(&tail, -1);
                ctx.readable = true;
                return Ok(());
            }
            if let Ok(value) = element.GetCurrentPattern(UIA_ValuePatternId).and_then(|p| p.cast::<IUIAutomationValuePattern>()) {
                let v = value.CurrentValue().map(|b| b.to_string()).unwrap_or_default();
                let chars: Vec<char> = v.chars().collect();
                let start = chars.len().saturating_sub(BEFORE_MAX as usize);
                ctx.before = chars[start..].iter().collect();
                ctx.readable = true;
            }
        }
        Ok(())
    }

    /// URL de la barre d'adresse d'un navigateur (premier contrôle « Edit » de la fenêtre).
    fn browser_url(hwnd: HWND) -> Option<String> {
        unsafe {
            let uia = automation().ok()?;
            let root = uia.ElementFromHandle(hwnd).ok()?;
            let cond = uia
                .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_EditControlTypeId.0))
                .ok()?;
            let edit = root.FindFirst(TreeScope_Descendants, &cond).ok()?;
            let value: IUIAutomationValuePattern = edit.GetCurrentPattern(UIA_ValuePatternId).ok()?.cast().ok()?;
            let v = value.CurrentValue().ok()?.to_string();
            looks_like_url(&v).then_some(v)
        }
    }

    pub fn capture() -> Result<InsertionContext> {
        with_com(|| {
            let started = Instant::now();
            let (hwnd, title, exe) = foreground_window();
            let mut ctx = InsertionContext {
                app: friendly_app_name(&exe),
                exe: exe.clone(),
                window_title: title,
                ..Default::default()
            };
            if let Err(e) = read_focused(&mut ctx) {
                log::debug!("lecture du champ focalisé impossible : {e:#}");
            }
            if is_browser(&exe) {
                let t = Instant::now();
                ctx.url = browser_url(hwnd);
                log::debug!("URL navigateur en {} ms : {:?}", t.elapsed().as_millis(), ctx.url);
            }
            ctx.capture_ms = started.elapsed().as_millis() as u64;
            Ok(ctx)
        })
    }

    pub fn verify_pasted(text: &str) -> Verification {
        let needle = normalize_ws(text);
        if needle.is_empty() {
            return Verification::Unavailable;
        }
        let result = with_com(|| {
            let uia = automation()?;
            let element = unsafe { uia.GetFocusedElement() }?;
            let budget = (needle.chars().count() as i32 + 80).min(4000);
            unsafe {
                if let Some((caret, _)) = caret_range(&element) {
                    let before = caret.Clone()?;
                    before.MoveEndpointByRange(TextPatternRangeEndpoint_End, &caret, TextPatternRangeEndpoint_Start)?;
                    let _ = before.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -budget);
                    let _ = before.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, 2);
                    let hay = normalize_ws(&range_text(&before, -1));
                    return Ok(Some(hay.contains(&needle)));
                }
                if let Ok(text) = element.GetCurrentPattern(UIA_TextPatternId).and_then(|p| p.cast::<IUIAutomationTextPattern>()) {
                    let doc = text.DocumentRange()?;
                    let hay = normalize_ws(&range_text(&doc, -1));
                    return Ok(Some(hay.contains(&needle)));
                }
                if let Ok(value) = element.GetCurrentPattern(UIA_ValuePatternId).and_then(|p| p.cast::<IUIAutomationValuePattern>()) {
                    let hay = normalize_ws(&value.CurrentValue()?.to_string());
                    return Ok(Some(hay.contains(&needle)));
                }
            }
            Ok(None)
        });
        match result {
            Ok(Some(true)) => Verification::Verified,
            Ok(Some(false)) => Verification::Missing,
            _ => Verification::Unavailable,
        }
    }
}

#[cfg(windows)]
pub use win::{capture, verify_pasted};

#[cfg(not(windows))]
pub fn capture() -> anyhow::Result<InsertionContext> {
    Ok(InsertionContext::default())
}

#[cfg(not(windows))]
pub fn verify_pasted(_text: &str) -> Verification {
    Verification::Unavailable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(before: &str, after: &str) -> InsertionContext {
        InsertionContext {
            before: before.into(),
            after: after.into(),
            caret_known: true,
            readable: true,
            ..Default::default()
        }
    }

    #[test]
    fn spacing_adds_space_after_word_and_lowercases_mid_sentence() {
        assert_eq!(smart_spacing("Bonjour à tous.", &ctx("Je disais", "")), " bonjour à tous.");
        assert_eq!(smart_spacing("Bonjour.", &ctx("Fin de phrase.", "")), " Bonjour.");
        assert_eq!(smart_spacing("Bonjour.", &ctx("Fin de phrase. ", "")), "Bonjour.");
        assert_eq!(smart_spacing("Bonjour.", &ctx("", "")), "Bonjour.");
        assert_eq!(smart_spacing("Bonjour.", &ctx("Ligne\n", "")), "Bonjour.");
    }

    #[test]
    fn spacing_keeps_acronyms_and_handles_after_text() {
        assert_eq!(smart_spacing("URL de test", &ctx("voici l'", "")), "URL de test");
        assert_eq!(smart_spacing("milieu", &ctx("", "suite")), "milieu ");
        assert_eq!(smart_spacing("milieu", &ctx("(", ")")), "milieu");
    }

    #[test]
    fn spacing_is_identity_without_caret() {
        let mut c = ctx("Je disais", "");
        c.caret_known = false;
        assert_eq!(smart_spacing("Bonjour", &c), "Bonjour");
    }

    #[test]
    fn url_heuristic() {
        assert!(looks_like_url("github.com/morluto/rea"));
        assert!(looks_like_url("https://docs.rs"));
        assert!(!looks_like_url("rechercher quelque chose"));
        assert!(!looks_like_url(""));
    }

    /// Lecture réelle du champ focalisé : lancé à la main (`cargo test -- --ignored context`).
    #[test]
    #[ignore]
    fn capture_focused_element_live() {
        let c = capture().unwrap();
        eprintln!("{c:#?}");
    }
}
