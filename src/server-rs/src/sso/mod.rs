//! Optional single sign-on through an OpenID Connect provider (sso/index.ts).
//!
//! Off unless `AUTH_ISSUER` is set. At boot, before anything binds:
//!
//! ```ignore
//! sso::initialize_sso()?; // Err lists every misconfiguration; contacts nothing
//! ```
//!
//! Afterwards [`get_sso`] is the running single sign-on, or `None` when it is
//! off, and [`sso_mode`] is what the sign-in page is told.

pub mod accounts;
pub mod app_password;
pub mod config;
pub mod events;
pub mod session;
pub mod sign_in;

use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::json;

use crate::auth_client::{AuthClient, AuthClientOptions, Clock, Transport};
use crate::logger;

pub use config::{parse_link_users, parse_sso_config, post_logout_redirect, SsoConfig, SsoConfigError, SsoEnv};

/// Single sign-on as this process runs it: its settings and the provider client.
#[derive(Debug, Clone)]
pub struct Sso {
    pub config: SsoConfig,
    pub client: AuthClient,
}

static CURRENT: RwLock<Option<Arc<Sso>>> = RwLock::new(None);

fn set_current(sso: Option<Arc<Sso>>) {
    *CURRENT.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = sso;
}

/// What tests replace in the provider client: how it reaches the provider,
/// and its clock.
#[derive(Clone, Default)]
pub struct ClientOverrides {
    pub transport: Option<Arc<dyn Transport>>,
    pub now: Option<Clock>,
}

/// The settings of `env` and a provider client for them, or `None` when
/// `AUTH_ISSUER` is unset. Touches neither the network nor the running
/// single sign-on.
pub fn build_sso(env: &SsoEnv, overrides: ClientOverrides) -> Result<Option<Sso>, SsoConfigError> {
    let Some(config) = parse_sso_config(env)? else {
        return Ok(None);
    };
    let client = AuthClient::new(AuthClientOptions {
        issuer: config.issuer.clone(),
        back_channel_url: config.back_channel_url.clone(),
        client_id: config.client_id.clone(),
        client_secret: Some(config.client_secret.clone()),
        redirect_uri: config.redirect_uri.clone(),
        audience: config.audience.clone(),
        transport: overrides.transport,
        now: overrides.now,
    })
    .map_err(|error| SsoConfigError::new(vec![error.to_string()]))?;
    Ok(Some(Sso { config, client }))
}

/// Reads the single sign-on settings from the environment and builds the
/// provider client, or turns the feature off when `AUTH_ISSUER` is unset.
/// Contacts nothing: the client fetches the provider's discovery document on
/// its first use. An `Err` lists every problem with the settings.
pub fn initialize_sso() -> Result<Option<Arc<Sso>>, SsoConfigError> {
    initialize_sso_with(&SsoEnv::from_env(), ClientOverrides::default())
}

