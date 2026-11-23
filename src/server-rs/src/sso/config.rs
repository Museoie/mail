//! Single sign-on settings (sso/config.ts).

use std::collections::{HashMap, HashSet};
use std::fmt;

use url::Url;

use crate::auth_client::js_trim;
use crate::env;

/// The environment single sign-on is configured from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SsoEnv {
    pub auth_issuer: Option<String>,
    pub auth_client_id: Option<String>,
    pub auth_client_secret: Option<String>,
    pub auth_redirect_uri: Option<String>,
    pub auth_backchannel_url: Option<String>,
    pub auth_audience: Option<String>,
    pub auth_local_login: Option<String>,
    pub auth_link_users: Option<String>,
    /// The web app's public hostname, which the default redirect URI is built on.
    pub app_hostname: Option<String>,
}

impl SsoEnv {
    /// The settings as the process environment has them now.
    pub fn from_env() -> SsoEnv {
        SsoEnv {
            auth_issuer: env::var("AUTH_ISSUER"),
            auth_client_id: env::var("AUTH_CLIENT_ID"),
            auth_client_secret: env::var("AUTH_CLIENT_SECRET"),
            auth_redirect_uri: env::var("AUTH_REDIRECT_URI"),
            auth_backchannel_url: env::var("AUTH_BACKCHANNEL_URL"),
            auth_audience: env::var("AUTH_AUDIENCE"),
            auth_local_login: env::var("AUTH_LOCAL_LOGIN"),
            auth_link_users: env::var("AUTH_LINK_USERS"),
            app_hostname: env::var("APP_HOSTNAME"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub back_channel_url: Option<String>,
    pub audience: Option<String>,
    /// Password sign-in, sign-up and reset, and local passwords on IMAP/SMTP, stay on.
    pub local_login: bool,
    /// Auth username → the local username its first sign-in links to.
    pub link_users: HashMap<String, String>,
}

/// The settings cannot be used. Lists every problem at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsoConfigError {
    pub problems: Vec<String>,
}

impl SsoConfigError {
    pub fn new(problems: Vec<String>) -> SsoConfigError {
        SsoConfigError { problems }
    }
}

impl fmt::Display for SsoConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Single sign-on is misconfigured:")?;
        for problem in &self.problems {
            write!(f, "\n  - {problem}")?;
        }
        Ok(())
    }
}

impl std::error::Error for SsoConfigError {}

fn is_http_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
}

/// The value trimmed, or `None` when it is unset or nothing but white space.
fn trimmed(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(js_trim).filter(|value| !value.is_empty())
}

fn parse_boolean(name: &str, value: Option<&str>, problems: &mut Vec<String>) -> bool {
    let normalized = js_trim(value.unwrap_or("")).to_lowercase();
    match normalized.as_str() {
        "true" | "1" => true,
        "" | "false" | "0" => false,
        _ => {
            problems.push(format!("{name} must be true or false, not \"{}\"", value.unwrap_or("")));
            false
        }
    }
}

/// `admin=alice,bob=bob` → auth username → local username. Each local account
/// and each auth username may appear once, and the read-only account is never
/// linked: it is a view onto admin's mail that only a local password opens.
pub fn parse_link_users(value: Option<&str>, problems: &mut Vec<String>) -> HashMap<String, String> {
    let mut links = HashMap::new();
    let mut locals = HashSet::new();
    for entry in value.unwrap_or("").split(',') {
        if js_trim(entry).is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split('=').collect();
        let local = js_trim(parts[0]);
        let remote = parts.get(1).map_or("", |part| js_trim(part));
        if parts.len() != 2 || local.is_empty() || remote.is_empty() {
            problems
                .push(format!("AUTH_LINK_USERS entry \"{}\" is not <local account>=<auth username>", js_trim(entry)));
            continue;
        }
        if locals.contains(local) {
            problems.push(format!("AUTH_LINK_USERS names the local account \"{local}\" twice"));
            continue;
        }
        if links.contains_key(remote) {
            problems.push(format!("AUTH_LINK_USERS names the auth username \"{remote}\" twice"));
            continue;
        }
        locals.insert(local.to_string());
        links.insert(remote.to_string(), local.to_string());
    }
    links
}

