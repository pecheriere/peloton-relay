//! `peloton-relay seed` and `peloton-relay bind`: put a credential into one
//! account, bound to one Peloton user.

use std::fmt;
use std::io::{self, Read};
use std::time::Duration;

use peloton::body::read_limited;
use peloton::endpoints::PLATFORM_HEADER;
use peloton::ids::PelotonId;
use peloton::oauth::TokenSet;
use peloton::text::shown;
use peloton::upstream::Me;
use secrecy::ExposeSecret as _;
use url::Url;

use crate::account::{AccountName, Accounts, user_suffix};
use crate::config::ACCOUNTS_VAR;
use crate::store::{PeerConflict, State, StateLock, StoreError, peer_conflict};

/// Upper bound for the token JSON read from the input.
const MAX_INPUT_BYTES: u64 = 64 * 1024;

/// Why seeding or binding failed. Never carries tokens, usernames or
/// response bodies.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SeedError {
    /// The account is not one the relay is configured to serve.
    #[error("account \"{account}\" is not configured ({ACCOUNTS_VAR})")]
    UnknownAccount {
        /// The account asked for.
        account: AccountName,
    },
    /// The account is bound to a Peloton user other than the credential's.
    #[error(
        "account \"{account}\" is bound to another Peloton user; seed with --rebind to replace it"
    )]
    BoundToAnotherUser {
        /// The account asked for.
        account: AccountName,
    },
    /// The credential's Peloton user is bound to another account.
    #[error("this Peloton user is already bound to account \"{account}\"")]
    UserBoundElsewhere {
        /// The account the user is bound to.
        account: AccountName,
    },
    /// A binding that must be checked cannot be read or does not parse.
    #[error("cannot read the binding of account \"{account}\"")]
    BindingUnreadable {
        /// The account whose binding is unreadable.
        account: AccountName,
        /// Why: corrupt, or which I/O failure.
        #[source]
        source: StoreError,
    },
    /// `bind` found no state file in the account.
    #[error("account \"{account}\" has no state file to bind")]
    NoState {
        /// The account asked for.
        account: AccountName,
    },
    /// The input could not be read, is too large or is not a token set.
    #[error("input is not a valid token set")]
    InvalidInput,
    /// Peloton could not be reached or its answer was not understood.
    #[error("could not fetch the profile from Peloton")]
    Upstream,
    /// Peloton refused the access token.
    #[error("Peloton rejected the access token with status {status}")]
    Rejected {
        /// HTTP status code.
        status: u16,
    },
    /// Peloton answered with a status other than success or a rejection.
    #[error("Peloton answered with status {status}")]
    Unexpected {
        /// HTTP status code.
        status: u16,
    },
    /// Another writer (a seed or bind, or the relay's token refresh) kept a
    /// lock for the whole wait.
    #[error("another seed, bind or token refresh holds the lock; try again")]
    Busy,
    /// The state, binding or lock file could not be read or written.
    #[error("could not read or write the state, binding or lock file")]
    Store(#[source] StoreError),
}

impl From<StoreError> for SeedError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::Locked => Self::Busy,
            other => Self::Store(other),
        }
    }
}

/// What a successful seed did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seeded {
    /// The seeded account.
    pub account: AccountName,
    /// The Peloton username of the credential, when Peloton reports one.
    pub username: Option<String>,
    /// The Peloton user the account is now bound to.
    pub user_id: PelotonId,
    /// The user the account was bound to before, when the seed rebound it.
    pub previous: Option<PelotonId>,
}

impl fmt::Display for Seeded {
    /// The lines `seed` prints. Control, format (bidi and other invisible) and
    /// line-separator characters in the username, which comes from Peloton,
    /// are replaced so they cannot drive or reorder the terminal output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let account = &self.account;
        if let Some(previous) = &self.previous {
            writeln!(
                f,
                "account \"{account}\": was bound to Peloton user id {}",
                user_suffix(previous)
            )?;
        }
        write!(f, "account \"{account}\": bound to Peloton user ")?;
        match &self.username {
            Some(username) => f.write_str(&shown(username))?,
            None => f.write_str("(no username)")?,
        }
        write!(f, " (id {})", user_suffix(&self.user_id))
    }
}

