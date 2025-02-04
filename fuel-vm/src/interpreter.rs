//! [`Interpreter`] implementation

use crate::{
    call::CallFrame,
    checked_transaction::CheckPredicateParams,
    constraints::reg_key::*,
    consts::*,
    context::Context,
    error::SimpleResult,
    state::Debugger,
};
use alloc::vec::Vec;
use core::{
    mem,
    ops::Index,
};

use fuel_asm::{
    Flags,
    PanicReason,
};
use fuel_tx::{
    field,
    output,
    Chargeable,
    Create,
    Executable,
    FeeParameters,
    GasCosts,
    Output,
    PrepareSign,
    Receipt,
    Script,
    Transaction,
    TransactionRepr,
    UniqueIdentifier,
    Upgrade,
    Upload,
    ValidityError,
};
use fuel_types::{
    AssetId,
    Bytes32,
    ChainId,
    ContractId,
    Word,
};

mod alu;
mod balances;
mod blockchain;
mod constructors;
pub mod contract;
mod crypto;
pub mod diff;
mod executors;
mod flow;
mod gas;
mod initialization;
mod internal;
mod log;
mod memory;
mod metadata;
mod post_execution;
mod receipts;

mod debug;
mod ecal;

use crate::profiler::Profiler;

#[cfg(feature = "profile-gas")]
use crate::profiler::InstructionLocation;

pub use balances::RuntimeBalances;
pub use ecal::{
    EcalHandler,
    PredicateErrorEcal,
};
pub use memory::{
    Memory,
    MemoryRange,
};

use crate::checked_transaction::{
    CreateCheckedMetadata,
    EstimatePredicates,
    IntoChecked,
    NonRetryableFreeBalances,
    RetryableAmount,
    ScriptCheckedMetadata,
    UpgradeCheckedMetadata,
    UploadCheckedMetadata,
};

#[cfg(feature = "test-helpers")]
pub use self::receipts::ReceiptsCtx;

#[cfg(not(feature = "test-helpers"))]
use self::receipts::ReceiptsCtx;

/// ECAL opcode is not supported and return an error if you try to call.
#[derive(Debug, Copy, Clone, Default)]
pub struct NotSupportedEcal;

/// VM interpreter.
///
/// The internal state of the VM isn't expose because the intended usage is to
/// either inspect the resulting receipts after a transaction execution, or the
/// resulting mutated transaction.
///
/// These can be obtained with the help of a [`crate::transactor::Transactor`]
/// or a client implementation.
#[derive(Debug, Clone)]
pub struct Interpreter<S, Tx = (), Ecal = NotSupportedEcal> {
    registers: [Word; VM_REGISTER_COUNT],
    memory: Memory,
    frames: Vec<CallFrame>,
    receipts: ReceiptsCtx,
    tx: Tx,
    initial_balances: InitialBalances,
    storage: S,
    debugger: Debugger,
    context: Context,
    balances: RuntimeBalances,
    profiler: Profiler,
    interpreter_params: InterpreterParams,
    /// `PanicContext` after the latest execution. It is consumed by
    /// `append_panic_receipt` and is `PanicContext::None` after consumption.
    panic_context: PanicContext,
    ecal_state: Ecal,
}

/// Interpreter parameters
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterParams {
    /// Gas Price
    pub gas_price: Word,
    /// Gas costs
    pub gas_costs: GasCosts,
    /// Maximum number of inputs
    pub max_inputs: u16,
    /// Maximum size of the contract in bytes
    pub contract_max_size: u64,
    /// Offset of the transaction data in the memory
    pub tx_offset: usize,
    /// Maximum length of the message data
    pub max_message_data_length: u64,
    /// Chain ID
    pub chain_id: ChainId,
    /// Fee parameters
    pub fee_params: FeeParameters,
    /// Base Asset ID
    pub base_asset_id: AssetId,
}

#[cfg(feature = "test-helpers")]
impl Default for InterpreterParams {
    fn default() -> Self {
        Self {
            gas_price: 0,
            gas_costs: Default::default(),
            max_inputs: fuel_tx::TxParameters::DEFAULT.max_inputs(),
            contract_max_size: fuel_tx::ContractParameters::DEFAULT.contract_max_size(),
            tx_offset: fuel_tx::TxParameters::DEFAULT.tx_offset(),
            max_message_data_length: fuel_tx::PredicateParameters::DEFAULT
                .max_message_data_length(),
            chain_id: ChainId::default(),
            fee_params: FeeParameters::default(),
            base_asset_id: Default::default(),
        }
    }
}

