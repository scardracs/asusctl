#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Could not parse colour")]
    ParseColour,

    #[error("Could not parse speed")]
    ParseSpeed,

    #[error("Could not parse direction")]
    ParseDirection,

    #[error("Could not parse brightness")]
    ParseBrightness,

    #[error("IO Error: {0}: {1}")]
    IoPath(String, #[source] std::io::Error),

    #[error("TOML Parse Error: {0}")]
    TomlDe(#[source] toml::de::Error),

    #[error("TOML Serialize Error: {0}")]
    TomlSer(#[source] toml::ser::Error),
}

impl From<toml::de::Error> for Error {
    fn from(e: toml::de::Error) -> Self {
        Self::TomlDe(e)
    }
}

impl From<toml::ser::Error> for Error {
    fn from(e: toml::ser::Error) -> Self {
        Self::TomlSer(e)
    }
}
