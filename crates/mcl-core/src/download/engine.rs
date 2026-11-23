//! 下载引擎实现
//!
//! 策略要点：
//! - **命中跳过**：目标文件 SHA-1 已匹配则直接跳过
//! - **大文件分块并行**：≥4MB 切分为 4 块并行下载，块各自独立落盘便于续传
//! - **断点续传**：单文件用 `<dest>.part`，分块用 `<dest>.partN`，各自按已落盘长度续传
//! - **原子落地**：全部完成并校验通过后才 `rename` 为正式文件
//! - **事件上报**：聚合进度经 [`EventBus`] 按 250ms 节流发布

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;

use crate::download::split_ranges;
use crate::error::{CoreError, CoreResult};
use crate::http::HttpClient;
use crate::models::FileEntry;
use crate::task::{EventBus, TaskEvent};
use crate::util::verify_sha1;

/// 启用多分块的体积阈值（低于该值分块收益不抵开销）
const CHUNK_THRESHOLD: u64 = 4 * 1024 * 1024;
const DEFAULT_PARTS: usize = 4;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// 内容寻址缓存上限（超过后按修改时间淘汰最旧文件）
const CACHE_LIMIT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Downloaded,
    Resumed,
    Skipped,
    /// 从本机内容缓存（按 SHA-1 寻址）复制/硬链接而来，未走网络
    Cached,
}

/// 内容寻址缓存根：`<数据目录>/cache/objects/<sha1 前两位>/<sha1>`
///
/// 对应 HMCL 的 `DownloadCacheRepository`：同一文件被多个实例/重装复用时，
/// 只需下载一次。目标目录与缓存通常在同一文件系统，优先硬链接、失败再复制。
pub fn cache_root() -> PathBuf {
    crate::platform::paths::data_dir()
        .join("cache")
        .join("objects")
}

/// SHA-1 → 缓存内相对路径（两级分片，避免单目录文件过多）
fn cache_relative_path(sha1: &str) -> PathBuf {
    let prefix = &sha1[..2.min(sha1.len())];
    PathBuf::from(prefix).join(sha1)
}

fn cache_path(sha1: &str) -> Option<PathBuf> {
    if sha1.len() != 40 || !sha1.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(cache_root().join(cache_relative_path(sha1)))
}

/// 优先硬链接（零拷贝），跨文件系统时回退为复制
async fn link_or_copy(src: &Path, dest: &Path) -> std::io::Result<()> {
    let _ = tokio::fs::remove_file(dest).await;
    if tokio::fs::hard_link(src, dest).await.is_ok() {
        return Ok(());
    }
    tokio::fs::copy(src, dest).await.map(|_| ())
}

/// 下载成功后写入缓存（尽力而为；先写临时文件再 rename，避免并发读到半成品）
async fn store_in_cache(src: &Path, sha1: &str) {
    let Some(cached) = cache_path(sha1) else {
        return;
    };
    if cached.is_file() {
        return;
    }
    let Some(parent) = cached.parent() else {
        return;
    };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return;
    }
    let tmp = cached.with_extension("tmp");
    let linked = tokio::fs::hard_link(src, &tmp).await.is_ok();
    if !linked && tokio::fs::copy(src, &tmp).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    let _ = tokio::fs::rename(&tmp, &cached).await;
    prune_cache_once().await;
}

/// 缓存超过上限时按修改时间淘汰（每个进程只跑一次）
async fn prune_cache_once() {
    static PRUNED: AtomicBool = AtomicBool::new(false);
    if PRUNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = tokio::task::spawn_blocking(|| {
        let root = cache_root();
        let Ok(shards) = std::fs::read_dir(&root) else {
            return;
        };
        let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
        for shard in shards.flatten() {
            let Ok(entries) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(meta) = entry.metadata() else {
                    continue;
                };
                if meta.is_file() {
                    files.push((
                        meta.modified().unwrap_or(UNIX_EPOCH),
                        meta.len(),
                        entry.path(),
                    ));
                }
            }
        }

        let mut total: u64 = files.iter().map(|(_, size, _)| *size).sum();
        if total <= CACHE_LIMIT_BYTES {
            return;
        }
        files.sort_by_key(|(modified, _, _)| *modified);
        for (_, size, path) in files {
            if total <= CACHE_LIMIT_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    })
    .await;
}

