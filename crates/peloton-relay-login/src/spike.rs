//! `spike`: prove the GraphQL schedule and Stack operations against the
//! owner's account, saving every raw answer, and leave the account as it was.
//!
//! Steps, each reported PASS, FAIL or SKIP:
//! 1. `CheckDoesClassExistInUserStack` with a locally encoded class id. A
//!    rejected token stops the spike before anything else is sent.
//! 2. Schema introspection, saved when the gateway allows it.
//! 3. The three library reads (REST), saved.
//! 4. Schedule: list, add at the first whole hour at least 13 days ahead,
//!    prove the new item is listed, read it, move it one hour, remove it, and
//!    check the list is as before. Later calls always send back the raw ids
//!    Peloton returned, whether or not they decode as expected.
//! 5. Stack: read and print it (refusing a partial list), add the class, write
//!    the saved Stack back with `ModifyStack` using Peloton's own `joinToken`s,
//!    and check the read-back order is identical.

use std::collections::BTreeSet;
use std::path::Path;

use peloton::endpoints;
use peloton::graphql::{
    ADD_CLASS_TO_SCHEDULE, ADD_CLASS_TO_STACK, CHECK_CLASS_IN_STACK, MODIFY_STACK, OpKind,
    REMOVE_CLASS_FROM_SCHEDULE, RESCHEDULE_CLASS, SCHEDULED_CLASS, VIEW_USER_STACK,
    YOUR_SCHEDULE_ITEMS,
};
use peloton::ids::{OnDemandClassId, PelotonId, ScheduledClassId};
use secrecy::SecretString;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use crate::capture::{find_worktree, guard_dir, prepare_dir, resolve_dir, save};
use crate::session::{GraphqlReply, Session};

/// How far ahead the spike schedules, so it cannot collide with real plans.
const SCHEDULE_LEAD: Duration = Duration::days(13);
/// The schedule window the spike lists (Peloton's own horizon).
const SCHEDULE_WINDOW: Duration = Duration::days(14);
/// Longest GraphQL error message echoed to the terminal.
const MAX_MESSAGE: usize = 160;

/// A compact introspection query: every type with its fields, arguments,
/// input fields, union members and enum values.
const INTROSPECTION: &str = "query IntrospectionQuery {
  __schema {
    queryType { name }
    mutationType { name }
    types {
      kind
      name
      fields(includeDeprecated: true) {
        name
        args { name type { ...TypeRef } }
        type { ...TypeRef }
      }
      inputFields { name type { ...TypeRef } }
      interfaces { name }
      possibleTypes { name }
      enumValues(includeDeprecated: true) { name }
    }
  }
}

fragment TypeRef on __Type {
  kind
  name
  ofType { kind name ofType { kind name ofType { kind name ofType { kind name } } } }
}
";

/// One GraphQL operation as the spike sends it.
struct Op {
    name: &'static str,
    kind: OpKind,
    document: &'static str,
    /// The root field of `data` holding the answer.
    field: &'static str,
}

const CHECK: Op = Op {
    name: "CheckDoesClassExistInUserStack",
    kind: OpKind::Query,
    document: CHECK_CLASS_IN_STACK,
    field: "checkDoesClassExistInUserStack",
};
const SCHEMA: Op = Op {
    name: "IntrospectionQuery",
    kind: OpKind::Query,
    document: INTROSPECTION,
    field: "__schema",
};
const LIST: Op = Op {
    name: "YourScheduleItems",
    kind: OpKind::Query,
    document: YOUR_SCHEDULE_ITEMS,
    field: "userScheduledItemsList",
};
const READ: Op = Op {
    name: "ScheduledClass",
    kind: OpKind::Query,
    document: SCHEDULED_CLASS,
    field: "scheduledClass",
};
const ADD: Op = Op {
    name: "AddClassToSchedule",
    kind: OpKind::Mutation,
    document: ADD_CLASS_TO_SCHEDULE,
    field: "addClassToSchedule",
};
const MOVE: Op = Op {
    name: "RescheduleClass",
    kind: OpKind::Mutation,
    document: RESCHEDULE_CLASS,
    field: "rescheduleClass",
};
const REMOVE: Op = Op {
    name: "RemoveClassFromSchedule",
    kind: OpKind::Mutation,
    document: REMOVE_CLASS_FROM_SCHEDULE,
    field: "removeClassFromSchedule",
};
const VIEW: Op = Op {
    name: "ViewUserStack",
    kind: OpKind::Query,
    document: VIEW_USER_STACK,
    field: "viewUserStack",
};
const STACK_ADD: Op = Op {
    name: "AddClassToStack",
    kind: OpKind::Mutation,
    document: ADD_CLASS_TO_STACK,
    field: "addClassToStack",
};
const MODIFY: Op = Op {
    name: "ModifyStack",
    kind: OpKind::Mutation,
    document: MODIFY_STACK,
    field: "modifyStack",
};

