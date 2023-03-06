use crate::{repl::ReplyStreamEvent, utils::dump};
use anyhow::Result;
use crossbeam::channel::Receiver;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::as_24_bit_terminal_escaped;
use syntect::{easy::HighlightLines, parsing::SyntaxReference};

pub fn render_stream(rx: Receiver<ReplyStreamEvent>, ctrlc: Arc<AtomicBool>) -> Result<()> {
    let mut buffer = String::new();
    let mut markdown_render = MarkdownRender::new();
    loop {
        if ctrlc.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Ok(evt) = rx.try_recv() {
            match evt {
                ReplyStreamEvent::Text(text) => {
                    if text.contains('\n') {
                        let text = format!("{buffer}{text}");
                        let mut lines: Vec<&str> = text.split('\n').collect();
                        buffer = lines.pop().unwrap_or_default().to_string();
                        let output = lines.join("\n");
                        dump(markdown_render.render(&output), 1);
                    } else {
                        buffer = format!("{buffer}{text}");
                    }
                }
                ReplyStreamEvent::Done => {
                    let output = markdown_render.render(&buffer);
                    dump(output, 2);
                    break;
                }
            }
        }
    }

    Ok(())
}

pub struct MarkdownRender {
    syntax_set: SyntaxSet,
    theme: Theme,
    md_syntax: SyntaxReference,
    code_syntax: Option<SyntaxReference>,
    code_block: bool,
}

impl MarkdownRender {
    pub fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults().themes["Solarized (dark)"].clone();
        let md_syntax = syntax_set.find_syntax_by_extension("md").unwrap().clone();
        Self {
            syntax_set,
            theme,
            md_syntax,
            code_syntax: None,
            code_block: false,
        }
    }

    pub fn render(&mut self, src: &str) -> String {
        src.split('\n')
            .map(|line| self.render_line(line).unwrap_or_else(|| line.to_string()))
            .collect::<Vec<String>>()
            .join("\n")
    }

    pub fn render_line(&mut self, line: &str) -> Option<String> {
        if let Some(lang) = detect_code_block(line) {
            if self.code_block {
                self.code_block = false;
                self.code_syntax = None;
            } else {
                self.code_block = true;
                if !lang.is_empty() {
                    self.code_syntax = self.find_syntax(&lang).cloned();
                }
            }
            self.render_line_inner(line, &self.md_syntax)
        } else if self.code_block {
            self.code_syntax
                .as_ref()
                .and_then(|syntax| self.render_line_inner(line, syntax))
        } else {
            self.render_line_inner(line, &self.md_syntax)
        }
    }

    fn render_line_inner(&self, line: &str, syntax: &SyntaxReference) -> Option<String> {
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let ranges = highlighter.highlight_line(line, &self.syntax_set).ok()?;
        Some(as_24_bit_terminal_escaped(&ranges[..], false))
    }

    fn find_syntax(&self, lang: &str) -> Option<&SyntaxReference> {
        self.syntax_set.find_syntax_by_extension(lang).or_else(|| {
            LANGEGUATE_NAME_EXTS
                .iter()
                .find(|(name, _)| *name == lang.to_lowercase())
                .and_then(|(_, ext)| self.syntax_set.find_syntax_by_extension(ext))
        })
    }
}

const LANGEGUATE_NAME_EXTS: [(&str, &str); 21] = [
    ("asp", "asa"),
    ("actionscript", "as"),
    ("c#", "cs"),
    ("clojure", "clj"),
    ("erlang", "erl"),
    ("haskell", "hs"),
    ("javascript", "js"),
    ("bibtex", "bib"),
    ("latex", "tex"),
    ("tex", "sty"),
    ("ocaml", "ml"),
    ("ocamllex", "mll"),
    ("ocamlyacc", "mly"),
    ("objective-c++", "mm"),
    ("objective-c", "m"),
    ("pascal", "pas"),
    ("perl", "pl"),
    ("python", "py"),
    ("restructuredtext", "rst"),
    ("ruby", "rb"),
    ("rust", "rs"),
];

fn detect_code_block(line: &str) -> Option<String> {
    if !line.starts_with("```") {
        return None;
    }
    let lang = line
        .chars()
        .skip(3)
        .take_while(|v| v.is_alphanumeric())
        .collect();
    Some(lang)
}
