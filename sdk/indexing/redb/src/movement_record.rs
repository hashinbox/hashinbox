use indexing::{IndexError, MovementId, ValueMovement};

use crate::record::{AddressRecord, AssetRecord, MovementRecord, MovementTag};

impl MovementRecord {
    pub(super) fn from_domain(value: &ValueMovement) -> Self {
        let (kind, from, to) = match value {
            ValueMovement::Transfer { from, to, .. } => (
                MovementTag::Transfer,
                Some(AddressRecord::from_domain(from)),
                Some(AddressRecord::from_domain(to)),
            ),
            ValueMovement::Input { owner, .. } => (
                MovementTag::Input,
                owner.as_ref().map(AddressRecord::from_domain),
                None,
            ),
            ValueMovement::Output { owner, .. } => (
                MovementTag::Output,
                None,
                owner.as_ref().map(AddressRecord::from_domain),
            ),
            ValueMovement::Mint { to, .. } => (
                MovementTag::Mint,
                None,
                Some(AddressRecord::from_domain(to)),
            ),
            ValueMovement::Burn { from, .. } => (
                MovementTag::Burn,
                Some(AddressRecord::from_domain(from)),
                None,
            ),
        };
        Self {
            kind,
            id: value.id().0.clone(),
            asset: AssetRecord::from_domain(value.asset()),
            amount: value.amount().to_string(),
            from,
            to,
        }
    }

    pub(super) fn into_domain(self) -> Result<ValueMovement, IndexError> {
        let id = MovementId(self.id);
        let asset = self.asset.into_domain();
        let amount = crate::amount_record::decode(&self.amount)?;
        Ok(match (self.kind, self.from, self.to) {
            (MovementTag::Transfer, Some(from), Some(to)) => ValueMovement::Transfer {
                id,
                asset,
                amount,
                from: from.into_domain(),
                to: to.into_domain(),
            },
            (MovementTag::Input, owner, None) => ValueMovement::Input {
                id,
                asset,
                amount,
                owner: owner.map(AddressRecord::into_domain),
            },
            (MovementTag::Output, None, owner) => ValueMovement::Output {
                id,
                asset,
                amount,
                owner: owner.map(AddressRecord::into_domain),
            },
            (MovementTag::Mint, None, Some(to)) => ValueMovement::Mint {
                id,
                asset,
                amount,
                to: to.into_domain(),
            },
            (MovementTag::Burn, Some(from), None) => ValueMovement::Burn {
                id,
                asset,
                amount,
                from: from.into_domain(),
            },
            _ => {
                return Err(crate::Repository::record_error(
                    "movement record shape is invalid",
                ));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use base::Decimal;
    use indexing::{AssetId, CanonicalAddress, ChainId, IndexScope};

    use super::*;

    fn address(value: &str) -> CanonicalAddress {
        CanonicalAddress {
            scope: IndexScope {
                chain: ChainId("chain".into()),
                network: "test".into(),
            },
            value: value.into(),
        }
    }

    fn asset() -> AssetId {
        AssetId {
            chain: ChainId("chain".into()),
            asset: "native".into(),
        }
    }

    #[test]
    fn every_movement_shape_round_trips() {
        let amount = Decimal::from(7_u64);
        let movements = vec![
            ValueMovement::Transfer {
                id: MovementId("transfer".into()),
                asset: asset(),
                amount: amount.clone(),
                from: address("from"),
                to: address("to"),
            },
            ValueMovement::Input {
                id: MovementId("input".into()),
                asset: asset(),
                amount: amount.clone(),
                owner: Some(address("owner")),
            },
            ValueMovement::Output {
                id: MovementId("output".into()),
                asset: asset(),
                amount: amount.clone(),
                owner: None,
            },
            ValueMovement::Mint {
                id: MovementId("mint".into()),
                asset: asset(),
                amount: amount.clone(),
                to: address("to"),
            },
            ValueMovement::Burn {
                id: MovementId("burn".into()),
                asset: asset(),
                amount,
                from: address("from"),
            },
        ];

        for movement in movements {
            assert_eq!(
                MovementRecord::from_domain(&movement)
                    .into_domain()
                    .expect("movement record"),
                movement
            );
        }
    }

    #[test]
    fn malformed_amount_precedes_invalid_movement_shape() {
        for (amount, expected) in [
            ("bad", "stored amount is not a valid decimal"),
            ("01", "stored amount is not canonical"),
            ("-1", "stored monetary amount must not be negative"),
            ("1", "movement record shape is invalid"),
        ] {
            let record = MovementRecord {
                kind: MovementTag::Transfer,
                id: "invalid".into(),
                asset: AssetRecord::from_domain(&asset()),
                amount: amount.into(),
                from: None,
                to: None,
            };
            let error = record.into_domain().unwrap_err();
            assert_eq!(error.kind, indexing::IndexErrorKind::Store);
            assert_eq!(error.message, expected);
            assert!(!error.retryable);
        }
    }
}