impl InterpreterParams {
    /// Constructor for `InterpreterParams`
    pub fn new<T: Into<CheckPredicateParams>>(gas_price: Word, params: T) -> Self {
        let params: CheckPredicateParams = params.into();
        Self {
            gas_price,
            gas_costs: params.gas_costs,
            max_inputs: params.max_inputs,
            contract_max_size: params.contract_max_size,
            tx_offset: params.tx_offset,
            max_message_data_length: params.max_message_data_length,
            chain_id: params.chain_id,
            fee_params: params.fee_params,
            base_asset_id: params.base_asset_id,
        }
    }
}

/// Sometimes it is possible to add some additional context information
/// regarding panic reasons to simplify debugging.
// TODO: Move this enum into `fuel-tx` and use it inside of the `Receipt::Panic` as meta
//  information. Maybe better to have `Vec<PanicContext>` to provide more information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanicContext {
    /// No additional information.
    None,
    /// `ContractId` retrieved during instruction execution.
    ContractId(ContractId),
}

impl<S, Tx, Ecal> Interpreter<S, Tx, Ecal> {
    /// Returns the current state of the VM memory
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// Returns mutable access to the vm memory
    pub fn memory_mut(&mut self) -> &mut Memory {
        &mut self.memory
    }

    /// Returns the current state of the registers
    pub const fn registers(&self) -> &[Word] {
        &self.registers
    }

    /// Returns mutable access to the registers
    pub fn registers_mut(&mut self) -> &mut [Word] {
        &mut self.registers
    }

    pub(crate) fn call_stack(&self) -> &[CallFrame] {
        self.frames.as_slice()
    }

    /// Debug handler
    pub const fn debugger(&self) -> &Debugger {
        &self.debugger
    }

    /// The current transaction.
    pub fn transaction(&self) -> &Tx {
        &self.tx
    }

    /// The initial balances.
    pub fn initial_balances(&self) -> &InitialBalances {
        &self.initial_balances
    }

    /// Get max_inputs value
    pub fn max_inputs(&self) -> u16 {
        self.interpreter_params.max_inputs
    }

    /// Gas price for current block
    pub fn gas_price(&self) -> Word {
        self.interpreter_params.gas_price
    }

    #[cfg(feature = "test-helpers")]
    /// Sets the gas price of the `Interpreter`
    pub fn set_gas_price(&mut self, gas_price: u64) {
        self.interpreter_params.gas_price = gas_price;
    }

    /// Gas costs for opcodes
    pub fn gas_costs(&self) -> &GasCosts {
        &self.interpreter_params.gas_costs
    }

    /// Get the Fee Parameters
    pub fn fee_params(&self) -> &FeeParameters {
        &self.interpreter_params.fee_params
    }

    /// Get the base Asset ID
    pub fn base_asset_id(&self) -> &AssetId {
        &self.interpreter_params.base_asset_id
    }

    /// Get contract_max_size value
    pub fn contract_max_size(&self) -> u64 {
        self.interpreter_params.contract_max_size
    }

    /// Get tx_offset value
    pub fn tx_offset(&self) -> usize {
        self.interpreter_params.tx_offset
    }

    /// Get max_message_data_length value
    pub fn max_message_data_length(&self) -> u64 {
        self.interpreter_params.max_message_data_length
    }

    /// Get the chain id
    pub fn chain_id(&self) -> ChainId {
        self.interpreter_params.chain_id
    }

    /// Receipts generated by a transaction execution.
    pub fn receipts(&self) -> &[Receipt] {
        self.receipts.as_ref().as_slice()
    }

    /// Compute current receipts root
    pub fn compute_receipts_root(&self) -> Bytes32 {
        self.receipts.root()
    }

    /// Mutable access to receipts for testing purposes.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn receipts_mut(&mut self) -> &mut ReceiptsCtx {
        &mut self.receipts
    }

    pub(crate) fn contract_id(&self) -> Option<ContractId> {
        self.frames.last().map(|frame| *frame.to())
    }

    /// Reference to the underlying profiler
    #[cfg(feature = "profile-any")]
    pub const fn profiler(&self) -> &Profiler {
        &self.profiler
    }
}

pub(crate) fn flags(flag: Reg<FLAG>) -> Flags {
    Flags::from_bits_truncate(*flag)
}

pub(crate) fn is_wrapping(flag: Reg<FLAG>) -> bool {
    flags(flag).contains(Flags::WRAPPING)
}

pub(crate) fn is_unsafe_math(flag: Reg<FLAG>) -> bool {
    flags(flag).contains(Flags::UNSAFEMATH)
}

