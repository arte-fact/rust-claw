/// Runs a synchronous (SQLite/filesystem) operation on the blocking pool.
/// `E` is the caller's error type; it must absorb both the op's error and a join failure.
pub(crate) async fn run<T, InnerError, E>(
    op: impl FnOnce() -> Result<T, InnerError> + Send + 'static,
) -> Result<T, E>
where
    T: Send + 'static,
    InnerError: Into<E> + Send + 'static,
    E: From<tokio::task::JoinError>,
{
    tokio::task::spawn_blocking(op).await?.map_err(Into::into)
}
