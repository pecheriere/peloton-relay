//! `spike`: prove the GraphQL schedule and Stack operations against the
//! owner's account, saving every raw answer, and leave the account as it was.
//!
//! Steps, each reported PASS, FAIL or SKIP:
//! 1. `CheckDoesClassExistInUserStack` with a locally encoded class id. A
//!    rejected token stops the spike before anything else is sent.
//! 2. Schema introspection, saved when the gateway allows it.
//! 3. The three library reads (REST), saved.
//! 4. Schedule: list, add at the first whole hour at least 13 days ahead, read
//!    it, move it one hour, remove it, and check the list is as before.
//! 5. Stack: read and print it, add the class, write the saved Stack back with
//!    `ModifyStack`, and check the read-back order is identical.

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

/// Reads a schedule mutation's union result into the scheduled-class id.
///
/// # Errors
///
/// Returns Peloton's error code and message, or a shape problem.
pub fn scheduled_result(value: &Value) -> Result<ScheduledClassId, String> {
    let typename = value.get("__typename").and_then(Value::as_str);
    if typename == Some("ScheduledClass") {
        return value
            .get("id")
            .and_then(Value::as_str)
            .ok_or("ScheduledClass without an id")?
            .parse()
            .map_err(|_| "ScheduledClass id does not decode as an event id".to_owned());
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
/// Returns a message when the shape is unexpected or an entry's class id
/// cannot be turned into a ride id (nothing is ever silently dropped).
pub fn stack_items(stack: &Value) -> Result<Vec<StackItem>, String> {
    let Some(list) = stack.pointer("/userStack/stackedClassList") else {
        return match stack.get("numClasses").and_then(Value::as_i64) {
            Some(0) => Ok(Vec::new()),
            _ => Err("Stack response without userStack.stackedClassList".to_owned()),
        };
    };
    let list = list.as_array().ok_or("stackedClassList is not a list")?;
    let mut items = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
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
        let order = entry.get("playOrder").and_then(Value::as_i64);
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

/// The class ids that write `saved` back, proven equal to Peloton's own.
///
/// # Errors
///
/// Refuses (naming the entry) when Peloton's `joinToken` for an entry is
/// missing or differs from the local encoding, so a restore never sends an
/// id Peloton did not issue.
pub fn restore_ids(saved: &[StackItem]) -> Result<Vec<String>, String> {
    saved
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let local = OnDemandClassId::from_ride(&item.ride);
            match item.join_token.as_deref() {
                Some(token) if token == local.as_str() => Ok(local.as_str().to_owned()),
                Some(token) => {
                    let same_ride = token
                        .parse::<OnDemandClassId>()
                        .is_ok_and(|t| t.ride_id() == &item.ride);
                    Err(format!(
                        "Stack entry {index} ({}): Peloton's joinToken differs from the local encoding ({})",
                        item.ride,
                        if same_ride {
                            "same ride, different bytes"
                        } else {
                            "not an on-demand class id for this ride"
                        }
                    ))
                }
                None => Err(format!(
                    "Stack entry {index} ({}): Peloton returned no joinToken",
                    item.ride
                )),
            }
        })
        .collect()
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
    /// Sends one operation, saves the raw answer as `file` and returns
    /// `data.<field>`.
    fn call(
        &self,
        op: &str,
        kind: OpKind,
        document: &str,
        variables: &Value,
        field: &str,
        file: &str,
    ) -> Result<Value, CallFailure> {
        self.session.pause();
        let reply = self
            .session
            .graphql(self.access, op, kind, document, variables)
            .map_err(CallFailure::Other)?;
        save(self.dir, file, &reply.body).map_err(CallFailure::Other)?;
        root_field(&reply, field)
    }

    /// Like [`Spike::call`], with failures as plain messages.
    fn call_msg(
        &self,
        op: &str,
        kind: OpKind,
        document: &str,
        variables: &Value,
        field: &str,
        file: &str,
    ) -> Result<Value, String> {
        self.call(op, kind, document, variables, field, file)
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
        start: &str,
        end: &str,
        file: &str,
    ) -> Result<BTreeSet<String>, String> {
        self.call_msg(
            "YourScheduleItems",
            OpKind::Query,
            YOUR_SCHEDULE_ITEMS,
            &json!({ "startTime": start, "endTime": end }),
            "userScheduledItemsList",
            file,
        )
        .map(|list| schedule_ids(&list))
    }

    fn view_stack(&self, file: &str) -> Result<Vec<StackItem>, String> {
        let stack = self.call_msg(
            "ViewUserStack",
            OpKind::Query,
            VIEW_USER_STACK,
            &json!({}),
            "viewUserStack",
            file,
        )?;
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
        "CheckDoesClassExistInUserStack",
        OpKind::Query,
        CHECK_CLASS_IN_STACK,
        &json!({ "pelotonClassId": spike.class.as_str() }),
        "checkDoesClassExistInUserStack",
        "check_class_in_stack.json",
    ) {
        Ok(value) => {
            steps.record(step, Status::Pass, format!("Peloton answered {value}"));
            Ok(value.as_bool())
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
    match spike.call(
        "IntrospectionQuery",
        OpKind::Query,
        INTROSPECTION,
        &json!({}),
        "__schema",
        "graphql_schema.json",
    ) {
        Ok(_) => steps.record(step, Status::Pass, "saved graphql_schema.json"),
        Err(CallFailure::Auth) => steps.record(step, Status::Skip, "gateway refused introspection"),
        Err(CallFailure::Other(message)) => steps.record(
            step,
            Status::Skip,
            format!("gateway refused introspection ({message})"),
        ),
    }
}

/// Step 4.
fn schedule_round_trip(spike: &Spike<'_>, steps: &mut Steps) -> Result<(), String> {
    let now = OffsetDateTime::now_utc();
    let window_start = iso(now)?;
    let window_end = iso(now + SCHEDULE_WINDOW)?;

    let before = spike.list_schedule(&window_start, &window_end, "schedule_before.json");
    let Ok(before) = before.map_err(|e| steps.record("schedule list (before)", Status::Fail, e))
    else {
        steps.record(
            "schedule writes",
            Status::Skip,
            "cannot verify cleanup without the list",
        );
        return Ok(());
    };
    steps.record(
        "schedule list (before)",
        Status::Pass,
        format!("{} item(s) in the next 14 days", before.len()),
    );

    let left_over = schedule_writes(spike, steps, spike_start(now)?)?;

    let after = spike
        .list_schedule(&window_start, &window_end, "schedule_after.json")
        .and_then(|after| {
            if after == before {
                Ok("the schedule is back to its pre-spike items".to_owned())
            } else {
                for extra in after.difference(&before) {
                    print_left_over(extra);
                }
                Err(format!(
                    "the schedule differs from before: {} added, {} missing",
                    after.difference(&before).count(),
                    before.difference(&after).count()
                ))
            }
        });
    steps.result("schedule list (after)", after);

    if let Some(id) = left_over {
        print_left_over(id.as_str());
    }
    Ok(())
}

/// Adds the class at `start`, reads it, moves it one hour and removes it.
/// Returns the scheduled class still on the account if removal failed.
fn schedule_writes(
    spike: &Spike<'_>,
    steps: &mut Steps,
    start: OffsetDateTime,
) -> Result<Option<ScheduledClassId>, String> {
    let moved = start + Duration::HOUR;
    let start_iso = iso(start)?;
    let moved_iso = iso(moved)?;
    let ride = spike.class.ride_id();

    let added = spike
        .call_msg(
            "AddClassToSchedule",
            OpKind::Mutation,
            ADD_CLASS_TO_SCHEDULE,
            &json!({ "id": spike.class.as_str(), "startTime": start_iso }),
            "addClassToSchedule",
            "schedule_add.json",
        )
        .and_then(|v| scheduled_result(&v));
    let Some(first) = added.as_ref().ok().cloned() else {
        steps.result("schedule add", added.map(|_| String::new()));
        steps.record(
            "schedule read, move and remove",
            Status::Skip,
            "nothing was scheduled",
        );
        return Ok(None);
    };
    steps.result(
        "schedule add",
        expect_event(&first, ride, start).map(|()| format!("scheduled at {start_iso}")),
    );

    let read = spike
        .call_msg(
            "ScheduledClass",
            OpKind::Query,
            SCHEDULED_CLASS,
            &json!({ "id": first.as_str() }),
            "scheduledClass",
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

    let rescheduled = spike
        .call_msg(
            "RescheduleClass",
            OpKind::Mutation,
            RESCHEDULE_CLASS,
            &json!({ "id": first.as_str(), "startTime": moved_iso }),
            "rescheduleClass",
            "schedule_reschedule.json",
        )
        .and_then(|v| scheduled_result(&v));
    let current = rescheduled.as_ref().map_or(first.clone(), Clone::clone);
    steps.result(
        "schedule move (+1 h)",
        rescheduled.and_then(|new_id| {
            expect_event(&new_id, ride, moved)?;
            Ok(if new_id == first {
                "moved; Peloton kept the same id".to_owned()
            } else {
                "moved; Peloton issued a new id".to_owned()
            })
        }),
    );

    let removed = spike
        .call_msg(
            "RemoveClassFromSchedule",
            OpKind::Mutation,
            REMOVE_CLASS_FROM_SCHEDULE,
            &json!({ "id": current.as_str() }),
            "removeClassFromSchedule",
            "schedule_remove.json",
        )
        .and_then(|v| scheduled_result(&v).map(|_| "removed".to_owned()));
    let left_over = removed.is_err().then_some(current);
    steps.result("schedule remove", removed);
    Ok(left_over)
}

fn expect_event(
    id: &ScheduledClassId,
    ride: &PelotonId,
    start: OffsetDateTime,
) -> Result<(), String> {
    if id.ride_id() != ride {
        return Err("the scheduled class names a different ride".to_owned());
    }
    if id.scheduled_start_time() != start {
        return Err("the scheduled class has a different start time".to_owned());
    }
    Ok(())
}

fn print_left_over(id: &str) {
    let what = id.parse::<ScheduledClassId>().map_or_else(
        |_| "an item that does not decode as a scheduled class".to_owned(),
        |e| format!("ride {} at {} UTC", e.ride_id(), e.scheduled_start_time()),
    );
    eprintln!();
    eprintln!("!!! Remove this from your Peloton schedule by hand: {what}");
}

/// Step 5.
fn stack_round_trip(spike: &Spike<'_>, steps: &mut Steps, in_stack: Option<bool>) {
    let saved = match spike.view_stack("stack_before.json") {
        Ok(saved) => saved,
        Err(e) => {
            steps.record("Stack read (before)", Status::Fail, e);
            steps.record(
                "Stack writes",
                Status::Skip,
                "the current Stack could not be saved",
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
            steps.record("Stack encoding matches Peloton's", Status::Fail, e);
            steps.record(
                "Stack writes",
                Status::Skip,
                "not writing ids Peloton did not issue",
            );
            return;
        }
    };
    if !saved.is_empty() {
        steps.record(
            "Stack encoding matches Peloton's",
            Status::Pass,
            "every joinToken equals the local encoding",
        );
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
            "ModifyStack",
            OpKind::Mutation,
            MODIFY_STACK,
            &json!({ "input": { "pelotonClassIdList": restore } }),
            "modifyStack",
            "stack_modify.json",
        )
        .map(|_| "wrote the saved Stack back".to_owned());
    ok &= steps.result("Stack restore", modified);

    let verified = spike.view_stack("stack_after.json").and_then(|after| {
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
            "AddClassToStack",
            OpKind::Mutation,
            ADD_CLASS_TO_STACK,
            &json!({ "input": { "pelotonClassId": spike.class.as_str() } }),
            "addClassToStack",
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
        CallFailure, StackItem, restore_ids, root_field, schedule_ids, scheduled_result,
        spike_start, stack_items,
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
    fn scheduled_result_reads_class_or_error() {
        let id = event(RIDE, 1_700_000_000);
        let ok = json!({"__typename": "ScheduledClass", "id": id});
        assert_eq!(scheduled_result(&ok).expect("id").ride_id(), &ride(RIDE));
        let err =
            json!({"__typename": "CannotScheduleClassError", "code": "X", "message": "too far"});
        assert_eq!(
            scheduled_result(&err),
            Err("CannotScheduleClassError: X: too far".to_owned())
        );
        let bad = json!({"__typename": "ScheduledClass", "id": "nope"});
        assert!(scheduled_result(&bad).is_err());
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
    fn restore_ids_require_peloton_issued_tokens() {
        let local = OnDemandClassId::from_ride(&ride(RIDE));
        let item = |token: Option<&str>| StackItem {
            ride: ride(RIDE),
            title: "A".to_owned(),
            join_token: token.map(str::to_owned),
        };
        assert_eq!(
            restore_ids(&[item(Some(local.as_str()))]),
            Ok(vec![local.as_str().to_owned()])
        );
        assert!(restore_ids(&[item(None)]).is_err());
        let other = OnDemandClassId::from_ride(&ride(OTHER));
        assert!(restore_ids(&[item(Some(other.as_str()))]).is_err());
        assert_eq!(restore_ids(&[]), Ok(Vec::new()));
    }
}
