//! `spike`: prove the GraphQL schedule and Stack operations against the
//! owner's account, saving every raw answer, and leave the account as it was.
//!
//! Steps, each reported PASS, FAIL or SKIP:
//! 1. `CheckDoesClassExistInUserStack` with a locally encoded class id. A
//!    rejected token stops the spike before anything else is sent.
//! 2. The three library reads (REST), saved.
//! 3. Schedule: list, add at the first whole hour at least 13 days ahead,
//!    prove the new item is listed, read it, move it one hour, remove it, and
//!    check the list is as before. Later calls always send back the raw ids
//!    Peloton returned, whether or not they decode as expected.
//! 4. Stack: read and print it (refusing a partial list), add the class, write
//!    the saved Stack back with `ModifyStack` using Peloton's own `joinToken`s,
//!    and check the read-back order is identical.

use std::collections::BTreeSet;
use std::path::Path;

use peloton::endpoints;
use peloton::graphql::Op;
use peloton::graphql::ops::{
    ADD_CLASS_TO_SCHEDULE as ADD, ADD_CLASS_TO_STACK as STACK_ADD, CHECK_CLASS_IN_STACK as CHECK,
    MODIFY_STACK as MODIFY, REMOVE_CLASS_FROM_SCHEDULE as REMOVE, RESCHEDULE_CLASS as MOVE,
    SCHEDULED_CLASS as READ, VIEW_USER_STACK as VIEW, YOUR_SCHEDULE_ITEMS as LIST,
};
use peloton::ids::{OnDemandClassId, PelotonId, ScheduledClassId};
use secrecy::SecretString;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::capture::{StoreError, find_worktree, guard_dir, prepare_dir, resolve_dir, save};
use crate::report;
use crate::session::{GraphqlReply, Session, SessionError};

/// How far ahead the spike schedules, so it cannot collide with real plans.
const SCHEDULE_LEAD: Duration = Duration::days(13);
/// The schedule window the spike lists (Peloton's own horizon).
const SCHEDULE_WINDOW: Duration = Duration::days(14);
/// Longest GraphQL error message echoed to the terminal.
const MAX_MESSAGE: usize = 160;

/// Why `spike` failed as a whole. Per-step failures are reported as they
/// happen and only counted here.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpikeError {
    /// The capture directory was refused or a file could not be saved.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Login failed.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The schedule dates could not be computed.
    #[error(transparent)]
    Date(#[from] DateError),
    /// The very first call showed the token is refused.
    #[error("GraphQL gateway rejected the token; stopping before any other call")]
    GatewayRejected,
    /// At least one step failed; each was reported when it happened.
    #[error("some spike steps failed; see above")]
    StepsFailed,
}

/// Date arithmetic or formatting failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DateError {
    /// The date is out of the supported range.
    #[error("date overflow")]
    Overflow,
    /// A time component was out of range.
    #[error("date")]
    Component(#[from] time::error::ComponentRange),
    /// The date could not be formatted.
    #[error("date format")]
    Format(#[from] time::error::Format),
}

/// Why a GraphQL call did not yield data. Summarised without the body.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CallFailure {
    /// The gateway refused the token (HTTP 401/403 or an auth error).
    #[error("the gateway rejected the token")]
    Auth,
    /// The answer was not JSON.
    #[error("status {status}, body is not JSON")]
    NotJson {
        /// The HTTP status.
        status: reqwest::StatusCode,
    },
    /// The answer carried GraphQL errors.
    #[error("status {status}, GraphQL errors: {summary}")]
    Graphql {
        /// The HTTP status.
        status: reqwest::StatusCode,
        /// Error codes and truncated messages, joined with " | ".
        summary: String,
    },
    /// The answer was a failure status without GraphQL errors.
    #[error("status {status}")]
    Status {
        /// The HTTP status.
        status: reqwest::StatusCode,
    },
    /// The answer has no (or a null) `data.<field>`.
    #[error("no data.{field}")]
    NoData {
        /// The expected root field.
        field: String,
    },
}

/// Peloton answered, but not in a shape (or with a content) the spike accepts.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AnswerError {
    /// The schedule list answered with an error variant.
    #[error("schedule list answered {typename}: {message}")]
    ScheduleListVariant {
        /// The union variant, or "no __typename".
        typename: String,
        /// Its message.
        message: String,
    },
    /// `yourScheduleItems` is missing or not a list.
    #[error("yourScheduleItems is not a list")]
    ScheduleItemsNotList,
    /// A `ScheduledClass` carried no id.
    #[error("ScheduledClass without an id")]
    ScheduledClassWithoutId,
    /// A schedule mutation answered with an error variant.
    #[error("{typename}: {code}: {message}")]
    ScheduleRejected {
        /// The union variant, or "unknown result".
        typename: String,
        /// Peloton's error code.
        code: String,
        /// Its (truncated) message.
        message: String,
    },
    /// The scheduled-class id does not decode.
    #[error("Peloton's scheduled-class id does not decode as an event id")]
    EventUndecodable,
    /// The scheduled-class id names another ride.
    #[error("the scheduled class names a different ride")]
    EventWrongRide,
    /// The scheduled-class id names another start time.
    #[error("the scheduled class has a different start time")]
    EventWrongStart,
    /// The scheduled class read back has another id.
    #[error("the scheduled class read back has a different id")]
    ReadBackIdDiffers,
    /// The Stack response has no `numClasses`.
    #[error("Stack response without numClasses")]
    StackWithoutCount,
    /// The Stack response has no list although `numClasses` is not zero.
    #[error("Stack response without userStack.stackedClassList")]
    StackWithoutList,
    /// `stackedClassList` is not a list.
    #[error("stackedClassList is not a list")]
    StackListNotList,
    /// `numClasses` disagrees with the list.
    #[error("numClasses is {count} but the list holds {entries} entries")]
    StackCountMismatch {
        /// The reported count.
        count: i64,
        /// The entries actually listed.
        entries: usize,
    },
    /// A Stack entry has no `playOrder`.
    #[error("Stack entry {index} has no playOrder")]
    StackEntryWithoutOrder {
        /// The zero-based entry index.
        index: usize,
    },
    /// A Stack entry has no `pelotonClass`.
    #[error("Stack entry {index} has no pelotonClass")]
    StackEntryWithoutClass {
        /// The zero-based entry index.
        index: usize,
    },
    /// A Stack entry has no `classId`.
    #[error("Stack entry {index} has no classId")]
    StackEntryWithoutClassId {
        /// The zero-based entry index.
        index: usize,
    },
    /// A Stack entry's `classId` is neither a ride id nor a class id.
    #[error("Stack entry {index}: classId is neither a ride id nor a class id")]
    StackEntryBadClassId {
        /// The zero-based entry index.
        index: usize,
    },
    /// A saved Stack entry has no `joinToken`.
    #[error("Stack entry {entry} ({ride}): Peloton returned no joinToken")]
    RestoreWithoutToken {
        /// The one-based entry number.
        entry: usize,
        /// The entry's ride.
        ride: PelotonId,
    },
    /// A saved Stack entry's `joinToken` is not for its ride.
    #[error(
        "Stack entry {entry} ({ride}): Peloton's joinToken is not an on-demand class id for this ride"
    )]
    RestoreForeignToken {
        /// The one-based entry number.
        entry: usize,
        /// The entry's ride.
        ride: PelotonId,
    },
    /// The Stack read back differs from the saved one.
    #[error("the Stack differs from the saved one")]
    StackDiffers,
    /// The Stack after the add is not the saved Stack plus the class.
    #[error("the Stack after the add is not the saved Stack plus the class")]
    StackAddMismatch,
}

