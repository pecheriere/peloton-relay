//! Decoding of the library, schedule and Stack responses, from the scrubbed
//! spike fixtures and from synthetic union members.

use peloton::ids::OnDemandClassId;
use peloton::upstream::{
    BrowseCategories, LibraryPage, RideFilters, ScheduleItem, ScheduleList, ScheduleResult,
    ScheduledClass, StackResponse,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Where the scrubber puts the earliest scheduled start of a run.
const SYNTHETIC_START: &str = "2030-01-07T12:00:00Z";

const SCHEDULED_ID: &str = "eyJob21lX3BlbG90b25faWQiOiAiMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwZjEiLCAicmlkZV9pZCI6ICIwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBmMiIsICJzY2hlZHVsZWRfc3RhcnRfdGltZSI6IDE4OTQwMTc2MDAsICJzdHVkaW9fcGVsb3Rvbl9pZCI6IG51bGwsICJ0eXBlIjogImV2ZW50In0=";

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap()
}

/// The root field `field` of the GraphQL fixture `name`.
fn root(name: &str, field: &str) -> Value {
    let mut body = fixture(name);
    body["data"][field].take()
}

fn at(text: &str) -> OffsetDateTime {
    OffsetDateTime::parse(text, &Rfc3339).unwrap()
}

/// Checks that a scheduled class is internally consistent: its id names its
/// class and start time, and its join token is its id.
fn check_scheduled(class: &ScheduledClass) {
    let start = class.scheduled_start_time.unwrap();
    assert_eq!(class.id.scheduled_start_time(), start);
    assert!(class.scheduled_end_time.unwrap() > start);
    let summary = class.peloton_class.as_ref().unwrap();
    assert_eq!(summary.class_id.as_ref(), Some(class.id.ride_id()));
    assert_eq!(summary.join_token.as_deref(), Some(class.id.as_str()));
    assert_eq!(summary.typename.as_deref(), Some("LiveClass"));
    assert_eq!(summary.duration, Some(1800));
    assert!(summary.title.is_some());
    assert!(summary.instructor.as_ref().unwrap().name.is_some());
    assert!(summary.air_time.is_some());
    let discipline = summary.fitness_discipline.as_ref().unwrap();
    assert!(discipline.slug.is_some() && discipline.display_name.is_some());
    assert!(summary.difficulty_level.is_none(), "null in the capture");
}

fn scheduled(result: ScheduleResult) -> ScheduledClass {
    match result {
        ScheduleResult::ScheduledClass(class) => *class,
        other => panic!("not a scheduled class: {other:?}"),
    }
}

#[test]
fn schedule_lists_decode() {
    for name in ["schedule_before.json", "schedule_after.json"] {
        let list: ScheduleList =
            serde_json::from_value(root(name, "userScheduledItemsList")).unwrap();
        let ScheduleList::YourScheduleItemList(items) = list;
        assert!(items.your_schedule_items.is_empty(), "{name}");
    }
    let list: ScheduleList =
        serde_json::from_value(root("schedule_during.json", "userScheduledItemsList")).unwrap();
    let ScheduleList::YourScheduleItemList(items) = list;
    let [ScheduleItem::ScheduledClass(class)] = items.your_schedule_items.as_slice() else {
        panic!("one scheduled class expected");
    };
    check_scheduled(class);
    assert_eq!(class.scheduled_start_time, Some(at(SYNTHETIC_START)));
}

