use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::result::Result as StdResult;

use anyhow::{anyhow, Context};
use blockstore::Blockstore;
use cid::Cid;
use num_traits::{Signed, Zero};
use wasmtime::{Engine, Module};

use fvm_shared::address::Address;
use fvm_shared::bigint::{BigInt, Sign};
use fvm_shared::clock::ChainEpoch;
use fvm_shared::econ::TokenAmount;
use fvm_shared::encoding::Cbor;
use fvm_shared::message::Message;
use fvm_shared::receipt::Receipt;
use fvm_shared::version::NetworkVersion;
use fvm_shared::ActorID;

use crate::account_actor::is_account_actor;
use crate::call_manager::{CallManager, CallManagerFactory, StaticCallManager};
use crate::externs::Externs;
use crate::gas::{price_list_by_epoch, GasCharge, GasOutputs};
use crate::kernel::{ClassifyResult, Context as _, ExecutionError, Result, SyscallError};
use crate::state_tree::{ActorState, StateTree};
use crate::syscall_error;
use crate::Config;

use super::{
    ApplyKind, ApplyRet, CallError, Executor, Machine, MachineContext, BURNT_FUNDS_ACTOR_ADDR,
    REWARD_ACTOR_ADDR,
};

/// The core of the FVM.
///
/// ## Generic types
/// * B => Blockstore.
/// * E => Externs.
/// * K => Kernel.
//
// If the inner value is `None` it means the machine got poisend and is unusable.
#[repr(transparent)]
pub struct DefaultMachine<B, E, C, CF = ()>(Option<Box<InnerDefaultMachine<B, E, C, CF>>>);

#[doc(hidden)]
impl<B, E, C, CF> Deref for DefaultMachine<B, E, C, CF> {
    type Target = InnerDefaultMachine<B, E, C, CF>;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("machine is poisoned")
    }
}

#[doc(hidden)]
impl<B, E, C, CF> DerefMut for DefaultMachine<B, E, C, CF> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("machine is poisoned")
    }
}

#[doc(hidden)]
pub struct InnerDefaultMachine<B, E, C, CF = ()> {
    config: Config,
    /// The context for the execution.
    context: MachineContext,
    /// The wasmtime engine is created on construction of the DefaultMachine, and
    /// is dropped when the DefaultMachine is dropped.
    engine: Engine,
    /// Boundary A calls are handled through externs. These are calls from the
    /// FVM to the Filecoin node.
    externs: E,
    /// The state tree. It is updated with the results from every message
    /// execution as the call stack for every message concludes.
    ///
    /// Owned.
    state_tree: StateTree<B>,

    /// A factory for creating call managers.
    call_manager_factory: CF,
    /// A marker to select the specific call manager we're going to use.
    call_manager_marker: PhantomData<fn() -> C>,
}

impl<B, E, C> DefaultMachine<B, E, C>
where
    B: Blockstore + 'static,
    E: Externs + 'static,
    C: StaticCallManager + CallManager<Machine = Self> + 'static,
{
    pub fn new(
        config: Config,
        epoch: ChainEpoch,
        base_fee: TokenAmount,
        network_version: NetworkVersion,
        state_root: Cid,
        blockstore: B,
        externs: E,
    ) -> anyhow::Result<Self> {
        Self::new_with_factory(
            config,
            epoch,
            base_fee,
            network_version,
            state_root,
            blockstore,
            externs,
            (),
        )
    }
}

impl<B, E, C, CF> DefaultMachine<B, E, C, CF>
where
    B: Blockstore + 'static,
    E: Externs + 'static,
    C: CallManager<Machine = Self> + 'static,
    CF: CallManagerFactory<C>,
{
    pub fn new_with_factory(
        config: Config,
        epoch: ChainEpoch,
        base_fee: TokenAmount,
        network_version: NetworkVersion,
        state_root: Cid,
        blockstore: B,
        externs: E,
        call_manager_factory: CF,
    ) -> anyhow::Result<Self> {
        let context = MachineContext::new(
            epoch,
            base_fee,
            state_root,
            price_list_by_epoch(epoch),
            network_version,
        );

        // Initialize the WASM engine.
        let engine = Engine::new(&config.engine)?;

        // TODO: fix the error handling to use anyhow up and down the stack, or at least not use
        //  non-send errors in the state-tree.
        let state_tree = StateTree::new_from_root(blockstore, &context.initial_state_root)?;

        Ok(DefaultMachine(Some(Box::new(InnerDefaultMachine {
            config,
            context,
            engine,
            externs,
            state_tree,
            call_manager_factory,
            call_manager_marker: PhantomData,
        }))))
    }
}

