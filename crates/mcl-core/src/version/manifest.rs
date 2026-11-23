//! 版本 JSON 继承链合并
//!
//! Minecraft 版本 JSON 支持 `inheritsFrom`：加载器版本（如 Fabric/Forge）只描述差异，
//! 需要与父版本递归合并后才能拼装启动命令行。合并语义：
//!
//! | 字段类型 | 合并规则 |
//! |---|---|
//! | 对象（`downloads`/`arguments`…） | 递归合并，子级标量覆盖父级 |
//! | `libraries` 数组 | 按 Maven 坐标的 `group:artifact` 去重（**版本由子级覆盖**），保证父级类库不丢失 |
//! | 其他数组（`game`/`jvm` 参数） | 父级在前、子级在后**追加** |
//! | 标量（`id`/`mainClass`…） | 子级覆盖 |
//! | `inheritsFrom` | 合并后移除 |
//!
//! 实现保持**纯函数**（不触碰文件系统与网络），便于快照测试。

use serde_json::Value;

/// 合并继承链。`parents` 按「最远祖先 → 直接父级」排序，`child` 为最终版本。
///
/// ```
/// # use serde_json::json;
/// # use mcl_core::version::manifest::merge_inherited;
/// let parent = json!({ "id": "1.20.1", "libraries": [{ "name": "a" }] });
/// let child = json!({ "id": "fabric-loader-0.15", "inheritsFrom": "1.20.1" });
/// let merged = merge_inherited(&child, &[parent]);
/// assert_eq!(merged["id"], "fabric-loader-0.15");
/// assert_eq!(merged["libraries"][0]["name"], "a");
/// assert!(merged.get("inheritsFrom").is_none());
/// ```
pub fn merge_inherited(child: &Value, parents: &[Value]) -> Value {
    let mut merged = Value::Object(Default::default());
    for parent in parents {
        merge_value(&mut merged, parent, None);
    }
    merge_value(&mut merged, child, None);

    if let Value::Object(map) = &mut merged {
        map.remove("inheritsFrom");
    }
    merged
}

