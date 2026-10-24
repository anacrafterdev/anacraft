//! What the pages ask about the person in front of them, answered from this
//! machine: the CLI's tokens, its config file, its subscription record.

use anyhow::Result;

use crate::auth::{Account, Auth, Tokens};
use crate::config::Config;
use crate::ga::Ga;
use crate::license::{self, Tier};

/// The machine `craft serve` runs on, as the handlers reach it. A value
/// rather than free functions so a handler names what it reads from.
#[derive(Clone, Copy)]
pub(super) struct Ctx;

impl Ctx {
    pub fn has_tokens(&self) -> Result<bool> {
        Ok(Tokens::load()?.is_some())
    }

    pub fn account(&self) -> Result<Option<Account>> {
        Auth::account()
    }

    pub fn config(&self) -> Result<Config> {
        Config::load()
    }

    pub fn save(&self, cfg: Config) -> Result<()> {
        cfg.save()
    }

    /// The plan, the way every command asks.
    pub async fn tier(&self) -> Result<Option<Tier>> {
        Ok(license::sync(&Config::load()?).await)
    }

    pub fn ga(&self) -> Result<Ga> {
        Ga::new()
    }
}
