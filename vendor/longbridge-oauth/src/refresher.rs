// Added by Tick Loop; see ../README.md.
use std::{future::Future, pin::Pin};

use crate::{error::OAuthResult, storage::StoredToken};

/// The token a [`TokenRefresher`] renews, once it has.
pub type RefreshFuture<'a> = Pin<Box<dyn Future<Output = OAuthResult<StoredToken>> + Send + 'a>>;

/// Renews tokens in place of the client's own refresh, for an application that keeps its tokens
/// and refreshes them itself, such as one that shares them with other processes.
///
/// Set it with [`OAuthBuilder::token_refresher`](crate::OAuthBuilder::token_refresher). The
/// client then asks it for every renewal and saves what it returns through the configured
/// [`TokenStorage`](crate::TokenStorage) as before.
pub trait TokenRefresher: Send + Sync + 'static {
    /// The token that replaces `current`, which has expired or expires within five minutes.
    fn refresh<'a>(&'a self, current: &'a StoredToken) -> RefreshFuture<'a>;
}
