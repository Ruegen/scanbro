use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Network(String),

    #[error("mDNS error: {0}")]
    Mdns(String),

    #[error("XML parse error: {0}")]
    Xml(String),

    #[error("image decode error: {0}")]
    Image(#[from] image::ImageError),

    #[error("eSCL protocol error: {0}")]
    Escl(String),

    #[error("multipart parse error: {0}")]
    Multipart(String),

    #[error("PDF error: {0}")]
    Pdf(String),

    #[error("scanner unreachable")]
    Disconnected,

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        if err.is_timeout() {
            Self::Escl("The scanner took too long. Try Scan again.".into())
        } else if err.is_connect() {
            Self::Escl("Couldn't connect to the scanner. Try Scan again.".into())
        } else {
            Self::Escl("The scanner dropped the connection. Try Scan again.".into())
        }
    }
}

impl From<mdns_sd::Error> for AppError {
    fn from(err: mdns_sd::Error) -> Self {
        Self::Mdns(err.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
