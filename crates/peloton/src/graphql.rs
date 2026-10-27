//! GraphQL operations the relay may send to Peloton's gateway.
//!
//! Each document holds exactly one operation, named as Peloton's web app names
//! it. Field selections come from the web app's captured requests and from
//! open-source clients that use the same operations; nothing outside this file
//! is sent to the gateway.

use url::form_urlencoded;

use crate::endpoints::GRAPHQL_URL;

/// Whether an operation reads or writes; the gateway wants it in the URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// A read.
    Query,
    /// A write.
    Mutation,
}

impl OpKind {
    /// The GraphQL keyword, also used as the URL query parameter name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Mutation => "mutation",
        }
    }
}

/// One GraphQL operation: its name, kind, document, and the root field of
/// `data` that holds its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    /// The operation name, as in the document and the gateway URL.
    pub name: &'static str,
    /// Whether it reads or writes.
    pub kind: OpKind,
    /// The document sent as `query`.
    pub document: &'static str,
    /// The root field of `data` holding the answer.
    pub field: &'static str,
}

/// The gateway URL for operation `op`: `…/graphql?query=<op>` or `?mutation=<op>`.
#[must_use]
pub fn op_url(op: &str, kind: OpKind) -> String {
    let op: String = form_urlencoded::byte_serialize(op.as_bytes()).collect();
    format!("{GRAPHQL_URL}?{}={op}", kind.as_str())
}

/// The class fields the relay reads wherever Peloton returns a class. A
/// scheduled on-demand class comes back as a `LiveClass`, so both members
/// select the same details.
macro_rules! class_summary_fragment {
    () => {
        "
fragment ClassSummary on PelotonClass {
  __typename
  classId
  joinToken
  title
  duration
  fitnessDiscipline {
    slug
  }
  ... on OnDemandInstructorClass {
    airTime
    fitnessDiscipline {
      slug
      displayName
    }
    difficultyLevel {
      slug
      displayName
    }
    instructor {
      name
    }
  }
  ... on LiveClass {
    airTime
    fitnessDiscipline {
      slug
      displayName
    }
    difficultyLevel {
      slug
      displayName
    }
    instructor {
      name
    }
  }
}
"
    };
}

/// The fields of a Stack, as returned by the Stack mutations.
macro_rules! stack_fields {
    () => {
        "
    __typename
    numClasses
    totalTime
    userStack {
      stackedClassList {
        playOrder
        pelotonClass {
          ...ClassSummary
        }
      }
    }
"
    };
}

/// The schedule union returned by the schedule mutations, with `$fields`
/// selected on a `ScheduledClass`.
macro_rules! schedule_result {
    ($fields:expr) => {
        concat!(
            "
    __typename
    ... on ScheduledClass {",
            $fields,
            "    }
    ... on ScheduledClassDoesNotExistError {
      code
      message
    }
    ... on CannotScheduleClassError {
      code
      message
    }
"
        )
    };
}

/// The fields the relay reads from a scheduled class.
macro_rules! scheduled_class_fields {
    () => {
        "
      id
      scheduledStartTime
      scheduledEndTime
      pelotonClass {
        ...ClassSummary
      }
"
    };
}

/// Lists the user's scheduled items between `$startTime` and `$endTime`.
pub const YOUR_SCHEDULE_ITEMS: &str = concat!(
    "query YourScheduleItems($startTime: DateTime!, $endTime: DateTime!) {
  userScheduledItemsList(startTime: $startTime, endTime: $endTime) {
    __typename
    ... on YourScheduleItemList {
      yourScheduleItems {
        __typename
        ... on ScheduledClass {",
    scheduled_class_fields!(),
    "        }
      }
    }
  }
}
",
    class_summary_fragment!()
);

/// Reads one scheduled class by its scheduled-class id `$id`.
pub const SCHEDULED_CLASS: &str = concat!(
    "query ScheduledClass($id: String!) {
  scheduledClass(scheduledClassId: $id) {
    __typename
    ... on ScheduledClass {",
    scheduled_class_fields!(),
    "    }
  }
}
",
    class_summary_fragment!()
);

/// Schedules the on-demand class `$id` at `$startTime`.
pub const ADD_CLASS_TO_SCHEDULE: &str = concat!(
    "mutation AddClassToSchedule($id: ID!, $startTime: DateTime!) {
  addClassToSchedule(scheduledClassInput: {id: $id, scheduledStartTime: $startTime}) {",
    schedule_result!(scheduled_class_fields!()),
    "  }
}
",
    class_summary_fragment!()
);

/// Moves the scheduled class `$id` to `$startTime`; Peloton issues a new id.
pub const RESCHEDULE_CLASS: &str = concat!(
    "mutation RescheduleClass($id: ID!, $startTime: DateTime!) {
  rescheduleClass(rescheduleClassInput: {id: $id, scheduledStartTime: $startTime}) {",
    schedule_result!(scheduled_class_fields!()),
    "  }
}
",
    class_summary_fragment!()
);

/// Removes the scheduled class `$id`.
pub const REMOVE_CLASS_FROM_SCHEDULE: &str = concat!(
    "mutation RemoveClassFromSchedule($id: String!) {
  removeClassFromSchedule(scheduledClassId: $id) {",
    schedule_result!(
        "
      id
"
    ),
    "  }
}
"
);

/// Reads the Stack in play order.
pub const VIEW_USER_STACK: &str = concat!(
    "query ViewUserStack {
  viewUserStack {
    __typename
    numClasses
    totalTime
    ... on StackResponseSuccess {
      userStack {
        stackedClassList {
          playOrder
          pelotonClass {
            ...ClassSummary
          }
        }
      }
    }
  }
}
",
    class_summary_fragment!()
);