/// [`initialize_sso`] from the given settings. `overrides` is for tests,
/// which point the client at a fake provider.
pub fn initialize_sso_with(env: &SsoEnv, overrides: ClientOverrides) -> Result<Option<Arc<Sso>>, SsoConfigError> {
    let sso = match build_sso(env, overrides) {
        Ok(Some(sso)) => Arc::new(sso),
        Ok(None) => {
            set_current(None);
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    set_current(Some(Arc::clone(&sso)));
    logger::info(
        "Single sign-on is on",
        json!({
            "issuer": sso.config.issuer,
            "localLogin": sso.config.local_login,
            "linkedAccounts": sso.config.link_users.len(),
        }),
    );
    Ok(Some(sso))
}

/// The running single sign-on, or `None` when it is off.
pub fn get_sso() -> Option<Arc<Sso>> {
    CURRENT.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// The running single sign-on, for code that only runs while it is on.
pub fn require_sso() -> Result<Arc<Sso>> {
    get_sso().ok_or_else(|| anyhow!("Single sign-on is off"))
}

/// Turns single sign-on off again. For tests.
pub fn reset_sso() {
    set_current(None);
}

/// How the browser may sign in: what the sign-in page shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct SsoMode {
    pub enabled: bool,
    #[serde(rename = "localLogin")]
    pub local_login: bool,
}

/// The mode a given single sign-on (or its absence) means.
pub fn sso_mode_of(sso: Option<&Sso>) -> SsoMode {
    SsoMode { enabled: sso.is_some(), local_login: sso.is_none_or(|sso| sso.config.local_login) }
}

pub fn sso_mode() -> SsoMode {
    sso_mode_of(get_sso().as_deref())
}

/// Whether passwords stored in this app's own database may sign in.
pub fn is_local_login_allowed() -> bool {
    sso_mode().local_login
}

/// Serializes the tests that turn the process-wide single sign-on on and off.
#[cfg(test)]
pub(crate) static GLOBAL_SSO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth_client::{ProviderRequest, ProviderResponse};
    use futures_util::future::{BoxFuture, FutureExt};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn env() -> SsoEnv {
        SsoEnv {
            auth_issuer: Some("https://auth.example.test".into()),
            auth_client_id: Some("mail".into()),
            auth_client_secret: Some("test_key_not_real_do_not_use".into()),
            app_hostname: Some("mail.example.test".into()),
            ..Default::default()
        }
    }

    struct CountingTransport(AtomicUsize);

    impl Transport for CountingTransport {
        fn send(&self, _request: ProviderRequest) -> BoxFuture<'static, Result<ProviderResponse, String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            async { Ok(ProviderResponse { status: 500, body: None }) }.boxed()
        }
    }

    #[test]
    fn stays_off_without_auth_issuer_leaving_every_sign_in_as_it_was() {
        let _guard = GLOBAL_SSO_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(initialize_sso_with(&SsoEnv::default(), ClientOverrides::default()).unwrap().is_none());
        assert!(get_sso().is_none());
        assert_eq!(sso_mode(), SsoMode { enabled: false, local_login: true });
        assert!(is_local_login_allowed());
        assert_eq!(require_sso().unwrap_err().to_string(), "Single sign-on is off");
        reset_sso();
    }

    #[test]
    fn builds_the_provider_client_without_contacting_the_provider() {
        let _guard = GLOBAL_SSO_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let transport = Arc::new(CountingTransport(AtomicUsize::new(0)));
        let sso = initialize_sso_with(&env(), ClientOverrides { transport: Some(transport.clone()), now: None })
            .unwrap()
            .unwrap();
        assert_eq!(sso.config.client_id, "mail");
        assert_eq!(sso.config.redirect_uri, "https://mail.example.test/api/auth/callback");
        assert!(Arc::ptr_eq(&get_sso().unwrap(), &sso));
        assert!(Arc::ptr_eq(&require_sso().unwrap(), &sso));
        assert_eq!(transport.0.load(Ordering::SeqCst), 0);
        reset_sso();
        assert!(get_sso().is_none());
    }

    #[test]
    fn turns_password_sign_in_off_unless_auth_local_login_keeps_it() {
        let _guard = GLOBAL_SSO_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        initialize_sso_with(&env(), ClientOverrides::default()).unwrap();
        assert_eq!(sso_mode(), SsoMode { enabled: true, local_login: false });
        assert!(!is_local_login_allowed());
        initialize_sso_with(&SsoEnv { auth_local_login: Some("true".into()), ..env() }, ClientOverrides::default())
            .unwrap();
        assert_eq!(sso_mode(), SsoMode { enabled: true, local_login: true });
        assert!(is_local_login_allowed());
        // Turning it off again leaves password sign-in as the only way in.
        initialize_sso_with(&SsoEnv::default(), ClientOverrides::default()).unwrap();
        assert_eq!(sso_mode(), SsoMode { enabled: false, local_login: true });
        reset_sso();
    }

    #[test]
    fn refuses_settings_the_provider_client_refuses_as_a_configuration_problem() {
        let _guard = GLOBAL_SSO_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_sso();
        let attempt = || {
            initialize_sso_with(
                &SsoEnv { auth_issuer: Some("http://auth.example.test".into()), ..env() },
                ClientOverrides::default(),
            )
        };
        let error = attempt().unwrap_err();
        assert_eq!(error.problems, ["issuer must be an https URL (http only for localhost)"]);
        assert_eq!(
            error.to_string(),
            "Single sign-on is misconfigured:\n  - issuer must be an https URL (http only for localhost)"
        );
        assert!(get_sso().is_none());
    }

    #[test]
    fn a_refused_configuration_leaves_the_running_single_sign_on_as_it_was() {
        let _guard = GLOBAL_SSO_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let running = initialize_sso_with(&env(), ClientOverrides::default()).unwrap().unwrap();
        let refused = initialize_sso_with(&SsoEnv { auth_client_id: None, ..env() }, ClientOverrides::default());
        assert!(refused.is_err());
        assert!(Arc::ptr_eq(&get_sso().unwrap(), &running));
        reset_sso();
    }

    #[test]
    fn the_mode_serializes_as_the_sign_in_page_reads_it() {
        let mode = SsoMode { enabled: true, local_login: false };
        assert_eq!(serde_json::to_string(&mode).unwrap(), r#"{"enabled":true,"localLogin":false}"#);
        assert_eq!(sso_mode_of(None), SsoMode { enabled: false, local_login: true });
    }
}
