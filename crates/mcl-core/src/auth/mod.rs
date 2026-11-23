//! 账号与认证
//!
//! 认证链（T1，★★★★）：MSA → Xbox Live(XBL) → XSTS → Minecraft。
//! 实现参考 HMCL 微软认证模块的报文流程（GPL，算法参考、独立 Rust 实现），
//! **M0 认证链 PoC 的落地位置就是本模块**。
//!
//! `AuthProvider` trait：微软 / 离线 / authlib-injector 三种实现互不感知，
//! 便于 mock 测试与未来扩展。

pub mod microsoft;

use crate::error::{CoreError, CoreResult};
use crate::models::{Account, AccountKind};
use async_trait::async_trait;

/// 认证提供者抽象。异步接口以便接入 tokio 网络栈。
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// 提供者标识（UI 类型标签依据）
    fn id(&self) -> &'static str;

    /// 登录并返回账号档案（令牌由实现内部写入 keyring，不进入返回值）
    async fn login(&self) -> CoreResult<Account>;

    /// 刷新令牌。离线提供者返回原账号。
    async fn refresh(&self, account: &Account) -> CoreResult<Account>;

    fn kind(&self) -> AccountKind;
}

/// 离线模式：本地生成 UUID，明示风险提示
pub struct OfflineAuthProvider {
    pub username: String,
}

#[async_trait]
impl AuthProvider for OfflineAuthProvider {
    fn id(&self) -> &'static str {
        "offline"
    }

    fn kind(&self) -> AccountKind {
        AccountKind::Offline
    }

    async fn login(&self) -> CoreResult<Account> {
        if self.username.trim().is_empty() {
            return Err(CoreError::Auth("用户名为空".into()));
        }
        Ok(Account {
            id: format!("offline:{}", self.username),
            name: self.username.clone(),
            uuid: offline_uuid(&self.username),
            kind: AccountKind::Offline,
            expires_at: None,
        })
    }

    async fn refresh(&self, account: &Account) -> CoreResult<Account> {
        Ok(account.clone())
    }
}

/// 离线 UUID 派生。
///
/// TODO(M0)：替换为 Minecraft 规范的 `MD5("OfflinePlayer:<name>")` + 版本位改写，
/// 以兼容服务端离线模式的白名单/数据（当前用稳定哈希占位，保证同用户名同 UUID）。
pub fn offline_uuid(username: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    format!("OfflinePlayer:{username}").hash(&mut hasher);
    let value = hasher.finish();

    // 按 RFC 4122 形态拼装：第三组首半字节为版本位（3），第四组首半字节为变体位（8）
    let time_low = (value >> 32) as u32;
    let time_mid = (value >> 16) as u16;
    let time_hi_and_version = (value as u16 & 0x0FFF) | 0x3000;
    let clock_seq = ((value >> 48) as u16 & 0x3FFF) | 0x8000;
    let node = value & 0xFFFF_FFFF_FFFF;

    format!("{time_low:08x}-{time_mid:04x}-{time_hi_and_version:04x}-{clock_seq:04x}-{node:012x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::refresh_needed;

    #[tokio::test]
    async fn offline_login_creates_stable_uuid() {
        let provider = OfflineAuthProvider {
            username: "Steve".into(),
        };
        let account = provider.login().await.expect("离线登录应成功");

        assert_eq!(account.name, "Steve");
        assert_eq!(account.kind, AccountKind::Offline);
        assert_eq!(account.uuid, offline_uuid("Steve"));
        // 无过期时间 → 永不触发刷新
        assert!(!refresh_needed(account.expires_at, 1_000_000));
        // UUID 版本位为 3
        assert_eq!(&account.uuid[14..15], "3");
    }

    #[tokio::test]
    async fn offline_login_rejects_empty_username() {
        let provider = OfflineAuthProvider {
            username: "  ".into(),
        };
        assert!(provider.login().await.is_err());
    }

    #[tokio::test]
    async fn refresh_returns_same_offline_account() {
        let provider = OfflineAuthProvider {
            username: "Alex".into(),
        };
        let account = provider.login().await.expect("登录成功");
        let refreshed = provider.refresh(&account).await.expect("刷新成功");
        assert_eq!(account.uuid, refreshed.uuid);
    }
}
