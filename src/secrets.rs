//! Passwords: kept in the system's keychain, asked for each start, or
//! printed by a command of yours; for accounts that sign in with Microsoft
//! or Google here (`oauth.rs`), the refresh token instead, in the keychain,
//! and access tokens got with it, in memory. Never written to a file by
//! tuimail, never on a command line, never in a message or log.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::account::{Account, Auth, PasswordFrom};
use crate::oauth::{self, Tokens};
use crate::text;

/// The keychain's name for tuimail's entries.
const SERVICE: &str = "tuimail";
/// How long a password command may take without a terminal (with one, at
/// start, it may wait for you to unlock something).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const INTERACTIVE_TIMEOUT: Duration = Duration::from_secs(300);
/// A command's output past this is refused, not read.
const MAX_OUTPUT: u64 = 64 * 1024;

/// A password or token. Its `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// Picks the system's keychain as where entries go. Returns whether there
/// is one: without (Linux with no Secret Service), passwords are asked for.
pub fn init_keychain() -> bool {
    #[cfg(target_os = "macos")]
    let store = apple_native_keyring_store::keychain::Store::new()
        .map(|s| s as Arc<keyring_core::CredentialStore>);
    #[cfg(windows)]
    let store =
        windows_native_keyring_store::Store::new().map(|s| s as Arc<keyring_core::CredentialStore>);
    #[cfg(all(unix, not(target_os = "macos")))]
    let store = zbus_secret_service_keyring_store::Store::new()
        .map(|s| s as Arc<keyring_core::CredentialStore>);
    match store {
        Ok(store) => {
            keyring_core::set_default_store(store);
            true
        }
        Err(_) => false,
    }
}

fn entry(id: &str) -> Result<keyring_core::Entry> {
    keyring_core::Entry::new(SERVICE, id).map_err(|e| anyhow!("the keychain: {e}"))
}

/// The keychain entry of an account's refresh token: its own, apart from
/// the password's, so one is never sent as the other (to the provider as a
/// refresh token, or to a server as a password) when an account changes
/// how it logs in.
pub fn token_key(id: &str) -> String {
    format!("{id}#refresh-token")
}

/// The account's password from the keychain, or `None` if none is kept.
pub async fn keychain_get(id: &str) -> Result<Option<Secret>> {
    let id = id.to_string();
    tokio::task::spawn_blocking(move || match entry(&id)?.get_password() {
        Ok(p) => Ok(Some(Secret(p))),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow!("cannot read the keychain: {e}")),
    })
    .await?
}

pub async fn keychain_set(id: &str, secret: &Secret) -> Result<()> {
    let (id, secret) = (id.to_string(), secret.clone());
    tokio::task::spawn_blocking(move || {
        entry(&id)?
            .set_password(secret.expose())
            .map_err(|e| anyhow!("cannot save the password in the keychain: {e}"))
    })
    .await?
}

pub async fn keychain_delete(id: &str) -> Result<()> {
    let id = id.to_string();
    tokio::task::spawn_blocking(move || match entry(&id)?.delete_credential() {
        Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
        Err(e) => Err(anyhow!("cannot remove the password from the keychain: {e}")),
    })
    .await?
}

/// Runs `argv` (a full path and its arguments, no shell) and returns the
/// first line it prints. With `interactive`, it shares the terminal (before
/// tuimail takes it over), so a program can ask to be unlocked; otherwise it
/// gets no input and its errors are thrown away.
pub async fn run_command(argv: &[String], interactive: bool) -> Result<Secret> {
    let program = argv.first().context("password_command is empty")?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    // Not wherever tuimail was started: a script there could be picked up
    // by the program as its own.
    if let Some(home) = dirs::home_dir() {
        command.current_dir(home);
    }
    if interactive {
        command.stdin(Stdio::inherit()).stderr(Stdio::inherit());
    } else {
        command.stdin(Stdio::null()).stderr(Stdio::null());
    }
    let shown = text::one_line(program, 80);
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot run {shown}"))?;
    let mut stdout = child.stdout.take().context("no output")?;
    let timeout = if interactive {
        INTERACTIVE_TIMEOUT
    } else {
        COMMAND_TIMEOUT
    };
    let run = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(MAX_OUTPUT + 1)
            .read_to_end(&mut out)
            .await?;
        let status = child.wait().await?;
        anyhow::Ok((out, status))
    };
    let (out, status) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| anyhow!("{shown} took too long"))??;
    if !status.success() {
        bail!("{shown} failed ({status})");
    }
    if out.len() as u64 > MAX_OUTPUT {
        bail!("{shown} printed too much to be a password");
    }
    let text =
        String::from_utf8(out).map_err(|_| anyhow!("{shown} printed something that isn't text"))?;
    let line = text.lines().next().unwrap_or("").trim_end_matches('\r');
    if line.is_empty() {
        bail!("{shown} printed no password");
    }
    Ok(Secret(line.to_string()))
}

