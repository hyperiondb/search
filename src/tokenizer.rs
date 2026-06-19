use std::collections::HashSet;
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use unicode_normalization::UnicodeNormalization;

pub const DEFAULT_MIN_GRAM: usize = 2;
pub const DEFAULT_MAX_GRAM: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NgramConfig {
    pub min_gram: usize,
    pub max_gram: usize,
    pub ascii_folding: bool,
}

impl Default for NgramConfig {
    fn default() -> Self {
        NgramConfig {
            min_gram: DEFAULT_MIN_GRAM,
            max_gram: DEFAULT_MAX_GRAM,
            ascii_folding: true,
        }
    }
}

pub fn ascii_fold_lower(input: &str, ascii_folding: bool) -> String {
    let lowered = input.to_lowercase();
    if !ascii_folding {
        return lowered;
    }
    let mut out = String::with_capacity(lowered.len());
    for ch in lowered.nfkd() {
        if is_combining_mark(ch) {
            continue;
        }
        match ch {
            'ł' => out.push('l'),
            'đ' => out.push('d'),
            'ø' => out.push('o'),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ß' => out.push_str("ss"),
            'þ' => out.push_str("th"),
            'ð' => out.push('d'),
            _ => out.push(ch),
        }
    }
    out
}

fn is_combining_mark(ch: char) -> bool {
    matches!(ch as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric()
}

fn split_words(folded: &str) -> Vec<String> {
    folded
        .split(|c: char| !is_word_char(c))
        .filter(|w| !w.is_empty())
        .map(|w| w.to_string())
        .collect()
}

pub fn ngrams_of_word(word: &str, cfg: &NgramConfig, out: &mut Vec<String>) {
    let chars: Vec<char> = word.chars().collect();
    let len = chars.len();
    if len == 0 {
        return;
    }
    if len < cfg.min_gram {
        out.push(chars.iter().collect());
        return;
    }
    for n in cfg.min_gram..=cfg.max_gram {
        if n > len {
            break;
        }
        for start in 0..=(len - n) {
            let gram: String = chars[start..start + n].iter().collect();
            out.push(gram);
        }
    }
}

pub fn ngram_tokens(text: &str, cfg: &NgramConfig) -> Vec<String> {
    let folded = ascii_fold_lower(text, cfg.ascii_folding);
    let mut out = Vec::new();
    for word in split_words(&folded) {
        ngrams_of_word(&word, cfg, &mut out);
    }
    out
}

pub fn ngram_set(text: &str, cfg: &NgramConfig) -> HashSet<String> {
    ngram_tokens(text, cfg).into_iter().collect()
}

pub fn ngram_match(haystack: &str, needle: &str, cfg: &NgramConfig) -> bool {
    let needle_grams = ngram_set(needle, cfg);
    if needle_grams.is_empty() {
        return false;
    }
    let hay_grams = ngram_set(haystack, cfg);
    needle_grams.iter().all(|g| hay_grams.contains(g))
}

pub fn pack_typmod(parts: &[String]) -> Result<i32, String> {
    let min: usize = parts
        .first()
        .map(|s| s.trim().parse().map_err(|_| "min_gram must be an integer".to_string()))
        .unwrap_or(Ok(DEFAULT_MIN_GRAM))?;
    let max: usize = parts
        .get(1)
        .map(|s| s.trim().parse().map_err(|_| "max_gram must be an integer".to_string()))
        .unwrap_or(Ok(DEFAULT_MAX_GRAM))?;
    let mut ascii_folding = true;
    if let Some(opts) = parts.get(2) {
        for kv in opts.split([';', ',']) {
            let mut it = kv.splitn(2, '=');
            let k = it.next().unwrap_or("").trim().to_lowercase();
            let v = it.next().unwrap_or("").trim().to_lowercase();
            if k == "ascii_folding" {
                ascii_folding = v != "false" && v != "0" && v != "off";
            }
        }
    }
    if min < 1 || min > 255 {
        return Err(format!("min_gram out of range: {min}"));
    }
    if max < min || max > 255 {
        return Err(format!("max_gram must be between min_gram and 255: {max}"));
    }
    Ok(((min as i32) << 16) | ((max as i32) << 8) | (ascii_folding as i32))
}

pub fn unpack_typmod(tm: i32) -> NgramConfig {
    if tm < 0 {
        return NgramConfig::default();
    }
    NgramConfig {
        min_gram: ((tm >> 16) & 0xFF) as usize,
        max_gram: ((tm >> 8) & 0xFF) as usize,
        ascii_folding: (tm & 1) != 0,
    }
}

#[derive(Clone)]
pub struct AsciiNgramTokenizer {
    pub cfg: NgramConfig,
}

impl AsciiNgramTokenizer {
    pub fn new(cfg: NgramConfig) -> Self {
        AsciiNgramTokenizer { cfg }
    }
}

pub struct VecTokenStream {
    tokens: Vec<Token>,
    idx: usize,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.idx < self.tokens.len() {
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.idx - 1]
    }
}