#[cfg(feature = "profile-gas")]
fn current_location(
    current_contract: Option<ContractId>,
    pc: crate::constraints::reg_key::Reg<{ crate::constraints::reg_key::PC }>,
    is: crate::constraints::reg_key::Reg<{ crate::constraints::reg_key::IS }>,
) -> InstructionLocation {
    // Safety: pc should always be above is, but fallback to zero here for weird cases,
    //         as the profiling code should be robust against regards cases like this.
    let offset = (*pc).saturating_sub(*is);
    InstructionLocation::new(current_contract, offset)
}

impl<S, Tx, Ecal> AsRef<S> for Interpreter<S, Tx, Ecal> {
    fn as_ref(&self) -> &S {
        &self.storage
    }
}

impl<S, Tx, Ecal> AsMut<S> for Interpreter<S, Tx, Ecal> {
    fn as_mut(&mut self) -> &mut S {
        &mut self.storage
    }
}

/// The definition of the executable transaction supported by the `Interpreter`.
pub trait ExecutableTransaction:
    Default
    + Clone
    + Chargeable
    + Executable
    + IntoChecked
    + EstimatePredicates
    + UniqueIdentifier
    + field::Outputs
    + field::Witnesses
    + Into<Transaction>
    + PrepareSign
    + fuel_types::canonical::Serialize
{
    /// Casts the `Self` transaction into `&Script` if any.
    fn as_script(&self) -> Option<&Script> {
        None
    }

    /// Casts the `Self` transaction into `&mut Script` if any.
    fn as_script_mut(&mut self) -> Option<&mut Script> {
        None
    }

    /// Casts the `Self` transaction into `&Create` if any.
    fn as_create(&self) -> Option<&Create> {
        None
    }

    /// Casts the `Self` transaction into `&mut Create` if any.
    fn as_create_mut(&mut self) -> Option<&mut Create> {
        None
    }

    /// Casts the `Self` transaction into `&Upgrade` if any.
    fn as_upgrade(&self) -> Option<&Upgrade> {
        None
    }

    /// Casts the `Self` transaction into `&mut Upgrade` if any.
    fn as_upgrade_mut(&mut self) -> Option<&mut Upgrade> {
        None
    }

    /// Casts the `Self` transaction into `&Upload` if any.
    fn as_upload(&self) -> Option<&Upload> {
        None
    }

    /// Casts the `Self` transaction into `&mut Upload` if any.
    fn as_upload_mut(&mut self) -> Option<&mut Upload> {
        None
    }

    /// Returns the type of the transaction like `Transaction::Create` or
    /// `Transaction::Script`.
    fn transaction_type() -> Word;

    /// Replaces the `Output::Variable` with the `output`(should be also
    /// `Output::Variable`) by the `idx` index.
    fn replace_variable_output(
        &mut self,
        idx: usize,
        output: Output,
    ) -> SimpleResult<()> {
        if !output.is_variable() {
            return Err(PanicReason::ExpectedOutputVariable.into());
        }

        // TODO increase the error granularity for this case - create a new variant of
        // panic reason
        self.outputs_mut()
            .get_mut(idx)
            .and_then(|o| match o {
                Output::Variable { amount, .. } if amount == &0 => Some(o),
                _ => None,
            })
            .map(|o| mem::replace(o, output))
            .ok_or(PanicReason::OutputNotFound)?;
        Ok(())
    }

    /// Update change and variable outputs.
    ///
    /// `revert` will signal if the execution was reverted. It will refund the unused gas
    /// cost to the base asset and reset output changes to their `initial_balances`.
    ///
    /// `remaining_gas` expects the raw content of `$ggas`
    ///
    /// `initial_balances` contains the initial state of the free balances
    ///
    /// `balances` will contain the current state of the free balances
    #[allow(clippy::too_many_arguments)]
    fn update_outputs<I>(
        &mut self,
        revert: bool,
        used_gas: Word,
        initial_balances: &InitialBalances,
        balances: &I,
        gas_costs: &GasCosts,
        fee_params: &FeeParameters,
        base_asset_id: &AssetId,
        gas_price: Word,
    ) -> Result<(), ValidityError>
    where
        I: for<'a> Index<&'a AssetId, Output = Word>,
    {
        let gas_refund = self
            .refund_fee(gas_costs, fee_params, used_gas, gas_price)
            .ok_or(ValidityError::GasCostsCoinsOverflow)?;

        self.outputs_mut().iter_mut().try_for_each(|o| match o {
            // If revert, set base asset to initial balance and refund unused gas
            //
            // Note: the initial balance deducts the gas limit from base asset
            Output::Change {
                asset_id, amount, ..
            } if revert && asset_id == base_asset_id => initial_balances.non_retryable
                [base_asset_id]
                .checked_add(gas_refund)
                .map(|v| *amount = v)
                .ok_or(ValidityError::BalanceOverflow),

            // If revert, reset any non-base asset to its initial balance
            Output::Change {
                asset_id, amount, ..
            } if revert => {
                *amount = initial_balances.non_retryable[asset_id];
                Ok(())
            }

            // The change for the base asset will be the available balance + unused gas
            Output::Change {
                asset_id, amount, ..
            } if asset_id == base_asset_id => balances[asset_id]
                .checked_add(gas_refund)
                .map(|v| *amount = v)
                .ok_or(ValidityError::BalanceOverflow),

            // Set changes to the remainder provided balances
            Output::Change {
                asset_id, amount, ..
            } => {
                *amount = balances[asset_id];
                Ok(())
            }

            // If revert, zeroes all variable output values
            Output::Variable { amount, .. } if revert => {
                *amount = 0;
                Ok(())
            }

            // Other outputs are unaffected
            _ => Ok(()),
        })
    }

    /// Finds `Output::Contract` corresponding to the `input` index.
    fn find_output_contract(&self, input: usize) -> Option<(usize, &Output)> {
        self.outputs().iter().enumerate().find(|(_idx, o)| {
            matches!(o, Output::Contract( output::contract::Contract {
                input_index, ..
            }) if *input_index as usize == input)
        })
    }

    /// Prepares the transaction for execution.
    fn prepare_init_execute(&mut self) {
        self.prepare_sign()
    }
}

