//! SPL Token: the programs this crate reads, the mint and token-account
//! layouts it decodes, and the transfer instructions it attributes.

use base::MovementKind;

use super::{
    Account, Address, AssetKind, Error, ErrorKind, Lamport,
    block::{AccountKey, Instruction, Movement, TokenBalance},
};

/// `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`: the SPL Token program, the
/// only mint owner this integration accepts.
pub const TOKEN_PROGRAM: [u8; 32] = [
    0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93, 0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
    0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91, 0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
];

/// `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`: Token-2022, whose
/// confidential-transfer extension need not report the amounts this crate
/// indexes. Its transfers are decoded when a block names them, but a mint of
/// this program is refused as a configured asset.
pub const TOKEN_2022_PROGRAM: [u8; 32] = [
    0x06, 0xdd, 0xf6, 0xe1, 0xee, 0x75, 0x8f, 0xde, 0x18, 0x42, 0x5d, 0xbc, 0xe4, 0x6c, 0xcd, 0xda,
    0xb6, 0x1a, 0xfc, 0x4d, 0x83, 0xb9, 0x0d, 0x27, 0xfe, 0xbd, 0xf9, 0x28, 0xd8, 0xa1, 0x8b, 0xfc,
];

/// Instruction tag of `Transfer`: `[3, amount: u64]`, over
/// `[source, destination, authority]`.
const TRANSFER: u8 = 3;
/// Instruction tag of `TransferChecked`: `[12, amount: u64, decimals: u8]`,
/// over `[source, mint, destination, authority]`.
const TRANSFER_CHECKED: u8 = 12;

/// Byte offsets of the mint layout this crate reads: `decimals` and the
/// initialization flag that follows it, in an 82-byte mint account.
const MINT_DECIMALS: usize = 44;
/// The smallest valid mint account; Token-2022 extensions follow these bytes.
const MINT_LENGTH: usize = 82;
/// Byte offsets of the token-account layout: the mint, the owner, the amount,
/// and the account state, in a 165-byte token account.
const ACCOUNT_LENGTH: usize = 165;

/// One SPL token movement an instruction names outright: the token accounts
/// it debits and credits, as indexes into its transaction's account keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TokenTransfer {
    pub(crate) source: usize,
    pub(crate) destination: usize,
    pub(crate) amount: Lamport,
}

impl Instruction {
    /// The token accounts and amount this instruction moves when it is an SPL
    /// Token `Transfer` or `TransferChecked`; `None` for anything else.
    ///
    /// Nothing here rejects a block. An instruction this crate cannot read is
    /// one it cannot attribute, and the transaction's own token balances still
    /// account for every unit it moved.
    pub(crate) fn spl_transfer(&self, keys: &[AccountKey]) -> Option<TokenTransfer> {
        let program = keys.get(self.program)?.address.as_bytes();
        if *program != TOKEN_PROGRAM && *program != TOKEN_2022_PROGRAM {
            return None;
        }
        let data = bs58::decode(&self.data).into_vec().ok()?;
        let (source, destination) = match (*data.first()?, self.accounts.as_slice()) {
            (TRANSFER, [source, destination, ..]) if data.len() == 9 => (*source, *destination),
            (TRANSFER_CHECKED, [source, _, destination, ..]) if data.len() == 10 => {
                (*source, *destination)
            }
            _ => return None,
        };
        let amount = u64::from_le_bytes(data[1..9].try_into().ok()?);
        Some(TokenTransfer {
            source,
            destination,
            amount: Lamport::from_atomic(amount),
        })
    }
}

impl TokenTransfer {
    /// The movement this transfer records between the owners of its two token
    /// accounts, as `balances` reports them.
    ///
    /// `None` unless the transaction reports both token accounts in one mint:
    /// an endpoint it does not report cannot be attributed to an owner, so the
    /// units fall out as residuals of whichever side it does report.
    pub(crate) fn movement(&self, id: String, balances: &[TokenBalance]) -> Option<Movement> {
        let source = TokenBalance::at(balances, self.source)?;
        let destination = TokenBalance::at(balances, self.destination)?;
        if source.mint != destination.mint || self.amount == Lamport::ZERO {
            return None;
        }
        Some(Movement {
            id,
            kind: MovementKind::Transfer,
            asset: AssetKind::SplToken(source.mint.clone()),
            amount: self.amount,
            from: Some(source.owner.clone()),
            to: Some(destination.owner.clone()),
        })
    }
}