/// Why one spike step failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StepError {
    /// The gateway did not yield data.
    #[error(transparent)]
    Call(#[from] CallFailure),
    /// A request could not be made.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// The raw answer could not be saved.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Peloton's answer was not acceptable.
    #[error(transparent)]
    Answer(#[from] AnswerError),
}

/// Extracts `data.<field>` from a gateway reply, or says why it is missing.
///
/// # Errors
///
/// [`CallFailure::Auth`] when the token was refused, otherwise another
/// [`CallFailure`] with the status and the GraphQL error codes and (truncated)
/// messages.
pub fn root_field(reply: &GraphqlReply, field: &str) -> Result<Value, CallFailure> {
    let status = reply.status;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(CallFailure::Auth);
    }
    let Ok(body) = serde_json::from_slice::<Value>(&reply.body) else {
        return Err(CallFailure::NotJson { status });
    };
    let errors = body
        .get("errors")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if errors.iter().any(is_auth_error) {
        return Err(CallFailure::Auth);
    }
    if !errors.is_empty() {
        let summary: Vec<String> = errors.iter().map(describe_error).collect();
        return Err(CallFailure::Graphql {
            status,
            summary: summary.join(" | "),
        });
    }
    if !status.is_success() {
        return Err(CallFailure::Status { status });
    }
    match body.pointer(&format!("/data/{field}")) {
        Some(Value::Null) | None => Err(CallFailure::NoData {
            field: field.to_owned(),
        }),
        Some(value) => Ok(value.clone()),
    }
}

fn error_code(error: &Value) -> Option<&str> {
    error.pointer("/extensions/code").and_then(Value::as_str)
}

fn is_auth_error(error: &Value) -> bool {
    const CODES: [&str; 3] = ["UNAUTHENTICATED", "UNAUTHORIZED", "FORBIDDEN"];
    const PHRASES: [&str; 6] = [
        "unauthenticated",
        "unauthorized",
        "not authenticated",
        "not authorized",
        "forbidden",
        "invalid token",
    ];
    let code_hit =
        error_code(error).is_some_and(|c| CODES.iter().any(|k| c.eq_ignore_ascii_case(k)));
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    code_hit || PHRASES.iter().any(|p| message.contains(p))
}

fn describe_error(error: &Value) -> String {
    let message: String = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)")
        .chars()
        .take(MAX_MESSAGE)
        .collect();
    match error_code(error) {
        Some(code) => format!("{code}: {message}"),
        None => message,
    }
}

/// The first whole UTC hour at least 13 days after `now`.
///
/// # Errors
///
/// Returns [`DateError`] when the date arithmetic overflows.
pub fn spike_start(now: OffsetDateTime) -> Result<OffsetDateTime, DateError> {
    let earliest = now
        .to_offset(UtcOffset::UTC)
        .checked_add(SCHEDULE_LEAD)
        .ok_or(DateError::Overflow)?;
    let hour = earliest
        .replace_minute(0)
        .and_then(|t| t.replace_second(0))
        .and_then(|t| t.replace_nanosecond(0))?;
    if hour == earliest {
        Ok(hour)
    } else {
        hour.checked_add(Duration::HOUR).ok_or(DateError::Overflow)
    }
}

fn iso(t: OffsetDateTime) -> Result<String, DateError> {
    Ok(t.to_offset(UtcOffset::UTC).format(&Rfc3339)?)
}

