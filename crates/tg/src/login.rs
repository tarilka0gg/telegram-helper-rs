//! Login over raw TL calls (instead of `Client::request_login_code`) so we can see *how* Telegram
//! delivered the code (app / SMS / call / ...), resend it, and give the user a real explanation.

use anyhow::{bail, Result};
use grammers_client::{
    client::PasswordToken,
    peer::User,
    session::{types::{PeerInfo, UpdateState, UpdatesState}, Session},
    sender::SenderPoolFatHandle,
    tl, Client, InvocationError,
};

use crate::dbsession::DbSession;

#[derive(Clone)]
pub struct CodeInfo {
    pub phone: String,
    pub hash: String,
    /// Human description of the delivery channel.
    pub via: String,
    /// What a `/resend` would switch to, if anything.
    pub next: Option<String>,
}

fn variant(dbg: String) -> String {
    dbg.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("?").to_string()
}

fn describe_type(t: &tl::enums::auth::SentCodeType) -> String {
    match variant(format!("{t:?}")).as_str() {
        "App" => "у застосунок Telegram — шукай чат «Telegram» (з галочкою) на іншому пристрої, де акаунт уже відкритий".into(),
        "Sms" => "SMS на номер".into(),
        "Call" => "голосовий дзвінок на номер".into(),
        "FlashCall" | "MissedCall" => "дзвінок-скидання (код = останні цифри номера, що дзвонить)".into(),
        "FragmentSms" => "SMS через Fragment (анонімний номер)".into(),
        "FirebaseSms" => "SMS через Firebase".into(),
        "EmailCode" => "на електронну пошту, прив'язану до акаунта".into(),
        "SetUpEmailRequired" => "Telegram вимагає спершу прив'язати email (вхід через сторонній клієнт неможливий, поки цього не зроблено в офіційному застосунку)".into(),
        other => other.to_string(),
    }
}

fn describe_next(t: &tl::enums::auth::CodeType) -> String {
    match variant(format!("{t:?}")).as_str() {
        "Sms" => "SMS".into(),
        "Call" => "дзвінок".into(),
        "FlashCall" | "MissedCall" => "дзвінок-скидання".into(),
        "FragmentSms" => "SMS через Fragment".into(),
        other => other.to_string(),
    }
}