/// Single sign-on settings, or `None` when `AUTH_ISSUER` is unset or empty,
/// which turns the feature off. Otherwise an `Err` names every problem at
/// once. Reads nothing over the network.
pub fn parse_sso_config(env: &SsoEnv) -> Result<Option<SsoConfig>, SsoConfigError> {
    let Some(issuer) = trimmed(&env.auth_issuer) else {
        return Ok(None);
    };

    let mut problems = Vec::new();
    if !is_http_url(issuer) {
        problems.push("AUTH_ISSUER must be an absolute https URL".to_string());
    }

    let client_id = trimmed(&env.auth_client_id).unwrap_or("");
    if client_id.is_empty() {
        problems.push("AUTH_CLIENT_ID is required when AUTH_ISSUER is set".to_string());
    }
    let client_secret = env.auth_client_secret.as_deref().unwrap_or("");
    if client_secret.is_empty() {
        problems.push("AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set".to_string());
    }

    let explicit_redirect_uri = trimmed(&env.auth_redirect_uri);
    let redirect_uri = match (explicit_redirect_uri, trimmed(&env.app_hostname)) {
        (Some(explicit), _) => explicit.to_string(),
        (None, Some(hostname)) => format!("https://{hostname}/api/auth/callback"),
        (None, None) => String::new(),
    };
    if redirect_uri.is_empty() {
        problems.push("AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set".to_string());
    } else if !is_http_url(&redirect_uri) {
        problems.push(
            if explicit_redirect_uri.is_some() {
                "AUTH_REDIRECT_URI must be an absolute http(s) URL"
            } else {
                "APP_HOSTNAME must be a hostname to build the default AUTH_REDIRECT_URI on"
            }
            .to_string(),
        );
    }

    let back_channel_url = trimmed(&env.auth_backchannel_url);
    if back_channel_url.is_some_and(|url| !is_http_url(url)) {
        problems.push("AUTH_BACKCHANNEL_URL must be an absolute http(s) URL".to_string());
    }
    let audience = trimmed(&env.auth_audience);

    let local_login = parse_boolean("AUTH_LOCAL_LOGIN", env.auth_local_login.as_deref(), &mut problems);
    let link_users = parse_link_users(env.auth_link_users.as_deref(), &mut problems);

    if !problems.is_empty() {
        return Err(SsoConfigError::new(problems));
    }
    Ok(Some(SsoConfig {
        issuer: issuer.to_string(),
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        redirect_uri,
        back_channel_url: back_channel_url.map(str::to_string),
        audience: audience.map(str::to_string),
        local_login,
        link_users,
    }))
}