/// Every scheduled-class id found anywhere under `list`, whatever its shape.
#[must_use]
pub fn schedule_ids(list: &Value) -> BTreeSet<String> {
    fn walk(value: &Value, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    match (key.as_str(), child) {
                        ("id", Value::String(id)) => {
                            out.insert(id.clone());
                        }
                        _ => walk(child, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = BTreeSet::new();
    walk(list, &mut out);
    out
}

/// The scheduled-class ids in a `userScheduledItemsList` union result.
///
/// # Errors
///
/// Returns the variant's type and message when Peloton answered with an
/// error variant instead of a list, so it never reads as an empty schedule.
pub fn listed_ids(list: &Value) -> Result<BTreeSet<String>, AnswerError> {
    let typename = list.get("__typename").and_then(Value::as_str);
    if typename != Some("YourScheduleItemList") {
        let message = list.get("message").and_then(Value::as_str).unwrap_or("");
        return Err(AnswerError::ScheduleListVariant {
            typename: typename.unwrap_or("no __typename").to_owned(),
            message: message.to_owned(),
        });
    }
    let items = list
        .get("yourScheduleItems")
        .filter(|items| items.is_array())
        .ok_or(AnswerError::ScheduleItemsNotList)?;
    Ok(schedule_ids(items))
}

/// Reads a schedule mutation's union result into the raw scheduled-class id
/// Peloton returned. The id is not decoded here: whatever Peloton issued is
/// what later calls must send back.
///
/// # Errors
///
/// Returns Peloton's error code and message, or a shape problem.
pub fn scheduled_result(value: &Value) -> Result<String, AnswerError> {
    let typename = value.get("__typename").and_then(Value::as_str);
    if typename == Some("ScheduledClass") {
        return value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(AnswerError::ScheduledClassWithoutId);
    }
    let code = value.get("code").and_then(Value::as_str).unwrap_or("?");
    let message: String = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(MAX_MESSAGE)
        .collect();
    Err(AnswerError::ScheduleRejected {
        typename: typename.unwrap_or("unknown result").to_owned(),
        code: code.to_owned(),
        message,
    })
}

/// Checks that the raw scheduled-class id `raw` decodes to `ride` at `start`.
///
/// # Errors
///
/// Says whether the id does not decode or names another ride or time.
pub fn check_event(raw: &str, ride: &PelotonId, start: OffsetDateTime) -> Result<(), AnswerError> {
    let id: ScheduledClassId = raw.parse().map_err(|_| AnswerError::EventUndecodable)?;
    if id.ride_id() != ride {
        return Err(AnswerError::EventWrongRide);
    }
    if id.scheduled_start_time() != start {
        return Err(AnswerError::EventWrongStart);
    }
    Ok(())
}

/// Whether the spike's item is newly listed: an id in `during` but not in
/// `before` that is the raw id itself or decodes to `ride` at `start`.
#[must_use]
pub fn newly_listed(
    before: &BTreeSet<String>,
    during: &BTreeSet<String>,
    raw: &str,
    ride: &PelotonId,
    start: OffsetDateTime,
) -> bool {
    during
        .difference(before)
        .any(|id| id == raw || check_event(id, ride, start).is_ok())
}

/// One Stack entry, as the spike reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackItem {
    /// The ride.
    pub ride: PelotonId,
    /// The class title.
    pub title: String,
    /// Peloton's own class id for it, if returned.
    pub join_token: Option<String>,
}

/// Reads the Stack entries, in play order, from a Stack response object.
///
/// # Errors
///
/// Returns a message when the shape is unexpected, when `numClasses` is
/// missing or disagrees with the list, or when an entry lacks a `playOrder` or a class
/// id that can be turned into a ride id (nothing is ever silently dropped).
pub fn stack_items(stack: &Value) -> Result<Vec<StackItem>, AnswerError> {
    let count = stack
        .get("numClasses")
        .and_then(Value::as_i64)
        .ok_or(AnswerError::StackWithoutCount)?;
    let Some(list) = stack.pointer("/userStack/stackedClassList") else {
        return if count == 0 {
            Ok(Vec::new())
        } else {
            Err(AnswerError::StackWithoutList)
        };
    };
    let list = list.as_array().ok_or(AnswerError::StackListNotList)?;
    if usize::try_from(count).ok() != Some(list.len()) {
        return Err(AnswerError::StackCountMismatch {
            count,
            entries: list.len(),
        });
    }
    let mut items = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let order = entry
            .get("playOrder")
            .and_then(Value::as_i64)
            .ok_or(AnswerError::StackEntryWithoutOrder { index })?;
        let class = entry
            .get("pelotonClass")
            .ok_or(AnswerError::StackEntryWithoutClass { index })?;
        let class_id = class
            .get("classId")
            .and_then(Value::as_str)
            .ok_or(AnswerError::StackEntryWithoutClassId { index })?;
        let ride = class_id
            .parse::<PelotonId>()
            .or_else(|_| {
                class_id
                    .parse::<OnDemandClassId>()
                    .map(|c| c.ride_id().clone())
            })
            .map_err(|_| AnswerError::StackEntryBadClassId { index })?;
        items.push((
            order,
            StackItem {
                ride,
                title: class
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("(untitled)")
                    .to_owned(),
                join_token: class
                    .get("joinToken")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
        ));
    }
    items.sort_by_key(|(order, _)| *order);
    Ok(items.into_iter().map(|(_, item)| item).collect())
}

/// The ids that write a saved Stack back, all issued by Peloton.
#[derive(Debug, PartialEq, Eq)]
pub struct Restore {
    /// Peloton's `joinToken` of each saved entry, in play order.
    pub ids: Vec<String>,
    /// Entries whose `joinToken` names the right ride but differs from the
    /// local encoding (a finding worth keeping).
    pub encoding_mismatches: Vec<String>,
}

/// Builds the restore list from Peloton's own `joinToken`s.
///
/// # Errors
///
/// Refuses (naming the entry) when a `joinToken` is missing or is not an
/// on-demand class id for that entry's ride, so a restore never sends an id
/// Peloton did not issue for that class.
pub fn restore_ids(saved: &[StackItem]) -> Result<Restore, AnswerError> {
    let mut restore = Restore {
        ids: Vec::with_capacity(saved.len()),
        encoding_mismatches: Vec::new(),
    };
    for (index, item) in saved.iter().enumerate() {
        let token = item
            .join_token
            .as_deref()
            .ok_or_else(|| AnswerError::RestoreWithoutToken {
                entry: index + 1,
                ride: item.ride.clone(),
            })?;
        let same_ride = token
            .parse::<OnDemandClassId>()
            .is_ok_and(|t| t.ride_id() == &item.ride);
        if !same_ride {
            return Err(AnswerError::RestoreForeignToken {
                entry: index + 1,
                ride: item.ride.clone(),
            });
        }
        if token != OnDemandClassId::from_ride(&item.ride).as_str() {
            restore
                .encoding_mismatches
                .push(format!("entry {} ({})", index + 1, item.ride));
        }
        restore.ids.push(token.to_owned());
    }
    Ok(restore)
}

/// The JSON type of `value`, for describing answers without echoing them.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// The outcome of one spike step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        }
    }
}

