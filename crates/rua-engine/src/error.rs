pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported provider kind: {0}")]
    UnsupportedProvider(String),

    #[error("unknown provider in model ref: {0}")]
    UnknownProvider(String),

    #[error("http client error: {0}")]
    HttpClient(#[from] rig_core::http_client::Error),

    #[error("completion error: {0}")]
    Completion(#[from] rig_core::completion::CompletionError),

    #[error("turn history is empty")]
    EmptyHistory,

    #[error("distill returned no text")]
    EmptyDistill,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("config error: {0}")]
    Config(String),
}
