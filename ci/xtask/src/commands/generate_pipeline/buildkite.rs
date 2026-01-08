use {
    serde::{Serialize, Serializer},
    std::collections::HashMap,
};

#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Step {
    Command(CommandStep),
    Wait(WaitStep),
    Group(GroupStep),
    Trigger(TriggerStep),
}

#[derive(Debug, Serialize, Default, PartialEq)]
pub struct CommandStep {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_on_build_failing: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub soft_fail: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<HashMap<String, String>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_in_minutes: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry: Option<HashMap<String, String>>,
}

#[derive(Debug, PartialEq)]
pub struct WaitStep {}

impl Serialize for WaitStep {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("wait", "~")?;
        map.end()
    }
}

#[derive(Debug, Default, Serialize, PartialEq)]
pub struct GroupStep {
    #[serde(skip_serializing_if = "String::is_empty")]
    #[serde(rename = "group")]
    pub name: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TriggerStep {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,

    #[serde(skip_serializing_if = "String::is_empty")]
    pub trigger: String,

    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,

    #[serde(rename = "async")]
    pub is_async: Option<bool>,

    pub soft_fail: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Build {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Pipeline {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,

    pub steps: Vec<Step>,
}

impl Pipeline {
    pub fn new() -> Self {
        Pipeline {
            priority: None,
            steps: Vec::new(),
        }
    }

    pub fn set_priority(&mut self, priority: i64) {
        self.priority = Some(priority);
    }

    pub fn add_step(&mut self, step: Step) {
        self.steps.push(step);
    }
}

#[cfg(test)]
mod tests {
    use {super::*, pretty_assertions::assert_eq, test_case::test_case};

    macro_rules! assert_json_eq {
        ($actual:expr, $expected:expr) => {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>($actual).unwrap(),
                serde_json::from_str::<serde_json::Value>($expected).unwrap()
            );
        };
    }

    #[test_case(
        Step::Command(CommandStep {
            name: String::from("basic test"),
            ..Default::default()
        }),
        r#"{
            "name": "basic test"
        }"#;
        "command_step_basic_name_only"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with command"),
            command: String::from("echo hello"),
            ..Default::default()
        }),
        r#"{
            "name": "test with command",
            "command": "echo hello"
        }"#;
        "command_step_with_single_command"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with commands"),
            commands: vec![String::from("echo hello"), String::from("echo world")],
            ..Default::default()
        }),
        r#"{
            "name": "test with commands",
            "commands": ["echo hello", "echo world"]
        }"#;
        "command_step_with_multiple_commands"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with cancel"),
            cancel_on_build_failing: Some(true),
            ..Default::default()
        }),
        r#"{
            "name": "test with cancel",
            "cancel_on_build_failing": true
        }"#;
        "command_step_with_cancel_on_build_failing_true"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test no cancel"),
            cancel_on_build_failing: Some(false),
            ..Default::default()
        }),
        r#"{
            "name": "test no cancel",
            "cancel_on_build_failing": false
        }"#;
        "command_step_with_cancel_on_build_failing_false"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test soft fail"),
            soft_fail: Some(true),
            ..Default::default()
        }),
        r#"{
            "name": "test soft fail",
            "soft_fail": true
        }"#;
        "command_step_with_soft_fail_true"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test no soft fail"),
            soft_fail: Some(false),
            ..Default::default()
        }),
        r#"{
            "name": "test no soft fail",
            "soft_fail": false
        }"#;
        "command_step_with_soft_fail_false"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with agents"),
            agents: Some(HashMap::from([
                (String::from("queue"), String::from("default")),
                (String::from("os"), String::from("linux"))
            ])),
            ..Default::default()
        }),
        r#"{
            "name": "test with agents",
            "agents": {"queue": "default", "os": "linux"}
        }"#;
        "command_step_with_agents"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with timeout"),
            timeout_in_minutes: Some(30),
            ..Default::default()
        }),
        r#"{
            "name": "test with timeout",
            "timeout_in_minutes": 30
        }"#;
        "command_step_with_timeout"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("test with retry"),
            retry: Some(HashMap::from([
                (String::from("automatic"), String::from("true")),
                (String::from("manual"), String::from("false"))
            ])),
            ..Default::default()
        }),
        r#"{
            "name": "test with retry",
            "retry": {"automatic": "true", "manual": "false"}
        }"#;
        "command_step_with_retry"
    )]
    #[test_case(
        Step::Command(CommandStep {
            name: String::from("full command step"),
            command: String::from("npm test"),
            cancel_on_build_failing: Some(true),
            soft_fail: Some(false),
            agents: Some(HashMap::from([(String::from("queue"), String::from("test"))])),
            timeout_in_minutes: Some(15),
            retry: Some(HashMap::from([(String::from("automatic"), String::from("true"))])),
            ..Default::default()
        }),
        r#"{
            "name": "full command step",
            "command": "npm test",
            "cancel_on_build_failing": true,
            "soft_fail": false,
            "agents": {"queue": "test"},
            "timeout_in_minutes": 15,
            "retry": {"automatic": "true"}
        }"#;
        "command_step_with_all_fields"
    )]
    #[test_case(
        Step::Wait(WaitStep {}),
        r#"{
            "wait": "~"
        }"#;
        "wait_step"
    )]
    #[test_case(
        Step::Group(GroupStep {
            name: String::from("empty group"),
            steps: vec![
                Step::Command(CommandStep {
                    name: String::from("step 1"),
                    command: String::from("echo 1"),
                    ..Default::default()
                }),
                Step::Wait(WaitStep {}),
                Step::Command(CommandStep {
                    name: String::from("step 2"),
                    command: String::from("echo 2"),
                    ..Default::default()
                })
            ],
        }),
        r#"{
            "group": "empty group",
            "steps": [
                {"name": "step 1", "command": "echo 1"},
                {"wait": "~"},
                {"name": "step 2", "command": "echo 2"}
            ]
        }"#;
        "group_step"
    )]
    #[test_case(
        Step::Trigger(TriggerStep {
            name: String::from("Trigger Build on agave-secondary"),
            trigger: String::from("agave-secondary"),
            branches: vec![String::from("!pull/*")],
            is_async: Some(true),
            soft_fail: Some(true),
            build: Some(Build {
                message: Some(String::from("${BUILDKITE_MESSAGE}")),
                commit: Some(String::from("${BUILDKITE_COMMIT}")),
                branch: Some(String::from("${BUILDKITE_BRANCH}")),
                env: Some(HashMap::from([(
                    String::from("TRIGGERED_BUILDKITE_TAG"),
                    String::from("${BUILDKITE_TAG}"),
                )])),
            }),
        }),
        r#"{
            "name": "Trigger Build on agave-secondary",
            "trigger": "agave-secondary",
            "branches": ["!pull/*"],
            "async": true,
            "soft_fail": true,
            "build": {
                "branch": "${BUILDKITE_BRANCH}",
                "commit": "${BUILDKITE_COMMIT}",
                "message": "${BUILDKITE_MESSAGE}",
                "env": {"TRIGGERED_BUILDKITE_TAG": "${BUILDKITE_TAG}"}
            }
        }"#;
        "trigger_step"
    )]
    fn test_step_serialize_json(step: Step, expected: &str) {
        let serialized = serde_json::to_string(&step).unwrap();
        assert_json_eq!(&serialized, expected);
    }

    #[test]
    fn test_pipeline_creation() {
        let mut pipeline = Pipeline::new();
        assert!(pipeline.steps.is_empty());

        pipeline.add_step(Step::Command(CommandStep {
            name: String::from("test"),
            ..Default::default()
        }));

        assert_eq!(pipeline.steps.len(), 1);
    }

    #[test]
    fn test_priority_pipeline() {
        let mut pipeline = Pipeline::new();
        pipeline.set_priority(10);
        assert_eq!(pipeline.priority, Some(10));

        pipeline.add_step(Step::Command(CommandStep {
            name: String::from("step 1"),
            command: String::from("echo 1"),
            ..Default::default()
        }));

        pipeline.add_step(Step::Command(CommandStep {
            name: String::from("step 2"),
            command: String::from("echo 2"),
            ..Default::default()
        }));

        let serialized = serde_json::to_string(&pipeline).unwrap();
        assert_json_eq!(
            &serialized,
            r#"{
            "priority": 10,
            "steps": [
                {"name": "step 1", "command": "echo 1"},
                {"name": "step 2", "command": "echo 2"}
            ]
        }"#
        );
    }
}
