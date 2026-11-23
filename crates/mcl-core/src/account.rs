//! 账号存储
//!
//! 持久化账号列表与当前账号。**令牌不落在此文件**——正版令牌由 keyring 管理
//!，本文件只保存非敏感的档案信息（名称/UUID/类型/过期时间）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, CoreResult};
use crate::models::{Account, AccountKind};

/// 账号存储文件格式
#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountFile {
    current: Option<String>,
    accounts: Vec<Account>,
}

pub struct AccountStore {
    path: PathBuf,
    data: AccountFile,
}

impl AccountStore {
    /// 打开（或初始化）账号存储
    pub fn open(path: impl Into<PathBuf>) -> CoreResult<Self> {
        let path = path.into();
        let data = if path.is_file() {
            let bytes = std::fs::read(&path)?;
            serde_json::from_slice(&bytes).unwrap_or_default()
        } else {
            AccountFile::default()
        };
        Ok(Self { path, data })
    }

    /// 默认位置：`<数据目录>/accounts.json`
    pub fn open_default() -> CoreResult<Self> {
        Self::open(crate::platform::paths::data_dir().join("accounts.json"))
    }

    fn persist(&self) -> CoreResult<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&self.data)?;
        // 先写临时文件再改名，避免写入中断导致账号文件损坏
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn list(&self) -> &[Account] {
        &self.data.accounts
    }

    pub fn current(&self) -> Option<&Account> {
        let id = self.data.current.as_deref()?;
        self.data.accounts.iter().find(|account| account.id == id)
    }

    /// 设为当前账号
    pub fn set_current(&mut self, id: &str) -> CoreResult<()> {
        if !self.data.accounts.iter().any(|account| account.id == id) {
            return Err(CoreError::Auth(format!("账号不存在：{id}")));
        }
        self.data.current = Some(id.to_string());
        self.persist()
    }

    /// 添加账号（重复 ID 时更新档案）；首个账号自动设为当前
    pub fn upsert(&mut self, account: Account) -> CoreResult<bool> {
        let is_new = match self
            .data
            .accounts
            .iter_mut()
            .find(|existing| existing.id == account.id)
        {
            Some(existing) => {
                *existing = account.clone();
                false
            }
            None => {
                self.data.accounts.push(account.clone());
                true
            }
        };

        if self.data.current.is_none() {
            self.data.current = Some(account.id.clone());
        }
        self.persist()?;
        Ok(is_new)
    }

    /// 添加离线账号
    pub fn add_offline(&mut self, username: &str) -> CoreResult<Account> {
        let username = username.trim();
        if username.is_empty() {
            return Err(CoreError::Auth("用户名不能为空".into()));
        }

        let account = Account {
            id: format!("offline:{username}"),
            name: username.to_string(),
            uuid: crate::auth::offline_uuid(username),
            kind: AccountKind::Offline,
            expires_at: None,
        };
        self.upsert(account.clone())?;
        Ok(account)
    }

    /// 删除账号；若删除的是当前账号则切换为列表首个（无账号则清空）
    pub fn remove(&mut self, id: &str) -> CoreResult<bool> {
        let before = self.data.accounts.len();
        self.data.accounts.retain(|account| account.id != id);
        let removed = self.data.accounts.len() != before;

        if removed && self.data.current.as_deref() == Some(id) {
            self.data.current = self.data.accounts.first().map(|account| account.id.clone());
        }
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> AccountStore {
        let dir = std::env::temp_dir().join(format!("mcl-account-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        AccountStore::open(dir.join("accounts.json")).expect("打开存储")
    }

    #[test]
    fn first_account_becomes_current() {
        let mut store = temp_store("first");
        let account = store.add_offline("Steve").expect("添加账号");

        assert_eq!(account.kind, AccountKind::Offline);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.current().map(|a| a.name.as_str()), Some("Steve"));
    }

    #[test]
    fn accounts_persist_across_reopen() {
        let dir = std::env::temp_dir().join("mcl-account-persist");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("accounts.json");

        {
            let mut store = AccountStore::open(&path).expect("打开存储");
            store.add_offline("Alex").expect("添加");
            store.add_offline("Steve").expect("添加");
        }

        let reopened = AccountStore::open(&path).expect("重新打开");
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(reopened.current().map(|a| a.name.as_str()), Some("Alex"));
    }

    #[test]
    fn set_current_validates_existence() {
        let mut store = temp_store("setcurrent");
        store.add_offline("Alex").expect("添加");

        assert!(store.set_current("offline:Missing").is_err());
        assert!(store.set_current("offline:Alex").is_ok());
    }

    #[test]
    fn removing_current_falls_back_to_remaining() {
        let mut store = temp_store("remove");
        store.add_offline("Alex").expect("添加");
        store.add_offline("Steve").expect("添加");

        assert!(store.remove("offline:Alex").expect("删除"));
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.current().map(|a| a.name.as_str()), Some("Steve"));
    }

    #[test]
    fn duplicate_username_updates_in_place() {
        let mut store = temp_store("duplicate");
        store.add_offline("Steve").expect("添加");

        let same = store.add_offline("Steve").expect("再次添加");
        let is_new = store.upsert(same).expect("更新");

        assert!(!is_new, "同名账号应更新而非新增");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn empty_username_is_rejected() {
        let mut store = temp_store("empty");
        assert!(store.add_offline("   ").is_err());
    }
}
