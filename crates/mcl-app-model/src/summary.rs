//! 实例展示文案：标题已包含的字段不再在副标题重复。
//!
//! 自动命名（如 `26.3 Fabric 0.20.1`）会把版本与加载器写进 `name`，
//! 若副标题再拼一遍同样的字段，首页与实例库会出现整行重复。

/// 组合实例副标题：`版本 · 加载器 · 状态`，但跳过 `name` 已包含的部分。
///
/// - `loader_label` 用 [`mcl_core::models::LoaderSpec::label`] 的结果（如 `Fabric 0.20.1`）；
/// - 至少会返回安装状态，因此不会为空。
pub fn instance_summary(
    name: &str,
    mc_version: &str,
    loader_label: &str,
    installed: bool,
) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !name.contains(mc_version) {
        parts.push(mc_version);
    }
    if !name.contains(loader_label) {
        parts.push(loader_label);
    }
    parts.push(if installed { "已安装" } else { "未安装" });
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_named_instance_does_not_repeat_fields() {
        assert_eq!(
            instance_summary("26.3 Fabric 0.20.1", "26.3", "Fabric 0.20.1", true),
            "已安装"
        );
        assert_eq!(
            instance_summary("26.3 原版", "26.3", "原版", false),
            "未安装"
        );
    }

    #[test]
    fn custom_name_keeps_version_and_loader() {
        assert_eq!(
            instance_summary("生存服", "26.3", "Fabric 0.20.1", true),
            "26.3 · Fabric 0.20.1 · 已安装"
        );
    }

    #[test]
    fn partial_overlap_only_drops_matching_field() {
        // 名字里只有版本，没有加载器 → 保留加载器
        assert_eq!(
            instance_summary("26.3 测试环境", "26.3", "Fabric 0.20.1", true),
            "Fabric 0.20.1 · 已安装"
        );
    }
}
