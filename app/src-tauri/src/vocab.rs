//! Vocabulaire personnel : termes à reconnaître, remplacements et snippets.
//!
//! - `term` : mot ou nom propre à respecter (amorce whisper + consigne Ollama) ;
//! - `replacement` : quand la transcription contient `phrase`, écrire `target` ;
//! - `snippet` : quand la dictée contient le déclencheur `phrase`, insérer `target`
//!   (texte libre, multi-ligne possible).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VocabKind {
    Term,
    Replacement,
    Snippet,
}

impl VocabKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VocabKind::Term => "term",
            VocabKind::Replacement => "replacement",
            VocabKind::Snippet => "snippet",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "term" => Some(VocabKind::Term),
            "replacement" => Some(VocabKind::Replacement),
            "snippet" => Some(VocabKind::Snippet),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VocabEntry {
    pub id: String,
    pub kind: VocabKind,
    /// Terme, phrase à remplacer ou déclencheur du snippet.
    pub phrase: String,
    /// Remplacement ou texte du snippet (vide pour un terme).
    pub target: String,
    pub note: String,
    pub uses: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Résultat d'une application de remplacements / snippets.
#[derive(Debug, Default)]
pub struct Applied {
    pub text: String,
    /// Identifiants des entrées utilisées (pour les compteurs).
    pub used_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Vocabulary {
    pub entries: Vec<VocabEntry>,
}

impl Vocabulary {
    pub fn new(entries: Vec<VocabEntry>) -> Self {
        Self { entries }
    }

    /// Termes à faire figurer dans l'amorce whisper, les plus utilisés d'abord,
    /// dans une limite de caractères.
    pub fn terms_hint(&self, max_chars: usize) -> String {
        let mut terms: Vec<&VocabEntry> = self
            .entries
            .iter()
            .filter(|e| !e.phrase.trim().is_empty())
            .collect();
        terms.sort_by(|a, b| b.uses.cmp(&a.uses).then_with(|| a.phrase.cmp(&b.phrase)));
        let mut out = String::new();
        for e in terms {
            // Pour un remplacement, c'est la forme cible qu'on veut voir écrite.
            let word = match e.kind {
                VocabKind::Replacement if !e.target.trim().is_empty() => e.target.trim(),
                VocabKind::Snippet => continue,
                _ => e.phrase.trim(),
            };
            let candidate = if out.is_empty() { word.to_string() } else { format!("{out}, {word}") };
            if candidate.chars().count() > max_chars {
                break;
            }
            out = candidate;
        }
        out
    }

    /// Liste des termes pour le prompt de mise en forme (vide s'il n'y en a pas).
    pub fn terms_for_prompt(&self, max: usize) -> Vec<String> {
        let mut terms: Vec<&VocabEntry> = self
            .entries
            .iter()
            .filter(|e| e.kind != VocabKind::Snippet && !e.phrase.trim().is_empty())
            .collect();
        terms.sort_by(|a, b| b.uses.cmp(&a.uses));
        terms
            .into_iter()
            .take(max)
            .map(|e| match e.kind {
                VocabKind::Replacement if !e.target.trim().is_empty() => e.target.trim().to_string(),
                _ => e.phrase.trim().to_string(),
            })
            .collect()
    }

    pub fn apply_replacements(&self, text: &str) -> Applied {
        self.apply(text, VocabKind::Replacement)
    }

    pub fn apply_snippets(&self, text: &str) -> Applied {
        self.apply(text, VocabKind::Snippet)
    }

    /// Une seule passe sur le texte : à chaque position, la phrase la plus longue qui
    /// correspond l'emporte, et le texte déjà remplacé n'est jamais re-analysé.
    fn apply(&self, text: &str, kind: VocabKind) -> Applied {
        let mut entries: Vec<&VocabEntry> = self
            .entries
            .iter()
            .filter(|e| e.kind == kind && !e.phrase.trim().is_empty())
            .collect();
        if entries.is_empty() {
            return Applied {
                text: text.to_string(),
                used_ids: Vec::new(),
            };
        }
        entries.sort_by_key(|e| std::cmp::Reverse(e.phrase.chars().count()));
        let needles: Vec<(Vec<char>, &VocabEntry)> = entries
            .iter()
            .map(|e| (e.phrase.trim().chars().map(fold).collect(), *e))
            .collect();

        let hay: Vec<char> = text.chars().collect();
        let folded: Vec<char> = hay.iter().map(|&c| fold(c)).collect();
        let mut out = String::with_capacity(text.len());
        let mut used: Vec<String> = Vec::new();
        let mut i = 0;
        while i < hay.len() {
            let at_word_start = i == 0 || !is_word_char(hay[i - 1]);
            let hit = if at_word_start {
                needles.iter().find(|(needle, _)| {
                    let end = i + needle.len();
                    end <= hay.len() && folded[i..end] == needle[..] && (end == hay.len() || !is_word_char(hay[end]))
                })
            } else {
                None
            };
            match hit {
                Some((needle, e)) => {
                    out.push_str(&e.target);
                    if !used.contains(&e.id) {
                        used.push(e.id.clone());
                    }
                    i += needle.len();
                    if target_absorbs_punctuation(&e.target) && i < hay.len() && is_sentence_punctuation(hay[i]) {
                        i += 1;
                    }
                }
                None => {
                    out.push(hay[i]);
                    i += 1;
                }
            }
        }
        Applied { text: out, used_ids: used }
    }
}

fn target_absorbs_punctuation(target: &str) -> bool {
    target
        .chars()
        .last()
        .map(|c| c.is_ascii_punctuation() || c == '\n')
        .unwrap_or(false)
}

fn is_sentence_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?')
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Remplace chaque occurrence de `phrase` (insensible à la casse, mots entiers) par
/// `target`. Si `target` se termine par une ponctuation ou un saut de ligne, une
/// ponctuation qui suivait immédiatement l'occurrence est absorbée.
/// Version mono-phrase, utilisée par les tests ; le pipeline passe par `Vocabulary::apply`.
#[cfg(test)]
pub fn replace_phrase_ci(text: &str, phrase: &str, target: &str) -> (String, usize) {
    let hay: Vec<char> = text.chars().collect();
    let needle: Vec<char> = phrase.chars().map(fold).collect();
    if needle.is_empty() || hay.len() < needle.len() {
        return (text.to_string(), 0);
    }
    let folded: Vec<char> = hay.iter().map(|&c| fold(c)).collect();
    let absorbs_punctuation = target_absorbs_punctuation(target);

    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut count = 0;
    while i < hay.len() {
        let end = i + needle.len();
        let matches = end <= hay.len()
            && folded[i..end] == needle[..]
            && (i == 0 || !is_word_char(hay[i - 1]))
            && (end == hay.len() || !is_word_char(hay[end]));
        if matches {
            out.push_str(target);
            count += 1;
            i = end;
            if absorbs_punctuation && i < hay.len() && is_sentence_punctuation(hay[i]) {
                i += 1;
            }
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: VocabKind, phrase: &str, target: &str) -> VocabEntry {
        VocabEntry {
            id: format!("{}:{phrase}", kind.as_str()),
            kind,
            phrase: phrase.into(),
            target: target.into(),
            note: String::new(),
            uses: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn replacement_is_case_insensitive_and_whole_word() {
        let (out, n) = replace_phrase_ci("dicté, Dicté et dictée ; indicté", "dicté", "Dictae");
        assert_eq!(out, "Dictae, Dictae et dictée ; indicté");
        assert_eq!(n, 2);
        let (out, n) = replace_phrase_ci("over consulting et Over Consulting.", "over consulting", "Overconsulting");
        assert_eq!(out, "Overconsulting et Overconsulting.");
        assert_eq!(n, 2);
        assert_eq!(replace_phrase_ci("rien", "", "x"), ("rien".into(), 0));
    }

    #[test]
    fn snippet_absorbs_following_punctuation_when_target_ends_with_one() {
        let (out, _) = replace_phrase_ci("Voilà ma signature.", "ma signature", "Cordialement,\nLaurent\n");
        assert_eq!(out, "Voilà Cordialement,\nLaurent\n");
        let (out, _) = replace_phrase_ci("Voilà ma signature.", "ma signature", "Laurent");
        assert_eq!(out, "Voilà Laurent.");
    }

    #[test]
    fn vocabulary_applies_longest_first_and_counts_uses() {
        let v = Vocabulary::new(vec![
            entry(VocabKind::Replacement, "rea", "REA"),
            entry(VocabKind::Replacement, "rea agents", "rea-agents"),
            entry(VocabKind::Term, "Dictae", ""),
            entry(VocabKind::Snippet, "bloc adresse", "12 rue des Lilas\n75000 Paris"),
        ]);
        let a = v.apply_replacements("j'utilise rea agents et rea");
        assert_eq!(a.text, "j'utilise rea-agents et REA");
        assert_eq!(a.used_ids.len(), 2);
        let s = v.apply_snippets("Mon adresse : bloc adresse merci");
        assert_eq!(s.text, "Mon adresse : 12 rue des Lilas\n75000 Paris merci");
        assert_eq!(s.used_ids, vec!["snippet:bloc adresse".to_string()]);
    }

    #[test]
    fn hints_prefer_most_used_and_respect_limit() {
        let mut a = entry(VocabKind::Term, "Alpha", "");
        a.uses = 1;
        let mut b = entry(VocabKind::Replacement, "beta", "Bêta");
        b.uses = 5;
        let c = entry(VocabKind::Snippet, "sig", "…");
        let v = Vocabulary::new(vec![a, b, c]);
        assert_eq!(v.terms_hint(100), "Bêta, Alpha");
        assert_eq!(v.terms_hint(5), "Bêta");
        assert_eq!(v.terms_for_prompt(10), vec!["Bêta".to_string(), "Alpha".to_string()]);
        assert_eq!(Vocabulary::default().terms_hint(50), "");
    }
}