impl ExecutableTransaction for Create {
    fn as_create(&self) -> Option<&Create> {
        Some(self)
    }

    fn as_create_mut(&mut self) -> Option<&mut Create> {
        Some(self)
    }

    fn transaction_type() -> Word {
        TransactionRepr::Create as Word
    }

    fn prepare_init_execute(&mut self) {}
}

impl ExecutableTransaction for Script {
    fn as_script(&self) -> Option<&Script> {
        Some(self)
    }

    fn as_script_mut(&mut self) -> Option<&mut Script> {
        Some(self)
    }

    fn transaction_type() -> Word {
        TransactionRepr::Script as Word
    }
}

impl ExecutableTransaction for Upgrade {
    fn as_upgrade(&self) -> Option<&Upgrade> {
        Some(self)
    }

    fn as_upgrade_mut(&mut self) -> Option<&mut Upgrade> {
        Some(self)
    }

    fn transaction_type() -> Word {
        TransactionRepr::Upgrade as Word
    }
}

impl ExecutableTransaction for Upload {
    fn as_upload(&self) -> Option<&Upload> {
        Some(self)
    }

    fn as_upload_mut(&mut self) -> Option<&mut Upload> {
        Some(self)
    }

    fn transaction_type() -> Word {
        TransactionRepr::Upload as Word
    }
}

/// The initial balances of the transaction.
#[derive(Default, Debug, Clone, Eq, PartialEq, Hash)]
pub struct InitialBalances {
    /// See [`NonRetryableFreeBalances`].
    pub non_retryable: NonRetryableFreeBalances,
    /// See [`RetryableAmount`].
    pub retryable: Option<RetryableAmount>,
}

/// Methods that should be implemented by the checked metadata of supported transactions.
pub trait CheckedMetadata {
    /// Returns the initial balances from the checked metadata of the transaction.
    fn balances(&self) -> InitialBalances;
}

impl CheckedMetadata for ScriptCheckedMetadata {
    fn balances(&self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.non_retryable_balances.clone(),
            retryable: Some(self.retryable_balance),
        }
    }
}

impl CheckedMetadata for CreateCheckedMetadata {
    fn balances(&self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.free_balances.clone(),
            retryable: None,
        }
    }
}

impl CheckedMetadata for UpgradeCheckedMetadata {
    fn balances(&self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.free_balances.clone(),
            retryable: None,
        }
    }
}

impl CheckedMetadata for UploadCheckedMetadata {
    fn balances(&self) -> InitialBalances {
        InitialBalances {
            non_retryable: self.free_balances.clone(),
            retryable: None,
        }
    }
}

pub(crate) struct InputContracts<'vm, I> {
    tx_input_contracts: I,
    panic_context: &'vm mut PanicContext,
}

impl<'vm, I: Iterator<Item = &'vm ContractId>> InputContracts<'vm, I> {
    pub fn new(tx_input_contracts: I, panic_context: &'vm mut PanicContext) -> Self {
        Self {
            tx_input_contracts,
            panic_context,
        }
    }

    /// Checks that the contract is declared in the transaction inputs.
    pub fn check(&mut self, contract: &ContractId) -> SimpleResult<()> {
        if !self.tx_input_contracts.any(|input| input == contract) {
            *self.panic_context = PanicContext::ContractId(*contract);
            Err(PanicReason::ContractNotInInputs.into())
        } else {
            Ok(())
        }
    }
}
