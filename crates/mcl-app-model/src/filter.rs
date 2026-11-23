//! 实例库的搜索与加载器筛选。
//!
//! 纯函数、零 UI 依赖：Slint 壳只负责把输入传进来，过滤结果由 Rust 回填，
//! 这样筛选规则可以脱离图形环境单测（R4/R6）。

/// 判断一个实例是否命中「搜索词 + 加载器筛选」。
///
/// - `loader_filter`：`all` / `fabric` / `forge`（空字符串等同 `all`）；
/// - 搜索匹配名称、版本行（含加载器版本）与加载器 id，大小写不敏感。
pub fn instance_matches(
    query: &str,
    loader_filter: &str,
    name: &str,
    version_line: &str,
    loader_id: &str,
) -> bool {
    let loader_ok = match loader_filter {
        "" | "all" => true,
        filter => loader_id == filter,
    };
    if !loader_ok {
        return false;
    }

    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    name.to_lowercase().contains(&needle)
        || version_line.to_lowercase().contains(&needle)
        || loader_id.contains(&needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "生存世界";
    const VERSION: &str = "1.20.1 · Fabric 0.15.11";
    const LOADER: &str = "fabric";

    #[test]
    fn empty_query_and_all_filter_match_everything() {
        assert!(instance_matches("", "all", NAME, VERSION, LOADER));
        assert!(instance_matches("   ", "", NAME, VERSION, LOADER));
    }

    #[test]
    fn matches_name_version_and_loader_case_insensitively() {
        assert!(instance_matches("生存", "all", NAME, VERSION, LOADER));
        assert!(instance_matches("1.20.1", "all", NAME, VERSION, LOADER));
        assert!(instance_matches("FABRIC", "all", NAME, VERSION, LOADER));
        assert!(!instance_matches("forge", "all", NAME, VERSION, LOADER));
    }

    #[test]
    fn loader_filter_excludes_other_loaders() {
        assert!(instance_matches("", "fabric", NAME, VERSION, LOADER));
        assert!(!instance_matches("", "forge", NAME, VERSION, LOADER));
        assert!(instance_matches(
            "",
            "forge",
            "机械动力",
            "1.20.1 · Forge 47.2.0",
            "forge"
        ));
    }

    #[test]
    fn unknown_query_misses() {
        assert!(!instance_matches("neoforge", "all", NAME, VERSION, LOADER));
    }
}