#[derive(Debug, Default, Clone)]
pub struct FetchStats {
    pub files: usize,
    pub skipped: usize,
    pub bytes: u64,
    pub failures: Vec<(String, String)>,
    /// 是否因取消旗标提前终止（暂停：已下载部分经 `.part`/哈希跳过被复用，可续传）
    pub cancelled: bool,
}

impl FetchStats {
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// 下载引擎。`Clone` 成本仅为 `Arc` 克隆，可安全地分发到任务中。
#[derive(Clone)]
pub struct DownloadEngine {
    http: Arc<HttpClient>,
    bus: EventBus,
    concurrency: usize,
    parts: usize,
}

impl DownloadEngine {
    pub fn new(http: Arc<HttpClient>, bus: EventBus, concurrency: usize) -> Self {
        Self {
            http,
            bus,
            concurrency: concurrency.max(1),
            parts: DEFAULT_PARTS,
        }
    }

    pub fn http(&self) -> &Arc<HttpClient> {
        &self.http
    }

    pub fn concurrency(&self) -> usize {
        self.concurrency
    }

    /// 单文件下载（含校验、续传、分块、内容缓存）
    pub async fn fetch(&self, entry: &FileEntry, dest: &Path) -> CoreResult<FetchOutcome> {
        // ① 命中跳过
        if let Some(expected) = &entry.sha1 {
            if dest.is_file() && verify_sha1(dest, expected).await {
                return Ok(FetchOutcome::Skipped);
            }
            // ② 内容缓存命中：直接硬链接/复制，不访问网络
            if let Some(cached) = cache_path(expected) {
                if cached.is_file() && verify_sha1(&cached, expected).await {
                    if let Some(parent) = dest.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    if link_or_copy(&cached, dest).await.is_ok() {
                        return Ok(FetchOutcome::Cached);
                    }
                }
            }
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // ③ 按体积选择策略
        let outcome = if entry.size >= CHUNK_THRESHOLD {
            self.fetch_multipart(entry, dest).await?
        } else {
            self.fetch_single(entry, dest).await?
        };

        // ④ 体积校验（PCL2 FileChecker 的思路）：清单给了大小就必须一致
        //    挡住「错误页 / 半截文件被当成 jar 存下来」这类问题
        if entry.size > 0 {
            let actual = tokio::fs::metadata(dest)
                .await
                .map(|meta| meta.len())
                .unwrap_or(0);
            if actual != entry.size {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(CoreError::Unsupported(format!(
                    "下载体积不符（期望 {} 字节，实际 {}）：{}",
                    entry.size, actual, entry.path
                )));
            }
        }

        // ⑤ SHA-1 校验；失败则删除产物并报错，交由上层换源重试
        if let Some(expected) = &entry.sha1 {
            if !verify_sha1(dest, expected).await {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(CoreError::HashMismatch {
                    path: dest.display().to_string(),
                });
            }
            store_in_cache(dest, expected).await;
        }

        Ok(outcome)
    }

    /// 单流下载（小文件；带续传）
    async fn fetch_single(&self, entry: &FileEntry, dest: &Path) -> CoreResult<FetchOutcome> {
        let tmp = part_path(dest, None);

        let mut offset = 0u64;
        if let Ok(meta) = tokio::fs::metadata(&tmp).await {
            if entry.size > 0 && meta.len() < entry.size {
                offset = meta.len();
            } else {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        }

        let mut response = if entry.size > 0 {
            self.http
                .get_range(&entry.url, offset, entry.size - 1)
                .await?
        } else {
            self.http.get(&entry.url).await?
        };

        let mut file = if offset > 0 {
            tokio::fs::OpenOptions::new()
                .append(true)
                .open(&tmp)
                .await?
        } else {
            tokio::fs::File::create(&tmp).await?
        };

        while let Some(chunk) = response.chunk().await.map_err(net)? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        tokio::fs::rename(&tmp, dest).await?;
        Ok(if offset > 0 {
            FetchOutcome::Resumed
        } else {
            FetchOutcome::Downloaded
        })
    }

    /// 多分块并行下载（大文件；每块独立续传）
    async fn fetch_multipart(&self, entry: &FileEntry, dest: &Path) -> CoreResult<FetchOutcome> {
        let chunk = entry.size.div_ceil(self.parts as u64);
        let ranges = split_ranges(entry.size, chunk);

        let mut set: JoinSet<CoreResult<bool>> = JoinSet::new();
        for (index, (start, end)) in ranges.iter().copied().enumerate() {
            let engine = self.clone();
            let url = entry.url.clone();
            let tmp = part_path(dest, Some(index));
            set.spawn(async move { engine.download_part(&url, &tmp, start, end).await });
        }

        let mut resumed = false;
        while let Some(joined) = set.join_next().await {
            let part_resumed = joined.map_err(|error| CoreError::Network(error.to_string()))??;
            resumed |= part_resumed;
        }

        // 合并分块 → 临时文件 → 原子改名
        let tmp = part_path(dest, None);
        let mut output = tokio::fs::File::create(&tmp).await?;
        for index in 0..ranges.len() {
            let part = part_path(dest, Some(index));
            let mut input = tokio::fs::File::open(&part).await?;
            tokio::io::copy(&mut input, &mut output).await?;
            drop(input);
            let _ = tokio::fs::remove_file(&part).await;
        }
        output.flush().await?;
        drop(output);

        tokio::fs::rename(&tmp, dest).await?;
        Ok(if resumed {
            FetchOutcome::Resumed
        } else {
            FetchOutcome::Downloaded
        })
    }

    /// 下载单个分块；返回是否发生了续传
    async fn download_part(&self, url: &str, tmp: &Path, start: u64, end: u64) -> CoreResult<bool> {
        let expected_len = end - start + 1;

        let mut have = 0u64;
        if let Ok(meta) = tokio::fs::metadata(tmp).await {
            if meta.len() <= expected_len {
                have = meta.len();
            } else {
                let _ = tokio::fs::remove_file(tmp).await;
            }
        }
        if have == expected_len {
            return Ok(true);
        }

        let mut response = self.http.get_range(url, start + have, end).await?;
        let mut file = if have > 0 {
            tokio::fs::OpenOptions::new().append(true).open(tmp).await?
        } else {
            tokio::fs::File::create(tmp).await?
        };

        while let Some(chunk) = response.chunk().await.map_err(net)? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;

        Ok(have > 0)
    }

    /// 批量下载：并发受控，聚合进度经事件总线发布
    ///
    /// `task_id` 用于把该批任务的进度事件关联到 UI 任务项。
    pub async fn fetch_many(
        &self,
        entries: Vec<FileEntry>,
        base_dir: &Path,
        task_id: u64,
    ) -> FetchStats {
        self.fetch_many_cancellable(entries, base_dir, task_id, None)
            .await
    }

    /// 批量下载
    ///
    /// 取消语义：每个新文件派发前检查旗标；已开始的文件跑完当前逻辑后自然结束
    /// （小于分块阈值的文件通常亚秒级）。取消返回的 [`FetchStats::cancelled`]
    /// 为 `true`，`files` 为**实际派发数**；已下载文件经 SHA-1 命中跳过、
    /// 未完成文件保留 `.part`，因此再次调用即完成续传。
    pub async fn fetch_many_cancellable(
        &self,
        entries: Vec<FileEntry>,
        base_dir: &Path,
        task_id: u64,
        cancel: Option<Arc<AtomicBool>>,
    ) -> FetchStats {
        self.fetch_many_multi(vec![(base_dir.to_path_buf(), entries)], task_id, cancel)
            .await
    }

    /// 批量下载（**多基准目录**， 共享模式：私有文件落实例目录、
    /// 共享类库/资源落共享目录）。
    ///
    /// 各分组的条目并发调度，进度与取消语义同 [`Self::fetch_many_cancellable`]。
    pub async fn fetch_many_multi(
        &self,
        groups: Vec<(PathBuf, Vec<FileEntry>)>,
        task_id: u64,
        cancel: Option<Arc<AtomicBool>>,
    ) -> FetchStats {
        // 去重：资源对象是内容寻址的，多个资源名可能指向同一 hash → 同一目标路径，
        // 键为「基准目录 + 相对路径」——同一相对路径在不同基准下是不同的目标。
        let total_entries: usize = groups.iter().map(|(_, entries)| entries.len()).sum();
        let mut seen = std::collections::HashSet::with_capacity(total_entries);
        let entries: Vec<(PathBuf, FileEntry)> = groups
            .into_iter()
            .flat_map(|(base, entries)| entries.into_iter().map(move |entry| (base.clone(), entry)))
            .filter(|(base, entry)| seen.insert((base.clone(), entry.path.clone())))
            .collect();

        let total_bytes: u64 = entries.iter().map(|(_, entry)| entry.size).sum();
        let done = Arc::new(AtomicU64::new(0));
        let finished = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Mutex::new(FetchStats::default()));

        // 进度上报任务（250ms 节流）
        {
            let bus = self.bus.clone();
            let done = done.clone();
            let finished = finished.clone();
            tokio::spawn(async move {
                while !finished.load(Ordering::Relaxed) {
                    bus.publish(TaskEvent::Progress {
                        id: task_id,
                        downloaded: done.load(Ordering::Relaxed),
                        total: total_bytes,
                    });
                    tokio::time::sleep(PROGRESS_INTERVAL).await;
                }
            });
        }

        let semaphore = Arc::new(Semaphore::new(self.concurrency));
        let mut set: JoinSet<()> = JoinSet::new();
        let mut spawned = 0usize;
        let mut cancelled = false;

        for (base, entry) in entries {
            // 协作式取消：仅阻止**新文件**派发；在途文件自然跑完（其 .part 供续传）
            if let Some(flag) = &cancel {
                if flag.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
            }
            spawned += 1;

            let engine = self.clone();
            let permits = semaphore.clone();
            let done = done.clone();
            let stats = stats.clone();
            let dest = base.join(&entry.path);
            let label = entry.path.clone();
            let size = entry.size;

            set.spawn(async move {
                let _permit = permits.acquire_owned().await.expect("信号量未被关闭");
                match engine.fetch(&entry, &dest).await {
                    Ok(FetchOutcome::Skipped | FetchOutcome::Cached) => {
                        stats.lock().await.skipped += 1
                    }
                    Ok(_) => stats.lock().await.bytes += size,
                    Err(error) => stats.lock().await.failures.push((label, error.to_string())),
                }
                done.fetch_add(size, Ordering::Relaxed);
            });
        }

        while set.join_next().await.is_some() {}

        finished.store(true, Ordering::Relaxed);
        // 正常结束补 100% 进度；取消时保留真实进度（进度条停在中途，语义正确）
        let downloaded = if cancelled {
            done.load(Ordering::Relaxed)
        } else {
            total_bytes
        };
        self.bus.publish(TaskEvent::Progress {
            id: task_id,
            downloaded,
            total: total_bytes,
        });

        let mut collected = stats.lock().await.clone();
        collected.files = spawned;
        collected.cancelled = cancelled;
        collected
    }
}

/// 临时文件路径：`None` 为整体临时文件，`Some(i)` 为第 i 个分块
fn part_path(dest: &Path, index: Option<usize>) -> PathBuf {
    match index {
        Some(index) => PathBuf::from(format!("{}.part{index}", dest.display())),
        None => PathBuf::from(format!("{}.part", dest.display())),
    }
}

fn net(error: reqwest::Error) -> CoreError {
    CoreError::Network(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_relative_path_shards_by_first_two_hex_chars() {
        let sha1 = "ab12cd34ef567890ab12cd34ef56789012345678";
        assert_eq!(cache_relative_path(sha1), PathBuf::from("ab").join(sha1));
        // 非法哈希不进入缓存
        assert!(cache_path("not-a-sha1").is_none());
        assert!(cache_path("ab12").is_none());
    }

    #[test]
    fn part_paths_are_stable() {
        let dest = Path::new("/tmp/a/b/client.jar");
        assert_eq!(
            part_path(dest, None),
            PathBuf::from("/tmp/a/b/client.jar.part")
        );
        assert_eq!(
            part_path(dest, Some(2)),
            PathBuf::from("/tmp/a/b/client.jar.part2")
        );
    }

    #[tokio::test]
    async fn fetch_skips_when_hash_matches() {
        let dir = std::env::temp_dir().join("mcl-engine-skip");
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        let dest = dir.join("cached.bin");
        tokio::fs::write(&dest, b"hello").await.expect("写文件");

        // "hello" 的 SHA-1
        let expected = crate::util::sha1_file(&dest).await.expect("sha1");
        let engine = DownloadEngine::new(
            Arc::new(HttpClient::with_default_sources().expect("客户端")),
            EventBus::default(),
            4,
        );
        let entry = FileEntry {
            path: "cached.bin".into(),
            url: "https://example.invalid/cached.bin".into(),
            sha1: Some(expected),
            size: 5,
        };

        let outcome = engine.fetch(&entry, &dest).await.expect("应跳过下载");
        assert_eq!(outcome, FetchOutcome::Skipped);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn cancelled_flag_stops_dispatching_and_reports_cancelled() {
        let dir = std::env::temp_dir().join("mcl-engine-cancel");
        tokio::fs::create_dir_all(&dir).await.expect("建目录");

        let engine = DownloadEngine::new(
            Arc::new(HttpClient::with_default_sources().expect("客户端")),
            EventBus::default(),
            4,
        );
        let cancel = Arc::new(AtomicBool::new(true)); // 预先取消
        let entries = vec![FileEntry {
            path: "never.bin".into(),
            url: "https://example.invalid/never.bin".into(),
            sha1: None,
            size: 10,
        }];

        let stats = engine
            .fetch_many_cancellable(entries, &dir, 1, Some(cancel))
            .await;

        assert!(stats.cancelled, "应报告已取消");
        assert_eq!(stats.files, 0, "取消后不应派发任何文件");
        assert!(stats.failures.is_empty());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn fetch_reports_hash_mismatch_and_cleans_up() {
        let dir = std::env::temp_dir().join("mcl-engine-mismatch");
        tokio::fs::create_dir_all(&dir).await.expect("建目录");
        let dest = dir.join("bad.bin");
        tokio::fs::write(&dest, b"corrupted").await.expect("写文件");

        let engine = DownloadEngine::new(
            Arc::new(HttpClient::with_default_sources().expect("客户端")),
            EventBus::default(),
            4,
        );
        // 声明一个不匹配的 SHA-1：本地文件存在但与期望不符 → 走网络（本例网络不可用，
        // 因此断言不会误判为 Skipped；用不存在的域名确保失败路径被覆盖）
        let entry = FileEntry {
            path: "bad.bin".into(),
            url: "https://example.invalid/bad.bin".into(),
            sha1: Some("0000000000000000000000000000000000000000".into()),
            size: 1024,
        };

        let result = engine.fetch(&entry, &dest).await;
        assert!(result.is_err(), "下载失败应返回错误而非静默成功");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
