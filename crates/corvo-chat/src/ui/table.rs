//! Lists of people as aligned tables, for the corvo tab.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// `head`, then `rows`, each cell padded to its column's widest cell by
/// display width, with two spaces between columns and none at the end.
pub fn table(head: &[&str], rows: &[Vec<String>]) -> Vec<String> {
    let head: Vec<String> = head.iter().map(|h| h.to_string()).collect();
    let all: Vec<&Vec<String>> = std::iter::once(&head).chain(rows).collect();
    let widths: Vec<usize> =
        (0..head.len()).map(|i| all.iter().filter_map(|r| r.get(i)).map(|c| c.width()).max().unwrap_or(0)).collect();
    all.iter()
        .map(|row| {
            let mut line = String::new();
            for (cell, width) in row.iter().zip(&widths) {
                line.push_str(cell);
                line.push_str(&" ".repeat(width - cell.width() + 2));
            }
            line.trim_end().to_string()
        })
        .collect()
}

/// `text` cut to `width` display columns, ending in `…` if it was longer.
pub fn cut(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + c.width().unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    let mut out = out.trim_end().to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_line_up_by_display_width() {
        let rows = vec![
            vec!["rahul".to_string(), "● online".to_string(), "482 910 5573".to_string()],
            vec!["日本".to_string(), "○ offline".to_string(), "120 004 8812".to_string()],
        ];
        let lines = table(&["NAME", "STATUS", "NUMBER"], &rows);
        assert_eq!(
            lines,
            ["NAME   STATUS     NUMBER", "rahul  ● online   482 910 5573", "日本   ○ offline  120 004 8812",]
        );
        assert!(lines.iter().all(|l| !l.ends_with(' ')), "no trailing spaces");
    }

    #[test]
    fn a_long_name_is_cut_with_an_ellipsis() {
        assert_eq!(cut("rahul", 16), "rahul");
        assert_eq!(cut("abcdefghijklmnop", 16), "abcdefghijklmnop");
        assert_eq!(cut("abcdefghijklmnopq", 16), "abcdefghijklmno…");
        assert_eq!(cut("zelda the very long", 16), "zelda the very…", "no space before the …");
        let wide = cut(&"日".repeat(10), 16);
        assert_eq!(wide, format!("{}…", "日".repeat(7)));
        assert!(wide.width() <= 16);
    }
}
