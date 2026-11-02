//! SPL Token: the programs, the mint layout, the associated token account,
//! the two instructions a token transfer needs, and the allowlisted [`Tokens`].

use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, bail};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};

use crate::Token;

/// The SPL Token program, the only mint owner accepted. Token-2022 is
/// refused: a confidential transfer need not report its amount.
pub(crate) const TOKEN_PROGRAM: Address =
    Address::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// The Associated Token Account program.
const ASSOCIATED_TOKEN_PROGRAM: Address =
    Address::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SYSTEM_PROGRAM: Address = Address::from_str_const("11111111111111111111111111111111");

/// `TransferChecked`: `[12, amount: u64 LE, decimals: u8]`.
const TRANSFER_CHECKED: u8 = 12;
/// `CreateIdempotent` of the Associated Token Account program.
const CREATE_IDEMPOTENT: u8 = 1;
/// Where an initialized mint keeps its decimals, then its initialization flag.
const MINT_DECIMALS: usize = 44;
const MINT_LENGTH: usize = 82;

/// The associated token account of `owner` for `mint`.
fn account(owner: &Address, mint: &Address) -> anyhow::Result<Address> {
    let seeds: [&[u8]; 3] = [owner.as_array(), TOKEN_PROGRAM.as_array(), mint.as_array()];
    let (account, _) = Address::try_find_program_address(&seeds, &ASSOCIATED_TOKEN_PROGRAM)
        .context("no associated token account address")?;
    Ok(account)
}

/// Creates `owner`'s token account of `token`, paid by `payer`, unless it
/// exists.
pub(crate) fn create_account(
    payer: &Address,
    owner: &Address,
    token: &Token,
) -> anyhow::Result<Instruction> {
    Ok(Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(account(owner, &token.mint)?, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(token.mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: vec![CREATE_IDEMPOTENT],
    })
}

/// `TransferChecked` of `units` of `token` from `from`'s token account to
/// `to`'s, signed by `from`.
pub(crate) fn transfer(
    from: &Address,
    to: &Address,
    token: &Token,
    units: u64,
) -> anyhow::Result<Instruction> {
    let mut data = vec![TRANSFER_CHECKED];
    data.extend_from_slice(&units.to_le_bytes());
    data.push(token.decimals);
    Ok(Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(account(from, &token.mint)?, false),
            AccountMeta::new_readonly(token.mint, false),
            AccountMeta::new(account(to, &token.mint)?, false),
            AccountMeta::new_readonly(*from, true),
        ],
        data,
    })
}

/// The decimals of an initialized SPL Token mint: an account `owner` owns
/// holding `data`.
///
/// # Errors
///
/// For another owner (Token-2022 included), or an uninitialized or short
/// account.
pub(crate) fn decimals(owner: &Address, data: &[u8]) -> anyhow::Result<u8> {
    if *owner != TOKEN_PROGRAM {
        bail!("not owned by the SPL Token program");
    }
    match data.get(MINT_DECIMALS..=MINT_DECIMALS + 1) {
        Some([decimals, 1]) if data.len() >= MINT_LENGTH => Ok(*decimals),
        _ => bail!("not an initialized mint"),
    }
}

/// The allowlisted SPL tokens: the only tokens indexed and sent. Cheap to
/// clone; clones share the set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tokens(Arc<BTreeMap<Address, u8>>);

impl Tokens {
    /// The tokens of `tokens`.
    #[must_use]
    pub fn new(tokens: impl IntoIterator<Item = Token>) -> Self {
        Self(Arc::new(
            tokens
                .into_iter()
                .map(|token| (token.mint, token.decimals))
                .collect(),
        ))
    }

    /// Whether `mint` is allowlisted.
    #[must_use]
    pub fn contains(&self, mint: &Address) -> bool {
        self.0.contains_key(mint)
    }

    /// The token of `asset`, when it is allowlisted; the asset's network is
    /// the caller's to check.
    #[must_use]
    pub fn get(&self, asset: &base::Asset) -> Option<Token> {
        let mint = asset.id.parse().ok()?;
        let decimals = *self.0.get(&mint)?;
        Some(Token {
            mint,
            decimals,
            ticker: None,
            display_decimals: None,
        })
    }
}
