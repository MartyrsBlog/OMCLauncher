//! HTTP 客户端：多源 failover + 重试
//!
//! 默认策略为**故障切换**（按源优先级依次尝试），而非多源竞速——遵守
//! BMCLAPI 等公益镜像的使用规范。
//!
//! （10s 超时），BMCLAPI 可达——failover 是可用性刚需，而非可选优化。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::download::DownloadSource;
use crate::error::{CoreError, CoreResult};

const ATTEMPTS_PER_SOURCE: usize = 2;
const RETRY_BACKOFF: Duration = Duration::from_millis(600);
/// 源失败后的冷却时长：期间跳过该源，避免每个请求都先去撞一遍不可达的源
const SOURCE_COOLDOWN: Duration = Duration::from_secs(90);

/// 默认请求节流：**不限速**（压力由下载引擎的并发信号量控制）
///
/// 对 BMCLAPI 以 64/128/256 并发 + 不限速各发起 300 个资源请求（合计 ~1200 次，
/// 请求速率 45~64/s），全部零失败、无 403。此前默认 20 QPS 的依据是社区报告
/// 吞吐损失约 7 倍（8.7 vs 62.5 文件/s），故取消默认限速。
///
/// 防御措施不依赖猜测阈值：403/429 视为限流信号，触发该源冷却退避并 failover
/// （见 `send_with_failover`）；镜像端若真实限流即自动适应。
pub const DEFAULT_QPS: f64 = 0.0;

pub struct HttpClient {
    client: reqwest::Client,
    sources: Vec<Arc<dyn DownloadSource>>,
    /// 每个源的冷却截止时间（`None` 表示健康）
    health: Arc<Mutex<Vec<Option<Instant>>>>,
    cooldown: Duration,
    /// 全局请求节流器（保护公益镜像，见 [`RateLimiter`]）
    limiter: Arc<RateLimiter>,
}

/// 全局请求节流器：保证任意两次请求起始时间至少间隔 `min_interval`。
///
/// 动机：对公益镜像（BMCLAPI）保持礼貌的可选手段。`qps <= 0` 表示不限速——
/// 默认不限速，压力控制交给并发信号量；403/429 的自动退避兜底见 `send_with_failover`。
/// BMCLAPI 使用条款要求「在下载界面标明来源」，对应 UI 义务见 `mcl-app` 设置页。
pub struct RateLimiter {
    min_interval: Duration,
    last_request: Mutex<Option<Instant>>,
}

impl RateLimiter {
    /// `qps <= 0` 表示不限速
    pub fn new(qps: f64) -> Self {
        let min_interval = if qps > 0.0 {
            Duration::from_secs_f64(1.0 / qps)
        } else {
            Duration::ZERO
        };
        Self {
            min_interval,
            last_request: Mutex::new(None),
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.min_interval.is_zero()
    }

    pub fn qps(&self) -> f64 {
        if self.min_interval.is_zero() {
            0.0
        } else {
            1.0 / self.min_interval.as_secs_f64()
        }
    }

    /// 取得一次请求配额（必要时异步等待）
    async fn acquire(&self) {
        if self.min_interval.is_zero() {
            return;
        }

        loop {
            let wait = {
                let mut last = self.last_request.lock().expect("节流锁未被毒化");
                let now = Instant::now();
                match *last {
                    Some(previous) => {
                        let next_allowed = previous + self.min_interval;
                        if now >= next_allowed {
                            *last = Some(now);
                            return;
                        }
                        next_allowed - now
                    }
                    None => {
                        *last = Some(now);
                        return;
                    }
                }
            };
            tokio::time::sleep(wait).await;
        }
    }
}

impl HttpClient {
    /// 构造客户端；`sources` 会按优先级排序（官方源优先，镜像兜底）
    pub fn new(mut sources: Vec<Arc<dyn DownloadSource>>) -> CoreResult<Self> {
        sources.sort_by_key(|source| source.priority());
        Self::with_ordered_sources(sources)
    }