fn merge_value(base: &mut Value, over: &Value, key: Option<&str>) {
    match (base, over) {
        (Value::Object(base_map), Value::Object(over_map)) => {
            for (k, v) in over_map {
                match base_map.get_mut(k) {
                    Some(slot) => merge_value(slot, v, Some(k)),
                    None => {
                        base_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (Value::Array(base_arr), Value::Array(over_arr)) => {
            if key == Some("libraries") {
                merge_libraries(base_arr, over_arr);
            } else {
                // arguments.game / arguments.jvm 等参数数组：父级在前、子级追加
                base_arr.extend(over_arr.iter().cloned());
            }
        }
        // 标量：子级覆盖
        (slot, value) => *slot = value.clone(),
    }
}

/// 库条目的去重键：Maven 坐标的 `group:artifact` 部分。
///
/// `"com.google.guava:guava:32.1.2"` 与 `"com.google.guava:guava:33.0.0"` 视为同一库
/// （版本不同不应导致重复拉取）；无冒号的非法坐标退化为整串比较。
fn library_key(name: &str) -> &str {
    let mut parts = name.split(':');
    match (parts.next(), parts.next()) {
        (Some(group), Some(artifact)) => &name[..group.len() + 1 + artifact.len()],
        _ => name,
    }
}

/// `libraries` 按 `group:artifact` 去重：同库条目做递归合并（子级字段覆盖父级），
fn merge_libraries(base_arr: &mut Vec<Value>, over_arr: &[Value]) {
    for item in over_arr {
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let key = library_key(name);

        let existing = base_arr.iter_mut().find(|candidate| {
            candidate
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| library_key(n) == key)
        });

        match existing {
            Some(slot) => merge_value(slot, item, Some("libraries")),
            None => base_arr.push(item.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn child_scalar_overrides_parent_and_inherits_from_removed() {
        let parent = json!({
            "id": "1.20.1",
            "mainClass": "net.minecraft.client.main.Main",
            "downloads": { "client": { "sha1": "parent-sha" } }
        });
        let child = json!({
            "id": "fabric-loader-0.15.11-1.20.1",
            "inheritsFrom": "1.20.1",
            "mainClass": "net.fabricmc.loader.impl.launch.knot.KnotClient"
        });

        let merged = merge_inherited(&child, &[parent]);

        assert_eq!(merged["id"], "fabric-loader-0.15.11-1.20.1");
        assert_eq!(
            merged["mainClass"],
            "net.fabricmc.loader.impl.launch.knot.KnotClient"
        );
        // 父级深层字段被保留
        assert_eq!(merged["downloads"]["client"]["sha1"], "parent-sha");
        assert!(merged.get("inheritsFrom").is_none());
    }

    #[test]
    fn library_key_uses_group_and_artifact_only() {
        assert_eq!(
            library_key("com.google.guava:guava:33.0.0"),
            "com.google.guava:guava"
        );
        assert_eq!(
            library_key("net.fabricmc:fabric-loader:0.15.11"),
            "net.fabricmc:fabric-loader"
        );
        // 非法坐标（无冒号）退化为整串
        assert_eq!(library_key("broken-name"), "broken-name");
    }

    #[test]
    fn libraries_are_additive_and_deduplicated_by_name() {
        let parent = json!({
            "libraries": [
                { "name": "org.ow2.asm:asm:9.5", "url": "https://libraries.minecraft.net/" },
                { "name": "com.google.guava:guava:32.1.2" }
            ]
        });
        let child = json!({
            "libraries": [
                { "name": "com.google.guava:guava:33.0.0" },
                { "name": "net.fabricmc:fabric-loader:0.15.11" }
            ]
        });

        let merged = merge_inherited(&child, &[parent]);
        let libs = merged["libraries"].as_array().expect("libraries 应为数组");

        assert_eq!(libs.len(), 3, "同名条目应合并而非重复追加");
        // 同名条目保留父级 url，版本被子级覆盖
        let guava = libs
            .iter()
            .find(|l| l["name"] == "com.google.guava:guava:33.0.0")
            .expect("guava 应存在");
        assert_eq!(guava["name"], "com.google.guava:guava:33.0.0");
        // asm 未被覆盖
        assert!(libs.iter().any(|l| l["name"] == "org.ow2.asm:asm:9.5"));
    }

    #[test]
    fn argument_arrays_are_appended_parent_first() {
        let parent = json!({
            "arguments": { "game": ["--username", "${auth_player_name}"], "jvm": ["-Djava.library.path=${natives_directory}"] }
        });
        let child = json!({
            "arguments": { "jvm": ["-DFabricMcEmu=net.minecraft.client.main.Main"] }
        });

        let merged = merge_inherited(&child, &[parent]);
        let jvm = merged["arguments"]["jvm"].as_array().expect("jvm 应为数组");

        assert_eq!(jvm.len(), 2);
        assert_eq!(jvm[0], "-Djava.library.path=${natives_directory}");
        assert_eq!(jvm[1], "-DFabricMcEmu=net.minecraft.client.main.Main");
    }

    #[test]
    fn multi_level_inheritance_merges_in_order() {
        let grandparent = json!({ "id": "1.20.1", "assets": "5" });
        let parent =
            json!({ "id": "forge-47.2.0", "assets": "5", "libraries": [{ "name": "forge:core" }] });
        let child = json!({ "id": "forge-47.2.0-custom", "inheritsFrom": "forge-47.2.0" });

        let merged = merge_inherited(&child, &[grandparent, parent]);

        assert_eq!(merged["assets"], "5");
        assert_eq!(merged["libraries"][0]["name"], "forge:core");
        assert_eq!(merged["id"], "forge-47.2.0-custom");
    }

    #[test]
    fn empty_parents_returns_child_without_inheritance_key() {
        let child = json!({ "id": "1.20.1", "inheritsFrom": "1.20" });
        let merged = merge_inherited(&child, &[]);
        assert_eq!(merged["id"], "1.20.1");
        assert!(merged.get("inheritsFrom").is_none());
    }
}