impl Account {
    /// The decimals this account declares as an SPL Token mint.
    ///
    /// The chain is the authority: a configured token states the decimals it
    /// expects and this answer decides whether the deployment may run.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Unsupported`] for a Token-2022 mint, and
    /// [`ErrorKind::InvalidConfiguration`] for an account that is not an
    /// initialized SPL Token mint.
    pub fn mint_decimals(&self) -> Result<u8, Error> {
        if *self.owner().as_bytes() == TOKEN_2022_PROGRAM {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "Token-2022 mints are unsupported: a confidential transfer need not \
                 report the amount it moves",
            ));
        }
        if *self.owner().as_bytes() != TOKEN_PROGRAM {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana mint account is not owned by the SPL Token program",
            ));
        }
        let data = self.data();
        match data.get(MINT_DECIMALS..=MINT_DECIMALS + 1) {
            Some([decimals, 1]) if data.len() >= MINT_LENGTH => Ok(*decimals),
            _ => Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "Solana mint account is not an initialized SPL Token mint",
            )),
        }
    }

    /// The units this account holds as an initialized token account of `mint`
    /// owned by `owner`; `None` for any other account shape.
    #[must_use]
    pub fn token_units(&self, mint: &Address, owner: &Address) -> Option<Lamport> {
        let program = *self.owner().as_bytes();
        let data = self.data();
        if program != TOKEN_PROGRAM && program != TOKEN_2022_PROGRAM
            || data.len() < ACCOUNT_LENGTH
            || data[108] == 0
            || data[..32] != *mint.as_bytes()
            || data[32..64] != *owner.as_bytes()
        {
            return None;
        }
        let units = u64::from_le_bytes(data[64..72].try_into().ok()?);
        Some(Lamport::from_atomic(units))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 165-byte token account of `mint` owned by `owner` holding `units`.
    fn token_account(program: [u8; 32], mint: &Address, owner: &Address, units: u64) -> Account {
        let mut data = vec![0; ACCOUNT_LENGTH];
        data[..32].copy_from_slice(mint.as_bytes());
        data[32..64].copy_from_slice(owner.as_bytes());
        data[64..72].copy_from_slice(&units.to_le_bytes());
        data[108] = 1;
        Account::new(
            Address::from_bytes(program),
            Lamport::from_atomic(2_039_280),
            false,
            data,
        )
    }

    fn mint(program: [u8; 32], decimals: u8, initialized: u8) -> Account {
        let mut data = vec![0; MINT_LENGTH];
        data[MINT_DECIMALS] = decimals;
        data[MINT_DECIMALS + 1] = initialized;
        Account::new(
            Address::from_bytes(program),
            Lamport::from_atomic(1_461_600),
            false,
            data,
        )
    }

    #[test]
    fn program_constants_are_the_canonical_base58_program_addresses() {
        assert_eq!(
            Address::from_bytes(TOKEN_PROGRAM).to_string(),
            "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        );
        assert_eq!(
            Address::from_bytes(TOKEN_2022_PROGRAM).to_string(),
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        );
    }

    #[test]
    fn mint_decimals_come_from_the_chain_and_refuse_token_2022() {
        assert_eq!(mint(TOKEN_PROGRAM, 6, 1).mint_decimals(), Ok(6));
        assert_eq!(mint(TOKEN_PROGRAM, 0, 1).mint_decimals(), Ok(0));
        let confidential = mint(TOKEN_2022_PROGRAM, 6, 1)
            .mint_decimals()
            .expect_err("Token-2022 must be refused, not indexed as zero");
        assert_eq!(confidential.kind, ErrorKind::Unsupported);
        assert!(!confidential.retryable);
        for account in [
            mint(TOKEN_PROGRAM, 6, 0),
            mint([0; 32], 6, 1),
            Account::new(
                Address::from_bytes(TOKEN_PROGRAM),
                Lamport::ZERO,
                false,
                vec![0; MINT_LENGTH - 1],
            ),
        ] {
            assert_eq!(
                account.mint_decimals().unwrap_err().kind,
                ErrorKind::InvalidConfiguration
            );
        }
    }

    #[test]
    fn token_units_require_the_named_mint_owner_and_an_initialized_account() {
        let (mint, owner) = (Address::from_bytes([3; 32]), Address::from_bytes([4; 32]));
        let other = Address::from_bytes([5; 32]);
        for program in [TOKEN_PROGRAM, TOKEN_2022_PROGRAM] {
            let account = token_account(program, &mint, &owner, 1_500_000);
            assert_eq!(
                account.token_units(&mint, &owner),
                Some(Lamport::from_atomic(1_500_000))
            );
            assert_eq!(account.token_units(&other, &owner), None);
            assert_eq!(account.token_units(&mint, &other), None);
        }
        let initialized = token_account(TOKEN_PROGRAM, &mint, &owner, 7);
        let mut data = initialized.data().to_vec();
        data[108] = 0;
        let uninitialized = Account::new(
            initialized.owner().clone(),
            initialized.lamports(),
            false,
            data,
        );
        assert_eq!(uninitialized.token_units(&mint, &owner), None);
        assert_eq!(
            initialized.token_units(&mint, &owner),
            Some(Lamport::from_atomic(7))
        );
        let foreign = token_account([9; 32], &mint, &owner, 7);
        assert_eq!(foreign.token_units(&mint, &owner), None);
    }
}