impl<B, E, C> Machine for DefaultMachine<B, E, C>
where
    B: Blockstore + 'static,
    E: Externs + 'static,
    C: CallManager<Machine = Self> + 'static,
{
    type Blockstore = B;
    type Externs = E;

    fn engine(&self) -> &Engine {
        &self.engine
    }

    fn config(&self) -> Config {
        self.config.clone()
    }

    fn blockstore(&self) -> &Self::Blockstore {
        self.state_tree.store()
    }

    fn context(&self) -> &MachineContext {
        &self.context
    }

    fn externs(&self) -> &Self::Externs {
        &self.externs
    }

    fn state_tree(&self) -> &StateTree<Self::Blockstore> {
        &self.state_tree
    }

    fn state_tree_mut(&mut self) -> &mut StateTree<Self::Blockstore> {
        &mut self.state_tree
    }

    /// Creates an uninitialized actor.
    // TODO: Remove
    fn create_actor(&mut self, addr: &Address, act: ActorState) -> Result<ActorID> {
        let state_tree = self.state_tree_mut();

        let addr_id = state_tree
            .register_new_address(addr)
            .context("failed to register new address")
            .or_fatal()?;

        state_tree
            .set_actor(&Address::new_id(addr_id), act)
            .context("failed to set actor")
            .or_fatal()?;
        Ok(addr_id)
    }

    fn load_module(&self, code: &Cid) -> Result<Module> {
        // TODO: cache compiled code, and modules?
        let binary = if code == &*crate::builtin::SYSTEM_ACTOR_CODE_ID {
            fvm_actor_system::wasm::WASM_BINARY
        } else if code == &*crate::builtin::INIT_ACTOR_CODE_ID {
            fvm_actor_init::wasm::WASM_BINARY
        } else if code == &*crate::builtin::CRON_ACTOR_CODE_ID {
            fvm_actor_cron::wasm::WASM_BINARY
        } else if code == &*crate::builtin::ACCOUNT_ACTOR_CODE_ID {
            fvm_actor_account::wasm::WASM_BINARY
        } else if code == &*crate::builtin::POWER_ACTOR_CODE_ID {
            fvm_actor_power::wasm::WASM_BINARY
        } else if code == &*crate::builtin::MINER_ACTOR_CODE_ID {
            fvm_actor_miner::wasm::WASM_BINARY
        } else if code == &*crate::builtin::MARKET_ACTOR_CODE_ID {
            fvm_actor_market::wasm::WASM_BINARY
        } else if code == &*crate::builtin::PAYCH_ACTOR_CODE_ID {
            fvm_actor_paych::wasm::WASM_BINARY
        } else if code == &*crate::builtin::MULTISIG_ACTOR_CODE_ID {
            fvm_actor_multisig::wasm::WASM_BINARY
        } else if code == &*crate::builtin::REWARD_ACTOR_CODE_ID {
            fvm_actor_reward::wasm::WASM_BINARY
        } else if code == &*crate::builtin::VERIFREG_ACTOR_CODE_ID {
            fvm_actor_verifreg::wasm::WASM_BINARY
        } else {
            None
        };

        let binary = binary.context("missing wasm binary").or_fatal()?;
        let module = Module::new(&self.engine, binary).or_fatal()?;
        Ok(module)
    }

    fn transfer(&mut self, from: ActorID, to: ActorID, value: &TokenAmount) -> Result<()> {
        if from == to {
            return Ok(());
        }
        if value.is_negative() {
            return Err(syscall_error!(SysErrForbidden;
                "attempted to transfer negative transfer value {}", value)
            .into());
        }

        // TODO: make sure these are actually fatal.
        let mut from_actor = self
            .state_tree
            .get_actor_id(from)?
            .ok_or_else(|| anyhow!("sender actor does not exist in state during transfer"))
            .or_fatal()?;

        let mut to_actor = self
            .state_tree
            .get_actor_id(to)?
            .ok_or_else(|| anyhow!("receiver actor does not exist in state during transfer"))
            .or_fatal()?;

        from_actor.deduct_funds(value).map_err(|e| {
            syscall_error!(SysErrInsufficientFunds;
                "transfer failed when deducting funds ({}): {}", value, e)
        })?;
        to_actor.deposit_funds(value);

        // TODO turn failures into fatal errors
        self.state_tree.set_actor_id(from, from_actor)?;
        // .map_err(|e| e.downcast_fatal("failed to set from actor"))?;
        // TODO turn failures into fatal errors
        self.state_tree.set_actor_id(to, to_actor)?;
        //.map_err(|e| e.downcast_fatal("failed to set to actor"))?;

        Ok(())
    }
}

