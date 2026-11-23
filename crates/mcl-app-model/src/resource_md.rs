//! 资源描述的 Markdown 分块与计数缩写。

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// 一级标题（18px / 700）
    Heading1,
    /// 二级标题（15px / 600）
    Heading2,
    /// 正文段落（12.5px）
    Paragraph,
    /// 列表项（11.5px，前缀圆点）
    Bullet,
    /// 代码/引用块（等宽，浅灰底）
    Code,
    /// 配图（`text` 是图片 URL）
    Image,
}

impl BlockKind {
    /// 传给 Slint 的稳定标识
    pub fn id(self) -> &'static str {
        match self {
            BlockKind::Heading1 => "h1",
            BlockKind::Heading2 => "h2",
            BlockKind::Paragraph => "p",
            BlockKind::Bullet => "li",
            BlockKind::Code => "code",
            BlockKind::Image => "img",
        }
    }
}

/// 一个可渲染的正文块
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: BlockKind,
    pub text: String,
}

/// Markdown / 混写 HTML 的正文 → 块列表。
///
/// 规则刻意保守：只识别标题、无序列表、围栏代码与普通段落；
pub fn blocks(markdown: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut in_code = false;

    fn flush(out: &mut Vec<Block>, paragraph: &mut Vec<String>) {
        if paragraph.is_empty() {
            return;
        }
        // 段落合并后才剥 HTML：`<img>` 行在前面已单独识别
        let joined = strip_html(&paragraph.join(" "));
        let joined = strip_images(&joined);
        let joined = inline_markup(&joined);
        if !joined.trim().is_empty() {
            out.push(Block {
                kind: BlockKind::Paragraph,
                text: joined.trim().to_string(),
            });
        }
        paragraph.clear();
    }

    for raw in markdown.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim();

        // 围栏代码块：整段原样保留（缩进不重排）
        if trimmed.starts_with("```") {
            if in_code {
                in_code = false;
            } else {
                flush(&mut out, &mut paragraph);
                in_code = true;
            }
            continue;
        }
        if in_code {
            out.push(Block {
                kind: BlockKind::Code,
                text: line.to_string(),
            });
            continue;
        }

        if trimmed.is_empty() {
            flush(&mut out, &mut paragraph);
            continue;
        }

        // 分隔线：丢弃
        if trimmed.chars().all(|ch| ch == '-') {
            flush(&mut out, &mut paragraph);
            continue;
        }

        if let Some(url) = image_url(trimmed) {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Image,
                text: url,
            });
            continue;
        }

        // 标题
        if let Some(rest) = trimmed.strip_prefix("### ") {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Heading2,
                text: inline_markup(rest),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("## ") {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Heading2,
                text: inline_markup(rest),
            });
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Heading1,
                text: inline_markup(rest),
            });
            continue;
        }

        // 无序列表
        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Bullet,
                text: inline_markup(rest),
            });
            continue;
        }

        // 有序列表：保留序号文本
        if let Some((number, rest)) = split_ordered_item(trimmed) {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Bullet,
                text: format!("{number}. {}", inline_markup(rest)),
            });
            continue;
        }

        // 引用
        if let Some(rest) = trimmed.strip_prefix("> ") {
            flush(&mut out, &mut paragraph);
            out.push(Block {
                kind: BlockKind::Code,
                text: inline_markup(rest),
            });
            continue;
        }

        paragraph.push(trimmed.to_string());
    }

    flush(&mut out, &mut paragraph);
    out
}

/// `1. xxx` → `("1", "xxx")`
fn split_ordered_item(text: &str) -> Option<(&str, &str)> {
    let (head, rest) = text.split_once(". ")?;
    if head.is_empty() || !head.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((head, rest))
}

/// 去掉 HTML 标签，保留标签之间的文字；`<br>` 视为换行。
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut tag = String::new();
    for ch in text.chars() {
        match ch {
            '<' => {
                depth += 1;
                tag.clear();
            }
            '>' if depth > 0 => {
                depth -= 1;
                if tag.eq_ignore_ascii_case("br") || tag.eq_ignore_ascii_case("br/") {
                    out.push('\n');
                }
            }
            _ if depth == 0 => out.push(ch),
            _ => {
                if tag.len() < 16 {
                    tag.push(ch);
                }
            }
        }
    }
    out
}

