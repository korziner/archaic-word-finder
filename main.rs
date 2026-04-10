use anyhow::{anyhow, Context, Result};
use bloomfilter::Bloom;
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use rust_stemmers::{Algorithm, Stemmer};
use serde::Serialize;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write, stdin};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Поиск и валидация слов с архаичными/кастомными символами",
    after_help = r#"
=== СИМВОЛЫ ДЛЯ ПОИСКА (--symbols) ===

Литеральный режим (по умолчанию):
  --symbols "!?†"
  • Проверка по точному совпадению символов (O(1), HashSet)
  • Максимальная производительность

Режим регулярного выражения (--symbols-regex):
  --symbols '[^\p{Cyrillic}\p{Latin}]' --symbols-regex
  • Поддержка Unicode-свойств: \p{Cyrillic}, \p{Latin}, \p{Nd}, \p{P} и др.
  • Полезно для поиска иероглифов, цифр, спецсимволов, диакритики
  • Производительность: ~1.2x медленнее литерального режима, но полностью гибко

=== ПРИМЕРЫ РЕГУЛЯРОК ===
  [^\p{Cyrillic}\p{Latin}]        # Всё, кроме кириллицы и латиницы
  [\p{Han}\p{Hiragana}\p{Katakana}] # Только китайские/японские иероглифы
  [\p{Nd}\p{Sc}\p{Sm}]             # Цифры, валюты, математические символы
"#
)]
struct Args {
    /// Путь к корпусу: директория ИЛИ "-" для чтения из stdin
    #[arg(long)]
    corpus: Option<PathBuf>,

    #[arg(long, required = true)]
    dictionary: PathBuf,

    /// Шаблон символов: литерал или regex (зависит от --symbols-regex)
    #[arg(long, default_value = "!")]
    symbols: String,

    /// Интерпретировать --symbols как регулярное выражение
    #[arg(long, default_value_t = false)]
    symbols_regex: bool,

    #[arg(long, default_value = "U+0456-U+0456,U+0438-U+0438,U+0435-U+0435")]
    utf_ranges: String,

    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=2))]
    max_consecutive: u8,

    #[arg(long, default_value_t = false)]
    noprogress: bool,

    #[arg(long, default_value = "results.jsonl")]
    output: PathBuf,

    #[arg(long, default_value_t = 5000)]
    max_combinations: usize,

    #[arg(long, default_value_t = false)]
    case_insensitive_fallback: bool,

    /// Читать из stdin (альтернатива --corpus -)
    #[arg(long, conflicts_with = "corpus")]
    stdin: bool,
}

#[derive(Serialize, Debug)]
struct MatchResult {
    source_file: String,
    original_word: String,
    position: usize,
    candidate: String,
    stemmed: String,
    validation_method: String,
}

enum SymbolMatcher {
    Literal(HashSet<char>),
    Regex(Regex),
}

impl SymbolMatcher {
    fn contains(&self, c: char) -> bool {
        match self {
            Self::Literal(set) => set.contains(&c),
            Self::Regex(re) => {
                let mut buf = [0u8; 4];
                re.is_match(c.encode_utf8(&mut buf))
            }
        }
    }
}

struct Dictionary {
    exact_bloom: Bloom<String>,
    exact_set: HashSet<String>,
    ci_bloom: Option<Bloom<String>>,
    ci_set: Option<HashSet<String>>,
}

fn load_dictionary(path: &Path, ci_fallback: bool) -> Result<Dictionary> {
    let file = BufReader::new(File::open(path).context("Не удалось открыть словарь")?);
    let mut exact_words = Vec::new();
    let mut ci_words = if ci_fallback { Some(Vec::new()) } else { None };

    for line in file.lines() {
        let w = line?.trim().to_string();
        if !w.is_empty() {
            exact_words.push(w.clone());
            if let Some(ref mut ci) = ci_words {
                ci.push(w.to_lowercase());
            }
        }
    }

    if exact_words.is_empty() {
        return Err(anyhow!("Словарь пуст"));
    }

    let capacity = exact_words.len();
    let fpr = 0.001_f64;
    let bits = ((-(capacity as f64) * fpr.ln()) / (0.693147_f64.powi(2))).ceil() as usize;

    let mut exact_bloom = Bloom::new(bits, capacity);
    for w in &exact_words { exact_bloom.set(w); }

    let (ci_bloom, ci_set) = if let Some(ci) = ci_words {
        let mut ci_b = Bloom::new(bits, capacity);
        for w in &ci { ci_b.set(w); }
        (Some(ci_b), Some(ci.into_iter().collect()))
    } else {
        (None, None)
    };

    Ok(Dictionary {
        exact_bloom,
        exact_set: exact_words.into_iter().collect(),
        ci_bloom,
        ci_set,
    })
}