/// Where the provider sends the browser after it ends the single sign-on
/// session: the root of the redirect URI's origin.
pub fn post_logout_redirect(redirect_uri: &str) -> String {
    let origin = Url::parse(redirect_uri).map(|url| url.origin().ascii_serialization()).unwrap_or_default();
    format!("{origin}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> SsoEnv {
        SsoEnv {
            auth_issuer: Some("https://auth.example.com".into()),
            auth_client_id: Some("mail".into()),
            auth_client_secret: Some("test_key_not_real_do_not_use".into()),
            app_hostname: Some("mail.example.com".into()),
            ..Default::default()
        }
    }

    fn some(value: &str) -> Option<String> {
        Some(value.to_string())
    }

    fn problems_of(env: SsoEnv) -> Vec<String> {
        match parse_sso_config(&env) {
            Err(error) => error.problems,
            Ok(_) => panic!("expected the settings to be refused"),
        }
    }

    fn links(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(remote, local)| (remote.to_string(), local.to_string())).collect()
    }

    #[test]
    fn is_off_with_auth_issuer_unset_or_empty_whatever_else_is_set() {
        assert_eq!(parse_sso_config(&SsoEnv::default()), Ok(None));
        assert_eq!(parse_sso_config(&SsoEnv { auth_issuer: some(""), ..base() }), Ok(None));
        assert_eq!(parse_sso_config(&SsoEnv { auth_issuer: some("  "), ..base() }), Ok(None));
        let malformed = SsoEnv { auth_issuer: some(""), auth_link_users: some("not a pair"), ..Default::default() };
        assert_eq!(parse_sso_config(&malformed), Ok(None));
    }

    #[test]
    fn reads_every_setting_defaulting_the_redirect_uri_from_app_hostname() {
        let config = parse_sso_config(&SsoEnv {
            auth_backchannel_url: some("http://auth.internal:3000"),
            auth_audience: some("mailbox"),
            auth_local_login: some("true"),
            auth_link_users: some("admin=alice, bob=bob"),
            ..base()
        });
        assert_eq!(
            config,
            Ok(Some(SsoConfig {
                issuer: "https://auth.example.com".into(),
                client_id: "mail".into(),
                client_secret: "test_key_not_real_do_not_use".into(),
                redirect_uri: "https://mail.example.com/api/auth/callback".into(),
                back_channel_url: some("http://auth.internal:3000"),
                audience: some("mailbox"),
                local_login: true,
                link_users: links(&[("alice", "admin"), ("bob", "bob")]),
            }))
        );
    }

    #[test]
    fn trims_what_it_reads_except_the_client_secret() {
        let config = parse_sso_config(&SsoEnv {
            auth_issuer: some("  https://auth.example.com "),
            auth_client_id: some(" mail\n"),
            auth_client_secret: some(" spaced secret "),
            auth_redirect_uri: some("   "),
            auth_backchannel_url: some("  "),
            auth_audience: some(" \t"),
            app_hostname: some(" mail.example.com "),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
        assert_eq!(config.issuer, "https://auth.example.com");
        assert_eq!(config.client_id, "mail");
        assert_eq!(config.client_secret, " spaced secret ");
        assert_eq!(config.redirect_uri, "https://mail.example.com/api/auth/callback");
        assert_eq!(config.back_channel_url, None);
        assert_eq!(config.audience, None);
    }

    #[test]
    fn keeps_an_explicit_auth_redirect_uri_over_the_app_hostname_default() {
        let config = parse_sso_config(&SsoEnv {
            auth_redirect_uri: some("https://webmail.example.com/api/auth/callback"),
            ..base()
        })
        .unwrap()
        .unwrap();
        assert_eq!(config.redirect_uri, "https://webmail.example.com/api/auth/callback");

        let without_hostname = parse_sso_config(&SsoEnv {
            auth_redirect_uri: some("http://localhost:3004/api/auth/callback"),
            app_hostname: None,
            ..base()
        })
        .unwrap()
        .unwrap();
        assert_eq!(without_hostname.redirect_uri, "http://localhost:3004/api/auth/callback");
    }

    #[test]
    fn leaves_password_sign_in_off_unless_auth_local_login_turns_it_on() {
        assert!(!parse_sso_config(&base()).unwrap().unwrap().local_login);
        for value in ["true", "TRUE", "1", " true "] {
            let config = parse_sso_config(&SsoEnv { auth_local_login: some(value), ..base() });
            assert!(config.unwrap().unwrap().local_login, "{value}");
        }
        for value in ["", "false", "0", "FALSE", "  "] {
            let config = parse_sso_config(&SsoEnv { auth_local_login: some(value), ..base() });
            assert!(!config.unwrap().unwrap().local_login, "{value}");
        }
    }

    #[test]
    fn refuses_to_start_listing_every_problem_at_once() {
        assert_eq!(
            problems_of(SsoEnv {
                auth_issuer: some("auth.example.com"),
                auth_backchannel_url: some("ftp://auth.internal"),
                auth_local_login: some("yes"),
                auth_link_users: some("admin"),
                ..Default::default()
            }),
            [
                "AUTH_ISSUER must be an absolute https URL",
                "AUTH_CLIENT_ID is required when AUTH_ISSUER is set",
                "AUTH_CLIENT_SECRET is required when AUTH_ISSUER is set",
                "AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set",
                "AUTH_BACKCHANNEL_URL must be an absolute http(s) URL",
                "AUTH_LOCAL_LOGIN must be true or false, not \"yes\"",
                "AUTH_LINK_USERS entry \"admin\" is not <local account>=<auth username>",
            ]
        );
    }

    #[test]
    fn refuses_a_malformed_redirect_uri_and_a_hostname_that_makes_one() {
        assert_eq!(
            problems_of(SsoEnv { auth_redirect_uri: some("/api/auth/callback"), ..base() }),
            ["AUTH_REDIRECT_URI must be an absolute http(s) URL"]
        );
        assert_eq!(
            problems_of(SsoEnv { auth_redirect_uri: some("ftp://mail.example.com/callback"), ..base() }),
            ["AUTH_REDIRECT_URI must be an absolute http(s) URL"]
        );
        assert_eq!(
            problems_of(SsoEnv { app_hostname: some("mail example com"), ..base() }),
            ["APP_HOSTNAME must be a hostname to build the default AUTH_REDIRECT_URI on"]
        );
        assert_eq!(
            problems_of(SsoEnv { app_hostname: some("  "), ..base() }),
            ["AUTH_REDIRECT_URI is required when APP_HOSTNAME is not set"]
        );
    }

    #[test]
    fn names_the_value_of_a_boolean_it_cannot_read_as_it_was_given() {
        assert_eq!(
            problems_of(SsoEnv { auth_local_login: some(" On "), ..base() }),
            ["AUTH_LOCAL_LOGIN must be true or false, not \" On \""]
        );
    }

    #[test]
    fn names_its_problems_in_the_error_message() {
        let error = parse_sso_config(&SsoEnv { auth_issuer: some("https://auth.example.com"), ..Default::default() })
            .unwrap_err();
        assert!(error.to_string().starts_with("Single sign-on is misconfigured:\n  - AUTH_CLIENT_ID is required"));
        assert_eq!(
            SsoConfigError::new(vec!["one".into(), "two".into()]).to_string(),
            "Single sign-on is misconfigured:\n  - one\n  - two"
        );
    }

    fn parse(value: Option<&str>) -> (HashMap<String, String>, Vec<String>) {
        let mut problems = Vec::new();
        let links = parse_link_users(value, &mut problems);
        (links, problems)
    }

    #[test]
    fn link_users_maps_each_auth_username_to_its_local_account_skipping_empty_entries() {
        assert_eq!(parse(Some("admin=alice,,bob = bob ,")).0, links(&[("alice", "admin"), ("bob", "bob")]));
        assert_eq!(parse(None), (HashMap::new(), Vec::new()));
        assert_eq!(parse(Some(" , ,")), (HashMap::new(), Vec::new()));
    }

    #[test]
    fn link_users_refuses_entries_that_are_not_one_pair() {
        assert_eq!(
            parse(Some("admin=alice=x,=bob,carol=")).1,
            [
                "AUTH_LINK_USERS entry \"admin=alice=x\" is not <local account>=<auth username>",
                "AUTH_LINK_USERS entry \"=bob\" is not <local account>=<auth username>",
                "AUTH_LINK_USERS entry \"carol=\" is not <local account>=<auth username>",
            ]
        );
    }

    #[test]
    fn link_users_refuses_a_name_used_twice_on_either_side() {
        let (linked, problems) = parse(Some("admin=bob,admin=carol,dave=bob"));
        assert_eq!(
            problems,
            [
                "AUTH_LINK_USERS names the local account \"admin\" twice",
                "AUTH_LINK_USERS names the auth username \"bob\" twice",
            ]
        );
        assert_eq!(linked, links(&[("bob", "admin")]));
    }

    #[test]
    fn post_logout_redirect_is_the_root_of_the_redirect_uris_origin() {
        assert_eq!(
            post_logout_redirect("https://mail.example.com:8443/api/auth/callback"),
            "https://mail.example.com:8443/"
        );
        assert_eq!(
            post_logout_redirect("https://mail.example.com:443/api/auth/callback?x=1"),
            "https://mail.example.com/"
        );
        assert_eq!(post_logout_redirect("http://localhost:3004/callback"), "http://localhost:3004/");
    }
}