/// 行首是图片时取出它的 URL：`![alt](url)` / `<img src="...">` / `[![..](..)](..)`。
///
/// 只接受 http(s)；返回 None 表示这一行不是图片行。
fn image_url(line: &str) -> Option<String> {
    let text = line.trim();
    let is_image = text.starts_with("![") || text.to_ascii_lowercase().starts_with("<img");
    if !is_image {
        return None;
    }
    let mut rest = text;
    while let Some(index) = rest.find("http") {
        let candidate = &rest[index..];
        let end = candidate
            .find(|ch: char| {
                ch == ')' || ch == '"' || ch == '\'' || ch.is_whitespace() || ch == '>'
            })
            .unwrap_or(candidate.len());
        let url = &candidate[..end];
        if url.starts_with("https://") || url.starts_with("http://") {
            return Some(url.to_string());
        }
        rest = &candidate[end..];
    }
    None
}

/// 去掉行内图片：`![alt](url)`，以及包在链接里的 `[![alt](url)](link)`。
///
/// Modrinth 的项目简介里大量出现徽章图片，不剥掉会整段变成源码。
fn strip_images(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '!' && index + 1 < chars.len() && chars[index + 1] == '[' {
            // 找 `](`，再找配对的 `)`
            let mut cursor = index + 2;
            let mut label_end = None;
            while cursor + 1 < chars.len() {
                if chars[cursor] == ']' && chars[cursor + 1] == '(' {
                    label_end = Some(cursor);
                    break;
                }
                cursor += 1;
            }
            if let Some(label_end) = label_end {
                let mut depth = 0usize;
                let mut end = None;
                for (offset, ch) in chars.iter().enumerate().skip(label_end + 1) {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if let Some(end) = end {
                    index = end + 1;
                    continue;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// 行内 Markdown 清理：`[文字](url)` → 文字，`**粗体**` / `` `代码` `` 去记号。
fn inline_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        // 链接
        if chars[index] == '[' {
            if let Some((label, next)) = parse_link(&chars, index) {
                out.push_str(&label);
                index = next;
                continue;
            }
        }
        // 粗体 / 斜体 / 行内代码记号直接跳过
        if matches!(chars[index], '*' | '`' | '_') {
            index += 1;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out.trim().to_string()
}

/// `[label](url)` 解析；返回 (label, 结束位置)
fn parse_link(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut index = start + 1;
    let mut label = String::new();
    while index < chars.len() && chars[index] != ']' {
        label.push(chars[index]);
        index += 1;
    }
    if index + 1 >= chars.len() || chars[index] != ']' || chars[index + 1] != '(' {
        return None;
    }
    index += 2;
    while index < chars.len() && chars[index] != ')' {
        index += 1;
    }
    if index >= chars.len() {
        return None;
    }
    Some((label, index + 1))
}

/// 下载量 / 关注数的中文缩写：`2.9 亿` / `86.2 万` / `1,204`
pub fn short_count(value: u64) -> String {
    if value >= 100_000_000 {
        format!("{:.1} 亿", value as f64 / 100_000_000.0)
    } else if value >= 10_000 {
        format!("{:.1} 万", value as f64 / 10_000.0)
    } else {
        format!("{value}")
    }
}

/// 支持版本区间：列表按新→旧给，取最新与最旧拼「1.14 – 1.21.9」。
///
/// 只有一个版本时直接返回它；空列表返回空串（UI 省略该行）。
pub fn version_range(versions: &[String]) -> String {
    let first = versions.first().map(String::as_str).unwrap_or_default();
    let last = versions.last().map(String::as_str).unwrap_or_default();
    if first.is_empty() {
        return String::new();
    }
    if first == last {
        return first.to_string();
    }
    format!("{last} – {first}")
}

/// 客户端 / 服务端支持文案：`客户端 / 服务端`、`仅客户端`、`仅服务端`。
///
/// Modrinth 用 `required` / `optional` / `unsupported`；空串按未知处理。
pub fn side_label(client: &str, server: &str) -> String {
    let client_ok = matches!(client, "required" | "optional");
    let server_ok = matches!(server, "required" | "optional");
    match (client_ok, server_ok) {
        (true, true) => "客户端 / 服务端".to_string(),
        (true, false) => "仅客户端".to_string(),
        (false, true) => "仅服务端".to_string(),
        (false, false) => String::new(),
    }
}

/// 加载器列表 → 展示名（`fabric` → `Fabric`）
pub fn loader_label(id: &str) -> String {
    match id.to_ascii_lowercase().as_str() {
        "fabric" => "Fabric".to_string(),
        "quilt" => "Quilt".to_string(),
        "forge" => "Forge".to_string(),
        "neoforge" => "NeoForge".to_string(),
        "liteloader" => "LiteLoader".to_string(),
        "modloader" => "ModLoader".to_string(),
        "rift" => "Rift".to_string(),
        "" => String::new(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_headings_bullets_and_paragraphs() {
        let md = "# 标题\n\n第一段。\n\n## 小节\n\n- 条目一\n- 条目二\n\n结尾。";
        let rows = blocks(md);
        let kinds: Vec<&str> = rows.iter().map(|row| row.kind.id()).collect();
        assert_eq!(kinds, vec!["h1", "p", "h2", "li", "li", "p"]);
        assert_eq!(rows[0].text, "标题");
        assert_eq!(rows[3].text, "条目一");
    }

    #[test]
    fn keeps_link_text_and_drops_inline_images() {
        let md = "见 [文档](https://example.com)，还有 ![行内图](https://example.com/a.png)。";
        let rows = blocks(md);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "见 文档，还有 。");
    }

    #[test]
    fn strips_inline_html() {
        let md = "<center>居中</center>\n\n第二行<br>换行";
        let rows = blocks(md);
        assert_eq!(rows[0].text, "居中");
        assert!(rows[1].text.contains("换行"));
    }

    #[test]
    fn drops_badge_images_wrapped_in_links() {
        let md =
            "[![Buy me a coffee](https://img.example/a.svg)](https://coffee.example)\n\n正文。";
        let rows = blocks(md);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "正文。");
    }

    #[test]
    fn keeps_images_as_blocks() {
        let md = "文字。\n\n![截图](https://cdn.example/a.png)\n\n<img src=\"https://cdn.example/b.webp\">";
        let rows = blocks(md);
        assert_eq!(rows[0].kind, BlockKind::Paragraph);
        assert_eq!(rows[1].kind, BlockKind::Image);
        assert_eq!(rows[1].text, "https://cdn.example/a.png");
        assert_eq!(rows[2].kind, BlockKind::Image);
        assert_eq!(rows[2].text, "https://cdn.example/b.webp");
    }

    #[test]
    fn short_count_uses_chinese_units() {
        assert_eq!(short_count(1204), "1204");
        assert_eq!(short_count(862_000), "86.2 万");
        assert_eq!(short_count(290_000_000), "2.9 亿");
    }

    #[test]
    fn version_range_orders_old_to_new() {
        let rows = vec![
            "1.21.9".to_string(),
            "1.20.1".to_string(),
            "1.14".to_string(),
        ];
        assert_eq!(version_range(&rows), "1.14 – 1.21.9");
        assert_eq!(version_range(&["1.20.1".to_string()]), "1.20.1");
        assert_eq!(version_range(&[]), "");
    }

    #[test]
    fn side_label_covers_three_combinations() {
        assert_eq!(side_label("required", "optional"), "客户端 / 服务端");
        assert_eq!(side_label("required", "unsupported"), "仅客户端");
        assert_eq!(side_label("unsupported", "required"), "仅服务端");
        assert_eq!(side_label("", ""), "");
    }
}
