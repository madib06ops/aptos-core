// Copyright (c) Aptos Foundation
// Licensed pursuant to the Innovation-Enabling Source Code License, available at https://github.com/aptos-labs/aptos-core/blob/main/LICENSE

//! Minimal gas policy. Execution is metered with MonoMove's own placeholder
//! block costs against the transaction's gas limit; everything else is left
//! for the real gas implementation.
//
// TODO(metering): intrinsic gas, IO gas per write/event, storage fees and
// refunds, and calibration of MonoMove's execution costs are all missing.

use aptos_types::fee_statement::FeeStatement;

/// Gas accounting for one transaction: the user payload's budget and what was
/// consumed out of it.
pub(crate) struct TxnGas {
    max_gas_amount: u64,
    used: u64,
}

impl TxnGas {
    pub fn new(max_gas_amount: u64) -> Self {
        Self {
            max_gas_amount,
            used: 0,
        }
    }

    /// The user payload's execution budget, in MonoMove gas units.
    ///
    /// TODO(metering): MonoMove units are uncalibrated; 1:1 against v1 gas
    /// units is a placeholder.
    pub fn payload_budget(&self) -> u64 {
        self.max_gas_amount
    }

    /// Records the payload's consumption given the meter's remaining balance.
    pub fn record_payload(&mut self, balance_after: u64) {
        self.used = self.max_gas_amount.saturating_sub(balance_after);
    }

    /// Gas units remaining, as the epilogue expects them.
    pub fn remaining(&self) -> u64 {
        self.max_gas_amount - self.used
    }

    /// The fee statement for the epilogue and the transaction output.
    /// TODO(metering): io/storage components are zero until they are charged.
    pub fn fee_statement(&self) -> FeeStatement {
        FeeStatement::builder()
            .total_charge_gas_units(self.used)
            .execution_gas_units(self.used)
            .io_gas_units(0)
            .storage_fee_octas(0)
            .storage_fee_refund_octas(0)
            .build()
    }
}
