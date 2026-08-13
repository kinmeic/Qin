//! Minimal Markdown rendering for terminal final answers. Headings, pipe
//! tables, fenced code blocks, and inline bold/italic/code are turned into
//! readable ANSI-styled text (or flattened to plain text when colors are
//! disabled). Anything unrecognized is passed through unchanged.

use unicode_width::UnicodeWidthStr;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";

pub fn render_for_terminal(text: &str, color: bool) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut output = String::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            index = render_code_block(&lines, index + 1, &mut output, color);
            continue;
        }
        if is_table_row(line) && index + 1 < lines.len() && is_table_separator(lines[index + 1]) {
            index = render_table(&lines, index, &mut output, color);
            continue;
        }
        if let Some(heading) = parse_heading(line) {
            output.push_str(&paint(
                &render_inline(heading, color),
                &format!("{BOLD}{CYAN}"),
                color,
            ));
            output.push('\n');
            index += 1;
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix("> ") {
            output.push_str(&paint(
                &format!("  │ {}", render_inline(quote, color)),
                DIM,
                color,
            ));
            output.push('\n');
            index += 1;
            continue;
        }
        if is_horizontal_rule(trimmed) {
            output.push_str(&paint(&"─".repeat(40), DIM, color));
            output.push('\n');
            index += 1;
            continue;
        }
        output.push_str(&render_inline(line, color));
        output.push('\n');
        index += 1;
    }
    output
}

fn paint(text: &str, code: &str, color: bool) -> String {
    if color && !text.is_empty() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_string()
    }
}

fn parse_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let marks = trimmed.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&marks) {
        let rest = &trimmed[marks..];
        if rest.starts_with(' ') {
            return Some(rest.trim());
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let chars: Vec<char> = line.chars().filter(|ch| !ch.is_whitespace()).collect();
    chars.len() >= 3 && chars.iter().all(|ch| matches!(ch, '-' | '*' | '_'))
}

fn is_table_row(line: &str) -> bool {
    line.trim_start().starts_with('|') && line.matches('|').count() >= 2
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|')
        && trimmed
            .chars()
            .all(|ch| matches!(ch, '|' | '-' | ':' | ' '))
        && trimmed.contains('-')
}

fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim().trim_start_matches('|').trim_end_matches('|');
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn render_table(lines: &[&str], start: usize, output: &mut String, color: bool) -> usize {
    let mut rows = Vec::new();
    let mut index = start;
    while index < lines.len() && is_table_row(lines[index]) {
        if !is_table_separator(lines[index]) {
            rows.push(split_table_row(lines[index]));
        }
        index += 1;
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return index;
    }
    // Measure widths on the flattened cell text so inline markers like
    // backticks do not inflate the column width.
    let rendered_rows: Vec<Vec<(String, String)>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| (render_inline(cell, false), render_inline(cell, color)))
                .collect()
        })
        .collect();
    let mut widths = vec![0_usize; columns];
    for row in &rendered_rows {
        for (column, (plain, _)) in row.iter().enumerate() {
            widths[column] = widths[column].max(UnicodeWidthStr::width(plain.as_str()));
        }
    }
    for (row_index, row) in rendered_rows.iter().enumerate() {
        let mut rendered = String::new();
        for (column, width) in widths.iter().enumerate() {
            let (plain, styled) = row
                .get(column)
                .map(|(plain, styled)| (plain.as_str(), styled.as_str()))
                .unwrap_or(("", ""));
            let padding = width - UnicodeWidthStr::width(plain);
            if row_index == 0 {
                rendered.push_str(&paint(styled, BOLD, color));
            } else {
                rendered.push_str(styled);
            }
            if column + 1 < columns {
                rendered.push_str(&" ".repeat(padding + 2));
            }
        }
        output.push_str(rendered.trim_end());
        output.push('\n');
        if row_index == 0 {
            let rule = "─".repeat(widths.iter().sum::<usize>() + 2 * (columns - 1));
            output.push_str(&paint(&rule, DIM, color));
            output.push('\n');
        }
    }
    index
}

fn render_code_block(lines: &[&str], mut index: usize, output: &mut String, color: bool) -> usize {
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if line.trim_start().starts_with("```") {
            break;
        }
        output.push_str(&paint(&format!("  {line}"), DIM, color));
        output.push('\n');
    }
    index
}

fn render_inline(text: &str, color: bool) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        result.push_str(&render_marks(&rest[..start], color));
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                result.push_str(&paint(&after[..end], YELLOW, color));
                rest = &after[end + 1..];
            }
            None => {
                result.push('`');
                result.push_str(&render_marks(after, color));
                return result;
            }
        }
    }
    result.push_str(&render_marks(rest, color));
    result
}

fn render_marks(text: &str, color: bool) -> String {
    let bold = replace_pairs(text, "**", BOLD, color);
    replace_pairs(&bold, "*", ITALIC, color)
}

fn replace_pairs(text: &str, mark: &str, code: &str, color: bool) -> String {
    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(mark) {
        result.push_str(&rest[..start]);
        let after = &rest[start + mark.len()..];
        match after.find(mark) {
            Some(end) if end > 0 => {
                result.push_str(&paint(&after[..end], code, color));
                rest = &after[end + mark.len()..];
            }
            _ => {
                result.push_str(mark);
                rest = after;
            }
        }
    }
    result.push_str(rest);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_headings_and_inline_marks() {
        let rendered = render_for_terminal("## Title\nUse `sudo` and **care**", true);
        assert!(rendered.contains("\x1b[1m"));
        assert!(rendered.contains("\x1b[33msudo\x1b[0m"));
        assert!(!rendered.contains("##"));
        let plain = render_for_terminal("## Title\nUse `sudo`", false);
        assert_eq!(plain, "Title\nUse sudo\n");
    }

    #[test]
    fn aligns_tables_by_display_width() {
        let rendered = render_for_terminal(
            "| 服务 | State |\n|---|---|\n| `面板` | enabled |\n| web | active |\n",
            false,
        );
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "服务  State");
        assert_eq!(lines[2], "面板  enabled");
        assert_eq!(lines[3], "web   active");
    }

    #[test]
    fn leaves_non_tables_and_code_blocks_readable() {
        let rendered = render_for_terminal("a | b only\n```sh\nls -la\n```\n", false);
        assert!(rendered.contains("a | b only"));
        assert!(rendered.contains("  ls -la"));
        assert!(!rendered.contains("```"));
    }
}
