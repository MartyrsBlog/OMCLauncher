//! 页码式分页的共享算术。
//!
//! 资源详情的版本表与下载中心的检索结果用**同一套**规则与文案：页码从 1 开始、
//! 空结果也算 1 页、越界夹到首/末页、底部左侧说明与「第 N / M 页」都由这里生成。
//! UI 只负责摆放按钮，不再各自算页数。

pub const VERSION_PAGE_SIZE: usize = 8;

/// 一页的展示状态：给 UI 的页码、总页数、左侧说明与「第 N / M 页」文案
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pager {
    pub page: usize,
    pub pages: usize,
    /// 左侧说明（「每页 8 个」/「共 79,910 个结果 · 每页 20 个」）
    pub hint: String,
    /// 「第 1 / 3,996 页」
    pub label: String,
}

impl Pager {
    /// `total` = 结果条数；`unit` = 左侧说明的量词（「个版本」/「个结果」）
    pub fn new(page: usize, total: usize, page_size: usize, unit: &str) -> Self {
        let pages = page_count(total, page_size);
        let page = clamp_page(page, pages);
        Self {
            page,
            pages,
            hint: format!("共 {} {unit} · 每页 {page_size} 个", thousands(total)),
            label: format!("第 {} / {} 页", thousands(page), thousands(pages)),
        }
    }

    /// 覆盖页数（「全部来源」按两站中较多的那一站计页），同时重算页码文案
    pub fn with_pages(mut self, pages: usize) -> Self {
        self.pages = pages.max(1);
        self.page = clamp_page(self.page, self.pages);
        self.label = format!("第 {} / {} 页", thousands(self.page), thousands(self.pages));
        self
    }

    /// 覆盖左侧说明（「全部来源」的每页条数与单站不同）
    pub fn with_hint(mut self, hint: String) -> Self {
        self.hint = hint;
        self
    }

    /// 当前页在完整列表里的下标区间 `[start, end)`
    pub fn bounds(&self, len: usize, page_size: usize) -> (usize, usize) {
        page_bounds(self.page, page_size, len)
    }

    pub fn can_prev(&self) -> bool {
        self.page > 1
    }

    pub fn can_next(&self) -> bool {
        self.page < self.pages
    }
}

/// 总页数（至少 1，空结果也占一页）
pub fn page_count(total: usize, page_size: usize) -> usize {
    if page_size == 0 {
        return 1;
    }
    total.div_ceil(page_size).max(1)
}

/// 把页码夹到 `[1, pages]`
pub fn clamp_page(page: usize, pages: usize) -> usize {
    page.clamp(1, pages.max(1))
}

/// 某页的下标区间 `[start, end)`
pub fn page_bounds(page: usize, page_size: usize, len: usize) -> (usize, usize) {
    if page_size == 0 {
        return (0, len);
    }
    let page = clamp_page(page, page_count(len, page_size));
    let start = (page - 1) * page_size;
    (start.min(len), (start + page_size).min(len))
}

/// 千分位（页码与结果数都用它，避免两处格式不一致）
pub fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_pages_with_floor_of_one() {
        assert_eq!(page_count(0, 8), 1);
        assert_eq!(page_count(1, 8), 1);
        assert_eq!(page_count(8, 8), 1);
        assert_eq!(page_count(9, 8), 2);
        assert_eq!(page_count(42, 8), 6);
        assert_eq!(page_count(79_910, 20), 3_996);
    }

    #[test]
    fn clamps_and_slices_bounds() {
        assert_eq!(clamp_page(0, 6), 1);
        assert_eq!(clamp_page(9, 6), 6);
        assert_eq!(page_bounds(1, 8, 42), (0, 8));
        assert_eq!(page_bounds(6, 8, 42), (40, 42));
        // 越界页按末页切片
        assert_eq!(page_bounds(99, 8, 42), (40, 42));
        assert_eq!(page_bounds(2, 8, 3), (0, 3));
    }

    #[test]
    fn formats_hint_and_label_consistently() {
        let download = Pager::new(1, 79_910, 20, "个结果");
        assert_eq!(download.pages, 3_996);
        assert_eq!(download.hint, "共 79,910 个结果 · 每页 20 个");
        assert_eq!(download.label, "第 1 / 3,996 页");
        assert!(!download.can_prev());
        assert!(download.can_next());

        let versions = Pager::new(7, 42, VERSION_PAGE_SIZE, "个版本");
        assert_eq!(versions.page, 6);
        assert_eq!(versions.hint, "共 42 个版本 · 每页 8 个");
        assert_eq!(versions.label, "第 6 / 6 页");
        assert!(versions.can_prev());
        assert!(!versions.can_next());

        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");

        // 「全部来源」：页数按两站较多的一站计，页数被覆盖后页码要重新夹取
        let merged = Pager::new(9, 100, 20, "个结果").with_pages(4);
        assert_eq!(merged.page, 4);
        assert_eq!(merged.label, "第 4 / 4 页");
        assert!(!merged.can_next());
    }
}
