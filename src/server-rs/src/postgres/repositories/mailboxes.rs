//! The mailboxes table (repositories/mailboxes.ts, models/mailbox.ts): the
//! boxes a user created over IMAP, beside the views the server derives.
//!
//! ```ignore
//! match create_mailbox(&CreateMailboxInput { user_id, name: "Archive".into(), ..Default::default() }).await? {
//!     CreateMailboxResult::Created(mailbox) => {}
//!     CreateMailboxResult::Exists => {}
//!     CreateMailboxResult::AtLimit => {}
//! }
//! ```

use std::cmp::Ordering;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed};
use serde_json::{json, Value};

use super::mails::model::{validate_object, validate_record, FieldType, ModelValidationError};
use super::mails::table::{run_query, Filter, PreparedQuery, Record, SelectFilter, SetValue, Table};
use crate::params;
use crate::postgres::{Param, Row};

// Table and column names.
pub const MAILBOXES: &str = "mailboxes";
pub const MAILBOX_ID: &str = "mailbox_id";
pub const MAILBOX_USER_ID: &str = "user_id";
pub const MAILBOX_NAME: &str = "name";
/// The email address associated with the mailbox, if any.
pub const MAILBOX_ADDRESS: &str = "address";
pub const MAILBOX_PARENT_ID: &str = "parent_id";
pub const MAILBOX_UID_VALIDITY: &str = "uid_validity";
pub const MAILBOX_UID_NEXT: &str = "uid_next";
pub const MAILBOX_SUBSCRIBED: &str = "subscribed";
/// `\Inbox`, `\Sent`, `\Drafts`, `\Trash` and the like.
pub const MAILBOX_SPECIAL_USE: &str = "special_use";
pub const MAILBOX_CREATED: &str = "created";

/// `VARCHAR(255)` is 255 characters, so a name of astral characters occupies
/// four bytes each and the column alone leaves the byte budget four times
/// wider than it reads. Writers cap on this instead; LIST/LSUB walk every
/// stored name per response, so bytes are what the matcher pays.
pub const MAILBOX_NAME_MAX_BYTES: usize = 255;

/// Ceiling on user-created mailboxes per user. LIST/LSUB materialise every
/// stored row and emit one response line each, so the row count, not any
/// single name, is what bounds a LIST response.
pub const MAILBOX_COUNT_MAX: i64 = 1000;

/// The `mailboxes` table. It declares `created` and no `updated`.
pub fn mailboxes_table() -> Table {
    Table { name: MAILBOXES, primary_key: MAILBOX_ID }
}

/// The columns of `mailboxes` in declaration order.
pub fn mailbox_columns() -> Vec<&'static str> {
    mailboxes_table().columns()
}

const MAILBOX_TYPE_CHECKER: [(&str, FieldType); 10] = [
    ("mailbox_id", FieldType::String),
    ("user_id", FieldType::String),
    ("name", FieldType::String),
    ("address", FieldType::NullableString),
    ("parent_id", FieldType::NullableString),
    ("uid_validity", FieldType::Number),
    ("uid_next", FieldType::Number),
    ("subscribed", FieldType::Boolean),
    ("special_use", FieldType::NullableString),
    ("created", FieldType::NullableString),
];

/// A row of `mailboxes`.
#[derive(Debug, Clone, PartialEq)]
pub struct MailboxModel {
    pub mailbox_id: String,
    pub user_id: String,
    pub name: String,
    pub address: Option<String>,
    pub parent_id: Option<String>,
    pub uid_validity: i64,
    pub uid_next: i64,
    pub subscribed: bool,
    /// Set on system mailboxes, which cannot be renamed or deleted.
    pub special_use: Option<String>,
    pub created: Option<String>,
}

impl MailboxModel {
    pub fn from_row(row: &Row) -> Result<MailboxModel, ModelValidationError> {
        MailboxModel::from_record(row.as_map())
    }

