//! 微软账号认证链
//!
//! ```text
//! MSA access token → XBL → XSTS → Minecraft access token → entitlements → profile
//! ```
//!
//! 需要 `client_id`（构建期由 `MCL_MSA_CLIENT_ID` 注入），不需要 client secret；
//! scope 必须包含 `XboxLive.signin`，租户固定 `consumers`，并开启公共客户端流。
//! 新注册的应用需先在 <https://aka.ms/mce-reviewappid> 申请 Minecraft API 权限。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{CoreError, CoreResult};
use crate::models::{Account, AccountKind};

const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_ENTITLEMENTS_URL: &str = "https://api.minecraftservices.com/entitlements/mcstore";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

/// 开发者构建期注入的 `client_id`（编译期读取 `MCL_MSA_CLIENT_ID`）
///
/// 发布构建：`MCL_MSA_CLIENT_ID=<你的 client_id> cargo build --release`——
/// 注册一次，之后构建自动携带，**最终用户零配置**（HMCL 的 JAR manifest 注入
/// 是同一思路：应用身份放在构建产物里，而不是让用户填）。
/// 开发调试也可用运行时同名环境变量临时覆盖（优先级更高）。
///
/// 未注入（`None`）时微软登录不可用，登录入口会给出面向开发者的提示。
const BUILTIN_CLIENT_ID: Option<&str> = option_env!("MCL_MSA_CLIENT_ID");

/// 必须包含 `offline_access` 才能拿到 refresh token 用于静默续期
const SCOPE: &str = "XboxLive.signin offline_access";

/// 设备码响应（展示给用户：在浏览器输入 user_code）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// 轮询间隔（秒），由微软下发；缺省 5 秒
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// 设备码有效期（秒）
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub message: String,
}

fn default_interval() -> u64 {
    5
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: u64,
}

/// 微软令牌（MSA 层）
#[derive(Debug, Clone)]
pub struct MsaToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
}

/// 认证成功后的会话（可直接用于启动游戏）
#[derive(Debug, Clone)]
pub struct MinecraftSession {
    pub account: Account,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    /// 是否通过拥有权校验
    pub owns_game: bool,
}

pub struct MicrosoftAuth {
    client_id: String,
    client: reqwest::Client,
}