/// What a successful bind did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The bound account.
    pub account: AccountName,
    /// The Peloton user the account is bound to.
    pub user_id: PelotonId,
}

impl fmt::Display for Bound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "account \"{}\": bound to the Peloton user of its state file (id {})",
            self.account,
            user_suffix(&self.user_id)
        )
    }
}

/// Reads a [`TokenSet`] from `input`, looks up its owner with `GET /me` on
/// `api_base`, and stores it in `account`, bound to that owner.
///
/// An unconfigured account is refused before the input is read. Under the
/// account's state lock, then the cross-account bindings lock, the seed is
/// refused when the owner is bound to another account (even with `rebind`),
/// when a binding it must check is unreadable, or when the account is bound
/// to another user and `rebind` is false. Nothing is written on any refusal,
/// nor unless `/me` succeeded.
///
/// The binding is written before the state. An I/O failure between the two
/// leaves the new binding next to the old state; the server refuses an old
/// state of another user (`state_refused`, reason `user_mismatch`) until a
/// seed succeeds, so it fails closed and never serves a credential of a user
/// the account is not bound to.
///
/// Each lock is waited for up to `lock_wait` (a refresh the relay has in
/// flight holds the state lock), so the two never overwrite each other.
///
/// # Errors
///
/// See [`SeedError`].
pub async fn run_seed(
    input: impl Read,
    accounts: &Accounts,
    account: &AccountName,
    rebind: bool,
    http: &reqwest::Client,
    api_base: &Url,
    lock_wait: Duration,
) -> Result<Seeded, SeedError> {
    ensure_configured(accounts, account)?;
    let tokens = read_tokens(input)?;
    let me = fetch_me(http, api_base, &tokens).await?;
    let accounts = accounts.clone();
    let account = account.clone();
    // File I/O and waiting for the locks block a thread; keep them off the
    // async workers.
    tokio::task::spawn_blocking(move || {
        accounts.ensure_dir(&account).map_err(StoreError::Io)?;
        let lock = accounts.state_file(&account).lock(lock_wait)?;
        let _bindings = accounts.lock_bindings(lock_wait)?;
        let previous = claim(&accounts, &account, &lock, &me.id, rebind)?;
        lock.save(&State {
            tokens,
            user_id: me.id.clone(),
        })?;
        Ok(Seeded {
            account,
            username: me.username,
            user_id: me.id,
            previous,
        })
    })
    .await
    .map_err(|_| {
        SeedError::Store(StoreError::Io(io::Error::other(
            "state file task did not complete",
        )))
    })?
}

/// Binds `account` to the Peloton user of the state file it already has,
/// without calling Peloton: for a state written before bindings existed.
///
/// Idempotent when the account is already bound to that user; never replaces
/// a binding to another user (seed with `--rebind` for that). Takes the same
/// locks and makes the same cross-account checks as [`run_seed`], and writes
/// nothing on any refusal. Blocks the calling thread while it waits up to
/// `lock_wait` for each lock.
///
/// # Errors
///
/// See [`SeedError`]; a corrupt state file is [`SeedError::Store`].
pub fn run_bind(
    accounts: &Accounts,
    account: &AccountName,
    lock_wait: Duration,
) -> Result<Bound, SeedError> {
    ensure_configured(accounts, account)?;
    let store = accounts.state_file(account);
    let lock = match store.lock(lock_wait) {
        // No account directory to hold the lock file: there is no state either.
        Err(StoreError::Io(err)) if err.kind() == io::ErrorKind::NotFound => {
            return Err(SeedError::NoState {
                account: account.clone(),
            });
        }
        other => other?,
    };
    let _bindings = accounts.lock_bindings(lock_wait)?;
    let state = store.load()?.ok_or_else(|| SeedError::NoState {
        account: account.clone(),
    })?;
    check_others(accounts, account, &state.user_id)?;
    match own_binding(&lock, account)? {
        Some(bound) if bound == state.user_id => {}
        Some(_) => {
            return Err(SeedError::BoundToAnotherUser {
                account: account.clone(),
            });
        }
        None => lock.bind(&state.user_id)?,
    }
    Ok(Bound {
        account: account.clone(),
        user_id: state.user_id,
    })
}