impl Tokenizer for AsciiNgramTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let grams = ngram_tokens(text, &self.cfg);
        let mut tokens = Vec::with_capacity(grams.len());
        for (position, text) in grams.into_iter().enumerate() {
            let len = text.len();
            tokens.push(Token {
                offset_from: 0,
                offset_to: len,
                position,
                text,
                position_length: 1,
            });
        }
        VecTokenStream { tokens, idx: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> NgramConfig {
        NgramConfig::default()
    }

    #[test]
    fn folds_lithuanian_diacritics() {
        assert_eq!(ascii_fold_lower("Ąžuolas", true), "azuolas");
        assert_eq!(ascii_fold_lower("ČĘĖĮŠŲŪŽ", true), "ceeisuuz");
        assert_eq!(ascii_fold_lower("Kavinė", true), "kavine");
    }

    #[test]
    fn folds_common_latin_extras() {
        assert_eq!(ascii_fold_lower("Łódź", true), "lodz");
        assert_eq!(ascii_fold_lower("Straße", true), "strasse");
        assert_eq!(ascii_fold_lower("Œuvre", true), "oeuvre");
    }

    #[test]
    fn lowercases_when_folding_disabled() {
        assert_eq!(ascii_fold_lower("Ąžuolas", false), "ąžuolas");
    }

    #[test]
    fn ngrams_basic_word() {
        let mut out = Vec::new();
        ngrams_of_word("kava", &cfg(), &mut out);
        assert!(out.contains(&"ka".to_string()));
        assert!(out.contains(&"av".to_string()));
        assert!(out.contains(&"va".to_string()));
        assert!(out.contains(&"kava".to_string()));
        assert!(out.contains(&"kav".to_string()));
        assert!(out.contains(&"ava".to_string()));
        assert!(!out.contains(&"k".to_string()));
    }

    #[test]
    fn short_word_below_min_kept_whole() {
        let mut out = Vec::new();
        ngrams_of_word("a", &cfg(), &mut out);
        assert_eq!(out, vec!["a".to_string()]);
    }

    #[test]
    fn max_gram_capped_at_five() {
        let mut out = Vec::new();
        ngrams_of_word("aparatas", &cfg(), &mut out);
        assert!(out.iter().all(|g| g.chars().count() <= 5));
        assert!(out.contains(&"apara".to_string()));
        assert!(!out.contains(&"aparat".to_string()));
    }

    #[test]
    fn tokens_split_on_whitespace_and_punct() {
        let toks = ngram_set("Kavos aparatas", &cfg());
        assert!(toks.contains("ka"));
        assert!(toks.contains("ap"));
        assert!(!toks.contains("s "));
        assert!(!toks.contains("s a"));
    }

    #[test]
    fn match_accent_insensitive() {
        assert!(ngram_match("Ąžuolų baldai", "azuol", &cfg()));
        assert!(ngram_match("Kavos aparatas", "kavos", &cfg()));
        assert!(ngram_match("Kavos aparatas", "apar", &cfg()));
    }

    #[test]
    fn match_is_conjunctive_substring() {
        assert!(!ngram_match("Telefonas Samsung", "canon", &cfg()));
        assert!(!ngram_match("Kavos aparatas", "kava", &cfg()));
        assert!(ngram_match("Canon EOS fotoaparatas", "canon", &cfg()));
        assert!(ngram_match("Sony PlayStation 5", "playstation", &cfg()));
    }

    #[test]
    fn match_case_insensitive() {
        assert!(ngram_match("HELLO World", "hello", &cfg()));
    }

    #[test]
    fn no_match_disjoint() {
        assert!(!ngram_match("kavos aparatas", "zzzz", &cfg()));
    }

    #[test]
    fn two_char_query_matches_substring() {
        assert!(ngram_match("kaina", "ka", &cfg()));
        assert!(ngram_match("sukamasis", "ka", &cfg()));
    }

    #[test]
    fn typmod_roundtrip() {
        let tm = pack_typmod(&[
            "2".to_string(),
            "5".to_string(),
            "ascii_folding=true".to_string(),
        ])
        .unwrap();
        let cfg = unpack_typmod(tm);
        assert_eq!(cfg.min_gram, 2);
        assert_eq!(cfg.max_gram, 5);
        assert!(cfg.ascii_folding);
        assert!(tm >= 0);
    }

    #[test]
    fn typmod_ascii_folding_false() {
        let tm = pack_typmod(&[
            "3".to_string(),
            "4".to_string(),
            "ascii_folding=false".to_string(),
        ])
        .unwrap();
        let cfg = unpack_typmod(tm);
        assert_eq!(cfg.min_gram, 3);
        assert_eq!(cfg.max_gram, 4);
        assert!(!cfg.ascii_folding);
    }

    #[test]
    fn typmod_rejects_bad_range() {
        assert!(pack_typmod(&["5".to_string(), "2".to_string(), "".to_string()]).is_err());
        assert!(pack_typmod(&["0".to_string(), "5".to_string(), "".to_string()]).is_err());
    }

    #[test]
    fn tantivy_tokenizer_emits_same_tokens() {
        let mut tk = AsciiNgramTokenizer::new(cfg());
        let mut stream = tk.token_stream("Kava");
        let mut emitted = Vec::new();
        while stream.advance() {
            emitted.push(stream.token().text.clone());
        }
        let mut expected = Vec::new();
        ngrams_of_word("kava", &cfg(), &mut expected);
        assert_eq!(emitted, expected);
    }
}