#[test]
fn scheduled_class_and_schedule_mutations_decode() {
    let read =
        scheduled(serde_json::from_value(root("scheduled_class.json", "scheduledClass")).unwrap());
    let added =
        scheduled(serde_json::from_value(root("schedule_add.json", "addClassToSchedule")).unwrap());
    let moved = scheduled(
        serde_json::from_value(root("schedule_reschedule.json", "rescheduleClass")).unwrap(),
    );
    for class in [&read, &added, &moved] {
        check_scheduled(class);
    }
    assert_eq!(read.id, added.id, "the same class maps to the same fake id");
    assert_ne!(moved.id, added.id, "rescheduling issues a new id");
    assert_eq!(moved.id.ride_id(), added.id.ride_id());
    assert_eq!(
        moved.id.scheduled_start_time() - added.id.scheduled_start_time(),
        time::Duration::hours(1),
        "relative times are kept"
    );

    let removed = scheduled(
        serde_json::from_value(root("schedule_remove.json", "removeClassFromSchedule")).unwrap(),
    );
    assert_eq!(removed.id, moved.id);
    assert!(removed.peloton_class.is_none(), "only the id is selected");
    assert!(removed.scheduled_start_time.is_none());
}

#[test]
fn stack_responses_decode() {
    for (name, field) in [
        ("stack_before.json", "viewUserStack"),
        ("stack_after.json", "viewUserStack"),
        ("stack_modify.json", "modifyStack"),
    ] {
        let StackResponse::StackResponseSuccess(stack) =
            serde_json::from_value(root(name, field)).unwrap();
        assert_eq!(stack.num_classes, Some(0), "{name}");
        assert_eq!(stack.total_time, Some(0), "{name}");
        assert!(stack.user_stack.stacked_class_list.is_empty(), "{name}");
    }

    let StackResponse::StackResponseSuccess(stack) =
        serde_json::from_value(root("stack_add.json", "addClassToStack")).unwrap();
    assert_eq!(stack.num_classes, Some(1));
    assert_eq!(stack.total_time, Some(1800));
    let [item] = stack.user_stack.stacked_class_list.as_slice() else {
        panic!("one stacked class expected");
    };
    assert_eq!(item.play_order, Some(0));
    let class = item.peloton_class.as_ref().unwrap();
    assert_eq!(class.typename.as_deref(), Some("OnDemandInstructorClass"));
    let ride = class.class_id.as_ref().unwrap();
    let token: OnDemandClassId = class.join_token.as_deref().unwrap().parse().unwrap();
    assert_eq!(token.ride_id(), ride);
    assert_eq!(
        token,
        OnDemandClassId::from_ride(ride),
        "Peloton's own layout"
    );
}

#[test]
fn check_class_in_stack_is_a_bare_boolean() {
    let found: bool = serde_json::from_value(root(
        "check_class_in_stack.json",
        "checkDoesClassExistInUserStack",
    ))
    .unwrap();
    assert!(!found);
}

#[test]
fn library_page_decodes() {
    let page: LibraryPage = serde_json::from_value(fixture("library_archived.json")).unwrap();
    assert!(!page.data.is_empty());
    assert!(page.page_count >= 1 && page.total >= 1);
    for class in &page.data {
        assert!(class.title.is_some());
        assert!(class.duration.is_some());
        assert!(class.original_air_time.is_some());
        assert_eq!(class.total_user_workouts, Some(0));
        let instructor = class.instructor_id.as_ref().unwrap();
        assert!(
            page.instructors.iter().any(|i| &i.id == instructor),
            "joins an instructor"
        );
        assert!(!class.class_type_ids.is_empty());
        for class_type in &class.class_type_ids {
            assert!(
                page.class_types.iter().any(|t| &t.id == class_type),
                "joins a class type"
            );
        }
    }
    assert!(page.class_types.iter().all(|t| t.name.is_some()));
}

#[test]
fn ride_filters_decode() {
    let filters: RideFilters = serde_json::from_value(fixture("ride_filters.json")).unwrap();
    let names: Vec<&str> = filters
        .filters
        .iter()
        .filter_map(|f| f.name.as_deref())
        .collect();
    for name in ["class_type_id", "instructor_id", "duration"] {
        assert!(names.contains(&name), "{name}");
    }
    let duration = filters
        .filters
        .iter()
        .find(|f| f.name.as_deref() == Some("duration"))
        .unwrap();
    assert_eq!(duration.kind.as_deref(), Some("collection"));
    assert!(
        duration
            .values
            .iter()
            .all(|v| v.value.is_some() && v.display_name.is_some())
    );
    let sort = filters.sorts.first().unwrap();
    assert!(sort.slug.is_some());
    let value = sort.value.as_ref().unwrap();
    assert!(value.sort.is_some() && value.desc.is_some());
}