fn ensure_configured(accounts: &Accounts, account: &AccountName) -> Result<(), SeedError> {
    if accounts.contains(account) {
        Ok(())
    } else {
        Err(SeedError::UnknownAccount {
            account: account.clone(),
        })
    }
}

/// Binds `account` (whose state lock is `lock`) to `user` if the checks
/// allow it, and returns the user it was bound to before when that changed.
/// The caller holds the bindings lock.
fn claim(
    accounts: &Accounts,
    account: &AccountName,
    lock: &StateLock,
    user: &PelotonId,
    rebind: bool,
) -> Result<Option<PelotonId>, SeedError> {
    check_others(accounts, account, user)?;
    let previous = match own_binding(lock, account)? {
        Some(bound) if bound == *user => return Ok(None),
        Some(_) if !rebind => {
            return Err(SeedError::BoundToAnotherUser {
                account: account.clone(),
            });
        }
        previous => previous,
    };
    lock.bind(user)?;
    Ok(previous)
}

/// Refuses `user` when another configured account is bound to it, or when
/// the binding of another account cannot be read (fail closed). The caller
/// holds the bindings lock.
fn check_others(
    accounts: &Accounts,
    account: &AccountName,
    user: &PelotonId,
) -> Result<(), SeedError> {
    match peer_conflict(&accounts.peers_of(account), user) {
        None => Ok(()),
        Some((other, PeerConflict::BoundElsewhere)) => Err(SeedError::UserBoundElsewhere {
            account: other.clone(),
        }),
        Some((other, PeerConflict::Corrupt)) => Err(SeedError::BindingUnreadable {
            account: other.clone(),
            source: StoreError::Corrupt,
        }),
        Some((other, PeerConflict::Unreadable(err))) => Err(SeedError::BindingUnreadable {
            account: other.clone(),
            source: StoreError::Io(err),
        }),
    }
}

/// The user `account` is bound to, read under its state lock. An unreadable
/// binding is refused even with `--rebind`: what it named is unknown.
fn own_binding(lock: &StateLock, account: &AccountName) -> Result<Option<PelotonId>, SeedError> {
    lock.binding()
        .map(|binding| binding.map(|binding| binding.user_id))
        .map_err(|source| SeedError::BindingUnreadable {
            account: account.clone(),
            source,
        })
}

fn read_tokens(input: impl Read) -> Result<TokenSet, SeedError> {
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| SeedError::InvalidInput)?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(SeedError::InvalidInput);
    }
    // The parse error is dropped on purpose: its message can quote the input.
    serde_json::from_slice(&bytes).map_err(|_| SeedError::InvalidInput)
}

async fn fetch_me(
    http: &reqwest::Client,
    api_base: &Url,
    tokens: &TokenSet,
) -> Result<Me, SeedError> {
    let mut url = api_base.clone();
    url.path_segments_mut()
        .map_err(|()| SeedError::Upstream)?
        .pop_if_empty()
        .push("me");
    let response = http
        .get(url)
        .bearer_auth(tokens.access_token.expose_secret())
        .header(PLATFORM_HEADER.0, PLATFORM_HEADER.1)
        .send()
        .await
        .map_err(|_| SeedError::Upstream)?;
    let status = response.status();
    if !status.is_success() {
        let status = status.as_u16();
        return Err(if matches!(status, 401 | 403) {
            SeedError::Rejected { status }
        } else {
            SeedError::Unexpected { status }
        });
    }
    let body = read_limited(response)
        .await
        .map_err(|_| SeedError::Upstream)?;
    serde_json::from_slice(&body).map_err(|_| SeedError::Upstream)
}