/// Appends `$input.pelotonClassId` (an on-demand class id) to the Stack.
pub const ADD_CLASS_TO_STACK: &str = concat!(
    "mutation AddClassToStack($input: AddClassToStackInput!) {
  addClassToStack(input: $input) {",
    stack_fields!(),
    "  }
}
",
    class_summary_fragment!()
);

/// Replaces the whole Stack with `$input.pelotonClassIdList`, in that order.
pub const MODIFY_STACK: &str = concat!(
    "mutation ModifyStack($input: ModifyStackInput!) {
  modifyStack(input: $input) {",
    stack_fields!(),
    "  }
}
",
    class_summary_fragment!()
);

/// Asks whether the on-demand class `$pelotonClassId` is in the Stack.
pub const CHECK_CLASS_IN_STACK: &str =
    "query CheckDoesClassExistInUserStack($pelotonClassId: ID!) {
  checkDoesClassExistInUserStack(pelotonClassId: $pelotonClassId)
}
";

#[cfg(test)]
mod tests {
    use super::{
        ADD_CLASS_TO_SCHEDULE, ADD_CLASS_TO_STACK, CHECK_CLASS_IN_STACK, MODIFY_STACK, OpKind,
        REMOVE_CLASS_FROM_SCHEDULE, RESCHEDULE_CLASS, SCHEDULED_CLASS, VIEW_USER_STACK,
        YOUR_SCHEDULE_ITEMS, op_url,
    };

    const OPERATIONS: [(&str, OpKind, &str); 9] = [
        ("YourScheduleItems", OpKind::Query, YOUR_SCHEDULE_ITEMS),
        ("ScheduledClass", OpKind::Query, SCHEDULED_CLASS),
        (
            "AddClassToSchedule",
            OpKind::Mutation,
            ADD_CLASS_TO_SCHEDULE,
        ),
        ("RescheduleClass", OpKind::Mutation, RESCHEDULE_CLASS),
        (
            "RemoveClassFromSchedule",
            OpKind::Mutation,
            REMOVE_CLASS_FROM_SCHEDULE,
        ),
        ("ViewUserStack", OpKind::Query, VIEW_USER_STACK),
        ("AddClassToStack", OpKind::Mutation, ADD_CLASS_TO_STACK),
        ("ModifyStack", OpKind::Mutation, MODIFY_STACK),
        (
            "CheckDoesClassExistInUserStack",
            OpKind::Query,
            CHECK_CLASS_IN_STACK,
        ),
    ];

    #[test]
    fn each_document_holds_its_named_operation_only() {
        for (name, kind, document) in OPERATIONS {
            let header = format!("{} {name}", kind.as_str());
            assert!(document.starts_with(&header), "{name}: header");
            let next = document.as_bytes()[header.len()];
            assert!(matches!(next, b'(' | b' '), "{name}: name ends");
            let operations =
                document.matches("query ").count() + document.matches("mutation ").count();
            assert_eq!(operations, 1, "{name}: one operation");
        }
    }

    #[test]
    fn documents_are_balanced_and_define_only_used_fragments() {
        for (name, _, document) in OPERATIONS {
            let opens = document.matches('{').count();
            let closes = document.matches('}').count();
            assert_eq!(opens, closes, "{name}: braces");
            let defines = document.contains("fragment ClassSummary on PelotonClass");
            let spreads = document.contains("...ClassSummary");
            assert_eq!(defines, spreads, "{name}: fragment defined iff used");
        }
    }

    #[test]
    fn op_url_puts_kind_and_name_in_the_query() {
        assert_eq!(
            op_url("ViewUserStack", OpKind::Query),
            "https://gql-graphql-gateway.prod.k8s.onepeloton.com/graphql?query=ViewUserStack"
        );
        assert_eq!(
            op_url("ModifyStack", OpKind::Mutation),
            "https://gql-graphql-gateway.prod.k8s.onepeloton.com/graphql?mutation=ModifyStack"
        );
        assert!(op_url("a&b=c", OpKind::Query).ends_with("?query=a%26b%3Dc"));
    }
}
