//! One-time sign-in codes for `POST /admin/seed`, and the limit on wrong ones.
//!
//! A code is 128 random bits from the thread-local CSPRNG, written as 32
//! lowercase hex characters. Only its SHA-256 digest is kept, compared in
//! constant time. Each account holds at most one code: issuing another
//! replaces it. Codes live in memory only, so a restart invalidates them.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use secrecy::SecretString;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

use crate::account::AccountName;

/// How long a code is valid after it is issued.
pub const CODE_TTL: Duration = Duration::from_mins(10);

/// Wrong, expired or reused codes allowed within [`FAILURE_WINDOW`], across
/// all accounts, before every further attempt is refused.
pub const MAX_FAILURES: usize = 5;

/// The window [`MAX_FAILURES`] counts in.
pub const FAILURE_WINDOW: Duration = Duration::from_mins(10);

type Sha256Digest = [u8; 32];

fn digest(code: &str) -> Sha256Digest {
    Sha256::digest(code.as_bytes()).into()
}

/// Where the code of one account stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Issued and not presented yet.
    Pending,
    /// Presented with a seed that is still running.
    Claimed,
    /// Spent by a seed.
    Used,
    /// Spent by a seed the account's binding refused.
    Refused,
}

#[derive(Debug)]
struct Slot {
    digest: Sha256Digest,
    expires: Instant,
    phase: Phase,
}

/// What the status endpoint reports about an account's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeStatus {
    /// No code was issued since the relay started.
    None,
    /// Issued and still valid, or presented with a seed that is running.
    Pending,
    /// Spent by a seed.
    Used,
    /// Spent by a seed the account's binding refused.
    Refused,
    /// Expired before it was spent.
    Expired,
}

impl CodeStatus {
    /// The status as the status endpoint writes it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pending => "pending",
            Self::Used => "used",
            Self::Refused => "refused",
            Self::Expired => "expired",
        }
    }
}

/// Why a presented code is not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeRefusal {
    /// Wrong, expired, already used or for another account; counted.
    Invalid,
    /// Too many recent failures; nothing is checked until `retry_after` has
    /// passed.
    Limited {
        /// How long until the oldest counted failure leaves the window.
        retry_after: Duration,
    },
}

/// A code accepted for a seed that is now running: hand it to
/// [`Codes::spend`] or [`Codes::release`] once the seed's outcome is known.
#[derive(Debug)]
#[must_use]
pub struct Claim {
    account: AccountName,
    digest: Sha256Digest,
}

/// The codes of every account, and the recent failures.
#[derive(Debug, Default)]
pub struct Codes {
    slots: HashMap<AccountName, Slot>,
    /// When each counted failure happened, oldest first; at most
    /// [`MAX_FAILURES`].
    failures: VecDeque<Instant>,
}

impl Codes {
    /// Issues a new code for `account` at `now`, replacing any older one.
    /// The code is returned once and never kept.
    pub fn issue(&mut self, account: &AccountName, now: Instant) -> SecretString {
        let code = format!("{:032x}", rand::random::<u128>());
        self.slots.insert(
            account.clone(),
            Slot {
                digest: digest(&code),
                expires: now + CODE_TTL,
                phase: Phase::Pending,
            },
        );
        SecretString::from(code)
    }