/// Step outcomes, printed as they happen and again at the end.
#[derive(Debug, Default)]
struct Steps {
    entries: Vec<(String, Status, String)>,
}

impl Steps {
    fn record(&mut self, step: &str, status: Status, detail: impl Into<String>) {
        let detail = detail.into();
        eprintln!("{} {step}: {detail}", status.label());
        self.entries.push((step.to_owned(), status, detail));
    }

    fn result<E: std::error::Error + 'static>(
        &mut self,
        step: &str,
        result: Result<String, E>,
    ) -> bool {
        match result {
            Ok(detail) => {
                self.record(step, Status::Pass, detail);
                true
            }
            Err(error) => {
                self.record(step, Status::Fail, report(&error));
                false
            }
        }
    }

    fn any_failed(&self) -> bool {
        self.entries.iter().any(|(_, s, _)| *s == Status::Fail)
    }

    fn print_summary(&self) {
        eprintln!();
        eprintln!("Summary:");
        for (step, status, detail) in &self.entries {
            eprintln!("  {} {step}: {detail}", status.label());
        }
    }
}

/// The logged-in context the steps share.
struct Spike<'a> {
    session: &'a Session,
    access: &'a SecretString,
    dir: &'a Path,
    class: OnDemandClassId,
}

impl Spike<'_> {
    /// Sends `op`, saves the raw answer as `file` and returns `data.<field>`.
    /// The answer is classified before it is saved, so a failed save (its
    /// own FAIL) never hides what Peloton did.
    fn call(
        &self,
        steps: &mut Steps,
        op: &Op,
        variables: &Value,
        file: &str,
    ) -> Result<Value, StepError> {
        self.session.pause();
        let reply = self
            .session
            .graphql(self.access, op.name, op.kind, op.document, variables)?;
        let result = root_field(&reply, op.field);
        if let Err(e) = save(self.dir, file, &reply.body) {
            steps.record(&format!("save {file}"), Status::Fail, report(&e));
        }
        Ok(result?)
    }

    fn rest(&self, steps: &mut Steps, step: &str, path_and_query: &str, file: &str) {
        self.session.pause();
        let result = self
            .session
            .get(self.access, path_and_query)
            .map_err(StepError::from)
            .and_then(|body| {
                save(self.dir, file, &body)
                    .map(|()| format!("saved {file}"))
                    .map_err(StepError::from)
            });
        steps.result(step, result);
    }

    fn list_schedule(
        &self,
        steps: &mut Steps,
        window: &Window,
        file: &str,
    ) -> Result<BTreeSet<String>, StepError> {
        let list = self.call(
            steps,
            &LIST,
            &json!({ "startTime": window.start, "endTime": window.end }),
            file,
        )?;
        Ok(listed_ids(&list)?)
    }

    fn view_stack(&self, steps: &mut Steps, file: &str) -> Result<Vec<StackItem>, StepError> {
        let stack = self.call(steps, &VIEW, &json!({}), file)?;
        Ok(stack_items(&stack)?)
    }
}

/// Logs in and runs every step; see the module docs.
///
/// # Errors
///
/// Returns a [`SpikeError`] when setup fails, when the gateway rejects the
/// token, or when any step failed.
pub fn run(session: &Session, dir: &Path, class: &PelotonId) -> Result<(), SpikeError> {
    let dir = resolve_dir(dir)?;
    guard_dir(&dir, find_worktree(&dir).as_deref())?;
    prepare_dir(&dir)?;

    let tokens = session.login()?;
    let spike = Spike {
        session,
        access: &tokens.access_token,
        dir: &dir,
        class: OnDemandClassId::from_ride(class),
    };
    let mut steps = Steps::default();

    let in_stack = check_class(&spike, &mut steps)?;
    spike.rest(
        &mut steps,
        "library search",
        &format!(
            "{}?browse_category=cycling&limit=5",
            endpoints::library_archived()
        ),
        "library_archived.json",
    );
    spike.rest(
        &mut steps,
        "library filters",
        &format!(
            "{}?library_type=on_demand&browse_category=cycling",
            endpoints::ride_filters()
        ),
        "ride_filters.json",
    );
    spike.rest(
        &mut steps,
        "browse categories",
        &format!("{}?library_type=on_demand", endpoints::browse_categories()),
        "browse_categories.json",
    );
    schedule_round_trip(&spike, &mut steps)?;
    stack_round_trip(&spike, &mut steps, in_stack);

    steps.print_summary();
    if steps.any_failed() {
        Err(SpikeError::StepsFailed)
    } else {
        Ok(())
    }
}

/// Step 1. Returns whether Peloton says the class is in the Stack, if known.
fn check_class(spike: &Spike<'_>, steps: &mut Steps) -> Result<Option<bool>, SpikeError> {
    let step = "check class in Stack (token + id encoding)";
    match spike.call(
        steps,
        &CHECK,
        &json!({ "pelotonClassId": spike.class.as_str() }),
        "check_class_in_stack.json",
    ) {
        Ok(value) => {
            let answer = value.as_bool();
            let detail = answer.map_or_else(
                || format!("Peloton answered a {} (not a boolean)", json_type(&value)),
                |b| format!("Peloton answered {b}"),
            );
            steps.record(step, Status::Pass, detail);
            Ok(answer)
        }
        Err(StepError::Call(CallFailure::Auth)) => {
            steps.record(step, Status::Fail, "GraphQL gateway rejected the token");
            Err(SpikeError::GatewayRejected)
        }
        Err(error) => {
            steps.record(step, Status::Fail, report(&error));
            Ok(None)
        }
    }
}

/// The schedule window the spike lists, as sent upstream.
struct Window {
    start: String,
    end: String,
}

/// Where the spike's scheduled item should be, for checks and hand cleanup.
struct Plan<'a> {
    ride: &'a PelotonId,
    start: OffsetDateTime,
    moved: OffsetDateTime,
}

impl Plan<'_> {
    fn describe(&self) -> String {
        format!("ride {} at {} or {} UTC", self.ride, self.start, self.moved)
    }
}