/// Why a GraphQL call did not yield data.
#[derive(Debug, PartialEq, Eq)]
pub enum CallFailure {
    /// The gateway refused the token (HTTP 401/403 or an auth error).
    Auth,
    /// Anything else, summarised without the body.
    Other(String),
}

/// Extracts `data.<field>` from a gateway reply, or says why it is missing.
///
/// # Errors
///
/// [`CallFailure::Auth`] when the token was refused, otherwise
/// [`CallFailure::Other`] with the status and the GraphQL error codes and
/// (truncated) messages.
pub fn root_field(reply: &GraphqlReply, field: &str) -> Result<Value, CallFailure> {
    let status = reply.status;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(CallFailure::Auth);
    }
    let Ok(body) = serde_json::from_slice::<Value>(&reply.body) else {
        return Err(CallFailure::Other(format!(
            "status {status}, body is not JSON"
        )));
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
        return Err(CallFailure::Other(format!(
            "status {status}, GraphQL errors: {}",
            summary.join(" | ")
        )));
    }
    if !status.is_success() {
        return Err(CallFailure::Other(format!("status {status}")));
    }
    match body.pointer(&format!("/data/{field}")) {
        Some(Value::Null) | None => Err(CallFailure::Other(format!("no data.{field}"))),
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
/// Returns a message when the date arithmetic overflows.
pub fn spike_start(now: OffsetDateTime) -> Result<OffsetDateTime, String> {
    let earliest = now
        .to_offset(UtcOffset::UTC)
        .checked_add(SCHEDULE_LEAD)
        .ok_or("date overflow")?;
    let hour = earliest
        .replace_minute(0)
        .and_then(|t| t.replace_second(0))
        .and_then(|t| t.replace_nanosecond(0))
        .map_err(|e| format!("date: {e}"))?;
    if hour == earliest {
        Ok(hour)
    } else {
        hour.checked_add(Duration::HOUR)
            .ok_or_else(|| "date overflow".to_owned())
    }
}

fn iso(t: OffsetDateTime) -> Result<String, String> {
    t.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|e| format!("date format: {e}"))
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

/// Reads a schedule mutation's union result into the raw scheduled-class id
/// Peloton returned. The id is not decoded here: whatever Peloton issued is
/// what later calls must send back.
///
/// # Errors
///
/// Returns Peloton's error code and message, or a shape problem.
pub fn scheduled_result(value: &Value) -> Result<String, String> {
    let typename = value.get("__typename").and_then(Value::as_str);
    if typename == Some("ScheduledClass") {
        return value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "ScheduledClass without an id".to_owned());
    }
    let code = value.get("code").and_then(Value::as_str).unwrap_or("?");
    let message: String = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(MAX_MESSAGE)
        .collect();
    Err(format!(
        "{}: {code}: {message}",
        typename.unwrap_or("unknown result")
    ))
}