fn parse_utf_ranges(range_str: &str) -> Result<Vec<char>> {
    let mut chars = Vec::new();
    for part in range_str.split(',') {
        let part = part.trim();
        if let Some((start_hex, end_hex)) = part.split_once('-') {
            let start = u32::from_str_radix(start_hex.trim_start_matches("U+"), 16)?;
            let end = u32::from_str_radix(end_hex.trim_start_matches("U+"), 16)?;
            for cp in start..=end {
                if let Some(c) = char::from_u32(cp) { chars.push(c); }
            }
        } else {
            let cp = u32::from_str_radix(part.trim_start_matches("U+"), 16)?;
            if let Some(c) = char::from_u32(cp) { chars.push(c); }
        }
    }
    Ok(chars)
}

fn generate_candidates(
    word: &str,
    matcher: &SymbolMatcher,
    replacements: &[char],
    max_consecutive: u8,
    limit: usize,
) -> Vec<(String, usize)> {
    let mut candidates = Vec::new();
    let chars: Vec<char> = word.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if matcher.contains(chars[i]) {
            let mut run_len = 1;
            while i + run_len < chars.len() && run_len < max_consecutive as usize && matcher.contains(chars[i + run_len]) {
                run_len += 1;
            }
            if run_len == 1 || run_len == 2 {
                let prefix: String = chars[..i].iter().collect();
                let suffix: String = chars[i + run_len..].iter().collect();
                if run_len == 1 {
                    for &rep in replacements {
                        candidates.push((format!("{}{}{}", prefix, rep, suffix), i));
                        if candidates.len() >= limit { return candidates; }
                    }
                } else {
                    for &r1 in replacements {
                        for &r2 in replacements {
                            candidates.push((format!("{}{}{}{}", prefix, r1, r2, suffix), i));
                            if candidates.len() >= limit { return candidates; }
                        }
                    }
                }
                i += run_len;
                continue;
            }
        }
        i += 1;
    }
    candidates
}

fn process_text(
    text: &str,
    source_label: &str,
    dict: &Arc<Dictionary>,
    stemmer: &Stemmer,
    matcher: &SymbolMatcher,
    replacements: &[char],
    max_consecutive: u8,
    max_combinations: usize,
) -> Vec<MatchResult> {
    let mut results = Vec::new();
    for (word, pos) in text.split_whitespace().filter(|w| !w.is_empty()).zip(0usize..) {
        let clean_word: String = word.chars().filter(|c| c.is_alphabetic() || matcher.contains(*c)).collect();
        if clean_word.len() < 2 || !clean_word.chars().any(|c| matcher.contains(c)) {
            continue;
        }

        let candidates = generate_candidates(&clean_word, matcher, replacements, max_consecutive, max_combinations);
        for (candidate, _rel_pos) in candidates {
            let bloom_pass = dict.exact_bloom.check(&candidate)
                || dict.ci_bloom.as_ref().map_or(false, |b| b.check(&candidate.to_lowercase()));
            if !bloom_pass { continue; }

            let lower = candidate.to_lowercase();
            let exact_match = dict.exact_set.contains(&candidate)
                || dict.ci_set.as_ref().map_or(false, |s| s.contains(&lower));
            let stemmed = stemmer.stem(&lower);
            let stem_match = dict.exact_set.contains(stemmed.as_ref())
                || dict.ci_set.as_ref().map_or(false, |s| s.contains(stemmed.as_ref()));

            if exact_match || stem_match {
                results.push(MatchResult {
                    source_file: source_label.to_string(),
                    original_word: clean_word.clone(),
                    position: pos,
                    candidate,
                    stemmed: stemmed.to_string(),
                    validation_method: if exact_match { "exact".to_string() } else { "stemmed".to_string() },
                });
            }
        }
    }
    results
}

