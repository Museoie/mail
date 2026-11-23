//! The conversation a mail belongs to (mails/thread.ts).

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::headers::to_mail_header_data;
use crate::domains::AddressOnDomains;
use super::threading::{collect_thread, has_thread_headers, linked_ids, normalize_subject, stored_id_forms, ThreadCandidate};
use crate::js;
use crate::models::SignedUser;
use crate::postgres::repositories::mails::thread::{
    get_thread_candidates, get_thread_seed, get_thread_subject_candidates, ThreadRow,
};
use crate::postgres::DbResult;

/// Rounds of widening the id set; each round follows one more link.
const MAX_ROUNDS: usize = 4;

/// The user's own addresses: those on the domains they own (and used to).
/// Another user's address on this server is a correspondent like any other.
pub fn own_addresses_of<S: AsRef<str>>(domains: &[S]) -> AddressOnDomains {
    AddressOnDomains::new(domains)
}

/// The candidate rows by id, in the order each id was first seen; a later
/// copy of a row replaces the earlier one in place, as a `Map` does.
#[derive(Default)]
struct Candidates {
    rows: Vec<ThreadRow>,
    index: HashMap<String, usize>,
}

impl Candidates {
    fn add(&mut self, rows: Vec<ThreadRow>) {
        for row in rows {
            match self.index.get(row.mail_id()) {
                Some(&at) => self.rows[at] = row,
                None => {
                    self.index.insert(row.mail_id().to_string(), self.rows.len());
                    self.rows.push(row);
                }
            }
        }
    }
}

fn collected_ids(seed: &ThreadRow, candidates: &Candidates, is_own: &impl Fn(&str) -> bool) -> Vec<String> {
    collect_thread(seed, &candidates.rows, is_own).iter().map(|row| row.mail_id().to_string()).collect()
}

/// Every message in the conversation `mail_id` belongs to, oldest first, the
/// opened mail included, as list rows (spam rows carry `spam: true`). Empty
/// when the mail isn't the user's.
///
/// Header links are followed round by round. The subject lookup runs once,
/// and only when the conversation found so far holds header-less mail.
pub async fn get_thread(
    user: &SignedUser,
    mail_id: &str,
    own: &AddressOnDomains,
) -> DbResult<Vec<Map<String, Value>>> {
    let user_id = user.id.as_str();
    let Some(seed) = get_thread_seed(user_id, mail_id).await? else {
        return Ok(Vec::new());
    };
    let is_own = |address: &str| own.matches(address);
    let include_spam = seed.is_spam();
    let subject = normalize_subject(seed.subject());
    let mut candidates = Candidates::default();
    candidates.add(vec![seed.clone()]);

    let mut ids: Vec<String> = unique(linked_ids(&seed));
    let mut thread: Vec<String> = vec![seed.mail_id().to_string()];
    let mut searched_subject = false;

    for _ in 0..MAX_ROUNDS {
        candidates.add(get_thread_candidates(user_id, &stored_id_forms(&ids), include_spam).await?);
        thread = collected_ids(&seed, &candidates, &is_own);

        let wants_subject = !searched_subject
            && js::utf16_len(&subject) >= 3
            && thread.iter().any(|id| !has_thread_headers(&candidates.rows[candidates.index[id]]));
        if wants_subject {
            searched_subject = true;
            candidates.add(get_thread_subject_candidates(user_id, seed.mail_id(), include_spam).await?);
            thread = collected_ids(&seed, &candidates, &is_own);
        }

        let next_ids = unique(thread.iter().flat_map(|id| linked_ids(&candidates.rows[candidates.index[id]])).collect());
        if next_ids.len() == ids.len() && !wants_subject {
            break;
        }
        ids = next_ids;
    }

    let mut rows: Vec<&ThreadRow> = thread.iter().map(|id| &candidates.rows[candidates.index[id]]).collect();
    // A stable sort, so the seed (collected first) leads its ties.
    rows.sort_by_key(|row| js::date_parse(row.date()).unwrap_or(i64::MIN));
    Ok(rows
        .into_iter()
        .map(|row| {
            let mut header = to_mail_header_data(&row.record, own);
            if row.is_spam() {
                header.insert("spam".into(), Value::Bool(true));
            }
            header
        })
        .collect())
}

/// The ids in first-seen order, each once: a JavaScript `Set`.
fn unique(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.into_iter().filter(|id| seen.insert(id.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_addresses_are_those_on_the_users_domains() {
        let domain = crate::util::get_domain();
        let is_own = own_addresses_of(&[format!("bob.{domain}"), "example.org".to_string()]);
        assert!(is_own.matches(&format!("Me@BOB.{}", domain.to_uppercase())));
        assert!(is_own.matches("info@example.org"));
        assert!(!is_own.matches(&format!("me@{domain}")));
        assert!(!is_own.matches(&format!("me@alice.{domain}")));
    }

    #[test]
    fn unique_keeps_the_first_occurrence() {
        let ids = vec!["b".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(unique(ids), vec!["b", "a"]);
    }
}