/// Step 3.
fn schedule_round_trip(spike: &Spike<'_>, steps: &mut Steps) -> Result<(), DateError> {
    let now = OffsetDateTime::now_utc();
    let window = Window {
        start: iso(now)?,
        end: iso(now + SCHEDULE_WINDOW)?,
    };
    let start = spike_start(now)?;
    let plan = Plan {
        ride: spike.class.ride_id(),
        start,
        moved: start + Duration::HOUR,
    };

    let before = match spike.list_schedule(steps, &window, "schedule_before.json") {
        Ok(before) => before,
        Err(e) => {
            steps.record("schedule list (before)", Status::Fail, report(&e));
            steps.record(
                "schedule writes",
                Status::Skip,
                "cannot verify cleanup without the list",
            );
            return Ok(());
        }
    };
    steps.record(
        "schedule list (before)",
        Status::Pass,
        format!("{} item(s) in the next 14 days", before.len()),
    );

    let outcome = schedule_writes(spike, steps, &window, &plan, &before)?;

    match spike.list_schedule(steps, &window, "schedule_after.json") {
        Ok(after) if after == before && outcome.seen_in_list => steps.record(
            "schedule list (after)",
            Status::Pass,
            "the schedule is back to its pre-spike items",
        ),
        Ok(after) if after == before => steps.record(
            "schedule list (after)",
            Status::Fail,
            "UNPROVEN: equal to before, but the spike's item never showed up in the list, so the comparison proves nothing",
        ),
        Ok(after) => {
            for extra in after.difference(&before) {
                print_left_over(extra, &plan);
            }
            steps.record(
                "schedule list (after)",
                Status::Fail,
                format!(
                    "the schedule differs from before: {} added, {} missing",
                    after.difference(&before).count(),
                    before.difference(&after).count()
                ),
            );
        }
        Err(e) => {
            // An add was always attempted by now, and its reply may have been lost.
            steps.record("schedule list (after)", Status::Fail, report(&e));
            eprintln!();
            eprintln!(
                "!!! Could not re-read the schedule. Check it by hand for {}",
                plan.describe()
            );
        }
    }

    if let Some(raw) = outcome.left_over {
        print_left_over(&raw, &plan);
    }
    Ok(())
}

/// What the schedule writes left behind.
struct WriteOutcome {
    /// The list showed the spike's item, absent before, after the add.
    seen_in_list: bool,
    /// The raw id still on the account when removal did not succeed.
    left_over: Option<String>,
}

/// Adds the class, proves it shows up in the list, reads it, moves it one
/// hour and removes it, always sending back the raw ids Peloton returned.
fn schedule_writes(
    spike: &Spike<'_>,
    steps: &mut Steps,
    window: &Window,
    plan: &Plan<'_>,
    before: &BTreeSet<String>,
) -> Result<WriteOutcome, DateError> {
    let start_iso = iso(plan.start)?;
    let added = spike
        .call(
            steps,
            &ADD,
            &json!({ "id": spike.class.as_str(), "startTime": start_iso }),
            "schedule_add.json",
        )
        .and_then(|v| Ok(scheduled_result(&v)?));
    let first = match added {
        Ok(first) => first,
        Err(e) => {
            steps.record("schedule add", Status::Fail, report(&e));
            steps.record(
                "schedule read, move and remove",
                Status::Skip,
                "Peloton did not report a scheduled class",
            );
            eprintln!("note: if the add reached Peloton anyway, the after-list check will show it");
            return Ok(WriteOutcome {
                seen_in_list: false,
                left_over: None,
            });
        }
    };
    steps.record(
        "schedule add",
        Status::Pass,
        format!("Peloton scheduled it for {start_iso}"),
    );
    steps.result(
        "schedule add id decodes to the class and time",
        check_event(&first, plan.ride, plan.start).map(|()| "it does".to_owned()),
    );

    let seen_in_list = match spike.list_schedule(steps, window, "schedule_during.json") {
        Ok(during) if newly_listed(before, &during, &first, plan.ride, plan.start) => {
            steps.record(
                "schedule list (with spike item)",
                Status::Pass,
                "the new item is listed",
            );
            true
        }
        Ok(_) => {
            steps.record(
                "schedule list (with spike item)",
                Status::Fail,
                "the new item is not in the list (or was already there before)",
            );
            false
        }
        Err(e) => {
            steps.record("schedule list (with spike item)", Status::Fail, report(&e));
            false
        }
    };

    let read = spike
        .call(
            steps,
            &READ,
            &json!({ "id": first }),
            "scheduled_class.json",
        )
        .and_then(|v| {
            if v.get("id").and_then(Value::as_str) == Some(first.as_str()) {
                Ok("read back the scheduled class".to_owned())
            } else {
                Err(AnswerError::ReadBackIdDiffers.into())
            }
        });
    steps.result("schedule read", read);

    let current = schedule_move(spike, steps, &first, plan)?;

    let removed = spike
        .call(
            steps,
            &REMOVE,
            &json!({ "id": current }),
            "schedule_remove.json",
        )
        .and_then(|v| Ok(scheduled_result(&v).map(|_| "removed".to_owned())?));
    let left_over = removed.is_err().then_some(current);
    steps.result("schedule remove", removed);
    Ok(WriteOutcome {
        seen_in_list,
        left_over,
    })
}

/// Moves the scheduled class `first` one hour; returns the raw id that now
/// names it (Peloton's new one, or `first` if the move was not reported).
fn schedule_move(
    spike: &Spike<'_>,
    steps: &mut Steps,
    first: &str,
    plan: &Plan<'_>,
) -> Result<String, DateError> {
    let moved = spike
        .call(
            steps,
            &MOVE,
            &json!({ "id": first, "startTime": iso(plan.moved)? }),
            "schedule_reschedule.json",
        )
        .and_then(|v| Ok(scheduled_result(&v)?));
    match moved {
        Ok(new_id) => {
            let same = if new_id == first {
                "Peloton kept the same id"
            } else {
                "Peloton issued a new id"
            };
            steps.record(
                "schedule move (+1 h)",
                Status::Pass,
                format!("moved; {same}"),
            );
            steps.result(
                "schedule move id decodes to the class and new time",
                check_event(&new_id, plan.ride, plan.moved).map(|()| "it does".to_owned()),
            );
            Ok(new_id)
        }
        Err(e) => {
            steps.record("schedule move (+1 h)", Status::Fail, report(&e));
            Ok(first.to_owned())
        }
    }
}

