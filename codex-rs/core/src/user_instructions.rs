use serde::Deserialize;
use serde::Serialize;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub const USER_INSTRUCTIONS_OPEN_TAG_LEGACY: &str = "<user_instructions>";
pub const USER_INSTRUCTIONS_PREFIX: &str = "# AGENTS.md instructions for ";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename = "user_instructions", rename_all = "snake_case")]
pub(crate) struct UserInstructions {
    pub directory: String,
    pub text: String,
}

impl UserInstructions {
    pub fn is_user_instructions(message: &[ContentItem]) -> bool {
        if let [ContentItem::InputText { text }] = message {
            text.starts_with(USER_INSTRUCTIONS_PREFIX)
                || text.starts_with(USER_INSTRUCTIONS_OPEN_TAG_LEGACY)
        } else {
            false
        }
    }
}

impl From<UserInstructions> for ResponseItem {
    fn from(ui: UserInstructions) -> Self {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!(
                    "{USER_INSTRUCTIONS_PREFIX}{directory}\n\n<INSTRUCTIONS>\n{contents}\n</INSTRUCTIONS>",
                    directory = ui.directory,
                    contents = ui.text
                ),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename = "developer_instructions", rename_all = "snake_case")]
pub(crate) struct DeveloperInstructions {
    text: String,
}

impl DeveloperInstructions {
    pub fn new<T: Into<String>>(text: T) -> Self {
        Self { text: text.into() }
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

impl From<DeveloperInstructions> for ResponseItem {
    fn from(di: DeveloperInstructions) -> Self {
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: di.into_text(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_instructions() {
        let user_instructions = UserInstructions {
            directory: "test_directory".to_string(),
            text: "test_text".to_string(),
        };
        let response_item: ResponseItem = user_instructions.into();

        let ResponseItem::Message { role, content, .. } = response_item else {
            panic!("expected ResponseItem::Message");
        };

        assert_eq!(role, "user");

        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected one InputText content item");
        };

        assert_eq!(
            text,
            "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>",
        );
    }

    #[test]
    fn test_is_user_instructions() {
        assert!(UserInstructions::is_user_instructions(
            &[ContentItem::InputText {
                text: "# AGENTS.md instructions for test_directory\n\n<INSTRUCTIONS>\ntest_text\n</INSTRUCTIONS>".to_string(),
            }]
        ));
        assert!(UserInstructions::is_user_instructions(&[
            ContentItem::InputText {
                text: "<user_instructions>test_text</user_instructions>".to_string(),
            }
        ]));
        assert!(!UserInstructions::is_user_instructions(&[
            ContentItem::InputText {
                text: "test_text".to_string(),
            }
        ]));
    }
}