    /// 按给定顺序使用下载源（不做优先级排序）——「镜像优先」设置走这里
    pub fn with_ordered_sources(sources: Vec<Arc<dyn DownloadSource>>) -> CoreResult<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("OMCLauncher/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(180));
        // 用户配置的代理（空/非法时按直连）；启动时读取，修改后需重启生效
        if let Some(proxy) = crate::settings::LauncherSettings::load().proxy() {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().map_err(net)?;

        let source_count = sources.len();
        Ok(Self {
            client,
            sources,
            health: Arc::new(Mutex::new(vec![None; source_count])),
            cooldown: SOURCE_COOLDOWN,
            limiter: Arc::new(RateLimiter::new(DEFAULT_QPS)),
        })
    }

    /// 覆盖默认节流值（`qps <= 0` 表示不限速；用于性能实验）
    pub fn with_qps(mut self, qps: f64) -> Self {
        self.limiter = Arc::new(RateLimiter::new(qps));
        self
    }

    pub fn limiter(&self) -> &Arc<RateLimiter> {
        &self.limiter
    }

    /// 当前健康（未冷却）且按优先级排序的源下标；全部冷却时回退为全量尝试，
    /// 避免"所有源都被判定不可用"导致永久瘫痪
    fn candidate_sources(&self) -> Vec<usize> {
        let health = self.health.lock().expect("健康状态锁未被毒化");
        let now = Instant::now();

        let healthy: Vec<usize> = (0..self.sources.len())
            .filter(|index| health[*index].is_none_or(|until| until <= now))
            .collect();

        if healthy.is_empty() {
            (0..self.sources.len()).collect()
        } else {
            healthy
        }
    }

    fn report_success(&self, index: usize) {
        self.health.lock().expect("健康状态锁未被毒化")[index] = None;
    }

    fn report_failure(&self, index: usize) {
        self.health.lock().expect("健康状态锁未被毒化")[index] =
            Some(Instant::now() + self.cooldown);
    }

    /// 当前可用源数量（诊断用）
    pub fn healthy_source_count(&self) -> usize {
        self.candidate_sources().len()
    }

    /// 默认客户端：官方源 + BMCLAPI 镜像（游戏文件）+ MCIM（模组站内容兜底）
    /// 顺序由设置项 `download_source` 决定；MCIM 始终排在最后，避免影响游戏文件下载。
    pub fn with_default_sources() -> CoreResult<Self> {
        let official: Arc<dyn DownloadSource> = Arc::new(crate::download::OfficialSource);
        let mirror: Arc<dyn DownloadSource> = Arc::new(crate::download::BmclapiSource::default());
        let content: Arc<dyn DownloadSource> = Arc::new(crate::download::McimSource::default());
        let prefer_mirror = crate::settings::LauncherSettings::load().prefer_mirror();
        let sources = if prefer_mirror {
            vec![mirror, official, content]
        } else {
            vec![official, mirror, content]
        };
        Self::with_ordered_sources(sources)
    }

    pub fn sources(&self) -> &[Arc<dyn DownloadSource>] {
        &self.sources
    }

    /// GET JSON（自动 failover）
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> CoreResult<T> {
        let response = self.send_with_failover(url, None).await?;
        response.json::<T>().await.map_err(net)
    }

    /// GET 文本（自动 failover）
    pub async fn get_text(&self, url: &str) -> CoreResult<String> {
        let response = self.send_with_failover(url, None).await?;
        response.text().await.map_err(net)
    }

    /// POST JSON（不走镜像 failover；用于 Modrinth 这类单一 API）
    pub async fn post_json<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
    ) -> CoreResult<T> {
        let response = self.client.post(url).json(body).send().await.map_err(net)?;
        if !response.status().is_success() {
            return Err(crate::error::CoreError::Network(format!(
                "HTTP {}：{url}",
                response.status()
            )));
        }
        response.json::<T>().await.map_err(net)
    }

    /// 普通 GET（自动 failover，不指定 Range）——用于大小未知的文件
    pub async fn get(&self, url: &str) -> CoreResult<reqwest::Response> {
        self.send_with_failover(url, None).await
    }

    /// 范围请求（分块下载，`start`/`end` 均为包含端点）
    pub async fn get_range(
        &self,
        url: &str,
        start: u64,
        end: u64,
    ) -> CoreResult<reqwest::Response> {
        self.send_with_failover(url, Some((start, end))).await
    }