#[test]
fn browse_categories_decode() {
    let categories: BrowseCategories =
        serde_json::from_value(fixture("browse_categories.json")).unwrap();
    let slugs: Vec<&str> = categories
        .browse_categories
        .iter()
        .filter_map(|c| c.slug.as_deref())
        .collect();
    assert!(slugs.contains(&"cycling"));
    assert!(
        categories
            .browse_categories
            .iter()
            .all(|c| c.name.is_some())
    );
}

fn class_summary() -> Value {
    json!({
        "__typename": "LiveClass",
        "classId": "000000000000000000000000000000f2",
        "joinToken": SCHEDULED_ID,
        "title": "30 min Synthetic Ride",
        "duration": 1800,
        "fitnessDiscipline": {"slug": "CYCLING", "displayName": "Cycling"},
        "airTime": "2026-10-26T17:00:00.000Z",
        "difficultyLevel": {"slug": "intermediate", "displayName": "Intermediate"},
        "instructor": {"name": "Synthetic Instructor"}
    })
}

#[test]
fn schedule_result_dispatches_on_typename() {
    let class = scheduled(
        serde_json::from_value(json!({
            "__typename": "ScheduledClass",
            "id": SCHEDULED_ID,
            "scheduledStartTime": SYNTHETIC_START,
            "scheduledEndTime": "2030-01-07T12:30:00Z",
            "pelotonClass": class_summary(),
        }))
        .unwrap(),
    );
    check_scheduled_difficulty(&class);

    let missing: ScheduleResult = serde_json::from_value(json!({
        "__typename": "ScheduledClassDoesNotExistError",
        "code": "SCHEDULED_CLASS_DOES_NOT_EXIST",
        "message": "gone"
    }))
    .unwrap();
    let ScheduleResult::ScheduledClassDoesNotExistError(error) = missing else {
        panic!("expected does-not-exist: {missing:?}");
    };
    assert_eq!(
        error.code.as_deref(),
        Some("SCHEDULED_CLASS_DOES_NOT_EXIST")
    );

    let refused: ScheduleResult = serde_json::from_value(json!({
        "__typename": "CannotScheduleClassError",
        "code": "CANNOT_SCHEDULE",
        "message": "Too far ahead"
    }))
    .unwrap();
    let ScheduleResult::CannotScheduleClassError(error) = refused else {
        panic!("expected cannot-schedule: {refused:?}");
    };
    assert_eq!(error.message.as_deref(), Some("Too far ahead"));

    let bare: ScheduleResult =
        serde_json::from_value(json!({"__typename": "CannotScheduleClassError"})).unwrap();
    let ScheduleResult::CannotScheduleClassError(error) = bare else {
        panic!("expected cannot-schedule: {bare:?}");
    };
    assert!(error.code.is_none() && error.message.is_none());
}

fn check_scheduled_difficulty(class: &ScheduledClass) {
    let level = class
        .peloton_class
        .as_ref()
        .and_then(|c| c.difficulty_level.as_ref())
        .unwrap();
    assert_eq!(level.slug.as_deref(), Some("intermediate"));
    assert_eq!(level.display_name.as_deref(), Some("Intermediate"));
}

#[test]
fn schedule_result_rejects_unknown_members_and_bad_ids() {
    for odd in [
        json!({"__typename": "InviteFriendsError", "message": "no"}),
        json!({"code": "X", "message": "no typename"}),
        json!({"__typename": "ScheduledClass", "id": "not-base64"}),
        json!({"__typename": "ScheduledClass"}),
        json!({
            "__typename": "ScheduledClass",
            "id": OnDemandClassId::from_ride(&"000000000000000000000000000000f2".parse().unwrap()).as_str(),
        }),
        json!({
            "__typename": "ScheduledClass",
            "id": SCHEDULED_ID,
            "scheduledStartTime": "next Monday",
        }),
    ] {
        assert!(
            serde_json::from_value::<ScheduleResult>(odd.clone()).is_err(),
            "{odd}"
        );
    }
}

