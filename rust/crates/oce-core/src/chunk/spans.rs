//! 行区间辅助：与 Python `domain/chunk/spans.py` 对齐。
//!
//! span 是 1-based 闭区间行范围配上这些行的逐字文本。
//! `cap_span` 保证每个 span 文本不超预算；单行超预算的行直接跳过
//! （压缩后的 bundle 无切片价值，切开的片段会错报行号）。

/// (start_line, end_line, text)，1-based 闭区间。
pub type Span = (u32, u32, String);

/// Python `len(str)` 语义的字符数（Unicode 码点），与切块预算对齐。
/// ASCII 快路径：源码绝大多数行是纯 ASCII，避免逐字符迭代。
#[inline]
pub fn char_len(s: &str) -> usize {
    if s.is_ascii() {
        s.len()
    } else {
        s.chars().count()
    }
}

/// 返回 1-based 闭区间行范围的逐字文本。
pub fn slice_lines(lines: &[&str], start_line: u32, end_line: u32) -> String {
    let s = (start_line as usize - 1).min(lines.len());
    let e = (end_line as usize).min(lines.len());
    lines[s..e].join("\n")
}

/// 把 `end_line` 收回到尾部空行之前。
///
/// `join("\n")` 会丢弃末尾空元素，因此以空行结尾的区间文本行数少于声明行数。
pub fn trim_trailing_blank_lines(lines: &[&str], start_line: u32, end_line: u32) -> u32 {
    let mut end = end_line;
    while end > start_line && lines[end as usize - 1].trim().is_empty() {
        end -= 1;
    }
    end
}

/// 把一个行区间切成文本不超过 `max_chars` 的 span。
///
/// 只在行边界切分。单独超预算的行被跳过，因此每个返回 span 的文本
/// 都等于它声明的源码行。跳过会打断缓冲，所以周边行以独立 span 返回。
pub fn cap_span(lines: &[&str], start_line: u32, end_line: u32, max_chars: usize) -> Vec<Span> {
    assert!(max_chars >= 1, "max_chars must be positive");

    let mut spans: Vec<Span> = Vec::new();
    let mut buffer: Vec<&str> = Vec::new();
    let mut current_start = start_line;
    let mut length: usize = 0;

    for line_no in start_line..=end_line {
        let line = lines[line_no as usize - 1];
        let line_chars = char_len(line);
        if line_chars > max_chars {
            if !buffer.is_empty() {
                spans.push((current_start, line_no - 1, buffer.join("\n")));
                buffer = Vec::new();
                length = 0;
            }
            current_start = line_no + 1;
            continue;
        }
        let mut addition = line_chars + if buffer.is_empty() { 0 } else { 1 };
        if !buffer.is_empty() && length + addition > max_chars {
            spans.push((current_start, line_no - 1, buffer.join("\n")));
            buffer = Vec::new();
            length = 0;
            current_start = line_no;
            addition = line.len();
        }
        buffer.push(line);
        length += addition;
    }

    if !buffer.is_empty() {
        spans.push((current_start, end_line, buffer.join("\n")));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines<'a>(v: &[&'a str]) -> Vec<&'a str> {
        v.to_vec()
    }

    #[test]
    fn slice_lines_returns_verbatim_range() {
        let l = lines(&["a", "b", "c"]);
        assert_eq!(slice_lines(&l, 1, 2), "a\nb");
        assert_eq!(slice_lines(&l, 3, 3), "c");
    }

    #[test]
    fn trim_trailing_blank_lines_pulls_back() {
        let l = lines(&["a", "", "  ", "b"]);
        assert_eq!(trim_trailing_blank_lines(&l, 1, 3), 1);
        assert_eq!(trim_trailing_blank_lines(&l, 1, 4), 4);
    }

    #[test]
    fn cap_span_splits_on_budget() {
        let l = lines(&["ab", "cd", "ef"]);
        let spans = cap_span(&l, 1, 3, 5);
        // "ab\ncd" is 5 chars; "ef" must go to its own span.
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], (1, 2, "ab\ncd".to_string()));
        assert_eq!(spans[1], (3, 3, "ef".to_string()));
    }

    #[test]
    fn cap_span_skips_oversized_line() {
        let l = lines(&["ok", "xxxxxxxxxx", "ok2"]);
        let spans = cap_span(&l, 1, 3, 5);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0], (1, 1, "ok".to_string()));
        assert_eq!(spans[1], (3, 3, "ok2".to_string()));
    }
}