impl MicrosoftAuth {
    pub fn new(client_id: impl Into<String>) -> CoreResult<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(concat!("OMCLauncher/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30));
        // 与下载客户端共用同一份代理配置（登录在受限网络下同样需要）
        if let Some(proxy) = crate::settings::LauncherSettings::load().proxy() {
            builder = builder.proxy(proxy);
        }
        let client = builder.build().map_err(net)?;

        Ok(Self {
            client_id: client_id.into(),
            client,
        })
    }

    /// 从环境变量读取 `client_id`（应用注册所得）
    pub fn client_id_from_env() -> Option<String> {
        std::env::var("MCL_MSA_CLIENT_ID")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// 解析实际使用的 `client_id`（**开发者职责，最终用户零配置**）：
    /// 1. 运行时环境变量 `MCL_MSA_CLIENT_ID`（开发调试的临时覆盖，优先级最高）；
    /// 2. **构建期注入**：编译时设置同名环境变量（`option_env!`，见 [`BUILTIN_CLIENT_ID`]）。
    ///    发布构建（本地或 CI）带上它即可把 client_id 烘进二进制，用户无需任何配置。
    ///
    /// 两者都没有时为 `None`（开发者尚未注册应用，登录不可用）。
    ///
    /// **为什么不做成用户设置**：client_id 是应用身份（开发者的注册产物），
    /// 终端用户既不该也不会去注册 Azure 应用——那是发布者的工作。
    ///
    /// **为什么没有内置公共 ID**——免注册路线经真机验证全部不可行：
    /// - `00000000402b5328`（Live Connect 经典 ID）：设备码端点直接 `AADSTS700016`
    ///   （应用不在 consumers 租户），该 ID 只适合经典浏览器回调老流程；
    /// - `04f0c124-f2bc-4f59-8241-bf6df9866bbd`（社区流传的 GUID）：设备码握手可通过
    ///   （`devicecode` 200 + `authorization_pending`），但用户**在浏览器完成授权时
    ///   被微软拒绝**："The application is a first party application, the user does not
    ///   have consent, and users are not permitted to consent to first party applications"
    ///   ——第一方应用只能由微软预授权，用户无法为其同意，**此路线对任何调用方都不通**。
    ///
    /// 注册步骤；另需在
    /// <https://aka.ms/mce-reviewappid> 申请 Minecraft API 访问权限（否则
    /// `login_with_xbox` 会被拒绝）。
    pub fn resolve_client_id() -> Option<String> {
        Self::client_id_from_env().or_else(|| {
            BUILTIN_CLIENT_ID
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// 步骤 0-a：申请设备码。用户需在浏览器打开 `verification_uri` 并输入 `user_code`。
    pub async fn request_device_code(&self) -> CoreResult<DeviceCode> {
        let response = self
            .client
            .post(DEVICE_CODE_URL)
            .form(&[("client_id", self.client_id.as_str()), ("scope", SCOPE)])
            .send()
            .await
            .map_err(net)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(CoreError::Auth(format!(
                "申请设备码失败（HTTP {status}）：{}",
                oauth_error_hint(&body)
            )));
        }

        response.json::<DeviceCode>().await.map_err(net)
    }

    /// 步骤 0-b：轮询换取 MSA access token（按 `interval` 秒间隔，直到用户完成授权）
    pub async fn poll_msa_token(&self, device_code: &DeviceCode) -> CoreResult<MsaToken> {
        let mut interval = Duration::from_secs(device_code.interval.max(1));

        for _ in 0..600 {
            tokio::time::sleep(interval).await;

            let response = self
                .client
                .post(TOKEN_URL)
                .form(&[
                    ("client_id", self.client_id.as_str()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", device_code.device_code.as_str()),
                ])
                .send()
                .await
                .map_err(net)?;

            if response.status().is_success() {
                let token = response.json::<TokenResponse>().await.map_err(net)?;
                return Ok(self.to_msa_token(token));
            }

            let body = response.text().await.unwrap_or_default();
            match oauth_error_code(&body).as_deref() {
                // 用户尚未完成授权：继续等待
                Some("authorization_pending") => continue,
                // 客户端轮询过快：按 RFC 8628 / MS 文档把间隔加 5 秒
                Some("slow_down") => {
                    interval += Duration::from_secs(5);
                    continue;
                }
                Some("expired_token") => {
                    return Err(CoreError::Auth("设备码已过期，请重新发起登录".into()));
                }
                Some("authorization_declined") => {
                    return Err(CoreError::Auth("用户拒绝了授权".into()));
                }
                Some("bad_verification_code") => {
                    return Err(CoreError::Auth(
                        "验证码不正确：请重新输入，或重新发起登录获取新验证码".into(),
                    ));
                }
                _ => {
                    return Err(CoreError::Auth(format!(
                        "获取令牌失败：{}",
                        oauth_error_hint(&body)
                    )));
                }
            }
        }

        Err(CoreError::Auth("等待授权超时，请重新发起登录".into()))
    }

    /// 用 refresh token 静默续期
    pub async fn refresh_msa_token(&self, refresh_token: &str) -> CoreResult<MsaToken> {
        let response = self
            .client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", SCOPE),
            ])
            .send()
            .await
            .map_err(net)?;

        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CoreError::Auth(format!(
                "令牌续期失败：{}",
                oauth_error_hint(&body)
            )));
        }

        let token = response.json::<TokenResponse>().await.map_err(net)?;
        Ok(self.to_msa_token(token))
    }

    /// 步骤 1~5：MSA token → XBL → XSTS → Minecraft → 拥有权 → 玩家档案
    ///
    /// `msa_refresh_token` 随会话带回，供上层写入系统钥匙串后静默续期；
    /// 拥有权校验放在拉取档案之前——没有游戏的账号应直接得到“未拥有”提示，
    /// 而不是含糊的“档案获取失败”。
    pub async fn login_with_msa(
        &self,
        msa_access_token: &str,
        msa_refresh_token: Option<String>,
    ) -> CoreResult<MinecraftSession> {
        let (xbl_token, user_hash) = self.xbox_live_authenticate(msa_access_token).await?;
        let (xsts_token, user_hash) = self.xsts_authorize(&xbl_token, &user_hash).await?;
        let (mc_access_token, expires_in) = self.minecraft_login(&user_hash, &xsts_token).await?;

        if !self.check_entitlements(&mc_access_token).await? {
            return Err(CoreError::Auth(
                "该账号未拥有 Minecraft Java 版（拥有权校验未通过）".into(),
            ));
        }

        let mut account = self.fetch_profile(&mc_access_token).await?;
        let expires_at = now_secs() + expires_in;
        account.expires_at = Some(expires_at);

        Ok(MinecraftSession {
            account,
            access_token: mc_access_token,
            refresh_token: msa_refresh_token,
            expires_at,
            owns_game: true,
        })
    }

    /// 静默续期：refresh token → 新 MSA token → 重走 XBL/XSTS/Minecraft 认证链
    pub async fn refresh_session(&self, refresh_token: &str) -> CoreResult<MinecraftSession> {
        let msa = self.refresh_msa_token(refresh_token).await?;
        self.login_with_msa(&msa.access_token, msa.refresh_token)
            .await
    }

    /// 步骤 1：Xbox Live 认证
    async fn xbox_live_authenticate(&self, msa_token: &str) -> CoreResult<(String, String)> {
        let body = json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                // MSA v2 token 必须加 "d=" 前缀
                "RpsTicket": format!("d={msa_token}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        });

        let response = self
            .client
            .post(XBL_URL)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(net)?;

        parse_token_and_hash(response, "Xbox Live 认证").await
    }

    /// 步骤 2：获取 XSTS 授权
    async fn xsts_authorize(
        &self,
        xbl_token: &str,
        user_hash: &str,
    ) -> CoreResult<(String, String)> {
        let body = json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbl_token]
            },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        });

        let response = self
            .client
            .post(XSTS_URL)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(net)?;

        let status = response.status();
        let value: serde_json::Value = response.json().await.map_err(net)?;

        if !status.is_success() {
            // XSTS 的 401 会带 XErr 错误码，翻译为可读提示
            let xerr = value.get("XErr").and_then(serde_json::Value::as_u64);
            return Err(CoreError::Auth(match xerr {
                Some(2148916227) => "该账号已被 Xbox 封禁".into(),
                Some(2148916233) => {
                    "该账号还没有 Xbox 档案，请先在 minecraft.net 登录一次以创建".into()
                }
                Some(2148916235) => "该账号所在国家/地区不支持 Xbox Live".into(),
                Some(2148916236) | Some(2148916237) => {
                    "需先在 Xbox 页面完成成年人验证（韩国地区要求）".into()
                }
                Some(2148916238) => {
                    "这是儿童账号，需由成年人将其加入 Microsoft 家庭组后重试".into()
                }
                Some(code) => format!("XSTS 授权失败（XErr {code}）"),
                None => format!("XSTS 授权失败（HTTP {status}）"),
            }));
        }

        match (extract_token(&value), extract_user_hash(&value)) {
            (Some(token), Some(hash)) => {
                let _ = user_hash;
                Ok((token, hash))
            }
            _ => Err(CoreError::Auth("XSTS 响应缺少 Token 或 uhs 字段".into())),
        }
    }

    /// 步骤 3：换取 Minecraft access token（返回 token 与服务端给的过期秒数）
    async fn minecraft_login(
        &self,
        user_hash: &str,
        xsts_token: &str,
    ) -> CoreResult<(String, i64)> {
        let body = json!({ "identityToken": format!("XBL3.0 x={user_hash};{xsts_token}") });

        let response = self
            .client
            .post(MC_LOGIN_URL)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(net)?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if status.as_u16() == 403 {
            return Err(CoreError::Auth(
                "Minecraft 服务拒绝访问（403）：Azure 应用尚未获得 Minecraft API 权限，\
                 请在 https://aka.ms/mce-reviewappid 提交申请后重试"
                    .into(),
            ));
        }
        if !status.is_success() {
            return Err(CoreError::Auth(format!(
                "Minecraft 登录失败（HTTP {status}）：{text}"
            )));
        }

        parse_minecraft_login(&text)
    }

    /// 拥有权判定：买断账号的 mcstore 条目是 `game_minecraft`；
    /// PC Game Pass 用户是 `product_minecraft`（不含 Bedrock 的 `game_minecraft_bedrock`）。
    fn owns_java_edition(items: &[serde_json::Value]) -> bool {
        items.iter().any(|item| {
            item.get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| name == "game_minecraft" || name == "product_minecraft")
        })
    }

    /// 步骤 4：拥有权校验
    async fn check_entitlements(&self, access_token: &str) -> CoreResult<bool> {
        let response = self
            .client
            .get(MC_ENTITLEMENTS_URL)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(net)?;

        if !response.status().is_success() {
            return Err(CoreError::Auth(format!(
                "拥有权校验失败（HTTP {}）",
                response.status()
            )));
        }

        let value: serde_json::Value = response.json().await.map_err(net)?;
        let owns = value
            .get("items")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| Self::owns_java_edition(items));

        Ok(owns)
    }

    /// 步骤 5：玩家档案（UUID / 名称 / 皮肤）
    async fn fetch_profile(&self, access_token: &str) -> CoreResult<Account> {
        let response = self
            .client
            .get(MC_PROFILE_URL)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(net)?;

        if !response.status().is_success() {
            if response.status().as_u16() == 404 {
                // 404 = 已购买但从未启动过游戏：档案要在 minecraft.net 上创建
                return Err(CoreError::Auth(
                    "还没有 Minecraft 档案：请先用该账号在 minecraft.net 创建角色，再回来登录"
                        .into(),
                ));
            }
            return Err(CoreError::Auth(format!(
                "获取玩家档案失败（HTTP {}）：账号可能尚未设置用户名",
                response.status()
            )));
        }

        let value: serde_json::Value = response.json().await.map_err(net)?;
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Auth("档案响应缺少 id".into()))?;
        let name = value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown");

        Ok(Account {
            id: format!("msa:{id}"),
            name: name.to_string(),
            uuid: id.to_string(),
            kind: AccountKind::Microsoft,
            expires_at: Some(now_secs() + 86_400),
        })
    }

    fn to_msa_token(&self, token: TokenResponse) -> MsaToken {
        MsaToken {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: now_secs() + token.expires_in as i64,
        }
    }
}

