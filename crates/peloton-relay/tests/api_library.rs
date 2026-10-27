//! Tests for the class library routes and the `OpenAPI` document's
//! description of them, driving the router against a mock Peloton.

use axum::http::StatusCode;
use peloton::upstream as wire;
use peloton_relay::api::openapi;
use peloton_relay::model;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::macros::datetime;
use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod common;

#[allow(
    dead_code,
    reason = "shared by several test crates, each using a subset of the helpers"
)]
mod api_common;

use api_common::{assert_error, bare, fixture, fixture_json, send};

const NOW: OffsetDateTime = datetime!(2030-01-01 00:00 UTC);
const CLASS_TYPE: &str = "00000000000000000000000000000f10";
const INSTRUCTOR: &str = "00000000000000000000000000000f15";

/// Every browse category slug of `browse_categories.json`.
const CATEGORIES: [&str; 15] = [
    "bootcamp",
    "caesar",
    "bike_bootcamp",
    "caesar_bootcamp",
    "walking",
    "hiking",
    "cycling",
    "cardio",
    "strength",
    "meditation",
    "outdoor",
    "running",
    "pilates",
    "stretching",
    "yoga",
];

fn parse(body: &str) -> Value {
    serde_json::from_str(body).unwrap()
}

fn expected<T: serde::Serialize>(value: &T) -> Value {
    parse(&serde_json::to_string(value).unwrap())
}

async fn get(server: &MockServer, uri: &str) -> (StatusCode, String) {
    let dir = tempfile::tempdir().unwrap();
    let app = api_common::relay(&dir, server, true, NOW);
    let (status, _, body) = send(&app, bare("GET", uri)).await;
    (status, body)
}

async fn upstream_hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn library_page() -> wire::LibraryPage {
    serde_json::from_str(&fixture("library_archived.json")).unwrap()
}