impl<B, E, C, CF> Executor for DefaultMachine<B, E, C, CF>
where
    B: Blockstore + 'static,
    E: Externs + 'static,
    C: CallManager<Machine = Self> + 'static,
    CF: CallManagerFactory<C>,
{
    type CallManager = C;
    /// This is the entrypoint to execute a message.
    fn execute_message(&mut self, msg: Message, _: ApplyKind) -> anyhow::Result<ApplyRet> {
        // Validate if the message was correct, charge for it, and extract some preliminary data.
        let (sender_id, gas_cost, inclusion_cost) = match self.preflight_message(&msg)? {
            Ok(res) => res,
            Err(apply_ret) => return Ok(apply_ret),
        };

        // Apply the message.
        let (res, gas_used, mut backtrace) = self.map_mut(|machine| {
            let mut cm = machine.call_manager_factory.clone().make(
                machine,
                msg.gas_limit,
                msg.from,
                msg.sequence,
            );
            // This error is fatal because it should have already been acounted for inside
            // preflight_message.
            if let Err(e) = cm.charge_gas(inclusion_cost) {
                return (Err(e), cm.finish().2);
            }

            let result = cm.with_transaction(|cm| {
                // Invoke the message.
                let ret = cm.send(sender_id, msg.to, msg.method_num, &msg.params, &msg.value)?;

                // Charge for including the result (before we end the transaction).
                cm.charge_gas(
                    cm.context()
                        .price_list()
                        .on_chain_return_value(ret.return_data.len()),
                )?;

                Ok(ret)
            });
            let (gas_used, backtrace, machine) = cm.finish();
            (Ok((result, gas_used, backtrace)), machine)
        })?;

        // Extract the exit code and build the result of the message application.
        let receipt = match res {
            Ok(receipt) => {
                backtrace.clear();
                receipt
            }
            Err(ExecutionError::Syscall(SyscallError(errmsg, exit_code))) => {
                if exit_code.is_success() {
                    return Err(anyhow!(
                        "message invocation errored with an ok status: {}",
                        errmsg
                    ));
                }
                backtrace.push(CallError {
                    source: 0,
                    code: exit_code,
                    message: errmsg,
                });
                Receipt {
                    exit_code,
                    return_data: Default::default(),
                    gas_used,
                }
            }
            Err(ExecutionError::Fatal(e)) => {
                return Err(e.context(format!(
                    "[from={}, to={}, seq={}, m={}, h={}] fatal error",
                    msg.from, msg.to, msg.sequence, msg.method_num, self.context.epoch
                )))
            }
        };
        self.finish_message(msg, receipt, backtrace, gas_cost)
    }
}

