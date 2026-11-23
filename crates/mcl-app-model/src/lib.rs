//! 应用层：与 GUI 框架无关的视图模型、任务编排与本地文件读取。
//!
//! 分层规则：
//!
//! - 本 crate **只依赖 `mcl-core`**，不允许依赖 `slint` 或任何其它 GUI 框架；
//! - 展示用的数据在这里组装成纯 Rust 类型（如 [`ModItem`]），
//!   由各 UI 壳（当前的 Slint、将来的 GPUI）自己转成框架的行模型；
//! - 换壳时只需重写绑定层，本 crate 原样复用，并且可以脱离图形环境单测。
//!
//! CI 有一条硬规则：`cargo tree -p mcl-app-model` 中不得出现 `slint`。

mod avatar;
mod diagnostics;
mod files;
mod filter;
mod format;
mod icon;
mod instance;
mod loader_kind;
mod paging;
mod resource_cache;
mod resource_icon;
mod resource_md;
mod scan;
mod services;
mod summary;
mod tasks;
mod time;

pub use avatar::{
    fetch as fetch_avatar_head, render_head as render_skin_head, HEAD_SIZE as AVATAR_SIZE,
};
pub use diagnostics::export_diagnostics;
pub use files::{
    mod_version, read_recent_log, read_tail, scan_mods, ModItem, LOG_READ_LIMIT, LOG_TAIL_LINES,
};
pub use filter::instance_matches;
pub use format::format_size;
pub use icon::{
    builtin_icon, clear_instance_icon, instance_icon, set_instance_icon, IconImage, IconKind,
};
pub use instance::{instance_by_id, migrate_legacy_instances, mod_file_path};
pub use loader_kind::{LoaderKind, LoaderVersion};
pub use paging::{page_bounds, page_count, thousands, Pager, VERSION_PAGE_SIZE};
pub use resource_cache::{cache_dir as resource_cache_dir, prune_cache as prune_resource_cache};
pub use resource_icon::{
    cache_dir as cover_cache_dir, decode as decode_cover, decode_max as decode_image,
    prune_cache as prune_cover_cache, ICON_MAX as COVER_MAX,
};
pub use resource_md::{
    blocks as description_blocks, loader_label, short_count, side_label, version_range,
    Block as DescriptionBlock, BlockKind as DescriptionBlockKind,
};
pub use scan::scan_all_roots;
pub use scan::{dir_size, scan_orphans};
pub use services::{
    add_game_directory, game_directories, remove_game_directory, set_default_game_directory,
    CrashNotice, GameDirectoryInfo, ModUpdate, Services,
};
pub use summary::instance_summary;
pub use tasks::{
    pack_report_to_outcome, report_to_outcome, InstallOutcome, TaskKind, TaskRegistry,
    TaskSignificance,
};
pub use time::{date_only, iso_relative_label, last_played_label, now_unix};
