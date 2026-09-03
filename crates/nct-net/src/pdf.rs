// net.pdf — PDF text extraction (pure Rust, no external binary). Wraps the
// pdf-extract crate: parses the PDF object tree and concatenates text from
// content streams. Returns "" for image-only/scanned PDFs (no embedded text).
pub fn extract_text(bytes: &[u8]) -> Result<String, String> {
    let text = pdf_extract::extract_text_from_mem(bytes).map_err(|e| e.to_string())?;
    // Collapse the per-line streaming that pdf-extract returns into clean
    // paragraphs: mostly it yields one "line" per text span, so joining blank
    // lines into the markdown-ish shape an agent reads well.
    Ok(collapse(&text))
}

fn collapse(text: &str) -> String {
    // Normalize whitespace per line, then rebuild paragraphs: consecutive
    // non-empty lines join with a single space; a blank line (or run of them)
    // becomes exactly one paragraph break (\n\n). Runs of internal spaces in a
    // source line collapse to one space.
    let mut paras: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let normalized: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            if !cur.is_empty() {
                paras.push(std::mem::take(&mut cur));
            }
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&normalized);
        }
    }
    if !cur.is_empty() {
        paras.push(cur);
    }
    paras.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::collapse;

    #[test]
    fn collapse_joins_lines_and_keeps_blank_paragraphs() {
        let s = "Hello   world\n\nMore\n  text\n\n\nEnd";
        let c = collapse(s);
        assert!(c.contains("Hello world"), "got: {c:?}");
        assert!(c.contains("\n\n"), "should keep a paragraph break: {c:?}");
        assert!(c.starts_with("Hello world"));
        assert!(c.ends_with("End"));
        assert!(c.contains("More text"), "got: {c:?}");
    }

    #[test]
    fn collapse_single_line_stays_clean() {
        let c = collapse("One\nTwo\nThree");
        assert_eq!(c, "One Two Three");
    }
}