impl<B, E, C, CF> DefaultMachine<B, E, C, CF>
where
    B: Blockstore + 'static,
    E: Externs + 'static,
    C: CallManager<Machine = Self> + 'static,
    CF: CallManagerFactory<C>,
{
    // TODO: The return type here is very strange because we have three cases:
    // 1. Continue (return actor ID & gas).
    // 2. Short-circuit (return ApplyRet).
    // 3. Fail (return an error).
    //
    // We could use custom types, but that would be even more annoying.
    fn preflight_message(
        &mut self,
        msg: &Message,
    ) -> Result<StdResult<(ActorID, TokenAmount, GasCharge<'static>), ApplyRet>> {
        // TODO sanity check on message, copied from Forest, needs adaptation.
        msg.check().or_fatal()?;

        // TODO I don't like having price lists _inside_ the FVM, but passing
        //  these across the boundary is also a no-go.
        let pl = &self.context.price_list;
        let ser_msg = msg
            .marshal_cbor()
            .context("failed to re-marshal message")
            .or_fatal()?;
        let inclusion_cost = pl.on_chain_message(ser_msg.len());
        let inclusion_total = inclusion_cost.total();

        // Verify the cost of the message is not over the message gas limit.
        if inclusion_total > msg.gas_limit {
            return Ok(Err(ApplyRet::prevalidation_fail(
                syscall_error!(SysErrOutOfGas; "Out of gas ({} > {})", inclusion_total, msg.gas_limit),
                &self.context.base_fee * inclusion_total,
            )));
        }

        // Load sender actor state.
        let miner_penalty_amount = &self.context.base_fee * msg.gas_limit;

        let sender_id = match self
            .state_tree
            .lookup_id(&msg.from)
            .with_context(|| format!("failed to lookup actor {}", &msg.from))?
        {
            Some(id) => id,
            None => {
                return Ok(Err(ApplyRet::prevalidation_fail(
                    syscall_error!(SysErrSenderInvalid; "Sender invalid"),
                    miner_penalty_amount,
                )))
            }
        };

        let sender = match self
            .state_tree
            .get_actor(&Address::new_id(sender_id))
            .with_context(|| format!("failed to lookup actor {}", &msg.from))?
        {
            Some(act) => act,
            None => {
                return Ok(Err(ApplyRet::prevalidation_fail(
                    syscall_error!(SysErrSenderInvalid; "Sender invalid"),
                    miner_penalty_amount,
                )))
            }
        };

        // If sender is not an account actor, the message is invalid.
        if !is_account_actor(&sender.code) {
            return Ok(Err(ApplyRet::prevalidation_fail(
                syscall_error!(SysErrSenderInvalid; "send not from account actor"),
                miner_penalty_amount,
            )));
        };

        // Check sequence is correct
        if msg.sequence != sender.sequence {
            return Ok(Err(ApplyRet::prevalidation_fail(
                syscall_error!(SysErrSenderStateInvalid; "actor sequence invalid: {} != {}", msg.sequence, sender.sequence),
                miner_penalty_amount,
            )));
        };

        // Ensure from actor has enough balance to cover the gas cost of the message.
        let gas_cost: TokenAmount = msg.gas_fee_cap.clone() * msg.gas_limit;
        if sender.balance < gas_cost {
            return Ok(Err(ApplyRet::prevalidation_fail(
                syscall_error!(SysErrSenderStateInvalid;
                    "actor balance less than needed: {} < {}", sender.balance, gas_cost),
                miner_penalty_amount,
            )));
        }

        // Deduct message inclusion gas cost and increment sequence.
        self.state_tree
            .mutate_actor(&Address::new_id(sender_id), |act| {
                act.deduct_funds(&gas_cost)?;
                act.sequence += 1;
                Ok(())
            })?;

        Ok(Ok((sender_id, gas_cost, inclusion_cost)))
    }

    fn finish_message(
        &mut self,
        msg: Message,
        receipt: Receipt,
        backtrace: Vec<CallError>,
        gas_cost: BigInt,
    ) -> anyhow::Result<ApplyRet> {
        // NOTE: we don't support old network versions in the FVM, so we always burn.
        let GasOutputs {
            base_fee_burn,
            miner_tip,
            over_estimation_burn,
            refund,
            miner_penalty,
            ..
        } = GasOutputs::compute(
            receipt.gas_used,
            msg.gas_limit,
            &self.context.base_fee,
            &msg.gas_fee_cap,
            &msg.gas_premium,
        );

        let mut transfer_to_actor = |addr: &Address, amt: &TokenAmount| -> anyhow::Result<()> {
            if amt.sign() == Sign::Minus {
                return Err(anyhow!("attempted to transfer negative value into actor"));
            }
            if amt.is_zero() {
                return Ok(());
            }

            self.state_tree
                .mutate_actor(addr, |act| {
                    act.deposit_funds(amt);
                    Ok(())
                })
                .context("failed to lookup actor for transfer")?;
            Ok(())
        };

        transfer_to_actor(&BURNT_FUNDS_ACTOR_ADDR, &base_fee_burn)?;

        transfer_to_actor(&REWARD_ACTOR_ADDR, &miner_tip)?;

        transfer_to_actor(&BURNT_FUNDS_ACTOR_ADDR, &over_estimation_burn)?;

        // refund unused gas
        transfer_to_actor(&msg.from, &refund)?;

        if (&base_fee_burn + over_estimation_burn + &refund + &miner_tip) != gas_cost {
            // Sanity check. This could be a fatal error.
            return Err(anyhow!("Gas handling math is wrong"));
        }
        Ok(ApplyRet {
            msg_receipt: receipt,
            backtrace,
            penalty: miner_penalty,
            miner_tip,
        })
    }

    fn map_mut<F, T>(&mut self, f: F) -> T
    where
        F: FnOnce(Self) -> (T, Self),
    {
        replace_with::replace_with_and_return(self, || DefaultMachine(None), f)
    }
}