    /// Any JSON value: only an object with every column of the right type
    /// becomes a model.
    pub fn from_value(data: &Value) -> Result<MailboxModel, ModelValidationError> {
        match data {
            Value::Object(record) => MailboxModel::from_record(record),
            // An array is an object with none of the named fields.
            Value::Array(_) => MailboxModel::from_record(&Record::new()),
            other => Err(ModelValidationError::new(
                "MailboxModel",
                validate_object(other, &MAILBOX_TYPE_CHECKER, &[]),
            )),
        }
    }

    pub fn from_record(record: &Record) -> Result<MailboxModel, ModelValidationError> {
        validate_record("MailboxModel", record, &MAILBOX_TYPE_CHECKER)?;
        let text = |column: &str| record.get(column).and_then(Value::as_str).map(str::to_string);
        let number = |column: &str| {
            record
                .get(column)
                .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|n| n as i64)))
                .unwrap_or(0)
        };
        Ok(MailboxModel {
            mailbox_id: text("mailbox_id").unwrap_or_default(),
            user_id: text("user_id").unwrap_or_default(),
            name: text("name").unwrap_or_default(),
            address: text("address"),
            parent_id: text("parent_id"),
            uid_validity: number("uid_validity"),
            uid_next: number("uid_next"),
            subscribed: record.get("subscribed").and_then(Value::as_bool).unwrap_or(false),
            special_use: text("special_use"),
            created: text("created"),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "mailbox_id": self.mailbox_id,
            "user_id": self.user_id,
            "name": self.name,
            "address": self.address,
            "parent_id": self.parent_id,
            "uid_validity": self.uid_validity,
            "uid_next": self.uid_next,
            "subscribed": self.subscribed,
            "special_use": self.special_use,
            "created": self.created,
        })
    }
}

/// `a.localeCompare(b)`: the Bun server's default locale is `en-US`, whose
/// collation is the CLDR root order at its default (tertiary) strength.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    static COLLATOR: OnceLock<CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            Collator::try_new(Default::default(), CollatorOptions::default())
                .expect("the root collation is compiled in")
        })
        .compare(a, b)
}

/// Every mailbox row of a user, system and user-created, sorted by name.
pub async fn get_mailboxes_by_user(user_id: &str) -> Result<Vec<MailboxModel>> {
    let rows = mailboxes_table().query(&[(MAILBOX_USER_ID, SelectFilter::from(user_id))]).await?;
    let mut mailboxes = rows.iter().map(MailboxModel::from_record).collect::<Result<Vec<_>, _>>()?;
    mailboxes.sort_by(|a, b| locale_compare(&a.name, &b.name));
    Ok(mailboxes)
}

/// The statement `get_mailbox_by_name` runs. Direct SQL for the `LOWER()`
/// comparison: the table wrapper only supports exact equality filters.
pub fn build_mailbox_by_name_query(user_id: &str, name: &str) -> PreparedQuery {
    PreparedQuery {
        sql: "\n    SELECT * FROM mailboxes\n    WHERE user_id = $1\n      AND LOWER(name) = LOWER($2)\n    LIMIT 1\n  "
            .to_string(),
        values: params![user_id, name],
    }
}

/// The user's mailbox with this name, ignoring case.
pub async fn get_mailbox_by_name(user_id: &str, name: &str) -> Result<Option<MailboxModel>> {
    let result = run_query(&build_mailbox_by_name_query(user_id, name)).await?;
    Ok(result.rows.first().map(MailboxModel::from_record).transpose()?)
}