    /// Accepts `code` for `account` at `now` if it is that account's pending
    /// code and has not expired. Any other code counts as a failure.
    ///
    /// # Errors
    ///
    /// [`CodeRefusal::Limited`] while [`MAX_FAILURES`] failures fall within
    /// [`FAILURE_WINDOW`] (checked first, and not counted itself), else
    /// [`CodeRefusal::Invalid`].
    pub fn claim(
        &mut self,
        account: &AccountName,
        code: &str,
        now: Instant,
    ) -> Result<Claim, CodeRefusal> {
        while self
            .failures
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= FAILURE_WINDOW)
        {
            self.failures.pop_front();
        }
        if self.failures.len() >= MAX_FAILURES
            && let Some(oldest) = self.failures.front()
        {
            return Err(CodeRefusal::Limited {
                retry_after: (*oldest + FAILURE_WINDOW).saturating_duration_since(now),
            });
        }
        let presented = digest(code);
        match self.slots.get_mut(account) {
            Some(slot)
                if slot.phase == Phase::Pending
                    && now < slot.expires
                    && bool::from(slot.digest.ct_eq(&presented)) =>
            {
                slot.phase = Phase::Claimed;
                Ok(Claim {
                    account: account.clone(),
                    digest: presented,
                })
            }
            _ => {
                if self.failures.len() == MAX_FAILURES {
                    self.failures.pop_front();
                }
                self.failures.push_back(now);
                Err(CodeRefusal::Invalid)
            }
        }
    }

    /// Spends the claimed code on a successful seed: it is never accepted
    /// again.
    pub fn spend(&mut self, claim: Claim) {
        if let Some(slot) = self.claimed(claim) {
            slot.phase = Phase::Used;
        }
    }

    /// Spends the claimed code on a seed that the binding refused: it is
    /// never accepted again, and its status says it was refused.
    pub fn refuse(&mut self, claim: Claim) {
        if let Some(slot) = self.claimed(claim) {
            slot.phase = Phase::Refused;
        }
    }

    /// Makes the claimed code acceptable again, unless it was replaced
    /// meanwhile; its expiry is unchanged.
    pub fn release(&mut self, claim: Claim) {
        if let Some(slot) = self.claimed(claim) {
            slot.phase = Phase::Pending;
        }
    }

    /// The slot `claim` came from, if it was not replaced since.
    fn claimed(&mut self, claim: Claim) -> Option<&mut Slot> {
        let Claim { account, digest } = claim;
        self.slots
            .get_mut(&account)
            .filter(|slot| slot.phase == Phase::Claimed && slot.digest == digest)
    }

    /// Where the code of `account` stands at `now`.
    #[must_use]
    pub fn status(&self, account: &AccountName, now: Instant) -> CodeStatus {
        match self.slots.get(account) {
            None => CodeStatus::None,
            Some(slot) => match slot.phase {
                Phase::Used => CodeStatus::Used,
                Phase::Refused => CodeStatus::Refused,
                Phase::Claimed => CodeStatus::Pending,
                Phase::Pending if now < slot.expires => CodeStatus::Pending,
                Phase::Pending => CodeStatus::Expired,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use secrecy::ExposeSecret as _;

    use super::{CODE_TTL, CodeRefusal, CodeStatus, Codes, FAILURE_WINDOW, MAX_FAILURES};
    use crate::account::AccountName;

    fn name(text: &str) -> AccountName {
        text.parse().unwrap()
    }

    #[test]
    fn codes_are_128_bit_hex_and_distinct() {
        let mut codes = Codes::default();
        let now = Instant::now();
        let first = codes.issue(&name("alice"), now);
        let second = codes.issue(&name("bob"), now);
        for code in [&first, &second] {
            let text = code.expose_secret();
            assert_eq!(text.len(), 32);
            assert!(text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        }
        assert_ne!(first.expose_secret(), second.expose_secret());
    }

    #[test]
    fn a_code_is_accepted_once_for_its_account() {
        let mut codes = Codes::default();
        let now = Instant::now();
        let code = codes.issue(&name("bob"), now);
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Pending);
        assert_eq!(
            codes
                .claim(&name("alice"), code.expose_secret(), now)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
        let claim = codes
            .claim(&name("bob"), code.expose_secret(), now)
            .unwrap();
        // A second presentation while the seed runs is refused.
        assert_eq!(
            codes
                .claim(&name("bob"), code.expose_secret(), now)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Pending);
        codes.spend(claim);
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Used);
        assert_eq!(
            codes
                .claim(&name("bob"), code.expose_secret(), now)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
    }

    #[test]
    fn a_refused_code_is_spent() {
        let mut codes = Codes::default();
        let now = Instant::now();
        let code = codes.issue(&name("bob"), now);
        let claim = codes
            .claim(&name("bob"), code.expose_secret(), now)
            .unwrap();
        codes.refuse(claim);
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Refused);
        assert_eq!(
            codes
                .claim(&name("bob"), code.expose_secret(), now)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
    }

    #[test]
    fn a_released_code_can_be_presented_again() {
        let mut codes = Codes::default();
        let now = Instant::now();
        let code = codes.issue(&name("bob"), now);
        let claim = codes
            .claim(&name("bob"), code.expose_secret(), now)
            .unwrap();
        codes.release(claim);
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Pending);
        assert!(codes.claim(&name("bob"), code.expose_secret(), now).is_ok());
    }

    #[test]
    fn a_new_code_replaces_the_old_one() {
        let mut codes = Codes::default();
        let now = Instant::now();
        let old = codes.issue(&name("bob"), now);
        let claim = codes.claim(&name("bob"), old.expose_secret(), now).unwrap();
        let new = codes.issue(&name("bob"), now);
        // The seed of the old code ends after the new one was issued: the
        // new code stays pending either way.
        codes.spend(claim);
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::Pending);
        assert_eq!(
            codes
                .claim(&name("bob"), old.expose_secret(), now)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
        assert!(codes.claim(&name("bob"), new.expose_secret(), now).is_ok());
    }

    #[test]
    fn a_code_expires_after_ten_minutes() {
        let mut codes = Codes::default();
        let now = Instant::now();
        assert_eq!(codes.status(&name("bob"), now), CodeStatus::None);
        let code = codes.issue(&name("bob"), now);
        let just_before = (now + CODE_TTL)
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        assert_eq!(codes.status(&name("bob"), just_before), CodeStatus::Pending);
        let at_expiry = now + CODE_TTL;
        assert_eq!(codes.status(&name("bob"), at_expiry), CodeStatus::Expired);
        assert_eq!(
            codes
                .claim(&name("bob"), code.expose_secret(), at_expiry)
                .unwrap_err(),
            CodeRefusal::Invalid
        );
        let fresh = codes.issue(&name("bob"), at_expiry);
        assert!(
            codes
                .claim(&name("bob"), fresh.expose_secret(), just_before + CODE_TTL)
                .is_ok()
        );
    }

    #[test]
    fn failures_lock_every_account_out_until_the_window_passes() {
        let mut codes = Codes::default();
        let start = Instant::now();
        for second in 0..MAX_FAILURES {
            let at = start + Duration::from_secs(u64::try_from(second).unwrap());
            assert_eq!(
                codes.claim(&name("alice"), "wrong", at).unwrap_err(),
                CodeRefusal::Invalid
            );
        }
        // Even the right code of another account is refused now, unchecked.
        let locked = start + Duration::from_secs(30);
        let code = codes.issue(&name("bob"), locked);
        assert_eq!(
            codes
                .claim(&name("bob"), code.expose_secret(), locked)
                .unwrap_err(),
            CodeRefusal::Limited {
                retry_after: FAILURE_WINDOW.checked_sub(Duration::from_secs(30)).unwrap()
            }
        );
        // Refused attempts do not extend the lockout: once the first failure
        // leaves the window, one more attempt is checked.
        let reopened = start + FAILURE_WINDOW;
        assert!(
            codes
                .claim(&name("bob"), code.expose_secret(), reopened)
                .is_ok()
        );
    }

    #[test]
    fn the_lockout_is_retried_after_the_oldest_failure() {
        let mut codes = Codes::default();
        let start = Instant::now();
        for _ in 0..MAX_FAILURES {
            let _ = codes.claim(&name("bob"), "wrong", start);
        }
        let reopened = start + FAILURE_WINDOW;
        assert_eq!(
            codes.claim(&name("bob"), "wrong", reopened).unwrap_err(),
            CodeRefusal::Invalid
        );
        // Every failure at `start` left the window; counting starts over.
        for _ in 1..MAX_FAILURES {
            assert_eq!(
                codes.claim(&name("bob"), "wrong", reopened).unwrap_err(),
                CodeRefusal::Invalid
            );
        }
        assert!(matches!(
            codes.claim(&name("bob"), "wrong", reopened).unwrap_err(),
            CodeRefusal::Limited { .. }
        ));
    }
}
