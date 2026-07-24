use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Simple table output that accounts for CJK character width.
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.width()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.width());
            }
        }
    }
    let header_row: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    print_row(&header_row, &widths);
    let separator: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    print_row(&separator, &widths);
    for row in rows {
        print_row(row, &widths);
    }
}

fn print_row(cells: &[String], widths: &[usize]) {
    let mut line = String::new();
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("  ");
        }
        line.push_str(cell);
        if i + 1 < cells.len() {
            let pad = widths[i].saturating_sub(cell.width());
            line.push_str(&" ".repeat(pad));
        }
    }
    println!("{}", line.trim_end());
}

/// Truncate to a display width (appends … when it overflows).
pub fn truncate_width(s: &str, max_width: usize) -> String {
    if s.width() <= max_width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in s.chars() {
        let char_width = ch.width().unwrap_or(0);
        if width + char_width > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_width_handles_cjk() {
        assert_eq!(truncate_width("hello", 10), "hello");
        assert_eq!(truncate_width("abcdef", 4), "abc…");
        // Full-width chars are width 2: at width 5, 2 chars (width 4) + … fits
        assert_eq!(truncate_width("日本語のタイトル", 5), "日本…");
    }
}