fn process_stdin_stream(
    dict: Arc<Dictionary>,
    stemmer: Arc<Stemmer>,
    matcher: Arc<SymbolMatcher>,
    replacements: Vec<char>,
    args: &Args,
) -> Result<()> {
    let stdin_handle = stdin();
    let reader = BufReader::new(stdin_handle.lock());
    let lines: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
    let total_lines = lines.len() as u64;

    let pb = if !args.noprogress {
        let pb = ProgressBar::new(total_lines);
        pb.set_style(ProgressStyle::default_bar()
            .template("[{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} строк | {per_sec} | {msg}")
            .unwrap());
        pb.set_message("Обработка stdin...");
        Some(pb)
    } else { None };

    let out_file = Arc::new(std::sync::Mutex::new(BufWriter::new(
        OpenOptions::new().create(true).write(true).truncate(true).open(&args.output)?,
    )));

    let pb_clone = pb.as_ref().map(|p| p.clone());
    let chunk_size = std::cmp::max(50, lines.len() / (rayon::current_num_threads() * 4));

    lines.par_chunks(chunk_size).for_each(|chunk| {
        let mut chunk_results = Vec::new();
        let base_idx = (chunk.as_ptr() as usize - lines.as_ptr() as usize) / std::mem::size_of::<String>();

        for (local_idx, line) in chunk.iter().enumerate() {
            let source_label = format!("stdin:line_{}", base_idx + local_idx + 1);
            let results = process_text(line, &source_label, &dict, &stemmer, &matcher, &replacements, args.max_consecutive, args.max_combinations);
            chunk_results.extend(results);
        }

        if !chunk_results.is_empty() {
            if let Ok(mut writer) = out_file.lock() {
                for res in chunk_results {
                    let _ = serde_json::to_writer(&mut *writer, &res);
                    let _ = writer.write_all(b"\n");
                }
                let _ = writer.flush();
            }
        }

        if let Some(ref pb) = pb_clone {
            pb.inc(chunk.len() as u64);
        }
    });

    if let Some(ref pb) = pb {
        pb.finish_with_message("stdin: завершено");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let use_stdin = args.stdin || args.corpus.as_ref().map_or(false, |p| p.to_str() == Some("-"));

    let matcher = if args.symbols_regex {
        Arc::new(SymbolMatcher::Regex(
            Regex::new(&args.symbols).context("Не удалось скомпилировать регулярное выражение для --symbols")?
        ))
    } else {
        Arc::new(SymbolMatcher::Literal(args.symbols.chars().collect()))
    };

    let ci_mode = args.case_insensitive_fallback;
    println!("Инициализация словаря (гибридный={})...", ci_mode);
    let dict = Arc::new(load_dictionary(&args.dictionary, ci_mode)?);
    let replacements = parse_utf_ranges(&args.utf_ranges)?;
    if replacements.is_empty() {
        return Err(anyhow!("Список символов для замены пуст"));
    }
    let stemmer = Arc::new(Stemmer::create(Algorithm::Russian));

    if use_stdin {
        println!("Режим: чтение из stdin...");
        let start = Instant::now();
        process_stdin_stream(dict, stemmer, matcher, replacements, &args)?;
        println!("\nРезультаты сохранены в: {}", args.output.display());
        println!("Затраченное время: {:.2} сек", start.elapsed().as_secs_f64());
        return Ok(());
    }

    let corpus_path = args.corpus.as_ref().unwrap();
    println!("Сканирование корпуса: {}...", corpus_path.display());
    let files: Vec<PathBuf> = WalkDir::new(corpus_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| matches!(ext.to_str(), Some("txt" | "text" | "utf" | "md"))))
        .map(|e| e.into_path())
        .collect();

    if files.is_empty() {
        println!("Файлы не найдены.");
        return Ok(());
    }

    let pb = ProgressBar::new(files.len() as u64);
    if !args.noprogress {
        pb.set_style(ProgressStyle::default_bar()
            .template("[{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} файлов | {msg}")
            .unwrap());
        pb.set_message("Обработка...");
    }

    let start = Instant::now();
    let out_file = Arc::new(std::sync::Mutex::new(BufWriter::new(
        OpenOptions::new().create(true).write(true).truncate(true).open(&args.output)?,
    )));

    files.par_iter().for_each(|file_path| {
        let file = match File::open(file_path) {
            Ok(f) => f,
            Err(_) => { if !args.noprogress { pb.inc(1); } return; }
        };
        let reader = BufReader::new(file);
        let mut local_results: Vec<MatchResult> = Vec::new();

        for (line_idx, line) in reader.lines().enumerate() {
            let line = match line { Ok(l) => l, Err(_) => continue };
            let source_label = format!("{}:line_{}", file_path.display(), line_idx + 1);
            let results = process_text(&line, &source_label, &dict, &stemmer, &matcher, &replacements, args.max_consecutive, args.max_combinations);
            local_results.extend(results);
        }

        if !local_results.is_empty() {
            if let Ok(mut writer) = out_file.lock() {
                for res in local_results {
                    let _ = serde_json::to_writer(&mut *writer, &res);
                    let _ = writer.write_all(b"\n");
                }
                let _ = writer.flush();
            }
        }
        if !args.noprogress { pb.inc(1); }
    });

    if !args.noprogress { pb.finish_with_message("Обработка завершена."); }
    println!("\nРезультаты сохранены в: {}", args.output.display());
    println!("Затраченное время: {:.2} сек", start.elapsed().as_secs_f64());
    Ok(())
}
