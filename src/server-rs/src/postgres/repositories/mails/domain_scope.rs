//! Which of an owner's mail rows belong to one of their domains
//! (repositories/mails/domain-scope.ts).
//!
//! A domain's mail is stored with its owner, beside the mail of the owner's
//! other domains, so reading one domain (a viewer of it, or a login scoped to
//! it) filters the owner's rows to that domain:
//!
//! - a received mail belongs to the domains it was delivered to: its envelope
//!   recipients. The `To`/`Cc`/`Bcc` headers stand in only for a row stored
//!   before envelopes were recorded.
//! - a sent mail belongs to the domain of its `From` address.

use serde_json::Value;

use crate::params;
use crate::postgres::{pool, DbResult, Param};

fn array_or_empty(column: &str) -> String {
    format!("CASE WHEN jsonb_typeof({column}) = 'array' THEN {column} ELSE '[]'::jsonb END")
}

fn address_domain_in(elements: &str, names: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM jsonb_array_elements({elements}) AS party
     WHERE lower(regexp_replace(party->>'address', '^.*@', '')) = ANY({names}))"
    )
}

/// The predicate as SQL, over the domain names `names` (a `text[]`
/// expression) and the `mails` columns under `prefix`.
pub fn mail_in_domains_expression(names: &str, prefix: &str) -> String {
    let p = prefix;
    let received = format!(
        "CASE WHEN {p}envelope_to IS NOT NULL THEN {}
    ELSE {} || {} || {} END",
        array_or_empty(&format!("{p}envelope_to")),
        array_or_empty(&format!("{p}to_address")),
        array_or_empty(&format!("{p}cc_address")),
        array_or_empty(&format!("{p}bcc_address")),
    );
    format!(
        "(CASE WHEN {p}sent THEN {}
    ELSE {} END)",
        address_domain_in(&array_or_empty(&format!("{p}from_address")), names),
        address_domain_in(&received, names),
    )
}

/// `domains` as an SQL `text[]` literal. Domain names are lower-case letters,
/// digits, dots and hyphens; anything else is refused rather than quoted.
pub fn domains_array_literal(domains: &[String]) -> String {
    for domain in domains {
        let valid = !domain.is_empty()
            && domain.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        assert!(valid, "Not a domain name: {domain:?}");
    }
    if domains.is_empty() {
        "ARRAY[]::text[]".to_string()
    } else {
        let quoted: Vec<String> = domains.iter().map(|domain| format!("'{domain}'")).collect();
        format!("ARRAY[{}]::text[]", quoted.join(", "))
    }
}

/// [`mail_in_domains_expression`] with the names bound at `$<param_index>`.
pub fn mail_in_domains_condition(param_index: usize, prefix: &str) -> String {
    mail_in_domains_expression(&format!("${param_index}::text[]"), prefix)
}

/// Appends the predicate to a query's `WHERE` when `domains` scopes it,
/// binding the names as the next parameter. Unscoped, the query is returned
/// unchanged.
pub fn scope_to_domains(sql: String, mut values: Vec<Param>, domains: Option<&[String]>, prefix: &str) -> (String, Vec<Param>) {
    let Some(domains) = domains else { return (sql, values) };
    let sql = format!("{sql} AND {}", mail_in_domains_condition(values.len() + 1, prefix));
    values.push(Param::array(domains));
    (sql, values)
}

/// Where a mail a person may read is stored, and which of its domains let them.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewableMail {
    pub owner_id: String,
    /// The domains the person views that the mail belongs to.
    pub domains: Vec<String>,
}

pub fn build_find_viewable_mail_query(viewer_id: &str, mail_id: &str) -> (String, Vec<Param>) {
    (
        format!(
            "
    SELECT m.user_id, array_agg(d.name ORDER BY d.name) AS domains
    FROM mails m
    JOIN domains d ON d.user_id = m.user_id
    JOIN domain_roles r ON r.domain_id = d.domain_id AND r.user_id = $1
    WHERE m.mail_id = $2
      AND m.expunged = FALSE
      AND {}
    GROUP BY m.user_id",
            mail_in_domains_expression("ARRAY[d.name]::text[]", "m.")
        ),
        params![viewer_id, mail_id],
    )
}

/// The owner a viewer reads `mail_id` from, or `None` when it belongs to no
/// domain they view.
pub async fn find_viewable_mail(viewer_id: &str, mail_id: &str) -> DbResult<Option<ViewableMail>> {
    let (sql, values) = build_find_viewable_mail_query(viewer_id, mail_id);
    let result = pool().query(&sql, &values).await?;
    Ok(result.rows.first().map(|row| ViewableMail {
        owner_id: row.text("user_id"),
        domains: match row.get("domains") {
            Value::Array(items) => items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect(),
            _ => Vec::new(),
        },
    }))
}

pub fn build_find_viewable_attachment_query(viewer_id: &str, attachment_id: &str) -> (String, Vec<Param>) {
    let attachments = serde_json::json!([{ "content": { "data": attachment_id } }]).to_string();
    (
        format!(
            "
    SELECT m.mail_id
    FROM mails m
    JOIN domains d ON d.user_id = m.user_id
    JOIN domain_roles r ON r.domain_id = d.domain_id AND r.user_id = $1
    WHERE m.attachments @> $2::jsonb
      AND m.expunged = FALSE
      AND {}
    LIMIT 1",
            mail_in_domains_expression("ARRAY[d.name]::text[]", "m.")
        ),
        params![viewer_id, attachments],
    )
}

/// Whether a viewer may download `attachment_id`: a mail of a domain they view carries it.
pub async fn can_view_attachment(viewer_id: &str, attachment_id: &str) -> DbResult<bool> {
    let (sql, values) = build_find_viewable_attachment_query(viewer_id, attachment_id);
    Ok(pool().query(&sql, &values).await?.any())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn squash(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn received_mail_belongs_to_its_envelope_and_headers_only_stand_in() {
        let sql = squash(&mail_in_domains_condition(2, "m."));
        assert!(sql.starts_with("(CASE WHEN m.sent THEN EXISTS"));
        assert!(sql.contains("CASE WHEN m.envelope_to IS NOT NULL THEN CASE WHEN jsonb_typeof(m.envelope_to) = 'array' THEN m.envelope_to ELSE '[]'::jsonb END ELSE"));
        assert!(sql.contains("= ANY($2::text[])"));
    }

    #[test]
    fn literals_refuse_what_is_no_domain_name() {
        assert_eq!(domains_array_literal(&[]), "ARRAY[]::text[]");
        assert_eq!(
            domains_array_literal(&["a.example".to_string(), "b-c.example".to_string()]),
            "ARRAY['a.example', 'b-c.example']::text[]"
        );
        assert!(std::panic::catch_unwind(|| domains_array_literal(&["a'b".to_string()])).is_err());
    }

    #[test]
    fn unscoped_queries_are_unchanged() {
        let (sql, values) = scope_to_domains("SELECT 1 WHERE x".into(), params!["a"], None, "");
        assert_eq!((sql.as_str(), values.len()), ("SELECT 1 WHERE x", 1));
        let domains = vec!["d.example".to_string()];
        let (sql, values) = scope_to_domains("SELECT 1 WHERE x".into(), params!["a"], Some(&domains), "");
        assert!(sql.contains("ANY($2::text[])"));
        assert_eq!(values[1], Param::Text("{\"d.example\"}".into()));
    }
}
