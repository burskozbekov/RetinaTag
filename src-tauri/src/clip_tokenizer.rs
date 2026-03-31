// clip_tokenizer.rs — Byte-Pair Encoding tokenizer for CLIP
//
// Compatible with openai/clip-vit-base-patch32 and clip-vit-large-patch14.
// Expects two files:
//   clip_vocab.json   — {"token": id, ...}  (~400 KB)
//   clip_merges.txt   — BPE merge rules, one "a b" per line (~800 KB)

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;

const SOT_TOKEN: i64 = 49406; // <|startoftext|>
const EOT_TOKEN: i64 = 49407; // <|endoftext|>
const MAX_SEQ:   usize = 77;

pub struct ClipTokenizer {
    vocab:  HashMap<String, i64>,
    merges: HashMap<(String, String), usize>, // pair → merge rank
    byte_enc: [char; 256],                    // byte → unicode char
}

impl ClipTokenizer {
    pub fn load(models_dir: &Path) -> Result<Self> {
        let vocab_path  = models_dir.join("clip_vocab.json");
        let merges_path = models_dir.join("clip_merges.txt");

        let vocab_json = std::fs::read_to_string(&vocab_path)
            .with_context(|| format!("clip_vocab.json bulunamadı: {:?}", vocab_path))?;
        let vocab: HashMap<String, i64> = serde_json::from_str(&vocab_json)
            .context("clip_vocab.json parse edilemedi")?;

        let merges_text = std::fs::read_to_string(&merges_path)
            .with_context(|| format!("clip_merges.txt bulunamadı: {:?}", merges_path))?;

        let merges: HashMap<(String, String), usize> = merges_text
            .lines()
            .enumerate()
            .filter(|(i, l)| *i > 0 && !l.is_empty() && !l.starts_with('#'))
            .filter_map(|(i, l)| {
                let mut parts = l.splitn(2, ' ');
                let a = parts.next()?.to_string();
                let b = parts.next()?.to_string();
                Some(((a, b), i))
            })
            .collect();

        let byte_enc = build_byte_encoder();

        Ok(ClipTokenizer { vocab, merges, byte_enc })
    }

    /// Tokenize `text` into a fixed-length [MAX_SEQ] array of token IDs.
    /// Returns (input_ids, attention_mask) each of length MAX_SEQ.
    pub fn encode(&self, text: &str) -> (Vec<i64>, Vec<i64>) {
        let text = text.trim().to_lowercase();

        let mut tokens: Vec<i64> = vec![SOT_TOKEN];

        // Very simple word split (matches CLIP's original whitespace tokenisation)
        for word in text.split_whitespace() {
            let word_tokens = self.bpe(word);
            tokens.extend(word_tokens);
        }
        tokens.push(EOT_TOKEN);

        // Truncate
        if tokens.len() > MAX_SEQ {
            tokens.truncate(MAX_SEQ - 1);
            tokens.push(EOT_TOKEN);
        }

        let real_len = tokens.len();
        // Pad
        tokens.resize(MAX_SEQ, 0);

        let mask: Vec<i64> = (0..MAX_SEQ)
            .map(|i| if i < real_len { 1 } else { 0 })
            .collect();

        (tokens, mask)
    }

    fn bpe(&self, word: &str) -> Vec<i64> {
        // Convert each byte to its unicode representative character
        let mut syms: Vec<String> = word
            .bytes()
            .map(|b| self.byte_enc[b as usize].to_string())
            .collect();

        // Append </w> to the last symbol
        if let Some(last) = syms.last_mut() {
            *last = format!("{}</w>", last);
        }

        // Iteratively apply best (lowest rank) merge
        loop {
            if syms.len() < 2 {
                break;
            }
            let best = syms
                .windows(2)
                .enumerate()
                .filter_map(|(i, w)| {
                    self.merges
                        .get(&(w[0].clone(), w[1].clone()))
                        .map(|&rank| (i, rank))
                })
                .min_by_key(|&(_, rank)| rank);

            match best {
                None => break,
                Some((idx, _)) => {
                    let merged = format!("{}{}", syms[idx], syms[idx + 1]);
                    syms[idx] = merged;
                    syms.remove(idx + 1);
                }
            }
        }

        syms.iter()
            .map(|s| *self.vocab.get(s.as_str()).unwrap_or(&0))
            .collect()
    }
}

/// Builds CLIP's byte-to-unicode mapping (identical to the Python reference).
fn build_byte_encoder() -> [char; 256] {
    // Printable ASCII + Latin-1 supplement bytes get mapped to themselves.
    // The remaining 68 bytes get mapped to characters starting at U+0100.
    let mut result = ['\0'; 256];
    let mut n = 0u32;

    for b in 0u8..=255u8 {
        let is_printable = (b >= b'!' && b <= b'~')
            || (b >= 0xA1 && b <= 0xAC)
            || (b >= 0xAE);

        if is_printable {
            result[b as usize] = b as char;
        } else {
            result[b as usize] = char::from_u32(256 + n).unwrap_or('?');
            n += 1;
        }
    }
    result
}