#[tokio::test]
async fn library_maps_page_and_forwards_every_filter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .and(query_param("browse_category", "bike_bootcamp"))
        .and(query_param("class_type_id", CLASS_TYPE))
        .and(query_param("instructor_id", INSTRUCTOR))
        .and(query_param("duration", "1200"))
        .and(query_param("sort_by", "top_rated"))
        .and(query_param("desc", "false"))
        .and(query_param("page", "2"))
        .and(query_param("limit", "7"))
        .and(header("Peloton-Platform", "web"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("library_archived.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (status, body) = get(
        &server,
        &format!(
            "/v1/library?category=bike_bootcamp&class_type={CLASS_TYPE}&instructor={INSTRUCTOR}\
             &duration=20&sort=top_rated&desc=false&page=2&limit=7"
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        parse(&body),
        expected(&model::LibraryPage::from_upstream(&library_page()))
    );
}

#[test]
fn library_classes_join_instructors_and_class_types() {
    let page = model::LibraryPage::from_upstream(&library_page());
    let first = &page.classes[0];
    assert_eq!(first.id.to_string(), "00000000000000000000000000000f14");
    assert_eq!(first.title.as_deref(), Some("30 min Hip Hop Ride"));
    assert_eq!(first.duration_s, Some(1800));
    assert_eq!(first.discipline.as_deref(), Some("cycling"));
    let instructor = first.instructor.as_ref().expect("joined instructor");
    assert_eq!(instructor.id.to_string(), INSTRUCTOR);
    assert_eq!(instructor.name.as_deref(), Some("name-1"));
    assert_eq!(first.class_types, ["Music"]);
    assert_eq!(first.times_taken, Some(0));
    assert_eq!((page.page, page.page_count, page.total), (0, 3131, 15655));
    assert_eq!(page.classes.len(), 5);
}

#[tokio::test]
async fn library_defaults_send_no_filter_and_peloton_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .and(query_param("page", "0"))
        .and(query_param("limit", "20"))
        .and(query_param_is_missing("browse_category"))
        .and(query_param_is_missing("sort_by"))
        .and(query_param_is_missing("desc"))
        .and(query_param_is_missing("duration"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("library_archived.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (status, body) = get(&server, "/v1/library").await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn library_sort_defaults_to_descending_and_desc_alone_sorts_by_air_time() {
    let server = MockServer::start().await;
    for (sort, desc) in [("difficulty", "true"), ("original_air_time", "false")] {
        Mock::given(method("GET"))
            .and(path("/api/v2/ride/archived"))
            .and(query_param("sort_by", sort))
            .and(query_param("desc", desc))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(fixture_json("library_archived.json")),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    for uri in ["/v1/library?sort=difficulty", "/v1/library?desc=false"] {
        let (status, body) = get(&server, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
}

#[tokio::test]
async fn library_accepts_every_documented_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v2/ride/archived"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("library_archived.json")),
        )
        .mount(&server)
        .await;
    let mut uris: Vec<String> = CATEGORIES
        .iter()
        .map(|c| format!("/v1/library?category={c}"))
        .collect();
    uris.extend(
        [5, 10, 15, 20, 30, 45, 60, 75, 90, 120].map(|d| format!("/v1/library?duration={d}")),
    );
    uris.extend(
        [
            "original_air_time",
            "trending",
            "popularity",
            "top_rated",
            "difficulty",
        ]
        .map(|s| format!("/v1/library?sort={s}&desc=true")),
    );
    uris.extend(["/v1/library?limit=1", "/v1/library?limit=100&page=0"].map(str::to_owned));
    for uri in uris {
        let (status, body) = get(&server, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
}

#[tokio::test]
async fn library_rejects_unknown_enum_values() {
    let server = MockServer::start().await;
    for uri in [
        "/v1/library?category=zumba",
        "/v1/library?category=Cycling",
        "/v1/library?category=",
        "/v1/library?duration=25",
        "/v1/library?duration=1200",
        "/v1/library?duration=twenty",
        "/v1/library?sort=newest",
        "/v1/library?sort=popular",
        "/v1/library?desc=yes",
        "/v1/library?class_type=low_impact",
        "/v1/library?instructor=00000000000000000000000000000F15",
        "/v1/library?difficulty=beginner",
        "/v1/library?limit=0",
        "/v1/library?limit=101",
        "/v1/library?page=-1",
        "/v1/library?category=cycling&category=yoga",
        "/v1/library?bogus=1",
    ] {
        let (status, body) = get(&server, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn library_filters_list_current_values() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ride/filters"))
        .and(query_param("browse_category", "cycling"))
        .and(query_param("library_type", "on_demand"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture_json("ride_filters.json")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/browse_categories"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(fixture_json("browse_categories.json")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let (status, body) = get(&server, "/v1/library/filters?category=cycling").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let filters: wire::RideFilters = serde_json::from_str(&fixture("ride_filters.json")).unwrap();
    let categories: wire::BrowseCategories =
        serde_json::from_str(&fixture("browse_categories.json")).unwrap();
    let mapped = model::LibraryFilters::from_upstream("cycling", &filters, &categories);
    assert_eq!(parse(&body), expected(&mapped));

    let value = parse(&body);
    assert_eq!(value["category"], "cycling");
    let slugs: Vec<&str> = value["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["slug"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, CATEGORIES);
    assert_eq!(
        value["durations_min"],
        json!([5, 10, 15, 20, 30, 45, 60, 75, 90, 120])
    );
    let low_impact = value["class_types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == CLASS_TYPE)
        .expect("Low Impact class type");
    assert!(low_impact["name"].is_string());
    assert_eq!(value["instructors"].as_array().unwrap().len(), 3);
    assert_eq!(
        value["sorts"],
        json!([
            {"sort": "original_air_time", "desc": true, "name": "New"},
            {"sort": "trending", "desc": true, "name": "Trending"},
            {"sort": "popularity", "desc": true, "name": "Popular"},
            {"sort": "top_rated", "desc": true, "name": "Top Rated"},
            {"sort": "difficulty", "desc": false, "name": "Easiest"},
            {"sort": "difficulty", "desc": true, "name": "Hardest"},
        ])
    );
}

#[tokio::test]
async fn library_filters_skip_values_the_library_cannot_take() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/ride/filters"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "filters": [
                {"name": "class_type_id", "values": [
                    {"value": "not-an-id", "display_name": "Odd"},
                    {"value": CLASS_TYPE, "display_name": "Low Impact"},
                ]},
                {"name": "instructor_id", "values": [{"display_name": "No id"}]},
                {"name": "duration", "values": [
                    {"value": "90", "display_name": "90 s"},
                    {"value": "soon"},
                    {"value": "600", "display_name": "10 min"},
                ]},
            ],
            "sorts": [
                {"slug": "x", "display_name": "Mystery", "value": {"sort": "mystery", "desc": true}},
                {"slug": "y", "display_name": "No order", "value": {"sort": "trending"}},
            ],
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/browse_categories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "browse_categories": [{"name": "No slug"}, {"slug": "yoga", "name": "Yoga"}],
        })))
        .mount(&server)
        .await;
    let (status, body) = get(&server, "/v1/library/filters?category=yoga").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        parse(&body),
        json!({
            "category": "yoga",
            "categories": [{"slug": "yoga", "name": "Yoga"}],
            "class_types": [{"id": CLASS_TYPE, "name": "Low Impact"}],
            "instructors": [],
            "durations_min": [10],
            "sorts": [{"sort": "trending", "desc": true, "name": "No order"}],
        })
    );
}

#[tokio::test]
async fn library_filters_require_a_known_category() {
    let server = MockServer::start().await;
    for uri in [
        "/v1/library/filters",
        "/v1/library/filters?category=zumba",
        "/v1/library/filters?category=cycling&page=1",
    ] {
        let (status, body) = get(&server, uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}: {body}");
        assert_error(&body, "invalid_input");
    }
    assert_eq!(upstream_hits(&server).await, 0);
}

#[tokio::test]
async fn library_upstream_failure_is_502() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream-body-secret"))
        .mount(&server)
        .await;
    for uri in ["/v1/library", "/v1/library/filters?category=cycling"] {
        let (status, body) = get(&server, uri).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}");
        assert_error(&body, "upstream_error");
        assert!(!body.contains("upstream-body-secret"));
    }
}

/// The enum values of query parameter `name` of `GET route`.
fn parameter_enum(document: &Value, route: &str, name: &str) -> Vec<Value> {
    let parameter = document["paths"][route]["get"]["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("{route} lacks parameter {name}"))
        .clone();
    assert_eq!(parameter["in"], "query", "{name}");
    let description = parameter["description"].as_str().unwrap_or_default();
    assert!(!description.is_empty(), "{name} is undescribed");
    let schema = &parameter["schema"];
    let values = schema["enum"]
        .as_array()
        .or_else(|| {
            schema["oneOf"]
                .as_array()
                .and_then(|options| options.iter().find_map(|o| o["enum"].as_array()))
        })
        .unwrap_or_else(|| panic!("{name} has no enum: {schema}"));
    values.iter().filter(|v| !v.is_null()).cloned().collect()
}

#[test]
fn openapi_lists_new_routes_and_filter_enums() {
    let document = serde_json::to_value(openapi()).unwrap();
    let paths = document["paths"].as_object().unwrap();
    for (route, methods) in [
        ("/v1/library", &["get"][..]),
        ("/v1/library/filters", &["get"]),
        ("/v1/schedule", &["get", "post"]),
        ("/v1/schedule/{scheduled_id}", &["get", "patch", "delete"]),
        ("/v1/stack", &["get", "post", "put"]),
        ("/v1/stack/{class_id}", &["delete"]),
    ] {
        for method in methods {
            assert!(
                paths[route][*method].is_object(),
                "missing {method} {route}"
            );
        }
    }

    let categories = parameter_enum(&document, "/v1/library", "category");
    assert_eq!(categories, CATEGORIES.map(Value::from));
    assert_eq!(
        parameter_enum(&document, "/v1/library/filters", "category"),
        categories
    );
    assert_eq!(
        parameter_enum(&document, "/v1/library", "duration"),
        [5, 10, 15, 20, 30, 45, 60, 75, 90, 120].map(Value::from)
    );
    assert_eq!(
        parameter_enum(&document, "/v1/library", "sort"),
        [
            "original_air_time",
            "trending",
            "popularity",
            "top_rated",
            "difficulty"
        ]
        .map(Value::from)
    );
    let parameters = document["paths"]["/v1/library"]["get"]["parameters"]
        .as_array()
        .unwrap();
    let names: Vec<&str> = parameters
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "category",
            "class_type",
            "instructor",
            "duration",
            "sort",
            "desc",
            "page",
            "limit"
        ]
    );
    for name in ["category", "class_type", "instructor"] {
        let parameter = parameters.iter().find(|p| p["name"] == name).unwrap();
        let description = parameter["description"].as_str().unwrap();
        assert!(
            description.contains("/v1/library/filters"),
            "{name}: {description}"
        );
    }
    for parameter in parameters {
        assert_ne!(parameter["required"], true, "{parameter}");
    }
    let filters = &document["paths"]["/v1/library/filters"]["get"]["parameters"][0];
    assert_eq!(filters["required"], true);

    let schemas = document["components"]["schemas"].as_object().unwrap();
    for schema in [
        "LibraryPage",
        "LibraryFilters",
        "ScheduleList",
        "ScheduledClassView",
        "StackView",
        "ScheduleRequest",
        "RescheduleRequest",
        "StackAddRequest",
        "StackReplaceRequest",
    ] {
        assert!(schemas.contains_key(schema), "missing schema {schema}");
    }
}

#[test]
fn openapi_marks_writes_and_documents_their_errors() {
    let document = serde_json::to_value(openapi()).unwrap();
    let capability = "pecheriere.dev/cap/peloton-write";
    let tag = document["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "peloton-write")
        .expect("write tag")
        .clone();
    assert!(tag["description"].as_str().unwrap().contains(capability));
    let operation = |route: &str, method: &str| document["paths"][route][method].clone();
    let writes = [
        (
            "/v1/schedule",
            "post",
            &["201", "400", "422", "502", "503"][..],
        ),
        (
            "/v1/schedule/{scheduled_id}",
            "patch",
            &["200", "400", "404", "422", "502", "503"],
        ),
        (
            "/v1/schedule/{scheduled_id}",
            "delete",
            &["204", "400", "404", "502", "503"],
        ),
        (
            "/v1/stack",
            "post",
            &["200", "204", "400", "409", "502", "503"],
        ),
        ("/v1/stack", "put", &["200", "204", "400", "502", "503"]),
        (
            "/v1/stack/{class_id}",
            "delete",
            &["200", "204", "400", "404", "502", "503"],
        ),
    ];
    for (route, method, statuses) in writes {
        let op = operation(route, method);
        assert_eq!(op["tags"], json!(["peloton-write"]), "{method} {route}");
        let description = op["description"].as_str().unwrap_or_default();
        assert!(description.contains(capability), "{method} {route}");
        for status in statuses {
            assert!(
                op["responses"][*status].is_object(),
                "{method} {route} lacks {status}"
            );
        }
    }
    for (route, method) in [
        ("/v1/stack", "post"),
        ("/v1/stack", "put"),
        ("/v1/stack/{class_id}", "delete"),
    ] {
        let no_content = operation(route, method)["responses"]["204"]["description"].clone();
        assert!(
            no_content.as_str().unwrap().contains("GET /v1/stack"),
            "{method} {route}: {no_content}"
        );
    }
    for (route, status, code) in [
        ("/v1/schedule", "422", "cannot_schedule"),
        ("/v1/stack", "409", "already_in_stack"),
    ] {
        let description = operation(route, "post")["responses"][status]["description"].clone();
        assert!(description.as_str().unwrap().contains(code), "{route}");
    }
    for (route, method) in [
        ("/v1/library", "get"),
        ("/v1/schedule", "get"),
        ("/v1/stack", "get"),
    ] {
        assert!(
            operation(route, method)["tags"]
                .as_array()
                .is_none_or(|tags| tags.iter().all(|t| t != "peloton-write")),
            "{route} is a read"
        );
    }

    // One worked example: find a 20-min low-impact ride and schedule it.
    let description = document["info"]["description"].as_str().unwrap();
    for text in [
        "/v1/library/filters?category=cycling",
        "duration=20",
        "POST /v1/schedule",
        "America/Los_Angeles",
        "2026-11-02T08:00:00-08:00",
    ] {
        assert!(description.contains(text), "missing {text}: {description}");
    }
    let example = &document["components"]["schemas"]["ScheduleRequest"]["example"];
    assert_eq!(example["start_time"], "2026-11-02T08:00:00-08:00");
}