/// The user's mailbox tied to this email address: a system mailbox of one of
/// their accounts.
pub async fn get_mailbox_by_address(user_id: &str, address: &str) -> Result<Option<MailboxModel>> {
    let row = mailboxes_table()
        .query_one(&[(MAILBOX_USER_ID, SelectFilter::from(user_id)), (MAILBOX_ADDRESS, SelectFilter::from(address))], None)
        .await?;
    Ok(row.as_ref().map(MailboxModel::from_record).transpose()?)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreateMailboxInput {
    pub user_id: String,
    pub name: String,
    pub address: Option<String>,
    pub parent_id: Option<String>,
    pub special_use: Option<String>,
    /// Subscribed unless this is `Some(false)`.
    pub subscribed: Option<bool>,
}

/// The outcome of a CREATE attempt. A refusal names its own reason so the
/// protocol layer can answer with the response code the client acts on:
/// `ALREADYEXISTS` is success to a client running ensure-folder-exists, while
/// `LIMIT` is not.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateMailboxResult {
    Created(MailboxModel),
    Exists,
    AtLimit,
}

/// Creates a user-defined mailbox, refusing once the user is at
/// [`MAILBOX_COUNT_MAX`] rows.
///
/// The ceiling is enforced here because this is the only write path to the
/// table, and after the name probe so a name that already exists still
/// reports as existing at the ceiling.
pub async fn create_mailbox(input: &CreateMailboxInput) -> Result<CreateMailboxResult> {
    if get_mailbox_by_name(&input.user_id, &input.name).await?.is_some() {
        return Ok(CreateMailboxResult::Exists);
    }

    let stored = mailboxes_table().count(&[(MAILBOX_USER_ID, SelectFilter::from(&input.user_id))]).await?;
    if stored >= MAILBOX_COUNT_MAX {
        return Ok(CreateMailboxResult::AtLimit);
    }

    let row = mailboxes_table()
        .insert(
            &[
                (MAILBOX_USER_ID, Param::from(&input.user_id)),
                (MAILBOX_NAME, Param::from(&input.name)),
                (MAILBOX_ADDRESS, Param::from(input.address.clone())),
                (MAILBOX_PARENT_ID, Param::from(input.parent_id.clone())),
                (MAILBOX_SPECIAL_USE, Param::from(input.special_use.clone())),
                (MAILBOX_SUBSCRIBED, Param::from(input.subscribed != Some(false))),
                (MAILBOX_UID_VALIDITY, Param::from(1)),
                (MAILBOX_UID_NEXT, Param::from(1)),
            ],
            &["*"],
        )
        .await?
        .ok_or_else(|| anyhow!("mailboxes INSERT returned no row"))?;
    Ok(CreateMailboxResult::Created(MailboxModel::from_record(&row)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMailboxResult {
    Deleted,
    NotFound,
    /// A system mailbox (one with `special_use` set) cannot be deleted.
    Protected,
}

/// Deletes a user-created mailbox by name.
pub async fn delete_mailbox_by_name(user_id: &str, name: &str) -> Result<DeleteMailboxResult> {
    let Some(mailbox) = get_mailbox_by_name(user_id, name).await? else {
        return Ok(DeleteMailboxResult::NotFound);
    };
    if mailbox.special_use.is_some() {
        return Ok(DeleteMailboxResult::Protected);
    }
    mailboxes_table().hard_delete(Param::from(&mailbox.mailbox_id)).await?;
    Ok(DeleteMailboxResult::Deleted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMailboxResult {
    Renamed,
    NotFound,
    /// A system mailbox cannot be renamed.
    Protected,
    NameTaken,
}

/// Renames a mailbox once the old name exists and the new one is free.
pub async fn rename_mailbox(user_id: &str, old_name: &str, new_name: &str) -> Result<RenameMailboxResult> {
    let Some(mailbox) = get_mailbox_by_name(user_id, old_name).await? else {
        return Ok(RenameMailboxResult::NotFound);
    };
    if mailbox.special_use.is_some() {
        return Ok(RenameMailboxResult::Protected);
    }
    if get_mailbox_by_name(user_id, new_name).await?.is_some() {
        return Ok(RenameMailboxResult::NameTaken);
    }

    // Bump uid_validity when renaming: RFC 3501 requires clients to re-sync
    // after a rename.
    mailboxes_table()
        .update(
            Param::from(&mailbox.mailbox_id),
            &[
                (MAILBOX_NAME, Param::from(new_name)),
                (MAILBOX_UID_VALIDITY, Param::from(mailbox.uid_validity + 1)),
            ],
            &[MAILBOX_ID],
        )
        .await?;
    Ok(RenameMailboxResult::Renamed)
}

/// Sets the subscribed flag of a mailbox. `false` when the user has no
/// mailbox with this name.
pub async fn set_mailbox_subscribed(user_id: &str, name: &str, subscribed: bool) -> Result<bool> {
    let Some(mailbox) = get_mailbox_by_name(user_id, name).await? else {
        return Ok(false);
    };
    let rows = mailboxes_table()
        .update_where(
            &[(MAILBOX_ID, Filter::from(&mailbox.mailbox_id))],
            &[(MAILBOX_SUBSCRIBED, SetValue::from(subscribed))],
            &[MAILBOX_ID],
        )
        .await?;
    Ok(!rows.is_empty())
}

#[cfg(test)]
mod tests {
    use super::super::mails::table::script::{affected, rows, with_script};
    use super::*;

    const USER: &str = "cccccccc-0000-0000-0000-000000000003";

    fn mailbox_data(overrides: Value) -> Value {
        let mut data = json!({
            "mailbox_id": "dddddddd-0000-0000-0000-000000000004",
            "user_id": USER,
            "name": "INBOX",
            "address": "alice@example.com",
            "parent_id": null,
            "uid_validity": 1,
            "uid_next": 42,
            "subscribed": true,
            "special_use": "\\Inbox",
            "created": "2024-01-01T00:00:00+00:00",
        });
        for (key, value) in overrides.as_object().expect("overrides is an object") {
            data[key] = value.clone();
        }
        data
    }

    fn mailbox(overrides: Value) -> Result<MailboxModel, ModelValidationError> {
        MailboxModel::from_value(&mailbox_data(overrides))
    }

    // models.test.ts — "MailboxModel construction"

    #[test]
    fn constructs_from_valid_data() {
        let model = mailbox(json!({})).unwrap();
        assert_eq!(model.mailbox_id, "dddddddd-0000-0000-0000-000000000004");
        assert_eq!(model.name, "INBOX");
        assert_eq!(model.uid_validity, 1);
        assert_eq!(model.uid_next, 42);
        assert!(model.subscribed);
        assert_eq!(model.special_use.as_deref(), Some("\\Inbox"));
    }

    #[test]
    fn accepts_null_for_optional_fields() {
        let model = mailbox(json!({ "address": null, "parent_id": null, "special_use": null })).unwrap();
        assert_eq!(model.address, None);
        assert_eq!(model.parent_id, None);
        assert_eq!(model.special_use, None);
    }

    #[test]
    fn refuses_a_uid_validity_that_is_a_string() {
        assert!(mailbox(json!({ "uid_validity": "1" })).is_err());
    }

    #[test]
    fn refuses_a_subscribed_that_is_a_string() {
        assert!(mailbox(json!({ "subscribed": "true" })).is_err());
    }

    // models.test.ts — "MailboxModel.toJSON"

    #[test]
    fn json_returns_all_expected_fields() {
        let model = mailbox(json!({})).unwrap();
        let json = model.to_json();
        assert_eq!(json["mailbox_id"], json!(model.mailbox_id));
        assert_eq!(json["uid_next"], json!(42));
        assert_eq!(json["special_use"], json!("\\Inbox"));
    }

    // The repository's own rules.

    fn create(name: &str) -> CreateMailboxInput {
        CreateMailboxInput { user_id: USER.into(), name: name.into(), ..Default::default() }
    }

    #[tokio::test]
    async fn a_name_that_exists_reports_as_existing_even_at_the_ceiling() {
        // The name probe runs first, so the count is never read.
        let (result, trace) =
            with_script(vec![rows(json!([mailbox_data(json!({}))]))], create_mailbox(&create("inbox"))).await;
        assert_eq!(result.unwrap(), CreateMailboxResult::Exists);
        assert_eq!(trace.len(), 1);
        assert!(trace[0].sql.contains("LOWER(name) = LOWER($2)"));
    }

    #[tokio::test]
    async fn the_ceiling_refuses_a_new_mailbox_before_any_insert() {
        let script = vec![rows(json!([])), rows(json!([{ "count": MAILBOX_COUNT_MAX }]))];
        let (result, trace) = with_script(script, create_mailbox(&create("One more"))).await;
        assert_eq!(result.unwrap(), CreateMailboxResult::AtLimit);
        assert_eq!(trace[1].sql, "SELECT COUNT(*)::int AS count FROM mailboxes WHERE user_id = $1");
        assert_eq!(trace.len(), 2);
    }

    #[tokio::test]
    async fn a_created_mailbox_starts_subscribed_at_uid_validity_one() {
        let script = vec![
            rows(json!([])),
            rows(json!([{ "count": MAILBOX_COUNT_MAX - 1 }])),
            rows(json!([mailbox_data(json!({ "name": "Archive", "special_use": null }))])),
        ];
        let (result, trace) = with_script(script, create_mailbox(&create("Archive"))).await;
        assert!(matches!(result.unwrap(), CreateMailboxResult::Created(mailbox) if mailbox.name == "Archive"));
        // `mailboxes` has no `updated` column to stamp.
        assert_eq!(
            trace[2].sql,
            "INSERT INTO mailboxes (user_id, name, address, parent_id, special_use, subscribed, uid_validity, uid_next) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING *"
        );
        assert_eq!(trace[2].values[5..], params![true, 1, 1]);
    }

    #[tokio::test]
    async fn a_rename_bumps_uid_validity_so_clients_resync() {
        let stored = mailbox_data(json!({ "name": "Archive", "special_use": null, "uid_validity": 6 }));
        let script = vec![rows(json!([stored])), rows(json!([])), rows(json!([{ "mailbox_id": "dddddddd-0000-0000-0000-000000000004" }]))];
        let (result, trace) = with_script(script, rename_mailbox(USER, "archive", "Old mail")).await;
        assert_eq!(result.unwrap(), RenameMailboxResult::Renamed);
        assert_eq!(trace[2].sql, "UPDATE mailboxes SET name = $1, uid_validity = $2 WHERE mailbox_id = $3 RETURNING mailbox_id");
        assert_eq!(trace[2].values, params!["Old mail", 7, "dddddddd-0000-0000-0000-000000000004"]);
    }

    #[tokio::test]
    async fn a_system_mailbox_is_neither_renamed_nor_deleted() {
        let system = || vec![rows(json!([mailbox_data(json!({}))]))];
        let (renamed, trace) = with_script(system(), rename_mailbox(USER, "INBOX", "Mine")).await;
        assert_eq!(renamed.unwrap(), RenameMailboxResult::Protected);
        assert_eq!(trace.len(), 1);
        let (deleted, trace) = with_script(system(), delete_mailbox_by_name(USER, "INBOX")).await;
        assert_eq!(deleted.unwrap(), DeleteMailboxResult::Protected);
        assert_eq!(trace.len(), 1);

        let user_created = mailbox_data(json!({ "name": "Archive", "special_use": null }));
        let (deleted, trace) =
            with_script(vec![rows(json!([user_created])), affected(1)], delete_mailbox_by_name(USER, "Archive")).await;
        assert_eq!(deleted.unwrap(), DeleteMailboxResult::Deleted);
        assert_eq!(trace[1].sql, "DELETE FROM mailboxes WHERE mailbox_id = $1 RETURNING mailbox_id");
    }

    #[test]
    fn names_sort_the_way_the_bun_server_lists_them() {
        let mut names = vec!["work", "Archive", "archive", "Zebra", "étoile", "10", "9", "_x", "Etoile"];
        names.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(names, vec!["_x", "10", "9", "archive", "Archive", "Etoile", "étoile", "work", "Zebra"]);
    }
}