fn info_from(phone: &str, sc: tl::types::auth::SentCode) -> Result<CodeInfo> {
    if variant(format!("{:?}", sc.r#type)) == "SetUpEmailRequired" {
        bail!("{}", describe_type(&sc.r#type));
    }
    Ok(CodeInfo { phone: phone.to_string(), hash: sc.phone_code_hash, via: describe_type(&sc.r#type), next: sc.next_type.as_ref().map(describe_next) })
}

fn settings() -> tl::enums::CodeSettings {
    tl::types::CodeSettings { allow_flashcall: false, current_number: false, allow_app_hash: false, allow_missed_call: false, allow_firebase: false, logout_tokens: None, token: None, app_sandbox: None, unknown_number: false }.into()
}

pub async fn send_code(client: &Client, handle: &SenderPoolFatHandle, session: &DbSession, phone: &str, api_id: i32, api_hash: &str) -> Result<CodeInfo> {
    let req = tl::functions::auth::SendCode { phone_number: phone.to_string(), api_id, api_hash: api_hash.to_string(), settings: settings() };
    let sent = match client.invoke(&req).await {
        Err(InvocationError::Rpc(e)) if e.code == 303 => {
            // The account lives on another data centre: switch home DC and ask again.
            let old = session.home_dc_id()?;
            let new = e.value.ok_or_else(|| anyhow::anyhow!("DC migrate without target"))? as i32;
            handle.disconnect_from_dc(old);
            session.set_home_dc_id(new).await?;
            client.invoke(&req).await
        }
        other => other,
    }
    .map_err(|e| anyhow::anyhow!("SendCode: {e}"))?;
    match sent {
        tl::enums::auth::SentCode::Code(sc) => info_from(phone, sc),
        _ => bail!("unexpected SendCode result (already authorized?)"),
    }
}

pub async fn resend_code(client: &Client, info: &CodeInfo) -> Result<CodeInfo> {
    if info.next.is_none() {
        bail!("Telegram не пропонує іншого способу доставки для цього номера");
    }
    let sent = client
        .invoke(&tl::functions::auth::ResendCode { phone_number: info.phone.clone(), phone_code_hash: info.hash.clone(), reason: None })
        .await
        .map_err(|e| anyhow::anyhow!("ResendCode: {e}"))?;
    match sent {
        tl::enums::auth::SentCode::Code(sc) => info_from(&info.phone, sc),
        _ => bail!("unexpected ResendCode result"),
    }
}

pub enum SignIn {
    Done(User),
    Password(PasswordToken),
    InvalidCode,
    SignUpRequired,
}

pub async fn sign_in(client: &Client, session: &DbSession, info: &CodeInfo, code: &str) -> Result<SignIn> {
    let req = tl::functions::auth::SignIn { phone_number: info.phone.clone(), phone_code_hash: info.hash.clone(), phone_code: Some(code.to_string()), email_verification: None };
    match client.invoke(&req).await {
        Ok(tl::enums::auth::Authorization::Authorization(a)) => Ok(SignIn::Done(complete_login(client, session, a).await?)),
        Ok(tl::enums::auth::Authorization::SignUpRequired(_)) => Ok(SignIn::SignUpRequired),
        Err(e) if e.is("SESSION_PASSWORD_NEEDED") => {
            let pw: tl::types::account::Password = client.invoke(&tl::functions::account::GetPassword {}).await.map_err(|e| anyhow::anyhow!("GetPassword: {e}"))?.into();
            Ok(SignIn::Password(PasswordToken::new(pw)))
        }
        Err(e) if e.is("PHONE_CODE_*") => Ok(SignIn::InvalidCode),
        Err(e) => bail!("SignIn: {e}"),
    }
}

/// Mirror of grammers' private `complete_login`: remember who "self" is and the update state.
pub async fn complete_login(client: &Client, session: &DbSession, auth: tl::types::auth::Authorization) -> Result<User> {
    let state = client.invoke(&tl::functions::updates::GetState {}).await.ok();
    let user = User::from_raw(client, auth.user);
    let auth = user.to_ref().await.map_err(|e| anyhow::anyhow!("{e}"))?.ok_or_else(|| anyhow::anyhow!("no self ref"))?.auth;
    session.cache_peer(&PeerInfo::User { id: user.id().bare_id_unchecked(), auth: Some(auth), bot: Some(user.is_bot()), is_self: Some(true) }).await?;
    if let Some(tl::enums::updates::State::State(s)) = state {
        session.set_update_state(UpdateState::All(UpdatesState { pts: s.pts, qts: s.qts, date: s.date, seq: s.seq, channels: Vec::new() })).await?;
    }
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_type_descriptions() {
        let app = tl::enums::auth::SentCodeType::App(tl::types::auth::SentCodeTypeApp { length: 5 });
        assert!(describe_type(&app).contains("застосунок"));
        let sms = tl::enums::auth::SentCodeType::Sms(tl::types::auth::SentCodeTypeSms { length: 5 });
        assert!(describe_type(&sms).starts_with("SMS"));
        assert_eq!(describe_next(&tl::enums::auth::CodeType::Sms), "SMS");
    }
}

// ------------------------------------------------------------------ QR login

pub enum QrPoll {
    /// Show this `tg://login?token=…` as a QR code and keep polling.
    Waiting(String),
    Done(User),
    Password(PasswordToken),
}

fn qr_url(token: &[u8]) -> String {
    use base64::Engine;
    format!("tg://login?token={}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token))
}

/// One step of QR login: (re)export the token; follows a DC migration; completes on scan.
pub async fn qr_poll(client: &Client, session: &DbSession, api_id: i32, api_hash: &str) -> Result<QrPoll> {
    use tl::enums::auth::LoginToken as T;
    let req = tl::functions::auth::ExportLoginToken { api_id, api_hash: api_hash.to_string(), except_ids: vec![] };
    let mut res = match client.invoke(&req).await {
        Ok(r) => r,
        Err(e) if e.is("SESSION_PASSWORD_NEEDED") => return password_step(client).await,
        Err(e) => bail!("ExportLoginToken: {e}"),
    };
    if let T::MigrateTo(m) = &res {
        res = match client.invoke_in_dc(m.dc_id, &tl::functions::auth::ImportLoginToken { token: m.token.clone() }).await {
            Ok(r) => r,
            Err(e) if e.is("SESSION_PASSWORD_NEEDED") => return password_step(client).await,
            Err(e) => bail!("ImportLoginToken: {e}"),
        };
    }
    match res {
        T::Token(t) => Ok(QrPoll::Waiting(qr_url(&t.token))),
        T::Success(s) => match s.authorization {
            tl::enums::auth::Authorization::Authorization(a) => Ok(QrPoll::Done(complete_login(client, session, a).await?)),
            _ => bail!("sign-up required for this account"),
        },
        T::MigrateTo(_) => bail!("repeated DC migration"),
    }
}

async fn password_step(client: &Client) -> Result<QrPoll> {
    let pw: tl::types::account::Password = client.invoke(&tl::functions::account::GetPassword {}).await.map_err(|e| anyhow::anyhow!("GetPassword: {e}"))?.into();
    Ok(QrPoll::Password(PasswordToken::new(pw)))
}

#[cfg(test)]
mod qr_tests {
    #[test]
    fn qr_url_is_url_safe_base64() {
        let u = super::qr_url(&[0xfb, 0xff, 0xfe, 1, 2, 3]);
        assert_eq!(u, "tg://login?token=-__-AQID");
    }
}