/// Checks that the raw scheduled-class id `raw` decodes to `ride` at `start`.
///
/// # Errors
///
/// Says whether the id does not decode or names another ride or time.
pub fn check_event(raw: &str, ride: &PelotonId, start: OffsetDateTime) -> Result<(), String> {
    let id: ScheduledClassId = raw
        .parse()
        .map_err(|_| "Peloton's scheduled-class id does not decode as an event id".to_owned())?;
    if id.ride_id() != ride {
        return Err("the scheduled class names a different ride".to_owned());
    }
    if id.scheduled_start_time() != start {
        return Err("the scheduled class has a different start time".to_owned());
    }
    Ok(())
}

/// Whether `list` holds the spike's item: the raw id itself, or any id that
/// decodes to `ride` at `start`.
#[must_use]
pub fn list_contains(
    list: &BTreeSet<String>,
    raw: &str,
    ride: &PelotonId,
    start: OffsetDateTime,
) -> bool {
    list.contains(raw) || list.iter().any(|id| check_event(id, ride, start).is_ok())
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
/// Returns a message when the shape is unexpected, when `numClasses`
/// disagrees with the list, or when an entry lacks a `playOrder` or a class
/// id that can be turned into a ride id (nothing is ever silently dropped).
pub fn stack_items(stack: &Value) -> Result<Vec<StackItem>, String> {
    let count = stack.get("numClasses").and_then(Value::as_i64);
    let Some(list) = stack.pointer("/userStack/stackedClassList") else {
        return match count {
            Some(0) => Ok(Vec::new()),
            _ => Err("Stack response without userStack.stackedClassList".to_owned()),
        };
    };
    let list = list.as_array().ok_or("stackedClassList is not a list")?;
    if let Some(count) = count
        && usize::try_from(count).ok() != Some(list.len())
    {
        return Err(format!(
            "numClasses is {count} but the list holds {} entries",
            list.len()
        ));
    }
    let mut items = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let order = entry
            .get("playOrder")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("Stack entry {index} has no playOrder"))?;
        let class = entry
            .get("pelotonClass")
            .ok_or_else(|| format!("Stack entry {index} has no pelotonClass"))?;
        let class_id = class
            .get("classId")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Stack entry {index} has no classId"))?;
        let ride = class_id
            .parse::<PelotonId>()
            .or_else(|_| {
                class_id
                    .parse::<OnDemandClassId>()
                    .map(|c| c.ride_id().clone())
            })
            .map_err(|_| {
                format!("Stack entry {index}: classId is neither a ride id nor a class id")
            })?;
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
pub fn restore_ids(saved: &[StackItem]) -> Result<Restore, String> {
    let mut restore = Restore {
        ids: Vec::with_capacity(saved.len()),
        encoding_mismatches: Vec::new(),
    };
    for (index, item) in saved.iter().enumerate() {
        let token = item.join_token.as_deref().ok_or_else(|| {
            format!(
                "Stack entry {} ({}): Peloton returned no joinToken",
                index + 1,
                item.ride
            )
        })?;
        let same_ride = token
            .parse::<OnDemandClassId>()
            .is_ok_and(|t| t.ride_id() == &item.ride);
        if !same_ride {
            return Err(format!(
                "Stack entry {} ({}): Peloton's joinToken is not an on-demand class id for this ride",
                index + 1,
                item.ride
            ));
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

    fn result(&mut self, step: &str, result: Result<String, String>) -> bool {
        match result {
            Ok(detail) => {
                self.record(step, Status::Pass, detail);
                true
            }
            Err(detail) => {
                self.record(step, Status::Fail, detail);
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
    ) -> Result<Value, CallFailure> {
        self.session.pause();
        let reply = self
            .session
            .graphql(self.access, op.name, op.kind, op.document, variables)
            .map_err(CallFailure::Other)?;
        let result = root_field(&reply, op.field);
        if let Err(e) = save(self.dir, file, &reply.body) {
            steps.record(&format!("save {file}"), Status::Fail, e);
        }
        result
    }

    /// Like [`Spike::call`], with failures as plain messages.
    fn call_msg(
        &self,
        steps: &mut Steps,
        op: &Op,
        variables: &Value,
        file: &str,
    ) -> Result<Value, String> {
        self.call(steps, op, variables, file)
            .map_err(|failure| match failure {
                CallFailure::Auth => "the gateway rejected the token".to_owned(),
                CallFailure::Other(message) => message,
            })
    }

    fn rest(&self, steps: &mut Steps, step: &str, path_and_query: &str, file: &str) {
        self.session.pause();
        let result = self
            .session
            .get(self.access, path_and_query)
            .and_then(|body| save(self.dir, file, &body).map(|()| format!("saved {file}")));
        steps.result(step, result);
    }

    fn list_schedule(
        &self,
        steps: &mut Steps,
        window: &Window,
        file: &str,
    ) -> Result<BTreeSet<String>, String> {
        self.call_msg(
            steps,
            &LIST,
            &json!({ "startTime": window.start, "endTime": window.end }),
            file,
        )
        .map(|list| schedule_ids(&list))
    }

    fn view_stack(&self, steps: &mut Steps, file: &str) -> Result<Vec<StackItem>, String> {
        let stack = self.call_msg(steps, &VIEW, &json!({}), file)?;
        stack_items(&stack)
    }
}

/// Logs in and runs every step; see the module docs.
///
/// # Errors
///
/// Returns a message when setup fails, when the gateway rejects the token,
/// or when any step failed.
pub fn run(session: &Session, dir: &Path, class: &PelotonId) -> Result<(), String> {
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
    introspect(&spike, &mut steps);
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
        Err("some spike steps failed; see above".to_owned())
    } else {
        Ok(())
    }
}

/// Step 1. Returns whether Peloton says the class is in the Stack, if known.
fn check_class(spike: &Spike<'_>, steps: &mut Steps) -> Result<Option<bool>, String> {
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
        Err(CallFailure::Auth) => {
            steps.record(step, Status::Fail, "GraphQL gateway rejected the token");
            Err("GraphQL gateway rejected the token; stopping before any other call".to_owned())
        }
        Err(CallFailure::Other(message)) => {
            steps.record(step, Status::Fail, message);
            Ok(None)
        }
    }
}

/// Step 2.
fn introspect(spike: &Spike<'_>, steps: &mut Steps) {
    let step = "schema introspection";
    match spike.call(steps, &SCHEMA, &json!({}), "graphql_schema.json") {
        Ok(_) => steps.record(step, Status::Pass, "saved graphql_schema.json"),
        Err(CallFailure::Auth) => steps.record(step, Status::Skip, "gateway refused introspection"),
        Err(CallFailure::Other(message)) => steps.record(
            step,
            Status::Skip,
            format!("gateway refused introspection ({message})"),
        ),
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

/// Step 4.
fn schedule_round_trip(spike: &Spike<'_>, steps: &mut Steps) -> Result<(), String> {
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
            steps.record("schedule list (before)", Status::Fail, e);
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

    let outcome = schedule_writes(spike, steps, &window, &plan)?;

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
            steps.record("schedule list (after)", Status::Fail, e);
            if outcome.wrote {
                eprintln!();
                eprintln!(
                    "!!! Could not re-read the schedule. Check it by hand for {}",
                    plan.describe()
                );
            }
        }
    }

    if let Some(raw) = outcome.left_over {
        print_left_over(&raw, &plan);
    }
    Ok(())
}

/// What the schedule writes left behind.
struct WriteOutcome {
    /// Peloton reported a scheduled class (so the account was changed).
    wrote: bool,
    /// The list showed the spike's item after the add.
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
) -> Result<WriteOutcome, String> {
    let start_iso = iso(plan.start)?;
    let added = spike
        .call_msg(
            steps,
            &ADD,
            &json!({ "id": spike.class.as_str(), "startTime": start_iso }),
            "schedule_add.json",
        )
        .and_then(|v| scheduled_result(&v));
    let first = match added {
        Ok(first) => first,
        Err(e) => {
            steps.record("schedule add", Status::Fail, e);
            steps.record(
                "schedule read, move and remove",
                Status::Skip,
                "Peloton did not report a scheduled class",
            );
            eprintln!("note: if the add reached Peloton anyway, the after-list check will show it");
            return Ok(WriteOutcome {
                wrote: false,
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
        Ok(list) if list_contains(&list, &first, plan.ride, plan.start) => {
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
                "the new item is not in the list",
            );
            false
        }
        Err(e) => {
            steps.record("schedule list (with spike item)", Status::Fail, e);
            false
        }
    };

    let read = spike
        .call_msg(
            steps,
            &READ,
            &json!({ "id": first }),
            "scheduled_class.json",
        )
        .and_then(|v| {
            if v.get("id").and_then(Value::as_str) == Some(first.as_str()) {
                Ok("read back the scheduled class".to_owned())
            } else {
                Err("the scheduled class read back has a different id".to_owned())
            }
        });
    steps.result("schedule read", read);

    let current = schedule_move(spike, steps, &first, plan)?;

    let removed = spike
        .call_msg(
            steps,
            &REMOVE,
            &json!({ "id": current }),
            "schedule_remove.json",
        )
        .and_then(|v| scheduled_result(&v).map(|_| "removed".to_owned()));
    let left_over = removed.is_err().then_some(current);
    steps.result("schedule remove", removed);
    Ok(WriteOutcome {
        wrote: true,
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
) -> Result<String, String> {
    let moved = spike
        .call_msg(
            steps,
            &MOVE,
            &json!({ "id": first, "startTime": iso(plan.moved)? }),
            "schedule_reschedule.json",
        )
        .and_then(|v| scheduled_result(&v));
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
            steps.record("schedule move (+1 h)", Status::Fail, e);
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

/// Step 5.
fn stack_round_trip(spike: &Spike<'_>, steps: &mut Steps, in_stack: Option<bool>) {
    let saved = match spike.view_stack(steps, "stack_before.json") {
        Ok(saved) => saved,
        Err(e) => {
            steps.record("Stack read (before)", Status::Fail, e);
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
            steps.record("Stack restore ids", Status::Fail, e);
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
        .call_msg(
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
                Err("the Stack differs from the saved one".to_owned())
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
        .call_msg(
            steps,
            &STACK_ADD,
            &json!({ "input": { "pelotonClassId": spike.class.as_str() } }),
            "stack_add.json",
        )
        .and_then(|v| stack_items(&v))
        .and_then(|items| {
            if items.last().map(|i| &i.ride) == Some(ride) && items.len() == saved_len + 1 {
                Ok("the class was appended".to_owned())
            } else {
                Err("the Stack after the add is not the saved Stack plus the class".to_owned())
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
        CallFailure, Restore, StackItem, check_event, json_type, list_contains, restore_ids,
        root_field, schedule_ids, scheduled_result, spike_start, stack_items,
    };
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
        let Err(CallFailure::Other(message)) = root_field(&reply(400, &invalid), "x") else {
            panic!("expected a failure");
        };
        assert!(message.contains("GRAPHQL_VALIDATION_FAILED: Cannot query field"));
        assert!(matches!(
            root_field(&reply(200, &json!({"data": {"x": null}})), "x"),
            Err(CallFailure::Other(_))
        ));
        assert!(matches!(
            root_field(&reply(500, &json!({"data": {"x": 1}})), "x"),
            Err(CallFailure::Other(_))
        ));
        let not_json = GraphqlReply {
            status: reqwest::StatusCode::OK,
            body: b"<html>".to_vec(),
        };
        assert!(matches!(
            root_field(&not_json, "x"),
            Err(CallFailure::Other(_))
        ));
    }

    #[test]
    fn spike_start_is_next_whole_hour_at_least_13_days_ahead() {
        assert_eq!(
            spike_start(datetime!(2026-10-25 10:20:30 UTC)),
            Ok(datetime!(2026-11-07 11:00 UTC))
        );
        assert_eq!(
            spike_start(datetime!(2026-10-25 10:00 UTC)),
            Ok(datetime!(2026-11-07 10:00 UTC))
        );
        assert_eq!(
            spike_start(datetime!(2026-10-25 23:59:59.5 UTC)),
            Ok(datetime!(2026-11-08 00:00 UTC))
        );
        assert_eq!(
            spike_start(datetime!(2026-10-25 08:30 -07:00)),
            Ok(datetime!(2026-11-07 16:00 UTC))
        );
    }

    #[test]
    fn schedule_ids_finds_ids_in_any_shape() {
        let flat = json!([{"__typename": "ScheduledClass", "id": "a"}, {"id": "b"}]);
        let wrapped = json!({"items": [{"id": "a"}, {"nested": {"id": "b"}}], "count": 2});
        assert_eq!(schedule_ids(&flat), schedule_ids(&wrapped));
        assert_eq!(schedule_ids(&flat).len(), 2);
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
            scheduled_result(&err),
            Err("CannotScheduleClassError: X: too far".to_owned())
        );
        assert!(scheduled_result(&json!({"__typename": "ScheduledClass"})).is_err());
    }

    #[test]
    fn check_event_compares_ride_and_time() {
        let start = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
        let id = event(RIDE, 1_700_000_000);
        assert_eq!(check_event(&id, &ride(RIDE), start), Ok(()));
        assert!(check_event(&id, &ride(OTHER), start).is_err());
        assert!(check_event(&id, &ride(RIDE), start + time::Duration::HOUR).is_err());
        assert!(check_event("opaque", &ride(RIDE), start).is_err());
    }

    #[test]
    fn list_contains_matches_raw_or_decoded_ids() {
        let start = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time");
        let ours = event(RIDE, 1_700_000_000);
        let list: std::collections::BTreeSet<String> =
            ["opaque".to_owned(), ours.clone()].into_iter().collect();
        assert!(list_contains(&list, "opaque", &ride(OTHER), start));
        assert!(list_contains(&list, "different-raw", &ride(RIDE), start));
        let others: std::collections::BTreeSet<String> =
            [event(OTHER, 1_700_000_000), event(RIDE, 1_700_003_600)]
                .into_iter()
                .collect();
        assert!(!list_contains(&others, &ours, &ride(RIDE), start));
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
        let stack = json!({"userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": encoded.as_str(), "title": "A"}},
        ]}});
        assert_eq!(stack_items(&stack).expect("items")[0].ride, ride(RIDE));
        assert_eq!(stack_items(&json!({"numClasses": 0})), Ok(Vec::new()));
    }

    #[test]
    fn stack_items_refuses_undecodable_entries() {
        let stack = json!({"userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": "../x", "title": "A"}},
        ]}});
        assert!(stack_items(&stack).is_err());
        assert!(stack_items(&json!({"numClasses": 3})).is_err());
    }

    #[test]
    fn stack_items_refuses_partial_lists() {
        let stack = json!({"numClasses": 2, "userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A"}},
        ]}});
        let err = stack_items(&stack).expect_err("partial list");
        assert!(err.contains("numClasses is 2"));
    }

    #[test]
    fn stack_items_refuses_missing_play_order() {
        let stack = json!({"userStack": {"stackedClassList": [
            {"playOrder": 1, "pelotonClass": {"classId": RIDE, "title": "A"}},
            {"pelotonClass": {"classId": OTHER, "title": "B"}},
        ]}});
        let err = stack_items(&stack).expect_err("no playOrder");
        assert!(err.contains("playOrder"));
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
        assert!(restore_ids(&[item(None)]).is_err());
        let other = OnDemandClassId::from_ride(&ride(OTHER));
        assert!(restore_ids(&[item(Some(other.as_str()))]).is_err());
        assert!(restore_ids(&[item(Some("opaque"))]).is_err());
        assert_eq!(
            restore_ids(&[]),
            Ok(Restore {
                ids: Vec::new(),
                encoding_mismatches: Vec::new(),
            })
        );
    }

    #[test]
    fn json_type_names_without_echoing() {
        assert_eq!(json_type(&json!({"secret": "x"})), "object");
        assert_eq!(json_type(&json!(true)), "boolean");
        assert_eq!(json_type(&json!(null)), "null");
    }
}