#[test]
fn schedule_list_keeps_other_items_and_rejects_other_roots() {
    let list: ScheduleList = serde_json::from_value(json!({
        "__typename": "YourScheduleItemList",
        "yourScheduleItems": [
            {"__typename": "InviteDetails", "createdAt": "2030-01-01T00:00:00Z"},
            {"__typename": "ScheduledClass", "id": SCHEDULED_ID, "pelotonClass": class_summary()}
        ]
    }))
    .unwrap();
    let ScheduleList::YourScheduleItemList(items) = list;
    let [
        ScheduleItem::Other { typename },
        ScheduleItem::ScheduledClass(_),
    ] = items.your_schedule_items.as_slice()
    else {
        panic!("an invite, then a class: {items:?}");
    };
    assert_eq!(typename, "InviteDetails", "the kind is kept");

    for odd in [
        json!({"__typename": "InviteFriendsError", "message": "no"}),
        json!({"__typename": "YourScheduleItemList"}),
        json!({"__typename": "YourScheduleItemList", "yourScheduleItems": null}),
        json!({"__typename": "YourScheduleItemList", "yourScheduleItems": [{"id": SCHEDULED_ID}]}),
        json!({"__typename": "YourScheduleItemList", "yourScheduleItems": [{"__typename": 7}]}),
        json!({"__typename": "YourScheduleItemList", "yourScheduleItems": ["ScheduledClass"]}),
        json!({"__typename": "YourScheduleItemList", "yourScheduleItems": [
            {"__typename": "ScheduledClass", "id": "not-base64"}
        ]}),
    ] {
        assert!(
            serde_json::from_value::<ScheduleList>(odd.clone()).is_err(),
            "{odd}"
        );
    }
}

#[test]
fn stack_requires_its_list_but_keeps_odd_items() {
    let StackResponse::StackResponseSuccess(stack) = serde_json::from_value(json!({
        "__typename": "StackResponseSuccess",
        "numClasses": 2,
        "totalTime": 2400,
        "userStack": {"stackedClassList": [
            {"playOrder": 0, "pelotonClass": {"__typename": "OnDemandInstructorClass", "classId": "../me", "joinToken": "odd"}},
            {"playOrder": 1, "pelotonClass": null}
        ]}
    }))
    .unwrap();
    let [first, second] = stack.user_stack.stacked_class_list.as_slice() else {
        panic!("two items expected");
    };
    let odd = first.peloton_class.as_ref().unwrap();
    assert!(odd.class_id.is_none(), "malformed class id reads as absent");
    assert_eq!(odd.join_token.as_deref(), Some("odd"));
    assert!(second.peloton_class.is_none());

    for odd in [
        json!({"__typename": "StackResponseSuccess", "numClasses": 0}),
        json!({"__typename": "StackResponseSuccess", "userStack": null}),
        json!({"__typename": "StackResponseSuccess", "userStack": {}}),
        json!({"__typename": "StackResponseSuccess", "userStack": {"stackedClassList": null}}),
        json!({"__typename": "StackResponseError", "userStack": {"stackedClassList": []}}),
    ] {
        assert!(
            serde_json::from_value::<StackResponse>(odd.clone()).is_err(),
            "{odd}"
        );
    }
}

#[test]
fn library_page_ignores_the_unverified_difficulty_level() {
    for level in [json!("advanced"), json!(3), json!({"slug": "advanced"})] {
        let page: LibraryPage = serde_json::from_value(json!({
            "data": [{
                "id": "000000000000000000000000000000f2",
                "difficulty_estimate": 7.5,
                "difficulty_level": level,
            }],
            "page": 0,
            "page_count": 1,
            "total": 1,
        }))
        .unwrap();
        assert_eq!(page.data[0].difficulty_estimate, Some(7.5));
    }
}
