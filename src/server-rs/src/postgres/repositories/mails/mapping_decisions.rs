//! Which `mail_mailbox_uid` rows a save records for one `mails` row
//! (repositories/mails/mapping-decisions.ts).

/// Which UID space a mapping row's `uid` was drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingScope {
    /// The mapped destination's own counter.
    Mailbox,
    /// `mails.uid_domain`.
    Domain,
}

/// One `mail_mailbox_uid` row a save branch should write for a mail.
///
/// `scope` distinguishes the mapped destination's own counter from
/// `mails.uid_domain`, which the two rows draw from independently, and
/// decides how a duplicate has to be handled. See `core::record_mappings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingWrite {
    pub mailbox: String,
    pub uid: i64,
    pub scope: MappingScope,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MappingWriteInput {
    /// The mapped destination the write named; draws `uid_mailbox`.
    pub mailbox: Option<String>,
    pub uid_mailbox: Option<i64>,
    /// A domain view the mail belongs to; draws `uid_domain`.
    pub domain_mailbox: Option<String>,
    pub uid_domain: Option<i64>,
    /// The `sent` flag of the row being written, not of the incoming mail.
    pub sent: bool,
}

/// The mapping rows to record for one `mails` row, in order.
///
/// A box is recorded only when a UID was reserved for it: a write that named
/// no mapped destination reserves no `uid_mailbox`, and the mapping row
/// would address the box at UID 0.
///
/// `sent` gates the domain view because the two lanes draw `uid_domain` from
/// independent counters. On the merge branch the surviving row can be the
/// sent copy of the same Message-ID, and recording it under a received view
/// would both misfile the mail and put a sent lane's UID into the received
/// lane's number space.
pub fn decide_mapping_writes(input: &MappingWriteInput) -> Vec<MappingWrite> {
    let named = |mailbox: &Option<String>| mailbox.clone().filter(|name| !name.is_empty());
    let mut writes = Vec::new();
    if let (Some(mailbox), Some(uid)) = (named(&input.mailbox), input.uid_mailbox.filter(|uid| *uid > 0)) {
        writes.push(MappingWrite { mailbox, uid, scope: MappingScope::Mailbox });
    }
    if !input.sent {
        if let (Some(mailbox), Some(uid)) = (named(&input.domain_mailbox), input.uid_domain.filter(|uid| *uid > 0)) {
            writes.push(MappingWrite { mailbox, uid, scope: MappingScope::Domain });
        }
    }
    writes
}

#[cfg(test)]
mod tests {
    use super::*;

    const INBOX: &str = "INBOX";
    const ACCOUNT_BOX: &str = "INBOX/accounts/admin";

    fn write(mailbox: &str, uid: i64, scope: MappingScope) -> MappingWrite {
        MappingWrite { mailbox: mailbox.to_string(), uid, scope }
    }

    // mapping-decisions.test.ts — "decideMappingWrites"

    #[test]
    fn records_the_mapped_destination_and_the_domain_view_as_separate_rows() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            mailbox: Some(ACCOUNT_BOX.into()),
            uid_mailbox: Some(7),
            domain_mailbox: Some(INBOX.into()),
            uid_domain: Some(42),
            sent: false,
        });
        assert_eq!(
            writes,
            vec![write(ACCOUNT_BOX, 7, MappingScope::Mailbox), write(INBOX, 42, MappingScope::Domain)]
        );
    }

    #[test]
    fn records_the_domain_view_alone_for_a_write_that_named_no_mapped_destination() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            domain_mailbox: Some(INBOX.into()),
            uid_domain: Some(42),
            ..Default::default()
        });
        assert_eq!(writes, vec![write(INBOX, 42, MappingScope::Domain)]);
    }

    #[test]
    fn records_the_mapped_destination_alone_for_a_write_outside_the_inbox_tree() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            mailbox: Some("Archive".into()),
            uid_mailbox: Some(3),
            uid_domain: Some(42),
            ..Default::default()
        });
        assert_eq!(writes, vec![write("Archive", 3, MappingScope::Mailbox)]);
    }

    #[test]
    fn skips_a_box_whose_uid_was_never_reserved() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            mailbox: Some(ACCOUNT_BOX.into()),
            uid_mailbox: Some(0),
            domain_mailbox: Some(INBOX.into()),
            uid_domain: Some(0),
            sent: false,
        });
        assert_eq!(writes, Vec::new());
    }

    #[test]
    fn never_records_a_sent_row_under_a_domain_view() {
        let writes = decide_mapping_writes(&MappingWriteInput {
            mailbox: Some("Sent Messages/accounts/admin".into()),
            uid_mailbox: Some(9),
            domain_mailbox: Some(INBOX.into()),
            uid_domain: Some(42),
            sent: true,
        });
        assert_eq!(writes, vec![write("Sent Messages/accounts/admin", 9, MappingScope::Mailbox)]);
    }
}
