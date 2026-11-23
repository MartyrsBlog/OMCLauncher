//! 凭据存储
//!
//! 令牌经系统钥匙串保存：Windows Credential Manager / macOS Keychain / Linux secret-service。
//! **禁止明文落盘**——账号档案（名称/UUID）走 `accounts.json`，令牌只走这里。
//!
//! 运行环境无钥匙串时（如无 D-Bus 会话的容器、精简发行版），保存/读取会失败并返回可读错误；
//! 调用方应降级为「本次会话内存持有」而不是回退到明文文件。

use crate::error::{CoreError, CoreResult};

const SERVICE: &str = "OMCLauncher";

/// 令牌类型（同一账号可能同时存在多个令牌）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// Minecraft access token（启动游戏用）
    MinecraftAccess,
    /// MSA refresh token（静默续期用）
    MsaRefresh,
}

impl TokenKind {
    fn account_key(self, account_id: &str) -> String {
        match self {
            TokenKind::MinecraftAccess => format!("{account_id}:mc-access"),
            TokenKind::MsaRefresh => format!("{account_id}:msa-refresh"),
        }
    }
}

/// 保存令牌到系统钥匙串
pub fn save_token(account_id: &str, kind: TokenKind, token: &str) -> CoreResult<()> {
    let entry = keyring::Entry::new(SERVICE, &kind.account_key(account_id))
        .map_err(|error| CoreError::Auth(format!("访问系统钥匙串失败：{error}")))?;

    entry
        .set_password(token)
        .map_err(|error| CoreError::Auth(format!("写入系统钥匙串失败：{error}")))
}

/// 读取令牌（不存在返回 `Ok(None)`）
pub fn load_token(account_id: &str, kind: TokenKind) -> CoreResult<Option<String>> {
    let entry = keyring::Entry::new(SERVICE, &kind.account_key(account_id))
        .map_err(|error| CoreError::Auth(format!("访问系统钥匙串失败：{error}")))?;

    match entry.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(CoreError::Auth(format!("读取系统钥匙串失败：{error}"))),
    }
}

/// 删除令牌（清理账号时调用）
pub fn delete_token(account_id: &str, kind: TokenKind) -> CoreResult<()> {
    let entry = keyring::Entry::new(SERVICE, &kind.account_key(account_id))
        .map_err(|error| CoreError::Auth(format!("访问系统钥匙串失败：{error}")))?;

    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(CoreError::Auth(format!("删除系统钥匙串条目失败：{error}"))),
    }
}

/// 系统钥匙串是否可用（用于给用户友好提示）
pub fn is_available() -> bool {
    match keyring::Entry::new(SERVICE, "__probe__") {
        Ok(entry) => !matches!(
            entry.get_password(),
            Err(keyring::Error::PlatformFailure(_)) | Err(keyring::Error::NoStorageAccess(_))
        ),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_keys_are_namespaced_per_kind() {
        assert_eq!(
            TokenKind::MinecraftAccess.account_key("msa:abc"),
            "msa:abc:mc-access"
        );
        assert_eq!(
            TokenKind::MsaRefresh.account_key("msa:abc"),
            "msa:abc:msa-refresh"
        );
        assert_ne!(
            TokenKind::MinecraftAccess.account_key("msa:abc"),
            TokenKind::MsaRefresh.account_key("msa:abc")
        );
    }

    #[test]
    fn roundtrip_when_keyring_is_available() {
        if !is_available() {
            eprintln!("跳过：当前环境无可用钥匙串（无 D-Bus 会话或未安装 secret-service）");
            return;
        }

        let account = "test:mcl-core-roundtrip";
        save_token(account, TokenKind::MinecraftAccess, "secret-token").expect("写入令牌");
        assert_eq!(
            load_token(account, TokenKind::MinecraftAccess)
                .expect("读取令牌")
                .as_deref(),
            Some("secret-token")
        );
        delete_token(account, TokenKind::MinecraftAccess).expect("删除令牌");
        assert!(load_token(account, TokenKind::MinecraftAccess)
            .expect("读取令牌")
            .is_none());
    }
}