/// An account's password, shared by its connections: found once, asked
/// for again when the server refuses it.
#[derive(Clone)]
pub struct Credentials {
    account: Arc<Account>,
    id: String,
    secret: Arc<Mutex<Option<Secret>>>,
    /// The keychain can be used.
    keychain: bool,
    /// The server refused the password the keychain gave: it isn't read
    /// from there again until a new one is typed, so a wrong password isn't
    /// tried again and again (which can lock the account).
    keychain_refused: Arc<std::sync::atomic::AtomicBool>,
    /// Signing in here: the tokens got, the access token renewed with the
    /// refresh token when it runs out; held only a moment (never while a
    /// refresh is out), with how many sign-ins set them, so a refresh that
    /// ends after a sign-in doesn't replace its tokens.
    tokens: Arc<std::sync::Mutex<(u64, Option<Tokens>)>>,
    /// Held while refreshing: one refresh at a time.
    refreshing: Arc<Mutex<()>>,
}

/// No password is known: tuimail must ask.
#[derive(Debug)]
pub struct NeedPassword;

impl std::fmt::Display for NeedPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the password is needed")
    }
}

impl std::error::Error for NeedPassword {}

impl Credentials {
    pub fn new(account: Arc<Account>, keychain: bool) -> Self {
        let id = account.id();
        Self {
            account,
            id,
            secret: Arc::new(Mutex::new(None)),
            keychain,
            keychain_refused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            tokens: Arc::new(std::sync::Mutex::new((0, None))),
            refreshing: Arc::new(Mutex::new(())),
        }
    }

    /// The keychain entry for what this account keeps there: its password,
    /// or its refresh token when it signs in here ([`token_key`]).
    pub fn keychain_key(&self) -> String {
        if self.signs_in() {
            token_key(&self.id)
        } else {
            self.id.clone()
        }
    }

