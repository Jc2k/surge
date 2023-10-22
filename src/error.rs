use thiserror::Error;

#[derive(Error, Debug)]
#[error(transparent)]
pub struct Error(#[from] anyhow::Error);