    /// 按源优先级尝试（跳过冷却中的源），每源最多 [`ATTEMPTS_PER_SOURCE`] 次；
    /// 候选源全部失败后，对**冷却中的源再做一轮兜底尝试**。
    async fn send_with_failover(
        &self,
        official_url: &str,
        range: Option<(u64, u64)>,
    ) -> CoreResult<reqwest::Response> {
        let mut errors: Vec<String> = Vec::new();
        let candidates = self.candidate_sources();

        if let Some(response) = self
            .try_sources(&candidates, official_url, range, &mut errors)
            .await
        {
            return Ok(response);
        }

        // 窗口（候选集为空或只剩一个，恰逢其失效则请求直接失败）。冷却只是节流
        // 策略，失败路径上再试一轮冷却源，可显著降低瞬时窗口导致的失败率。
        let cooled: Vec<usize> = (0..self.sources.len())
            .filter(|index| !candidates.contains(index))
            .collect();
        if !cooled.is_empty() {
            if let Some(response) = self
                .try_sources(&cooled, official_url, range, &mut errors)
                .await
            {
                return Ok(response);
            }
        }

        Err(CoreError::Network(format!(
            "{official_url} 在所有可用源均失败：{}",
            errors.join("；")
        )))
    }

    /// 依次尝试给定源集合；成功返回响应，全部失败返回 `None`（错误累积到 `errors`）
    async fn try_sources(
        &self,
        indices: &[usize],
        official_url: &str,
        range: Option<(u64, u64)>,
        errors: &mut Vec<String>,
    ) -> Option<reqwest::Response> {
        for &index in indices {
            let source = &self.sources[index];
            let url = source.resolve(official_url);
            let mut source_failed = false;

            for attempt in 0..ATTEMPTS_PER_SOURCE {
                // 全局节流（可配置；默认不限速，压力由并发信号量控制）
                self.limiter.acquire().await;

                let mut request = self.client.get(&url);
                if let Some((start, end)) = range {
                    request =
                        request.header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
                }

                match request.send().await {
                    Ok(response) if response.status().is_success() => {
                        self.report_success(index);
                        return Some(response);
                    }
                    Ok(response) => {
                        let status = response.status();
                        errors.push(format!("{} 返回 HTTP {}", source.name(), status));
                        // 403/429 视为限流信号：冷却该源自动退避（不依赖预设阈值，
                        // 镜像端真实限流时即触发）；其余 4xx 多为资源不存在
                        // （换源也不会好），不触发冷却
                        source_failed = status == reqwest::StatusCode::FORBIDDEN
                            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    }
                    Err(error) => {
                        errors.push(format!("{} 请求失败：{error}", source.name()));
                        source_failed = true;
                    }
                }

                if attempt + 1 < ATTEMPTS_PER_SOURCE {
                    tokio::time::sleep(RETRY_BACKOFF * (attempt as u32 + 1)).await;
                }
            }

            if source_failed {
                tracing::warn!(
                    "下载源 {} 暂时不可用，冷却 {:?}",
                    source.name(),
                    self.cooldown
                );
                self.report_failure(index);
            }
        }

        None
    }
}

fn net(error: reqwest::Error) -> CoreError {
    CoreError::Network(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_are_sorted_by_priority() {
        let http = HttpClient::with_default_sources().expect("构造客户端");
        let names: Vec<&str> = http.sources().iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            vec!["官方", "BMCLAPI", "MCIM"],
            "低优先级数值应排在前面"
        );
    }

    #[test]
    fn cooling_down_source_is_skipped_then_recovers() {
        let mut http = HttpClient::with_default_sources().expect("构造客户端");
        http.cooldown = Duration::from_millis(50);

        assert_eq!(http.healthy_source_count(), 3);

        // 标记官方源失败 → 候选剩镜像与内容镜像
        http.report_failure(0);
        assert_eq!(http.healthy_source_count(), 2);

        http.report_failure(1);
        assert_eq!(http.healthy_source_count(), 1);

        // 全部冷却时回退为全量尝试（避免彻底瘫痪）
        http.report_failure(2);
        assert_eq!(http.healthy_source_count(), 3);

        // 冷却到期后恢复
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(http.healthy_source_count(), 3);

        http.report_success(0);
        assert_eq!(http.healthy_source_count(), 3);
    }
}