fn print_left_over(raw: &str, plan: &Plan<'_>) {
    let what = raw.parse::<ScheduledClassId>().map_or_else(
        |_| plan.describe(),
        |e| format!("ride {} at {} UTC", e.ride_id(), e.scheduled_start_time()),
    );
    eprintln!();
    eprintln!("!!! Remove this from your Peloton schedule by hand: {what}");
}

/// Step 4.
fn stack_round_trip(spike: &Spike<'_>, steps: &mut Steps, in_stack: Option<bool>) {
    let saved = match spike.view_stack(steps, "stack_before.json") {
        Ok(saved) => saved,
        Err(e) => {
            steps.record("Stack read (before)", Status::Fail, report(&e));
            steps.record(
                "Stack writes",
                Status::Skip,
                "the current Stack could not be read in full",
            );
            return;
        }
    };
    eprintln!();
    eprintln!(
        "Current Stack ({} classes, saved to stack_before.json):",
        saved.len()
    );
    for (index, item) in saved.iter().enumerate() {
        eprintln!("  {}. {}  {}", index + 1, item.ride, item.title);
    }
    steps.record(
        "Stack read (before)",
        Status::Pass,
        format!("{} classes", saved.len()),
    );

    let restore = match restore_ids(&saved) {
        Ok(restore) => restore,
        Err(e) => {
            steps.record("Stack restore ids", Status::Fail, report(&e));
            steps.record(
                "Stack writes",
                Status::Skip,
                "not writing ids Peloton did not issue for these classes",
            );
            return;
        }
    };
    if !saved.is_empty() {
        if restore.encoding_mismatches.is_empty() {
            steps.record(
                "Stack local encoding",
                Status::Pass,
                "every joinToken equals the local encoding",
            );
        } else {
            steps.record(
                "Stack local encoding",
                Status::Fail,
                format!(
                    "local encoding differs from Peloton's joinToken for {}; restoring with Peloton's",
                    restore.encoding_mismatches.join(", ")
                ),
            );
        }
    }

    let already = saved.iter().any(|item| &item.ride == spike.class.ride_id());
    if in_stack.is_some_and(|answer| answer != already) {
        eprintln!("note: CheckDoesClassExistInUserStack disagrees with the Stack read");
    }
    let mut ok = if already {
        steps.record(
            "Stack add",
            Status::Skip,
            "the class is already in the Stack; not adding a duplicate",
        );
        true
    } else {
        stack_add(spike, steps, saved.len())
    };

    let modified = spike
        .call(
            steps,
            &MODIFY,
            &json!({ "input": { "pelotonClassIdList": restore.ids } }),
            "stack_modify.json",
        )
        .map(|_| "wrote the saved Stack back".to_owned());
    ok &= steps.result("Stack restore", modified);

    let verified = spike
        .view_stack(steps, "stack_after.json")
        .and_then(|after| {
            if after
                .iter()
                .map(|i| &i.ride)
                .eq(saved.iter().map(|i| &i.ride))
            {
                Ok("the Stack matches the saved order".to_owned())
            } else {
                Err(AnswerError::StackDiffers.into())
            }
        });
    ok &= steps.result("Stack read (after)", verified);

    if !ok {
        print_rebuild(&saved);
    }
}

/// Appends the class to a Stack of `saved_len` classes and checks the result.
fn stack_add(spike: &Spike<'_>, steps: &mut Steps, saved_len: usize) -> bool {
    let ride = spike.class.ride_id();
    let added = spike
        .call(
            steps,
            &STACK_ADD,
            &json!({ "input": { "pelotonClassId": spike.class.as_str() } }),
            "stack_add.json",
        )
        .and_then(|v| Ok(stack_items(&v)?))
        .and_then(|items| {
            if items.last().map(|i| &i.ride) == Some(ride) && items.len() == saved_len + 1 {
                Ok("the class was appended".to_owned())
            } else {
                Err(AnswerError::StackAddMismatch.into())
            }
        });
    steps.result("Stack add", added)
}