    fn tokens(&self) -> std::sync::MutexGuard<'_, (u64, Option<Tokens>)> {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn is_refused(&self) -> bool {
        self.keychain_refused
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn set_refused(&self, refused: bool) {
        self.keychain_refused
            .store(refused, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether the password comes from a command, run for each login.
    fn uses_command(&self) -> bool {
        self.account.password == PasswordFrom::Command
            || (self.account.auth == Auth::Oauth2 && !self.signs_in())
    }

    /// Whether the account signs in with Microsoft or Google here: its
    /// "password" is a token got with the refresh token.
    pub fn signs_in(&self) -> bool {
        oauth::built_in(&self.account)
    }

    /// Whether the keychain keeps this account's password.
    pub fn uses_keychain(&self) -> bool {
        self.keychain && self.account.password == PasswordFrom::Keychain && !self.uses_command()
    }

    /// The password to log in with: kept in memory, else from the
    /// keychain, else the command's; `NeedPassword` if it must be asked.
    pub async fn get(&self) -> Result<Secret> {
        if self.signs_in() {
            return self.access_token().await;
        }
        if self.uses_command() {
            // After a refusal, no login until the user asks again (R):
            // trying the same refused password at every check can lock the
            // account.
            if self.is_refused() {
                return Err(anyhow!(NeedPassword));
            }
            // A token may be short-lived: the command is asked each time.
            return run_command(&self.account.password_command, false).await;
        }
        let mut kept = self.secret.lock().await;
        if let Some(secret) = kept.as_ref() {
            return Ok(secret.clone());
        }
        if self.uses_keychain()
            && !self.is_refused()
            && let Some(secret) = keychain_get(&self.id).await?
        {
            *kept = Some(secret.clone());
            return Ok(secret);
        }
        Err(anyhow!(NeedPassword))
    }

    /// An access token: the one in memory while it lasts, else a new one
    /// for the refresh token (in memory, else the keychain's).
    /// `NeedPassword` if there's none, or the provider took it back: the
    /// user signs in again.
    async fn access_token(&self) -> Result<Secret> {
        if self.is_refused() {
            return Err(anyhow!(NeedPassword));
        }
        // One refresh at a time: the account's connections wait for it
        // rather than each making its own.
        let _one = self.refreshing.lock().await;
        // Again: the refresh waited for may have found the sign-in taken
        // back, and the revoked token mustn't be tried once more.
        if self.is_refused() {
            return Err(anyhow!(NeedPassword));
        }
        let (signed_in, refresh) = {
            let kept = self.tokens();
            if let Some(tokens) = kept.1.as_ref().filter(|t| t.fresh()) {
                return Ok(tokens.access.clone());
            }
            (kept.0, kept.1.as_ref().and_then(|t| t.refresh.clone()))
        };
        let refresh = match refresh {
            Some(refresh) => refresh,
            None if self.uses_keychain() => keychain_get(&self.keychain_key())
                .await?
                .ok_or_else(|| anyhow!(NeedPassword))?,
            None => return Err(anyhow!(NeedPassword)),
        };
        let client = oauth::client_for(&self.account)
            .ok_or_else(|| anyhow!("this account can't sign in: see OAuth2 in tuimail's README"))?;
        let result = oauth::refresh(&client, &refresh).await;
        let mut kept = self.tokens();
        // Signed in again while this was out: those tokens stay.
        if kept.0 != signed_in {
            return (kept.1.as_ref())
                .map(|t| t.access.clone())
                .ok_or_else(|| anyhow!(NeedPassword));
        }
        match result {
            Ok(mut tokens) => {
                // Not every refresh brings a new refresh token.
                if tokens.refresh.is_none() {
                    tokens.refresh = Some(refresh);
                }
                let access = tokens.access.clone();
                kept.1 = Some(tokens);
                Ok(access)
            }
            Err(e) if e.downcast_ref::<oauth::SignInAgain>().is_some() => {
                kept.1 = None;
                self.set_refused(true);
                Err(anyhow!(NeedPassword))
            }
            Err(e) => Err(e),
        }
    }

    /// Uses the tokens of a sign-in from now on.
    pub fn set_tokens(&self, tokens: Tokens) {
        let mut kept = self.tokens();
        kept.0 += 1;
        kept.1 = Some(tokens);
        self.set_refused(false);
    }

    /// Uses `secret` from now on (typed in tuimail).
    pub async fn set(&self, secret: Secret) {
        *self.secret.lock().await = Some(secret);
        self.set_refused(false);
    }

    /// The user asks to try again (R): a password command is run again
    /// after a refusal.
    pub fn retry(&self) {
        if self.uses_command() || self.signs_in() {
            self.set_refused(false);
        }
    }

    /// The server refused `used`: it's forgotten, and not read from the
    /// keychain again, so tuimail asks; a password command isn't run again
    /// until the user asks (R). Only if it's still the one in use: a
    /// password typed, or a sign-in made, while that login was out stays.
    pub async fn refused(&self, used: &Secret) {
        if self.signs_in() {
            let mut kept = self.tokens();
            if kept.1.as_ref().is_none_or(|t| t.access == *used) {
                kept.1 = None;
                self.set_refused(true);
            }
            return;
        }
        let mut kept = self.secret.lock().await;
        if kept.as_ref().is_none_or(|s| s == used) {
            *kept = None;
            self.set_refused(true);
        }
    }

    /// Keeps the password in the keychain, once the server took it (`used`).
    /// Signing in here, the refresh token is what's kept: the one that came
    /// with the access token the server took, not one no login has used.
    pub async fn remember(&self, used: &Secret) -> Result<()> {
        if !self.uses_keychain() {
            return Ok(());
        }
        let kept = if self.signs_in() {
            (self.tokens().1.as_ref())
                .filter(|t| t.access == *used)
                .and_then(|t| t.refresh.clone())
        } else {
            self.secret.lock().await.clone().filter(|s| s == used)
        };
        let Some(secret) = kept else {
            return Ok(());
        };
        let key = self.keychain_key();
        if keychain_get(&key).await.ok().flatten().as_ref() == Some(&secret) {
            return Ok(());
        }
        keychain_set(&key, &secret).await
    }

    pub fn account(&self) -> &Account {
        &self.account
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_never_shows_in_debug_output() {
        let secret = Secret::new("hunter2");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_gives_its_first_line_and_must_succeed() {
        let sh = |script: &str| vec!["/bin/sh".to_string(), "-c".into(), script.into()];
        let secret = run_command(&sh("printf 'pa ss\\nmore\\n'"), false)
            .await
            .unwrap();
        assert_eq!(secret.expose(), "pa ss");
        assert!(run_command(&sh("exit 3"), false).await.is_err());
        assert!(
            run_command(&sh("printf ''"), false).await.is_err(),
            "nothing printed"
        );
        assert!(
            run_command(&sh("head -c 100000 /dev/zero"), false)
                .await
                .is_err(),
            "too much"
        );
        assert!(
            run_command(&["/nonexistent/x".into()], false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn signing_in_gives_its_access_token_until_the_server_refuses_it() {
        let account = Account {
            email: "me@outlook.com".into(),
            host: "outlook.office365.com".into(),
            auth: Auth::Oauth2,
            oauth_client_id: "my-app".into(),
            ..Account::default()
        };
        let creds = Credentials::new(Arc::new(account), false);
        assert!(creds.signs_in());
        let need = |r: Result<Secret>| r.unwrap_err().downcast_ref::<NeedPassword>().is_some();
        assert!(need(creds.get().await), "not signed in yet");
        let tokens = |access: &str| Tokens {
            access: Secret::new(access),
            refresh: Some(Secret::new("rt")),
            renew_at: std::time::Instant::now() + Duration::from_secs(600),
        };
        creds.set_tokens(tokens("at"));
        let used = creds.get().await.unwrap();
        assert_eq!(used.expose(), "at");
        // A sign-in while that login was out: the refusal of the old token
        // doesn't throw the new one away.
        creds.set_tokens(tokens("at-2"));
        creds.refused(&used).await;
        assert_eq!(creds.get().await.unwrap().expose(), "at-2");
        // Refused: forgotten, and the user signs in again.
        creds.refused(&Secret::new("at-2")).await;
        assert!(need(creds.get().await));
        creds.retry();
        assert!(need(creds.get().await), "nothing to refresh with");
        // Its refresh token has its own keychain entry, not the password's.
        assert_eq!(creds.keychain_key(), token_key(&creds.id));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_accounts_run_their_command_for_each_login() {
        let account = Account {
            email: "me@example.org".into(),
            auth: Auth::Oauth2,
            password_command: vec!["/bin/echo".into(), "token-1".into()],
            ..Account::default()
        };
        let creds = Credentials::new(Arc::new(account), false);
        let used = creds.get().await.unwrap();
        assert_eq!(used.expose(), "token-1");
        // Refused: not run again until the user asks.
        creds.refused(&used).await;
        assert!(
            creds
                .get()
                .await
                .unwrap_err()
                .downcast_ref::<NeedPassword>()
                .is_some()
        );
        creds.retry();
        assert_eq!(creds.get().await.unwrap().expose(), "token-1");
        let ask = Account {
            email: "me@example.org".into(),
            password: PasswordFrom::Ask,
            ..Account::default()
        };
        let creds = Credentials::new(Arc::new(ask), false);
        assert!(
            creds
                .get()
                .await
                .unwrap_err()
                .downcast_ref::<NeedPassword>()
                .is_some()
        );
        creds.set(Secret::new("typed")).await;
        let used = creds.get().await.unwrap();
        assert_eq!(used.expose(), "typed");
        // Typed again while the login with the first was out: kept.
        creds.set(Secret::new("typed again")).await;
        creds.refused(&used).await;
        assert_eq!(creds.get().await.unwrap().expose(), "typed again");
        creds.refused(&Secret::new("typed again")).await;
        assert!(creds.get().await.is_err());
    }
}
