//! The read-only subset of the Peloton API the relay talks to.

use crate::ids::PelotonId;

/// Base URL of the Peloton API.
pub const API_BASE: &str = "https://api.onepeloton.com/api";

/// Header sent with every Peloton API request.
pub const PLATFORM_HEADER: (&str, &str) = ("Peloton-Platform", "web");

/// Path of the current user's profile.
#[must_use]
pub fn me() -> String {
    "/me".to_owned()
}

/// Path of a user's workout list.
#[must_use]
pub fn user_workouts(user: &PelotonId) -> String {
    format!("/user/{user}/workouts")
}

/// Path of a single workout.
#[must_use]
pub fn workout(id: &PelotonId) -> String {
    format!("/workout/{id}")
}

/// Path of a workout's performance graph.
#[must_use]
pub fn performance_graph(id: &PelotonId) -> String {
    format!("/workout/{id}/performance_graph")
}

/// Path of a ride's details.
#[must_use]
pub fn ride_details(id: &PelotonId) -> String {
    format!("/ride/{id}/details")
}