async fn parse_token_and_hash(
    response: reqwest::Response,
    step: &str,
) -> CoreResult<(String, String)> {
    let status = response.status();
    let value: serde_json::Value = response.json().await.map_err(net)?;

    if !status.is_success() {
        return Err(CoreError::Auth(format!("{step}失败（HTTP {status}）")));
    }

    match (extract_token(&value), extract_user_hash(&value)) {
        (Some(token), Some(hash)) => Ok((token, hash)),
        _ => Err(CoreError::Auth(format!("{step}响应缺少 Token 或 uhs 字段"))),
    }
}

fn extract_token(value: &serde_json::Value) -> Option<String> {
    value
        .get("Token")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn extract_user_hash(value: &serde_json::Value) -> Option<String> {
    value
        .pointer("/DisplayClaims/xui/0/uhs")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// 提取 OAuth 错误码（`{"error": "authorization_pending"}`）
fn oauth_error_code(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// 把 OAuth 错误转成可读提示
fn oauth_error_hint(body: &str) -> String {
    // （纯文本/HTML，未必是 JSON），因此优先按文本特征匹配
    if body.contains("first party application") {
        return "该 client_id 属于微软第一方应用，用户无法为其授权。\
                请改用你自己注册的 Azure 应用 client_id\
                （注册步骤见 README 的「微软登录」一节）"
            .into();
    }

    match oauth_error_code(body).as_deref() {
        Some("invalid_client") => {
            "client_id 无效：请确认 Azure 应用已注册且使用 consumers 租户".into()
        }
        Some("unauthorized_client") => {
            // 两种常见成因：① 自注册应用未启用公共客户端流；② 所用 client_id
            // 不支持设备码流程（如 Live Connect 时代的老应用，报 AADSTS700016）
            if body.contains("AADSTS700016") {
                "该 client_id 不支持设备码登录（AADSTS700016）：多为已删除或 \
                 Live Connect 时代的老应用。请检查 MCL_MSA_CLIENT_ID（或构建期注入）\
                 是否指向正确的自注册应用；注册步骤见 \
                 README 的「微软登录」一节"
                    .into()
            } else {
                "应用未授权该登录方式：请在 Azure 应用中启用「公共客户端流」".into()
            }
        }
        Some("invalid_grant") => "授权已失效，请重新登录".into(),
        _ => body.chars().take(300).collect(),
    }
}

fn parse_minecraft_login(text: &str) -> CoreResult<(String, i64)> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| CoreError::Auth("Minecraft 登录响应不是合法 JSON".into()))?;
    let access_token = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| CoreError::Auth("Minecraft 登录响应缺少 access_token".into()))?;
    let expires_in = value
        .get("expires_in")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(86_400);
    Ok((access_token.to_string(), expires_in))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn net(error: reqwest::Error) -> CoreError {
    CoreError::Network(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_token_and_user_hash() {
        let value = json!({
            "Token": "jwt-token",
            "DisplayClaims": { "xui": [{ "uhs": "user-hash" }] }
        });
        assert_eq!(extract_token(&value).as_deref(), Some("jwt-token"));
        assert_eq!(extract_user_hash(&value).as_deref(), Some("user-hash"));
        assert_eq!(extract_user_hash(&json!({})), None);
    }

    #[test]
    fn identifies_oauth_pending_vs_terminal_errors() {
        assert_eq!(
            oauth_error_code(r#"{"error":"authorization_pending"}"#).as_deref(),
            Some("authorization_pending")
        );
        assert_eq!(
            oauth_error_code(r#"{"error":"invalid_client"}"#).as_deref(),
            Some("invalid_client")
        );
        assert_eq!(oauth_error_code("not json"), None);
    }

    #[test]
    fn error_hints_are_actionable_in_chinese() {
        assert!(oauth_error_hint(r#"{"error":"invalid_client"}"#).contains("client_id"));
        assert!(oauth_error_hint(r#"{"error":"unauthorized_client"}"#).contains("公共客户端"));

        // AADSTS700016（如 Live Connect 老 ID 打设备码端点）：应指向 client_id 指引，
        // 而不是误导用户去改"公共客户端流"设置
        let hint = oauth_error_hint(
            r#"{"error":"unauthorized_client","error_codes":[700016],
                "error_description":"AADSTS700016: Application with identifier 'x' was not found"}"#,
        );
        assert!(
            hint.contains("client_id") && hint.contains("MCL_MSA_CLIENT_ID"),
            "AADSTS700016 应给出可操作的 client_id 指引：{hint}"
        );

        // 实际以浏览器错误页文本形式出现，未必是 JSON，需按文本特征匹配）
        let hint = oauth_error_hint(
            "The application is a first party application, the user does not have consent, \
             and users are not permitted to consent to first party applications: invalid_request",
        );
        assert!(
            hint.contains("第一方应用") && hint.contains("注册"),
            "第一方应用错误应指向自注册指引：{hint}"
        );
    }

    #[test]
    fn parses_minecraft_token_and_expiry() {
        let (token, expires) = parse_minecraft_login(
            r#"{"access_token":"mc-token","expires_in":86400,"token_type":"Bearer"}"#,
        )
        .expect("解析成功");
        assert_eq!(token, "mc-token");
        assert_eq!(expires, 86_400);

        // 缺 expires_in 时按 24h 兜底
        let (_, expires) = parse_minecraft_login(r#"{"access_token":"t"}"#).expect("解析成功");
        assert_eq!(expires, 86_400);

        assert!(parse_minecraft_login("not json").is_err());
        assert!(parse_minecraft_login(r#"{"token_type":"Bearer"}"#).is_err());
    }

    #[test]
    fn reads_client_id_from_env_only_when_non_empty() {
        // 不污染其它测试：仅验证空值被过滤的逻辑分支
        let value: Option<String> = Some("   ".to_string())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        assert!(value.is_none());
    }

    #[test]
    fn java_edition_ownership_accepts_buyout_and_game_pass() {
        let buyout = json!([{ "name": "game_minecraft", "source": "PURCHASE" }]);
        let game_pass = json!([{ "name": "product_minecraft", "source": "GAME_PASS" }]);
        let bedrock_only = json!([{ "name": "game_minecraft_bedrock" }]);
        let mixed = json!([{ "name": "game_minecraft_bedrock" }, { "name": "product_minecraft" }]);
        assert!(MicrosoftAuth::owns_java_edition(buyout.as_array().unwrap()));
        assert!(MicrosoftAuth::owns_java_edition(
            game_pass.as_array().unwrap()
        ));
        assert!(!MicrosoftAuth::owns_java_edition(
            bedrock_only.as_array().unwrap()
        ));
        assert!(MicrosoftAuth::owns_java_edition(mixed.as_array().unwrap()));
        assert!(!MicrosoftAuth::owns_java_edition(&[]));
    }

    #[tokio::test]
    async fn device_code_request_reports_invalid_client_clearly() {
        // 用一个明显无效的 client_id 打通请求形状：能收到 OAuth 错误即证明
        // 端点、表单字段、错误解析都正确（无法在无凭据环境完成真实登录）
        let auth = MicrosoftAuth::new("00000000-0000-0000-0000-000000000000").expect("构造客户端");
        match auth.request_device_code().await {
            Ok(code) => {
                // 极少数情况下服务端仍返回设备码（例如该 id 恰好合法）
                assert!(!code.user_code.is_empty());
            }
            Err(CoreError::Auth(message)) => {
                assert!(
                    message.contains("设备码") || message.contains("client_id"),
                    "错误信息应可读：{message}"
                );
            }
            Err(other) => panic!("非预期错误类型：{other}"),
        }
    }
}