fn print_rebuild(saved: &[StackItem]) {
    eprintln!();
    eprintln!("!!! The Stack may not be as it was. Rebuild it in this order:");
    for (index, item) in saved.iter().enumerate() {
        eprintln!("!!!   {}. {}  {}", index + 1, item.ride, item.title);
    }
    if saved.is_empty() {
        eprintln!("!!!   (it was empty)");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::{
        AnswerError, CallFailure, Restore, SpikeError, StackItem, check_event, json_type,
        listed_ids, newly_listed, restore_ids, root_field, schedule_ids, scheduled_result,
        spike_start, stack_items,
    };
    use crate::report;
    use crate::session::GraphqlReply;
    use peloton::ids::{OnDemandClassId, PelotonId};

    const RIDE: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "fedcba9876543210fedcba9876543210";

    fn reply(status: u16, body: &serde_json::Value) -> GraphqlReply {
        GraphqlReply {
            status: reqwest::StatusCode::from_u16(status).expect("status"),
            body: serde_json::to_vec(body).expect("json"),
        }
    }

    fn ride(id: &str) -> PelotonId {
        id.parse().expect("ride")
    }

    fn event(ride: &str, start: i64) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(format!(
            r#"{{"home_peloton_id": "{OTHER}", "ride_id": "{ride}", "scheduled_start_time": {start}, "studio_peloton_id": null, "type": "event"}}"#
        ))
    }

    #[test]
    fn root_field_returns_data() {
        let r = reply(200, &json!({"data": {"x": true}}));
        assert_eq!(root_field(&r, "x"), Ok(json!(true)));
    }

    #[test]
    fn root_field_detects_auth_rejection() {
        for status in [401, 403] {
            assert_eq!(
                root_field(&reply(status, &json!({})), "x"),
                Err(CallFailure::Auth)
            );
        }
        let coded =
            json!({"errors": [{"message": "nope", "extensions": {"code": "UNAUTHENTICATED"}}]});
        assert_eq!(root_field(&reply(200, &coded), "x"), Err(CallFailure::Auth));
        let worded = json!({"errors": [{"message": "User is not authorized"}], "data": null});
        assert_eq!(
            root_field(&reply(200, &worded), "x"),
            Err(CallFailure::Auth)
        );
    }

    #[test]
    fn root_field_summarises_other_errors() {
        let invalid = json!({"errors": [{"message": "Cannot query field \"y\" on type \"Q\".", "extensions": {"code": "GRAPHQL_VALIDATION_FAILED"}}]});
        let error = root_field(&reply(400, &invalid), "x").expect_err("a failure");
        assert!(matches!(error, CallFailure::Graphql { .. }));
        assert!(
            report(&error).contains("GRAPHQL_VALIDATION_FAILED: Cannot query field"),
            "{error}"
        );
        assert_eq!(
            root_field(&reply(200, &json!({"data": {"x": null}})), "x"),
            Err(CallFailure::NoData {
                field: "x".to_owned()
            })
        );
        assert_eq!(
            root_field(&reply(500, &json!({"data": {"x": 1}})), "x"),
            Err(CallFailure::Status {
                status: reqwest::StatusCode::INTERNAL_SERVER_ERROR
            })
        );
        let not_json = GraphqlReply {
            status: reqwest::StatusCode::OK,
            body: b"<html>".to_vec(),
        };
        let error = root_field(&not_json, "x").expect_err("not json");
        assert_eq!(report(&error), "status 200 OK, body is not JSON");
    }

    #[test]
    fn spike_start_is_next_whole_hour_at_least_13_days_ahead() {
        for (now, start) in [
            (
                datetime!(2026-10-25 10:20:30 UTC),
                datetime!(2026-11-07 11:00 UTC),
            ),
            (
                datetime!(2026-10-25 10:00 UTC),
                datetime!(2026-11-07 10:00 UTC),
            ),
            (
                datetime!(2026-10-25 23:59:59.5 UTC),
                datetime!(2026-11-08 00:00 UTC),
            ),
            (
                datetime!(2026-10-25 08:30 -07:00),
                datetime!(2026-11-07 16:00 UTC),
            ),
        ] {
            assert_eq!(spike_start(now).expect("start"), start);
        }
    }

    #[test]
    fn spike_start_reports_date_overflow() {
        let error = spike_start(time::PrimitiveDateTime::MAX.assume_utc()).expect_err("overflow");
        assert_eq!(report(&error), "date overflow");
    }

    #[test]
    fn schedule_ids_finds_ids_in_any_shape() {
        let flat = json!([{"__typename": "ScheduledClass", "id": "a"}, {"id": "b"}]);
        let wrapped = json!({"items": [{"id": "a"}, {"nested": {"id": "b"}}], "count": 2});
        assert_eq!(schedule_ids(&flat), schedule_ids(&wrapped));
        assert_eq!(schedule_ids(&flat).len(), 2);
    }

    #[test]
    fn listed_ids_reads_the_list_variant_only() {
        let list = json!({
            "__typename": "YourScheduleItemList",
            "yourScheduleItems": [{"__typename": "ScheduledClass", "id": "a"}, {"__typename": "InviteDetails"}],
        });
        assert_eq!(listed_ids(&list).map(|ids| ids.len()), Ok(1));
        let empty = json!({"__typename": "YourScheduleItemList", "yourScheduleItems": []});
        assert_eq!(listed_ids(&empty).map(|ids| ids.len()), Ok(0));
        let error = json!({"__typename": "InviteFriendsError", "message": "down"});
        assert_eq!(
            listed_ids(&error),
            Err(AnswerError::ScheduleListVariant {
                typename: "InviteFriendsError".to_owned(),
                message: "down".to_owned(),
            })
        );
        assert_eq!(
            report(&listed_ids(&error).expect_err("error variant")),
            "schedule list answered InviteFriendsError: down"
        );
        let shapeless = json!({"__typename": "YourScheduleItemList"});
        assert_eq!(
            listed_ids(&shapeless),
            Err(AnswerError::ScheduleItemsNotList)
        );
    }

    #[test]
    fn scheduled_result_keeps_the_raw_id_whatever_it_is() {
        let id = event(RIDE, 1_700_000_000);
        let ok = json!({"__typename": "ScheduledClass", "id": id});
        assert_eq!(scheduled_result(&ok), Ok(id));
        // An id the spike cannot decode is still Peloton's, and must be kept.
        let odd = json!({"__typename": "ScheduledClass", "id": "opaque"});
        assert_eq!(scheduled_result(&odd), Ok("opaque".to_owned()));
        let err =
            json!({"__typename": "CannotScheduleClassError", "code": "X", "message": "too far"});
        assert_eq!(
            report(&scheduled_result(&err).expect_err("rejected")),
            "CannotScheduleClassError: X: too far"
        );
        assert_eq!(
            scheduled_result(&json!({"__typename": "ScheduledClass"})),
            Err(AnswerError::ScheduledClassWithoutId)
        );
    }

    #[test]
    fn check_event_compares_ride_and_time() {
        let start = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
        let id = event(RIDE, 1_700_000_000);
        assert_eq!(check_event(&id, &ride(RIDE), start), Ok(()));
        assert_eq!(
            check_event(&id, &ride(OTHER), start),
            Err(AnswerError::EventWrongRide)
        );
        assert_eq!(
            check_event(&id, &ride(RIDE), start + time::Duration::HOUR),
            Err(AnswerError::EventWrongStart)
        );
        assert_eq!(
            check_event("opaque", &ride(RIDE), start),
            Err(AnswerError::EventUndecodable)
        );
    }

    #[test]
    fn newly_listed_requires_an_item_absent_before() {
        use std::collections::BTreeSet;
        let start = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
        let ours = event(RIDE, 1_700_000_000);
        let set = |ids: &[&str]| ids.iter().map(|s| (*s).to_owned()).collect::<BTreeSet<_>>();
        let empty = set(&[]);
        // By raw id, or by a different raw id that decodes to ride + start.
        assert!(newly_listed(
            &empty,
            &set(&["opaque"]),
            "opaque",
            &ride(OTHER),
            start
        ));
        assert!(newly_listed(
            &empty,
            &set(&[&ours]),
            "different-raw",
            &ride(RIDE),
            start
        ));
        // Already there before: not proof of the spike's add.
        assert!(!newly_listed(
            &set(&["opaque"]),
            &set(&["opaque"]),
            "opaque",
            &ride(RIDE),
            start
        ));
        assert!(!newly_listed(
            &set(&[&ours]),
            &set(&[&ours]),
            "different-raw",
            &ride(RIDE),
            start
        ));
        // Another ride or another time.
        let others = set(&[&event(OTHER, 1_700_000_000), &event(RIDE, 1_700_003_600)]);
        assert!(!newly_listed(&empty, &others, &ours, &ride(RIDE), start));
    }

    #[test]
    fn stack_items_reads_in_play_order() {
        let stack = json!({"numClasses": 2, "userStack": {"stackedClassList": [
            {"playOrder": 2, "pelotonClass": {"classId": OTHER, "title": "B", "joinToken": null}},
            {"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A", "joinToken": "t"}},
        ]}});
        let items = stack_items(&stack).expect("items");
        assert_eq!(items[0].ride, ride(RIDE));
        assert_eq!(items[0].join_token.as_deref(), Some("t"));
        assert_eq!(items[1].title, "B");
        assert_eq!(items[1].join_token, None);
    }

    #[test]
    fn stack_items_accepts_base64_class_ids_and_empty_stacks() {
        let encoded = OnDemandClassId::from_ride(&ride(RIDE));
        let stack = json!({"numClasses": 1, "userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": encoded.as_str(), "title": "A"}},
        ]}});
        assert_eq!(stack_items(&stack).expect("items")[0].ride, ride(RIDE));
        assert_eq!(stack_items(&json!({"numClasses": 0})), Ok(Vec::new()));
    }

    #[test]
    fn stack_items_refuses_undecodable_entries() {
        let stack = json!({"numClasses": 1, "userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": "../x", "title": "A"}},
        ]}});
        assert_eq!(
            stack_items(&stack),
            Err(AnswerError::StackEntryBadClassId { index: 0 })
        );
        assert_eq!(
            stack_items(&json!({"numClasses": 3})),
            Err(AnswerError::StackWithoutList)
        );
    }

    #[test]
    fn stack_items_refuses_partial_lists() {
        let stack = json!({"numClasses": 2, "userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A"}},
        ]}});
        let err = stack_items(&stack).expect_err("partial list");
        assert_eq!(report(&err), "numClasses is 2 but the list holds 1 entries");
    }

    #[test]
    fn stack_items_requires_num_classes() {
        let list = json!([{"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A"}}]);
        for stack in [
            json!({"userStack": {"stackedClassList": list}}),
            json!({"numClasses": null, "userStack": {"stackedClassList": list}}),
            json!({}),
        ] {
            let err = stack_items(&stack).expect_err("no numClasses");
            assert_eq!(err, AnswerError::StackWithoutCount);
        }
    }

    #[test]
    fn stack_items_refuses_missing_play_order() {
        let stack = json!({"numClasses": 2, "userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A"}},
            {"pelotonClass": {"classId": OTHER, "title": "B"}},
        ]}});
        let err = stack_items(&stack).expect_err("no playOrder");
        assert_eq!(err, AnswerError::StackEntryWithoutOrder { index: 1 });
        assert_eq!(report(&err), "Stack entry 1 has no playOrder");
    }

    #[test]
    fn restore_ids_use_peloton_tokens_for_the_same_ride() {
        use base64::Engine as _;
        let local = OnDemandClassId::from_ride(&ride(RIDE));
        let item = |token: Option<&str>| StackItem {
            ride: ride(RIDE),
            title: "A".to_owned(),
            join_token: token.map(str::to_owned),
        };
        assert_eq!(
            restore_ids(&[item(Some(local.as_str()))]),
            Ok(Restore {
                ids: vec![local.as_str().to_owned()],
                encoding_mismatches: Vec::new(),
            })
        );
        // Same ride, different bytes: restore with Peloton's verbatim, and say so.
        let compact = base64::engine::general_purpose::STANDARD.encode(format!(
            r#"{{"home_peloton_id":null,"ride_id":"{RIDE}","studio_peloton_id":null,"type":"on_demand"}}"#
        ));
        let restore = restore_ids(&[item(Some(&compact))]).expect("same ride");
        assert_eq!(restore.ids, vec![compact]);
        assert_eq!(restore.encoding_mismatches.len(), 1);
        // Missing, other ride, or not a class id: refuse.
        let missing = restore_ids(&[item(None)]).expect_err("no joinToken");
        assert_eq!(
            report(&missing),
            format!("Stack entry 1 ({RIDE}): Peloton returned no joinToken")
        );
        let other = OnDemandClassId::from_ride(&ride(OTHER));
        for token in [other.as_str(), "opaque"] {
            let foreign = restore_ids(&[item(Some(token))]).expect_err("foreign joinToken");
            assert!(matches!(
                foreign,
                AnswerError::RestoreForeignToken { entry: 1, .. }
            ));
        }
        assert_eq!(
            restore_ids(&[]),
            Ok(Restore {
                ids: Vec::new(),
                encoding_mismatches: Vec::new(),
            })
        );
    }

    #[test]
    fn rejected_token_stops_the_spike_with_its_own_message() {
        assert_eq!(
            report(&SpikeError::GatewayRejected),
            "GraphQL gateway rejected the token; stopping before any other call"
        );
        assert_eq!(
            report(&SpikeError::StepsFailed),
            "some spike steps failed; see above"
        );
    }

    #[test]
    fn json_type_names_without_echoing() {
        assert_eq!(json_type(&json!({"secret": "x"})), "object");
        assert_eq!(json_type(&json!(true)), "boolean");
        assert_eq!(json_type(&json!(null)), "null");
    }
}
