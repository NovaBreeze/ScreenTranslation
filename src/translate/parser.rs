//! Formatting and parsing for numbered translation batches.

/// Format source lines as `<N> text`, using one-based numbering.
pub fn format_numbered(lines: &[String]) -> String {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| format!("<{}> {}", index + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Remove hidden chain-of-thought blocks from model-visible content.
pub fn strip_think_blocks(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;

    while let Some(relative_start) = lower[cursor..].find("<think>") {
        let start = cursor + relative_start;
        output.push_str(&input[cursor..start]);
        let body_start = start + "<think>".len();
        let Some(relative_end) = lower[body_start..].find("</think>") else {
            cursor = input.len();
            break;
        };
        cursor = body_start + relative_end + "</think>".len();
    }
    output.push_str(&input[cursor..]);
    output
}

/// Parse `<N>` sections into slots aligned with the original input.
///
/// Text following a marker may span multiple lines. Missing, empty, duplicate,
/// and out-of-range sections are left as `None` so the caller can retry them.
pub fn parse_numbered(input: &str, expected: usize) -> Vec<Option<String>> {
    let cleaned = strip_think_blocks(input);
    let mut result = vec![None; expected];
    let mut current: Option<(usize, String)> = None;

    for line in cleaned.lines() {
        if let Some((number, text)) = parse_marker(line) {
            commit(&mut result, current.take());
            current = Some((number, text.to_owned()));
        } else if let Some((_, text)) = current.as_mut() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line);
        }
    }
    commit(&mut result, current);
    result
}

/// Incrementally collects numbered sections and yields a section only after its
/// following marker arrives. `finish` flushes the final section.
pub struct NumberedStreamParser {
    input: String,
    expected: usize,
    emitted: Vec<bool>,
}

impl NumberedStreamParser {
    pub fn new(expected: usize) -> Self {
        Self {
            input: String::new(),
            expected,
            emitted: vec![false; expected],
        }
    }

    pub fn push(&mut self, chunk: &str) -> Vec<(usize, String)> {
        self.input.push_str(chunk);
        self.take_ready(false)
    }

    pub fn finish(&mut self) -> Vec<(usize, String)> {
        self.take_ready(true)
    }

    fn take_ready(&mut self, finish: bool) -> Vec<(usize, String)> {
        let cleaned = strip_think_blocks(&self.input);
        let mut sections = Vec::new();
        let mut current: Option<(usize, String)> = None;

        for line in cleaned.lines() {
            if let Some((number, text)) = parse_marker(line) {
                if let Some(section) = current.take() {
                    sections.push(section);
                }
                current = Some((number, text.to_owned()));
            } else if let Some((_, text)) = current.as_mut() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(line);
            }
        }
        if finish {
            if let Some(section) = current {
                sections.push(section);
            }
        }

        let mut ready = Vec::new();
        for (number, text) in sections {
            if number == 0 || number > self.expected || self.emitted[number - 1] {
                continue;
            }
            let text = text.trim().to_owned();
            if !text.is_empty() {
                self.emitted[number - 1] = true;
                ready.push((number - 1, text));
            }
        }
        ready
    }
}

fn parse_marker(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let close = trimmed.find('>')?;
    if !trimmed.starts_with('<') || close <= 1 {
        return None;
    }
    let number = trimmed[1..close].parse().ok()?;
    Some((number, trimmed[close + 1..].trim()))
}

fn commit(result: &mut [Option<String>], section: Option<(usize, String)>) {
    let Some((number, text)) = section else {
        return;
    };
    if number == 0 || number > result.len() || result[number - 1].is_some() {
        return;
    }
    let text = text.trim().to_owned();
    if !text.is_empty() {
        result[number - 1] = Some(text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_one_based_numbering() {
        assert_eq!(
            format_numbered(&["hello".into(), "world".into()]),
            "<1> hello\n<2> world"
        );
    }

    #[test]
    fn parses_and_aligns_out_of_order_sections() {
        let parsed = parse_numbered("<2> 二\n<1> 一\ncontinued", 3);
        assert_eq!(
            parsed,
            vec![Some("一\ncontinued".into()), Some("二".into()), None]
        );
    }

    #[test]
    fn strips_multiline_think_blocks_case_insensitively() {
        let parsed = parse_numbered(
            "<THINK>\nprivate reasoning\n</Think>\n<1> visible\n<2> answer",
            2,
        );
        assert_eq!(parsed, vec![Some("visible".into()), Some("answer".into())]);
    }

    #[test]
    fn rejects_empty_duplicate_and_out_of_range_sections() {
        let parsed = parse_numbered("<1> \n<2> first\n<2> duplicate\n<9> ignored", 3);
        assert_eq!(parsed, vec![None, Some("first".into()), None]);
    }

    #[test]
    fn drops_unclosed_think_tail() {
        assert_eq!(strip_think_blocks("<1> ok\n<think>secret"), "<1> ok\n");
    }

    #[test]
    fn stream_parser_handles_fragmented_markers_and_done_flush() {
        let mut parser = NumberedStreamParser::new(2);
        assert!(parser.push("<").is_empty());
        assert!(parser.push("1> hel").is_empty());
        assert_eq!(parser.push("lo\n<2"), Vec::<(usize, String)>::new());
        assert_eq!(parser.push("> world"), vec![(0, "hello".into())]);
        assert_eq!(parser.finish(), vec![(1, "world".into())]);
    }

    #[test]
    fn stream_parser_aligns_out_of_order_and_leaves_missing_numbers() {
        let mut parser = NumberedStreamParser::new(3);
        assert_eq!(parser.push("<3> 三\n<1> 一\n"), vec![(2, "三".into())]);
        assert_eq!(parser.finish(), vec![(0, "一".into())]);
    }

    #[test]
    fn stream_parser_never_emits_think_content() {
        let mut parser = NumberedStreamParser::new(2);
        assert!(parser.push("<think><1> secret").is_empty());
        assert!(parser.push("</think>\n<1> visible\n").is_empty());
        assert_eq!(parser.push("<2> answer"), vec![(0, "visible".into())]);
        assert_eq!(parser.finish(), vec![(1, "answer".into())]);
    }
}
