/// One entry in the chat history (domain model)
#[derive(Debug, Clone)]
pub struct ChatEntry {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}
