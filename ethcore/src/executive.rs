// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Transaction Execution environment.
use std::cmp;
use std::sync::Arc;
use hash::keccak;
use ethereum_types::{H256, U256, U512, Address};
use bytes::{Bytes, BytesRef};
use state::{Backend as StateBackend, State, Substate, CleanupMode};
use error::ExecutionError;
use machine::EthereumMachine as Machine;
use evm::{CallType, Finalize, FinalizationResult};
use vm::{
	self, EnvInfo, CreateContractAddress, ReturnData, CleanDustMode, ActionParams,
	ActionValue, Schedule, TrapError, Exec, ResumeCall, ResumeCreate
};
use factory::VmFactory;
use externalities::*;
use trace::{self, Tracer, VMTracer};
use transaction::{Action, SignedTransaction};
pub use executed::{Executed, ExecutionResult};

#[cfg(debug_assertions)]
/// Roughly estimate what stack size each level of evm depth will use. (Debug build)
const STACK_SIZE_PER_DEPTH: usize = 128 * 1024;

#[cfg(not(debug_assertions))]
/// Roughly estimate what stack size each level of evm depth will use.
const STACK_SIZE_PER_DEPTH: usize = 24 * 1024;

#[cfg(debug_assertions)]
/// Entry stack overhead prior to execution. (Debug build)
const STACK_SIZE_ENTRY_OVERHEAD: usize = 100 * 1024;

#[cfg(not(debug_assertions))]
/// Entry stack overhead prior to execution.
const STACK_SIZE_ENTRY_OVERHEAD: usize = 20 * 1024;

/// Returns new address created from address, nonce, and code hash
pub fn contract_address(address_scheme: CreateContractAddress, sender: &Address, nonce: &U256, code: &[u8]) -> (Address, Option<H256>) {
	use rlp::RlpStream;

	match address_scheme {
		CreateContractAddress::FromSenderAndNonce => {
			let mut stream = RlpStream::new_list(2);
			stream.append(sender);
			stream.append(nonce);
			(From::from(keccak(stream.as_raw())), None)
		},
		CreateContractAddress::FromSenderSaltAndCodeHash(salt) => {
			let code_hash = keccak(code);
			let mut buffer = [0u8; 20 + 32 + 32];
			&mut buffer[0..20].copy_from_slice(&sender[..]);
			&mut buffer[20..(20+32)].copy_from_slice(&salt[..]);
			&mut buffer[(20+32)..].copy_from_slice(&code_hash[..]);
			(From::from(keccak(&buffer[..])), Some(code_hash))
		},
		CreateContractAddress::FromSenderAndCodeHash => {
			let code_hash = keccak(code);
			let mut buffer = [0u8; 20 + 32];
			&mut buffer[..20].copy_from_slice(&sender[..]);
			&mut buffer[20..].copy_from_slice(&code_hash[..]);
			(From::from(keccak(&buffer[..])), Some(code_hash))
		},
	}
}

pub fn into_message_call_result(result: vm::Result<FinalizationResult>) -> vm::MessageCallResult {
	match result {
		Ok(FinalizationResult { gas_left, return_data, apply_state: true }) => vm::MessageCallResult::Success(gas_left, return_data),
		Ok(FinalizationResult { gas_left, return_data, apply_state: false }) => vm::MessageCallResult::Reverted(gas_left, return_data),
		_ => vm::MessageCallResult::Failed
	}
}

pub fn into_contract_create_result(result: vm::Result<FinalizationResult>, address: &Address, substate: &mut Substate) -> vm::ContractCreateResult {
	match result {
		Ok(FinalizationResult { gas_left, apply_state: true, .. }) => {
			substate.contracts_created.push(address.clone());
			vm::ContractCreateResult::Created(address.clone(), gas_left)
		},
		Ok(FinalizationResult { gas_left, apply_state: false, return_data }) => {
			vm::ContractCreateResult::Reverted(gas_left, return_data)
		},
		_ => vm::ContractCreateResult::Failed,
	}
}

/// Transaction execution options.
#[derive(Copy, Clone, PartialEq)]
pub struct TransactOptions<T, V> {
	/// Enable call tracing.
	pub tracer: T,
	/// Enable VM tracing.
	pub vm_tracer: V,
	/// Check transaction nonce before execution.
	pub check_nonce: bool,
	/// Records the output from init contract calls.
	pub output_from_init_contract: bool,
}

impl<T, V> TransactOptions<T, V> {
	/// Create new `TransactOptions` with given tracer and VM tracer.
	pub fn new(tracer: T, vm_tracer: V) -> Self {
		TransactOptions {
			tracer,
			vm_tracer,
			check_nonce: true,
			output_from_init_contract: false,
		}
	}

	/// Disables the nonce check
	pub fn dont_check_nonce(mut self) -> Self {
		self.check_nonce = false;
		self
	}

	/// Saves the output from contract creation.
	pub fn save_output_from_contract(mut self) -> Self {
		self.output_from_init_contract = true;
		self
	}
}

impl TransactOptions<trace::ExecutiveTracer, trace::ExecutiveVMTracer> {
	/// Creates new `TransactOptions` with default tracing and VM tracing.
	pub fn with_tracing_and_vm_tracing() -> Self {
		TransactOptions {
			tracer: trace::ExecutiveTracer::default(),
			vm_tracer: trace::ExecutiveVMTracer::toplevel(),
			check_nonce: true,
			output_from_init_contract: false,
		}
	}
}

impl TransactOptions<trace::ExecutiveTracer, trace::NoopVMTracer> {
	/// Creates new `TransactOptions` with default tracing and no VM tracing.
	pub fn with_tracing() -> Self {
		TransactOptions {
			tracer: trace::ExecutiveTracer::default(),
			vm_tracer: trace::NoopVMTracer,
			check_nonce: true,
			output_from_init_contract: false,
		}
	}
}

impl TransactOptions<trace::NoopTracer, trace::ExecutiveVMTracer> {
	/// Creates new `TransactOptions` with no tracing and default VM tracing.
	pub fn with_vm_tracing() -> Self {
		TransactOptions {
			tracer: trace::NoopTracer,
			vm_tracer: trace::ExecutiveVMTracer::toplevel(),
			check_nonce: true,
			output_from_init_contract: false,
		}
	}
}

impl TransactOptions<trace::NoopTracer, trace::NoopVMTracer> {
	/// Creates new `TransactOptions` without any tracing.
	pub fn with_no_tracing() -> Self {
		TransactOptions {
			tracer: trace::NoopTracer,
			vm_tracer: trace::NoopVMTracer,
			check_nonce: true,
			output_from_init_contract: false,
		}
	}
}

pub type ExecutiveTrapResult<'a, T> = vm::TrapResult<T, CallCreateExecutive<'a>, CallCreateExecutive<'a>>;
pub type ExecutiveTrapError<'a> = vm::TrapError<CallCreateExecutive<'a>, CallCreateExecutive<'a>>;

pub enum CallCreateExecutiveKind {
	Transfer(ActionParams),
	CallBuiltin(ActionParams),
	ExecCall(ActionParams, Box<Exec>, Substate),
	ExecCreate(ActionParams, Box<Exec>, Substate),
	ResumeCall(ActionParams, Box<ResumeCall>, Substate),
	ResumeCreate(ActionParams, Box<ResumeCreate>, Substate),
}

pub struct CallCreateExecutive<'a> {
	info: &'a EnvInfo,
	machine: &'a Machine,
	schedule: &'a Schedule,
	depth: usize,
	static_flag: bool,
	is_create: bool,
	kind: CallCreateExecutiveKind,
}

impl<'a> CallCreateExecutive<'a> {
	pub fn new_call_raw<'any>(params: ActionParams, info: &'a EnvInfo, machine: &'a Machine, schedule: &'a Schedule, factory: &'any VmFactory, depth: usize, static_flag: bool) -> Self {
		trace!("Executive::call(params={:?}) self.env_info={:?}, static={}", params, info, static_flag);

		// if destination is builtin, try to execute it
		let kind = if let Some(builtin) = machine.builtin(&params.code_address, info.number) {
			// Engines aren't supposed to return builtins until activation, but
			// prefer to fail rather than silently break consensus.
			if !builtin.is_active(info.number) {
				panic!("Consensus failure: engine implementation prematurely enabled built-in at {}", params.code_address);
			}

			CallCreateExecutiveKind::CallBuiltin(params)
		} else {
			if params.code.is_some() {
				// TODO: Remove this additional clone.
				CallCreateExecutiveKind::ExecCall(params.clone(), factory.create(params, schedule, depth), Substate::new())
			} else {
				CallCreateExecutiveKind::Transfer(params)
			}
		};

		Self {
			info, machine, schedule, depth, static_flag, kind,
			is_create: false,
		}
	}

	pub fn new_create_raw<'any>(params: ActionParams, info: &'a EnvInfo, machine: &'a Machine, schedule: &'a Schedule, factory: &'any VmFactory, depth: usize, static_flag: bool) -> Self {
		trace!("Executive::create(params={:?}) self.env_info={:?}, static={}", params, info, static_flag);

		// TODO: Remove this additional clone.
		let kind = CallCreateExecutiveKind::ExecCreate(params.clone(), factory.create(params, schedule, depth), Substate::new());

		Self {
			info, machine, schedule, depth, static_flag, kind,
			is_create: true,
		}
	}

	fn check_static_flag(params: &ActionParams, static_flag: bool, is_create: bool) -> vm::Result<()> {
		if (params.call_type == CallType::StaticCall ||
			((params.call_type == CallType::Call || is_create) &&
			 static_flag)) &&
			(is_create || params.value.value() > 0.into())
		{
			return Err(vm::Error::MutableCallInStaticContext);
		}

		Ok(())
	}

	fn check_eip684<B: 'a + StateBackend>(params: &ActionParams, state: &State<B>) -> vm::Result<()> {
		if state.exists_and_has_code_or_nonce(&params.address)? {
			return Err(vm::Error::OutOfGas);
		}

		Ok(())
	}

	fn transfer_exec_balance<B: 'a + StateBackend>(params: &ActionParams, schedule: &Schedule, state: &mut State<B>, substate: &mut Substate) -> vm::Result<()> {
		if let ActionValue::Transfer(val) = params.value {
			state.transfer_balance(&params.sender, &params.address, &val, substate.to_cleanup_mode(&schedule))?;
		}

		Ok(())
	}

	fn transfer_exec_balance_and_init_contract<B: 'a + StateBackend>(params: &ActionParams, schedule: &Schedule, state: &mut State<B>, substate: &mut Substate) -> vm::Result<()> {
		let nonce_offset = if schedule.no_empty {1} else {0}.into();
		let prev_bal = state.balance(&params.address)?;
		if let ActionValue::Transfer(val) = params.value {
			state.sub_balance(&params.sender, &val, &mut substate.to_cleanup_mode(&schedule))?;
			state.new_contract(&params.address, val + prev_bal, nonce_offset);
		} else {
			state.new_contract(&params.address, prev_bal, nonce_offset);
		}

		Ok(())
	}

	fn enact_result<B: 'a + StateBackend>(result: &vm::Result<FinalizationResult>, state: &mut State<B>, substate: &mut Substate, un_substate: Substate) {
		match *result {
			Err(vm::Error::OutOfGas)
				| Err(vm::Error::BadJumpDestination {..})
				| Err(vm::Error::BadInstruction {.. })
				| Err(vm::Error::StackUnderflow {..})
				| Err(vm::Error::BuiltIn {..})
				| Err(vm::Error::Wasm {..})
				| Err(vm::Error::OutOfStack {..})
				| Err(vm::Error::MutableCallInStaticContext)
				| Err(vm::Error::OutOfBounds)
				| Err(vm::Error::Reverted)
				| Ok(FinalizationResult { apply_state: false, .. }) => {
					state.revert_to_checkpoint();
			},
			Ok(_) | Err(vm::Error::Internal(_)) => {
				state.discard_checkpoint();
				substate.accrue(un_substate);
			}
		}
	}

	/// Creates `Externalities` from `Executive`.
	fn as_externalities<'any, B: 'any + StateBackend, T, V>(
		state: &'any mut State<B>,
		info: &'any EnvInfo,
		machine: &'any Machine,
		schedule: &'any Schedule,
		depth: usize,
		static_flag: bool,
		origin_info: OriginInfo,
		substate: &'any mut Substate,
		output: OutputPolicy,
		tracer: &'any mut T,
		vm_tracer: &'any mut V,
		static_call: bool,
	) -> Externalities<'any, T, V, B> where T: Tracer, V: VMTracer {
		let is_static = static_flag || static_call;
		Externalities::new(state, info, machine, schedule, depth, origin_info, substate, output, tracer, vm_tracer, is_static)
	}

	pub fn exec<B: 'a + StateBackend, T: Tracer, V: VMTracer>(mut self, state: &mut State<B>, substate: &mut Substate, tracer: &mut T, vm_tracer: &mut V) -> ExecutiveTrapResult<'a, FinalizationResult> {
		match self.kind {
			CallCreateExecutiveKind::Transfer(ref params) => {
				assert!(!self.is_create);

				let mut inner = || {
					Self::check_static_flag(params, self.static_flag, self.is_create)?;
					Self::transfer_exec_balance(params, self.schedule, state, substate)?;

					// TODO: Trace call.

					Ok(FinalizationResult {
						gas_left: params.gas,
						return_data: ReturnData::empty(),
						apply_state: true,
					})
				};

				Ok(inner())
			},
			CallCreateExecutiveKind::CallBuiltin(ref params) => {
				assert!(!self.is_create);

				let mut inner = || {
					let builtin = self.machine.builtin(&params.code_address, self.info.number).expect("TODO: add PROOF");

					Self::check_static_flag(&params, self.static_flag, self.is_create)?;
					state.checkpoint();
					Self::transfer_exec_balance(&params, self.schedule, state, substate)?;

					let default = [];
					let data = if let Some(ref d) = params.data { d as &[u8] } else { &default as &[u8] };

					let cost = builtin.cost(data);
					if cost <= params.gas {
						let mut builtin_out_buffer = Vec::new();
						let result = {
							let mut builtin_output = BytesRef::Flexible(&mut builtin_out_buffer);
							builtin.execute(data, &mut builtin_output)
						};
						if let Err(e) = result {
							state.revert_to_checkpoint();

							// TODO: Tracing

							Err(e.into())
						} else {
							state.discard_checkpoint();

							// TODO: Tracing

							let out_len = builtin_out_buffer.len();
							Ok(FinalizationResult {
								gas_left: params.gas - cost,
								return_data: ReturnData::new(builtin_out_buffer, 0, out_len),
								apply_state: true,
							})
						}
					} else {
						// just drain the whole gas
						state.revert_to_checkpoint();

						// TODO: Tracing

						Err(vm::Error::OutOfGas)
					}
				};

				Ok(inner())
			},
			CallCreateExecutiveKind::ExecCall(params, exec, mut unconfirmed_substate) => {
				assert!(!self.is_create);

				{
					let static_flag = self.static_flag;
					let is_create = self.is_create;
					let schedule = self.schedule;

					let mut pre_inner = || {
						Self::check_static_flag(&params, static_flag, is_create)?;
						state.checkpoint();
						Self::transfer_exec_balance(&params, schedule, state, substate)?;
						Ok(())
					};

					match pre_inner() {
						Ok(()) => (),
						Err(err) => return Ok(Err(err)),
					}
				}

				// TODO: tracing.

				let out = {
					let static_call = params.call_type == CallType::StaticCall;
					// TODO: use proper sub-tracers.
					let mut ext = Self::as_externalities(state, self.info, self.machine, self.schedule, self.depth, self.static_flag, OriginInfo::from(&params), &mut unconfirmed_substate, OutputPolicy::Return, tracer, vm_tracer, static_call);
					match exec.exec(&mut ext) {
						Ok(val) => Ok(val.finalize(ext)),
						Err(err) => Err(err),
					}
				};

				let res = match out {
					Ok(val) => val,
					Err(TrapError::Call(subparams, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCall(params, resume, unconfirmed_substate);
						return Err(TrapError::Call(subparams, self));
					},
					Err(TrapError::Create(subparams, address, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCreate(params, resume, unconfirmed_substate);
						return Err(TrapError::Create(subparams, address, self));
					},
				};

				Self::enact_result(&res, state, substate, unconfirmed_substate);
				Ok(res)
			},
			CallCreateExecutiveKind::ExecCreate(params, exec, mut unconfirmed_substate) => {
				assert!(self.is_create);

				{
					let static_flag = self.static_flag;
					let is_create = self.is_create;
					let schedule = self.schedule;

					let mut pre_inner = || {
						Self::check_eip684(&params, state);
						Self::check_static_flag(&params, static_flag, is_create)?;
						state.checkpoint();
						Self::transfer_exec_balance_and_init_contract(&params, schedule, state, substate)?;
						Ok(())
					};

					match pre_inner() {
						Ok(()) => (),
						Err(err) => return Ok(Err(err)),
					}
				}

				// TODO: tracing.

				let out = {
					let static_call = params.call_type == CallType::StaticCall;
					// TODO: use proper sub-tracers.
					let mut ext = Self::as_externalities(state, self.info, self.machine, self.schedule, self.depth, self.static_flag, OriginInfo::from(&params), &mut unconfirmed_substate, OutputPolicy::InitContract, tracer, vm_tracer, static_call);
					match exec.exec(&mut ext) {
						Ok(val) => Ok(val.finalize(ext)),
						Err(err) => Err(err),
					}
				};

				let res = match out {
					Ok(val) => val,
					Err(TrapError::Call(subparams, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCall(params, resume, unconfirmed_substate);
						return Err(TrapError::Call(subparams, self));
					},
					Err(TrapError::Create(subparams, address, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCreate(params, resume, unconfirmed_substate);
						return Err(TrapError::Create(subparams, address, self));
					},
				};

				Self::enact_result(&res, state, substate, unconfirmed_substate);
				Ok(res)
			},
			CallCreateExecutiveKind::ResumeCall(..) | CallCreateExecutiveKind::ResumeCreate(..) => panic!("This executive has already been executed once."),
		}
	}

	pub fn resume_call<B: 'a + StateBackend, T: Tracer, V: VMTracer>(mut self, result: vm::MessageCallResult, state: &mut State<B>, substate: &mut Substate, tracer: &mut T, vm_tracer: &mut V) -> ExecutiveTrapResult<'a, FinalizationResult> {
		match self.kind {
			CallCreateExecutiveKind::ResumeCall(params, resume, mut unconfirmed_substate) => {
				let out = {
					let exec = resume.resume_call(result);

					let static_call = params.call_type == CallType::StaticCall;
					// TODO: use proper sub-tracers.
					let mut ext = Self::as_externalities(state, self.info, self.machine, self.schedule, self.depth, self.static_flag, OriginInfo::from(&params), &mut unconfirmed_substate, if self.is_create { OutputPolicy::InitContract } else { OutputPolicy::Return }, tracer, vm_tracer, static_call);
					match exec.exec(&mut ext) {
						Ok(val) => Ok(val.finalize(ext)),
						Err(err) => Err(err),
					}
				};

				let res = match out {
					Ok(val) => val,
					Err(TrapError::Call(subparams, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCall(params, resume, unconfirmed_substate);
						return Err(TrapError::Call(subparams, self));
					},
					Err(TrapError::Create(subparams, address, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCreate(params, resume, unconfirmed_substate);
						return Err(TrapError::Create(subparams, address, self));
					},
				};

				Self::enact_result(&res, state, substate, unconfirmed_substate);
				Ok(res)
			},
			CallCreateExecutiveKind::ResumeCreate(..) =>
				panic!("Resumable as create, but called resume_call"),
			CallCreateExecutiveKind::Transfer(..) | CallCreateExecutiveKind::CallBuiltin(..) |
			CallCreateExecutiveKind::ExecCall(..) | CallCreateExecutiveKind::ExecCreate(..) =>
				panic!("Not resumable"),
		}
	}

	pub fn resume_create<B: 'a + StateBackend, T: Tracer, V: VMTracer>(mut self, result: vm::ContractCreateResult, state: &mut State<B>, substate: &mut Substate, tracer: &mut T, vm_tracer: &mut V) -> ExecutiveTrapResult<'a, FinalizationResult> {
		match self.kind {
			CallCreateExecutiveKind::ResumeCreate(params, resume, mut unconfirmed_substate) => {
				let out = {
					let exec = resume.resume_create(result);

					let static_call = params.call_type == CallType::StaticCall;
					// TODO: use proper sub-tracers.
					let mut ext = Self::as_externalities(state, self.info, self.machine, self.schedule, self.depth, self.static_flag, OriginInfo::from(&params), &mut unconfirmed_substate, if self.is_create { OutputPolicy::InitContract } else { OutputPolicy::Return }, tracer, vm_tracer, static_call);
					match exec.exec(&mut ext) {
						Ok(val) => Ok(val.finalize(ext)),
						Err(err) => Err(err),
					}
				};

				let res = match out {
					Ok(val) => val,
					Err(TrapError::Call(subparams, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCall(params, resume, unconfirmed_substate);
						return Err(TrapError::Call(subparams, self));
					},
					Err(TrapError::Create(subparams, address, resume)) => {
						self.kind = CallCreateExecutiveKind::ResumeCreate(params, resume, unconfirmed_substate);
						return Err(TrapError::Create(subparams, address, self));
					},
				};

				Self::enact_result(&res, state, substate, unconfirmed_substate);
				Ok(res)
			},
			CallCreateExecutiveKind::ResumeCall(..) =>
				panic!("Resumable as call, but called resume_create"),
			CallCreateExecutiveKind::Transfer(..) | CallCreateExecutiveKind::CallBuiltin(..) |
			CallCreateExecutiveKind::ExecCall(..) | CallCreateExecutiveKind::ExecCreate(..) =>
				panic!("Not resumable"),
		}
	}
}

/// Transaction executor.
pub struct Executive<'a, B: 'a> {
	state: &'a mut State<B>,
	info: &'a EnvInfo,
	machine: &'a Machine,
	schedule: &'a Schedule,
	depth: usize,
	static_flag: bool,
}

impl<'a, B: 'a + StateBackend> Executive<'a, B> {
	/// Basic constructor.
	pub fn new(state: &'a mut State<B>, info: &'a EnvInfo, machine: &'a Machine, schedule: &'a Schedule) -> Self {
		Executive {
			state: state,
			info: info,
			machine: machine,
			schedule: schedule,
			depth: 0,
			static_flag: false,
		}
	}

	/// Populates executive from parent properties. Increments executive depth.
	pub fn from_parent(state: &'a mut State<B>, info: &'a EnvInfo, machine: &'a Machine, schedule: &'a Schedule, parent_depth: usize, static_flag: bool) -> Self {
		Executive {
			state: state,
			info: info,
			machine: machine,
			schedule: schedule,
			depth: parent_depth + 1,
			static_flag: static_flag,
		}
	}

	/// This function should be used to execute transaction.
	pub fn transact<T, V>(&'a mut self, t: &SignedTransaction, options: TransactOptions<T, V>)
		-> Result<Executed<T::Output, V::Output>, ExecutionError> where T: Tracer, V: VMTracer,
	{
		self.transact_with_tracer(t, options.check_nonce, options.output_from_init_contract, options.tracer, options.vm_tracer)
	}

	/// Execute a transaction in a "virtual" context.
	/// This will ensure the caller has enough balance to execute the desired transaction.
	/// Used for extra-block executions for things like consensus contracts and RPCs
	pub fn transact_virtual<T, V>(&'a mut self, t: &SignedTransaction, options: TransactOptions<T, V>)
		-> Result<Executed<T::Output, V::Output>, ExecutionError> where T: Tracer, V: VMTracer,
	{
		let sender = t.sender();
		let balance = self.state.balance(&sender)?;
		let needed_balance = t.value.saturating_add(t.gas.saturating_mul(t.gas_price));
		if balance < needed_balance {
			// give the sender a sufficient balance
			self.state.add_balance(&sender, &(needed_balance - balance), CleanupMode::NoEmpty)?;
		}

		self.transact(t, options)
	}

	/// Execute transaction/call with tracing enabled
	fn transact_with_tracer<T, V>(
		&'a mut self,
		t: &SignedTransaction,
		check_nonce: bool,
		output_from_create: bool,
		mut tracer: T,
		mut vm_tracer: V
	) -> Result<Executed<T::Output, V::Output>, ExecutionError> where T: Tracer, V: VMTracer {
		let sender = t.sender();
		let nonce = self.state.nonce(&sender)?;

		let schedule = self.schedule;
		let base_gas_required = U256::from(t.gas_required(&schedule));

		if t.gas < base_gas_required {
			return Err(ExecutionError::NotEnoughBaseGas { required: base_gas_required, got: t.gas });
		}

		if !t.is_unsigned() && check_nonce && schedule.kill_dust != CleanDustMode::Off && !self.state.exists(&sender)? {
			return Err(ExecutionError::SenderMustExist);
		}

		let init_gas = t.gas - base_gas_required;

		// validate transaction nonce
		if check_nonce && t.nonce != nonce {
			return Err(ExecutionError::InvalidNonce { expected: nonce, got: t.nonce });
		}

		// validate if transaction fits into given block
		if self.info.gas_used + t.gas > self.info.gas_limit {
			return Err(ExecutionError::BlockGasLimitReached {
				gas_limit: self.info.gas_limit,
				gas_used: self.info.gas_used,
				gas: t.gas
			});
		}

		// TODO: we might need bigints here, or at least check overflows.
		let balance = self.state.balance(&sender)?;
		let gas_cost = t.gas.full_mul(t.gas_price);
		let total_cost = U512::from(t.value) + gas_cost;

		// avoid unaffordable transactions
		let balance512 = U512::from(balance);
		if balance512 < total_cost {
			return Err(ExecutionError::NotEnoughCash { required: total_cost, got: balance512 });
		}

		let mut substate = Substate::new();

		// NOTE: there can be no invalid transactions from this point.
		if !schedule.eip86 || !t.is_unsigned() {
			self.state.inc_nonce(&sender)?;
		}
		self.state.sub_balance(&sender, &U256::from(gas_cost), &mut substate.to_cleanup_mode(&schedule))?;

		let (result, output) = match t.action {
			Action::Create => {
				let (new_address, code_hash) = contract_address(self.machine.create_address_scheme(self.info.number), &sender, &nonce, &t.data);
				let params = ActionParams {
					code_address: new_address.clone(),
					code_hash: code_hash,
					address: new_address,
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: ActionValue::Transfer(t.value),
					code: Some(Arc::new(t.data.clone())),
					data: None,
					call_type: CallType::None,
					params_type: vm::ParamsType::Embedded,
				};
				let res = self.create(params, &mut substate, &mut tracer, &mut vm_tracer);
				let out = match &res {
					Ok(res) if output_from_create => res.return_data.to_vec(),
					_ => Vec::new(),
				};
				(res, out)
			},
			Action::Call(ref address) => {
				let params = ActionParams {
					code_address: address.clone(),
					address: address.clone(),
					sender: sender.clone(),
					origin: sender.clone(),
					gas: init_gas,
					gas_price: t.gas_price,
					value: ActionValue::Transfer(t.value),
					code: self.state.code(address)?,
					code_hash: self.state.code_hash(address)?,
					data: Some(t.data.clone()),
					call_type: CallType::Call,
					params_type: vm::ParamsType::Separate,
				};
				let res = self.call(params, &mut substate, &mut tracer, &mut vm_tracer);
				let out = match &res {
					Ok(res) => res.return_data.to_vec(),
					_ => Vec::new(),
				};
				(res, out)
			}
		};

		// finalize here!
		Ok(self.finalize(t, substate, result, output, tracer.drain(), vm_tracer.drain())?)
	}

	/// Calls contract function with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate and the output.
	/// Returns either gas_left or `vm::Error`.
	pub fn call<T, V>(
		&mut self,
		params: ActionParams,
		substate: &mut Substate,
		tracer: &mut T,
		vm_tracer: &mut V
	) -> vm::Result<FinalizationResult> where T: Tracer, V: VMTracer {
		let vm_factory = self.state.vm_factory();
		CallCreateExecutive::new_call_raw(
			params,
			self.info,
			self.machine,
			self.schedule,
			&vm_factory,
			self.depth,
			self.static_flag
		).exec(self.state, substate, tracer, vm_tracer).ok().unwrap()
	}

	/// Creates contract with given contract params.
	/// NOTE. It does not finalize the transaction (doesn't do refunds, nor suicides).
	/// Modifies the substate.
	pub fn create<T, V>(
		&mut self,
		params: ActionParams,
		substate: &mut Substate,
		tracer: &mut T,
		vm_tracer: &mut V,
	) -> vm::Result<FinalizationResult> where T: Tracer, V: VMTracer {
		let vm_factory = self.state.vm_factory();
		CallCreateExecutive::new_create_raw(
			params,
			self.info,
			self.machine,
			self.schedule,
			&vm_factory,
			self.depth,
			self.static_flag
		).exec(self.state, substate, tracer, vm_tracer).ok().unwrap()
	}

	/// Finalizes the transaction (does refunds and suicides).
	fn finalize<T, V>(
		&mut self,
		t: &SignedTransaction,
		mut substate: Substate,
		result: vm::Result<FinalizationResult>,
		output: Bytes,
		trace: Vec<T>,
		vm_trace: Option<V>
	) -> Result<Executed<T, V>, ExecutionError> {
		let schedule = self.schedule;

		// refunds from SSTORE nonzero -> zero
		let sstore_refunds = U256::from(schedule.sstore_refund_gas) * substate.sstore_clears_count;
		// refunds from contract suicides
		let suicide_refunds = U256::from(schedule.suicide_refund_gas) * U256::from(substate.suicides.len());
		let refunds_bound = sstore_refunds + suicide_refunds;

		// real ammount to refund
		let gas_left_prerefund = match result { Ok(FinalizationResult{ gas_left, .. }) => gas_left, _ => 0.into() };
		let refunded = cmp::min(refunds_bound, (t.gas - gas_left_prerefund) >> 1);
		let gas_left = gas_left_prerefund + refunded;

		let gas_used = t.gas - gas_left;
		let refund_value = gas_left * t.gas_price;
		let fees_value = gas_used * t.gas_price;

		trace!("exec::finalize: t.gas={}, sstore_refunds={}, suicide_refunds={}, refunds_bound={}, gas_left_prerefund={}, refunded={}, gas_left={}, gas_used={}, refund_value={}, fees_value={}\n",
			t.gas, sstore_refunds, suicide_refunds, refunds_bound, gas_left_prerefund, refunded, gas_left, gas_used, refund_value, fees_value);

		let sender = t.sender();
		trace!("exec::finalize: Refunding refund_value={}, sender={}\n", refund_value, sender);
		// Below: NoEmpty is safe since the sender must already be non-null to have sent this transaction
		self.state.add_balance(&sender, &refund_value, CleanupMode::NoEmpty)?;
		trace!("exec::finalize: Compensating author: fees_value={}, author={}\n", fees_value, &self.info.author);
		self.state.add_balance(&self.info.author, &fees_value, substate.to_cleanup_mode(&schedule))?;

		// perform suicides
		for address in &substate.suicides {
			self.state.kill_account(address);
		}

		// perform garbage-collection
		let min_balance = if schedule.kill_dust != CleanDustMode::Off { Some(U256::from(schedule.tx_gas) * t.gas_price) } else { None };
		self.state.kill_garbage(&substate.touched, schedule.kill_empty, &min_balance, schedule.kill_dust == CleanDustMode::WithCodeAndStorage)?;

		match result {
			Err(vm::Error::Internal(msg)) => Err(ExecutionError::Internal(msg)),
			Err(exception) => {
				Ok(Executed {
					exception: Some(exception),
					gas: t.gas,
					gas_used: t.gas,
					refunded: U256::zero(),
					cumulative_gas_used: self.info.gas_used + t.gas,
					logs: vec![],
					contracts_created: vec![],
					output: output,
					trace: trace,
					vm_trace: vm_trace,
					state_diff: None,
				})
			},
			Ok(r) => {
				Ok(Executed {
					exception: if r.apply_state { None } else { Some(vm::Error::Reverted) },
					gas: t.gas,
					gas_used: gas_used,
					refunded: refunded,
					cumulative_gas_used: self.info.gas_used + gas_used,
					logs: substate.logs,
					contracts_created: substate.contracts_created,
					output: output,
					trace: trace,
					vm_trace: vm_trace,
					state_diff: None,
				})
			},
		}
	}
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
	use std::sync::Arc;
	use std::str::FromStr;
	use rustc_hex::FromHex;
	use ethkey::{Generator, Random};
	use super::*;
	use ethereum_types::{H256, U256, U512, Address};
	use vm::{ActionParams, ActionValue, CallType, EnvInfo, CreateContractAddress};
	use evm::{Factory, VMType};
	use error::ExecutionError;
	use machine::EthereumMachine;
	use state::{Substate, CleanupMode};
	use test_helpers::{get_temp_state_with_factory, get_temp_state};
	use trace::trace;
	use trace::{FlatTrace, Tracer, NoopTracer, ExecutiveTracer};
	use trace::{VMTrace, VMOperation, VMExecutedOperation, MemoryDiff, StorageDiff, VMTracer, NoopVMTracer, ExecutiveVMTracer};
	use transaction::{Action, Transaction};

	fn make_frontier_machine(max_depth: usize) -> EthereumMachine {
		let mut machine = ::ethereum::new_frontier_test_machine();
		machine.set_schedule_creation_rules(Box::new(move |s, _| s.max_depth = max_depth));
		machine
	}

	fn make_byzantium_machine(max_depth: usize) -> EthereumMachine {
		let mut machine = ::ethereum::new_byzantium_test_machine();
		machine.set_schedule_creation_rules(Box::new(move |s, _| s.max_depth = max_depth));
		machine
	}

	#[test]
	fn test_contract_address() {
		let address = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let expected_address = Address::from_str("3f09c73a5ed19289fb9bdc72f1742566df146f56").unwrap();
		assert_eq!(expected_address, contract_address(CreateContractAddress::FromSenderAndNonce, &address, &U256::from(88), &[]).0);
	}

	// TODO: replace params with transactions!
	evm_test!{test_sender_balance: test_sender_balance_int}
	fn test_sender_balance(factory: Factory) {
		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new("3331600055".from_hex().unwrap()));
		params.value = ActionValue::Transfer(U256::from(0x7));
		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(0x100u64), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(79_975));
		assert_eq!(state.storage_at(&address, &H256::new()).unwrap(), H256::from(&U256::from(0xf9u64)));
		assert_eq!(state.balance(&sender).unwrap(), U256::from(0xf9));
		assert_eq!(state.balance(&address).unwrap(), U256::from(0x7));
		assert_eq!(substate.contracts_created.len(), 0);

		// TODO: just test state root.
	}

	evm_test!{test_create_contract_out_of_depth: test_create_contract_out_of_depth_int}
	fn test_create_contract_out_of_depth(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(62_976));
		// ended with max depth
		assert_eq!(substate.contracts_created.len(), 0);
	}

	#[test]
	fn test_call_to_precompiled_tracing() {
		// code:
		//
		// 60 00 - push 00 out size
		// 60 00 - push 00 out offset
		// 60 00 - push 00 in size
		// 60 00 - push 00 in offset
		// 60 01 - push 01 value
		// 60 03 - push 03 to
		// 61 ffff - push fff gas
		// f1 - CALL

		let code = "60006000600060006001600361fffff1".from_hex().unwrap();
		let sender = Address::from_str("4444444444444444444444444444444444444444").unwrap();
		let address = Address::from_str("5555555555555555555555555555555555555555").unwrap();

		let mut params = ActionParams::default();
		params.address = address.clone();
		params.code_address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		params.call_type = CallType::Call;
		let mut state = get_temp_state();
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_byzantium_machine(5);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();
		let mut tracer = ExecutiveTracer::default();
		let mut vm_tracer = ExecutiveVMTracer::toplevel();

		let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
		ex.call(params, &mut substate, &mut tracer, &mut vm_tracer).unwrap();

		assert_eq!(tracer.drain(), vec![FlatTrace {
			action: trace::Action::Call(trace::Call {
				from: "4444444444444444444444444444444444444444".into(),
				to: "5555555555555555555555555555555555555555".into(),
				value: 100.into(),
				gas: 100_000.into(),
				input: vec![],
				call_type: CallType::Call
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: 33021.into(),
				output: vec![]
			}),
			subtraces: 1,
			trace_address: Default::default()
		}, FlatTrace {
			action: trace::Action::Call(trace::Call {
				from: "5555555555555555555555555555555555555555".into(),
				to: "0000000000000000000000000000000000000003".into(),
				value: 1.into(),
				gas: 66560.into(),
				input: vec![],
				call_type: CallType::Call
			}), result: trace::Res::Call(trace::CallResult {
				gas_used: 600.into(),
				output: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 156, 17, 133, 165, 197, 233, 252, 84, 97, 40, 8, 151, 126, 232, 245, 72, 178, 37, 141, 49]
			}),
			subtraces: 0,
			trace_address: vec![0].into_iter().collect(),
		}]);
	}

	#[test]
	// Tracing is not suported in JIT
	fn test_call_to_create() {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 23
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.code_address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		params.call_type = CallType::Call;
		let mut state = get_temp_state();
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(5);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();
		let mut tracer = ExecutiveTracer::default();
		let mut vm_tracer = ExecutiveVMTracer::toplevel();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut substate, &mut tracer, &mut vm_tracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(44_752));

		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			subtraces: 1,
			action: trace::Action::Call(trace::Call {
				from: "cd1722f3947def4cf144679da39c4c32bdc35681".into(),
				to: "b010143a42d5980c7e5ef0e4a4416dc098a4fed3".into(),
				value: 100.into(),
				gas: 100000.into(),
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(55_248),
				output: vec![],
			}),
		}, FlatTrace {
			trace_address: vec![0].into_iter().collect(),
			subtraces: 0,
			action: trace::Action::Create(trace::Create {
				from: "b010143a42d5980c7e5ef0e4a4416dc098a4fed3".into(),
				value: 23.into(),
				gas: 67979.into(),
				init: vec![96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85]
			}),
			result: trace::Res::Create(trace::CreateResult {
				gas_used: U256::from(3224),
				address: Address::from_str("c6d80f262ae5e0f164e5fde365044d7ada2bfa34").unwrap(),
				code: vec![96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53]
			}),
		}];

		assert_eq!(tracer.drain(), expected_trace);

		let expected_vm_trace = VMTrace {
			parent_step: 0,
			code: vec![124, 96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85, 96, 0, 82, 96, 29, 96, 3, 96, 23, 240, 96, 0, 85],
			operations: vec![
				VMOperation { pc: 0, instruction: 124, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99997.into(), stack_push: vec_into![U256::from_dec_str("2589892687202724018173567190521546555304938078595079151649957320078677").unwrap()], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 30, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99994.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 32, instruction: 82, gas_cost: 6.into(), executed: Some(VMExecutedOperation { gas_used: 99988.into(), stack_push: vec_into![], mem_diff: Some(MemoryDiff { offset: 0, data: vec![0, 0, 0, 96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85] }), store_diff: None }) },
				VMOperation { pc: 33, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99985.into(), stack_push: vec_into![29], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 35, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99982.into(), stack_push: vec_into![3], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 37, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99979.into(), stack_push: vec_into![23], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 39, instruction: 240, gas_cost: 99979.into(), executed: Some(VMExecutedOperation { gas_used: 64755.into(), stack_push: vec_into![U256::from_dec_str("1135198453258042933984631383966629874710669425204").unwrap()], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 40, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 64752.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 42, instruction: 85, gas_cost: 20000.into(), executed: Some(VMExecutedOperation { gas_used: 44752.into(), stack_push: vec_into![], mem_diff: None, store_diff: Some(StorageDiff { location: 0.into(), value: U256::from_dec_str("1135198453258042933984631383966629874710669425204").unwrap() }) }) }
			],
			subs: vec![
				VMTrace {
					parent_step: 6,
					code: vec![96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85],
					operations: vec![
						VMOperation { pc: 0, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 67976.into(), stack_push: vec_into![16], mem_diff: None, store_diff: None }) },
						VMOperation { pc: 2, instruction: 128, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 67973.into(), stack_push: vec_into![16, 16], mem_diff: None, store_diff: None }) },
						VMOperation { pc: 3, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 67970.into(), stack_push: vec_into![12], mem_diff: None, store_diff: None }) },
						VMOperation { pc: 5, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 67967.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
						VMOperation { pc: 7, instruction: 57, gas_cost: 9.into(), executed: Some(VMExecutedOperation { gas_used: 67958.into(), stack_push: vec_into![], mem_diff: Some(MemoryDiff { offset: 0, data: vec![96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53] }), store_diff: None }) },
						VMOperation { pc: 8, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 67955.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
						VMOperation { pc: 10, instruction: 243, gas_cost: 0.into(), executed: Some(VMExecutedOperation { gas_used: 67955.into(), stack_push: vec_into![], mem_diff: None, store_diff: None }) }
					],
					subs: vec![]
				}
			]
		};
		assert_eq!(vm_tracer.drain().unwrap(), expected_vm_trace);
	}

	#[test]
	fn test_trace_reverted_create() {
		// code:
		//
		// 65 60016000fd - push 5 bytes
		// 60 00 - push 0
		// 52 mstore
		// 60 05 - push 5
		// 60 1b - push 27
		// 60 17 - push 23
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 01
		// 60 00
		// fd - revert

		let code = "6460016000fd6000526005601b6017f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.code_address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		params.call_type = CallType::Call;
		let mut state = get_temp_state();
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = ::ethereum::new_byzantium_test_machine();
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();
		let mut tracer = ExecutiveTracer::default();
		let mut vm_tracer = ExecutiveVMTracer::toplevel();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut substate, &mut tracer, &mut vm_tracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(62967));

		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			subtraces: 1,
			action: trace::Action::Call(trace::Call {
				from: "cd1722f3947def4cf144679da39c4c32bdc35681".into(),
				to: "b010143a42d5980c7e5ef0e4a4416dc098a4fed3".into(),
				value: 100.into(),
				gas: 100_000.into(),
				input: vec![],
				call_type: CallType::Call,
			}),
			result: trace::Res::Call(trace::CallResult {
				gas_used: U256::from(37_033),
				output: vec![],
			}),
		}, FlatTrace {
			trace_address: vec![0].into_iter().collect(),
			subtraces: 0,
			action: trace::Action::Create(trace::Create {
				from: "b010143a42d5980c7e5ef0e4a4416dc098a4fed3".into(),
				value: 23.into(),
				gas: 66_917.into(),
				init: vec![0x60, 0x01, 0x60, 0x00, 0xfd]
			}),
			result: trace::Res::FailedCreate(vm::Error::Reverted.into()),
		}];

		assert_eq!(tracer.drain(), expected_trace);
	}

	#[test]
	fn test_create_contract() {
		// Tracing is not supported in JIT
		// code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "601080600c6000396000f3006000355415600957005b60203560003555".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(100.into());
		let mut state = get_temp_state();
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(5);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();
		let mut tracer = ExecutiveTracer::default();
		let mut vm_tracer = ExecutiveVMTracer::toplevel();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params.clone(), &mut substate, &mut tracer, &mut vm_tracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(96_776));

		let expected_trace = vec![FlatTrace {
			trace_address: Default::default(),
			subtraces: 0,
			action: trace::Action::Create(trace::Create {
				from: params.sender,
				value: 100.into(),
				gas: params.gas,
				init: vec![96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85],
			}),
			result: trace::Res::Create(trace::CreateResult {
				gas_used: U256::from(3224),
				address: params.address,
				code: vec![96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53]
			}),
		}];

		assert_eq!(tracer.drain(), expected_trace);

		let expected_vm_trace = VMTrace {
			parent_step: 0,
			code: vec![96, 16, 128, 96, 12, 96, 0, 57, 96, 0, 243, 0, 96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53, 85],
			operations: vec![
				VMOperation { pc: 0, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99997.into(), stack_push: vec_into![16], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 2, instruction: 128, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99994.into(), stack_push: vec_into![16, 16], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 3, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99991.into(), stack_push: vec_into![12], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 5, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99988.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 7, instruction: 57, gas_cost: 9.into(), executed: Some(VMExecutedOperation { gas_used: 99979.into(), stack_push: vec_into![], mem_diff: Some(MemoryDiff { offset: 0, data: vec![96, 0, 53, 84, 21, 96, 9, 87, 0, 91, 96, 32, 53, 96, 0, 53] }), store_diff: None }) },
				VMOperation { pc: 8, instruction: 96, gas_cost: 3.into(), executed: Some(VMExecutedOperation { gas_used: 99976.into(), stack_push: vec_into![0], mem_diff: None, store_diff: None }) },
				VMOperation { pc: 10, instruction: 243, gas_cost: 0.into(), executed: Some(VMExecutedOperation { gas_used: 99976.into(), stack_push: vec_into![], mem_diff: None, store_diff: None }) }
			],
			subs: vec![]
		};
		assert_eq!(vm_tracer.drain().unwrap(), expected_vm_trace);
	}

	evm_test!{test_create_contract_value_too_high: test_create_contract_value_too_high_int}
	fn test_create_contract_value_too_high(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 e6 - push 230
		// f0 - create a contract trying to send 230.
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d600360e6f0600055".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(62_976));
		assert_eq!(substate.contracts_created.len(), 0);
	}

	evm_test!{test_create_contract_without_max_depth: test_create_contract_without_max_depth_int}
	fn test_create_contract_without_max_depth(factory: Factory) {
		// code:
		//
		// 7c 601080600c6000396000f3006000355415600957005b60203560003555 - push 29 bytes?
		// 60 00 - push 0
		// 52
		// 60 1d - push 29
		// 60 03 - push 3
		// 60 17 - push 17
		// f0 - create
		// 60 00 - push 0
		// 55 sstore
		//
		// other code:
		//
		// 60 10 - push 16
		// 80 - duplicate first stack item
		// 60 0c - push 12
		// 60 00 - push 0
		// 39 - copy current code to memory
		// 60 00 - push 0
		// f3 - return

		let code = "7c601080600c6000396000f3006000355415600957005b60203560003555600052601d60036017f0".from_hex().unwrap();

		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		let next_address = contract_address(CreateContractAddress::FromSenderAndNonce, &address, &U256::zero(), &[]).0;
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from(100));
		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(100), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(1024);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		{
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap();
		}

		assert_eq!(substate.contracts_created.len(), 1);
		assert_eq!(substate.contracts_created[0], next_address);
	}

	// test is incorrect, mk
	// TODO: fix (preferred) or remove
	evm_test_ignore!{test_aba_calls: test_aba_calls_int}
	fn test_aba_calls(factory: Factory) {
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 18 - push 18
		// 73 945304eb96065b2a98b57a48a06ae28d285a71b5 - push this address
		// 61 03e8 - push 1000
		// f1 - message call
		// 58 - get PC
		// 55 - sstore

		let code_a = "6000600060006000601873945304eb96065b2a98b57a48a06ae28d285a71b56103e8f15855".from_hex().unwrap();

		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 17 - push 17
		// 73 0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6 - push this address
		// 61 0x01f4 - push 500
		// f1 - message call
		// 60 01 - push 1
		// 01 - add
		// 58 - get PC
		// 55 - sstore
		let code_b = "60006000600060006017730f572e5295c57f15886f9b263e2f6d2d6c7b5ec66101f4f16001015855".from_hex().unwrap();

		let address_a = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address_b = Address::from_str("945304eb96065b2a98b57a48a06ae28d285a71b5" ).unwrap();
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();

		let mut params = ActionParams::default();
		params.address = address_a.clone();
		params.sender = sender.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code_a.clone()));
		params.value = ActionValue::Transfer(U256::from(100_000));

		let mut state = get_temp_state_with_factory(factory);
		state.init_code(&address_a, code_a.clone()).unwrap();
		state.init_code(&address_b, code_b.clone()).unwrap();
		state.add_balance(&sender, &U256::from(100_000), CleanupMode::NoEmpty).unwrap();

		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(73_237));
		assert_eq!(state.storage_at(&address_a, &H256::from(&U256::from(0x23))).unwrap(), H256::from(&U256::from(1)));
	}

	// test is incorrect, mk
	// TODO: fix (preferred) or remove
	evm_test_ignore!{test_recursive_bomb1: test_recursive_bomb1_int}
	fn test_recursive_bomb1(factory: Factory) {
		// 60 01 - push 1
		// 60 00 - push 0
		// 54 - sload
		// 01 - add
		// 60 00 - push 0
		// 55 - sstore
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 60 00 - push 0
		// 30 - load address
		// 60 e0 - push e0
		// 5a - get gas
		// 03 - sub
		// f1 - message call (self in this case)
		// 60 01 - push 1
		// 55 - sstore
		let sender = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let code = "600160005401600055600060006000600060003060e05a03f1600155".from_hex().unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.gas = U256::from(100_000);
		params.code = Some(Arc::new(code.clone()));
		let mut state = get_temp_state_with_factory(factory);
		state.init_code(&address, code).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let FinalizationResult { gas_left, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};

		assert_eq!(gas_left, U256::from(59_870));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::zero())).unwrap(), H256::from(&U256::from(1)));
		assert_eq!(state.storage_at(&address, &H256::from(&U256::one())).unwrap(), H256::from(&U256::from(1)));
	}

	// test is incorrect, mk
	// TODO: fix (preferred) or remove
	evm_test_ignore!{test_transact_simple: test_transact_simple_int}
	fn test_transact_simple(factory: Factory) {
		let keypair = Random.generate().unwrap();
		let t = Transaction {
			action: Action::Create,
			value: U256::from(17),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(100_000),
			gas_price: U256::zero(),
			nonce: U256::zero()
		}.sign(keypair.secret(), None);
		let sender = t.sender();
		let contract = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;

		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(18), CleanupMode::NoEmpty).unwrap();
		let mut info = EnvInfo::default();
		info.gas_limit = U256::from(100_000);
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);

		let executed = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			let opts = TransactOptions::with_no_tracing();
			ex.transact(&t, opts).unwrap()
		};

		assert_eq!(executed.gas, U256::from(100_000));
		assert_eq!(executed.gas_used, U256::from(41_301));
		assert_eq!(executed.refunded, U256::from(58_699));
		assert_eq!(executed.cumulative_gas_used, U256::from(41_301));
		assert_eq!(executed.logs.len(), 0);
		assert_eq!(executed.contracts_created.len(), 0);
		assert_eq!(state.balance(&sender).unwrap(), U256::from(1));
		assert_eq!(state.balance(&contract).unwrap(), U256::from(17));
		assert_eq!(state.nonce(&sender).unwrap(), U256::from(1));
		assert_eq!(state.storage_at(&contract, &H256::new()).unwrap(), H256::from(&U256::from(1)));
	}

	evm_test!{test_transact_invalid_nonce: test_transact_invalid_nonce_int}
	fn test_transact_invalid_nonce(factory: Factory) {
		let keypair = Random.generate().unwrap();
		let t = Transaction {
			action: Action::Create,
			value: U256::from(17),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(100_000),
			gas_price: U256::zero(),
			nonce: U256::one()
		}.sign(keypair.secret(), None);
		let sender = t.sender();

		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(17), CleanupMode::NoEmpty).unwrap();
		let mut info = EnvInfo::default();
		info.gas_limit = U256::from(100_000);
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			let opts = TransactOptions::with_no_tracing();
			ex.transact(&t, opts)
		};

		match res {
			Err(ExecutionError::InvalidNonce { expected, got })
				if expected == U256::zero() && got == U256::one() => (),
			_ => assert!(false, "Expected invalid nonce error.")
		}
	}

	evm_test!{test_transact_gas_limit_reached: test_transact_gas_limit_reached_int}
	fn test_transact_gas_limit_reached(factory: Factory) {
		let keypair = Random.generate().unwrap();
		let t = Transaction {
			action: Action::Create,
			value: U256::from(17),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(80_001),
			gas_price: U256::zero(),
			nonce: U256::zero()
		}.sign(keypair.secret(), None);
		let sender = t.sender();

		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(17), CleanupMode::NoEmpty).unwrap();
		let mut info = EnvInfo::default();
		info.gas_used = U256::from(20_000);
		info.gas_limit = U256::from(100_000);
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			let opts = TransactOptions::with_no_tracing();
			ex.transact(&t, opts)
		};

		match res {
			Err(ExecutionError::BlockGasLimitReached { gas_limit, gas_used, gas })
				if gas_limit == U256::from(100_000) && gas_used == U256::from(20_000) && gas == U256::from(80_001) => (),
			_ => assert!(false, "Expected block gas limit error.")
		}
	}

	evm_test!{test_not_enough_cash: test_not_enough_cash_int}
	fn test_not_enough_cash(factory: Factory) {

		let keypair = Random.generate().unwrap();
		let t = Transaction {
			action: Action::Create,
			value: U256::from(18),
			data: "3331600055".from_hex().unwrap(),
			gas: U256::from(100_000),
			gas_price: U256::one(),
			nonce: U256::zero()
		}.sign(keypair.secret(), None);
		let sender = t.sender();

		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from(100_017), CleanupMode::NoEmpty).unwrap();
		let mut info = EnvInfo::default();
		info.gas_limit = U256::from(100_000);
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);

		let res = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			let opts = TransactOptions::with_no_tracing();
			ex.transact(&t, opts)
		};

		match res {
			Err(ExecutionError::NotEnoughCash { required , got })
				if required == U512::from(100_018) && got == U512::from(100_017) => (),
			_ => assert!(false, "Expected not enough cash error. {:?}", res)
		}
	}

	evm_test!{test_keccak: test_keccak_int}
	fn test_keccak(factory: Factory) {
		let code = "6064640fffffffff20600055".from_hex().unwrap();

		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		let address = contract_address(CreateContractAddress::FromSenderAndNonce, &sender, &U256::zero(), &[]).0;
		// TODO: add tests for 'callcreate'
		//let next_address = contract_address(&address, &U256::zero());
		let mut params = ActionParams::default();
		params.address = address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(0x0186a0);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::from_str("0de0b6b3a7640000").unwrap());
		let mut state = get_temp_state_with_factory(factory);
		state.add_balance(&sender, &U256::from_str("152d02c7e14af6800000").unwrap(), CleanupMode::NoEmpty).unwrap();
		let info = EnvInfo::default();
		let machine = make_frontier_machine(0);
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let result = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.create(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer)
		};

		match result {
			Err(_) => {},
			_ => panic!("Expected OutOfGas"),
		}
	}

	evm_test!{test_revert: test_revert_int}
	fn test_revert(factory: Factory) {
		let contract_address = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();
		// EIP-140 test case
		let code = "6c726576657274656420646174616000557f726576657274206d657373616765000000000000000000000000000000000000600052600e6000fd".from_hex().unwrap();
		let returns = "726576657274206d657373616765".from_hex().unwrap();
		let mut state = get_temp_state_with_factory(factory.clone());
		state.add_balance(&sender, &U256::from_str("152d02c7e14af68000000").unwrap(), CleanupMode::NoEmpty).unwrap();
		state.commit().unwrap();

		let mut params = ActionParams::default();
		params.address = contract_address.clone();
		params.sender = sender.clone();
		params.origin = sender.clone();
		params.gas = U256::from(20025);
		params.code = Some(Arc::new(code));
		params.value = ActionValue::Transfer(U256::zero());
		let info = EnvInfo::default();
		let machine = ::ethereum::new_byzantium_test_machine();
		let schedule = machine.schedule(info.number);
		let mut substate = Substate::new();

		let mut output = [0u8; 14];
		let FinalizationResult { gas_left: result, return_data, .. } = {
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut substate, &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};
		(&mut output).copy_from_slice(&return_data[..(cmp::min(14, return_data.len()))]);

		assert_eq!(result, U256::from(1));
		assert_eq!(output[..], returns[..]);
		assert_eq!(state.storage_at(&contract_address, &H256::from(&U256::zero())).unwrap(), H256::from(&U256::from(0)));
	}

	fn wasm_sample_code() -> Arc<Vec<u8>> {
		Arc::new(
			"0061736d01000000010d0360027f7f0060017f0060000002270303656e7603726574000003656e760673656e646572000103656e76066d656d6f727902010110030201020404017000000501000708010463616c6c00020901000ac10101be0102057f017e4100410028020441c0006b22043602042004412c6a41106a220041003602002004412c6a41086a22014200370200200441186a41106a22024100360200200441186a41086a220342003703002004420037022c2004410036021c20044100360218200441186a1001200020022802002202360200200120032903002205370200200441106a2002360200200441086a200537030020042004290318220537022c200420053703002004411410004100200441c0006a3602040b0b0a010041040b0410c00000"
			.from_hex()
			.unwrap()
		)
	}

	#[test]
	fn wasm_activated_test() {
		let contract_address = Address::from_str("cd1722f3947def4cf144679da39c4c32bdc35681").unwrap();
		let sender = Address::from_str("0f572e5295c57f15886f9b263e2f6d2d6c7b5ec6").unwrap();

		let mut state = get_temp_state();
		state.add_balance(&sender, &U256::from(10000000000u64), CleanupMode::NoEmpty).unwrap();
		state.commit().unwrap();

		let mut params = ActionParams::default();
		params.origin = sender.clone();
		params.sender = sender.clone();
		params.address = contract_address.clone();
		params.gas = U256::from(20025);
		params.code = Some(wasm_sample_code());

		let mut info = EnvInfo::default();

		// 100 > 10
		info.number = 100;

		// Network with wasm activated at block 10
		let machine = ::ethereum::new_kovan_wasm_test_machine();

		let mut output = [0u8; 20];
		let FinalizationResult { gas_left: result, return_data, .. } = {
			let schedule = machine.schedule(info.number);
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params.clone(), &mut Substate::new(), &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};
		(&mut output).copy_from_slice(&return_data[..(cmp::min(20, return_data.len()))]);

		assert_eq!(result, U256::from(18433));
		// Transaction successfully returned sender
		assert_eq!(output[..], sender[..]);

		// 1 < 10
		info.number = 1;

		let mut output = [0u8; 20];
		let FinalizationResult { gas_left: result, return_data, .. } = {
			let schedule = machine.schedule(info.number);
			let mut ex = Executive::new(&mut state, &info, &machine, &schedule);
			ex.call(params, &mut Substate::new(), &mut NoopTracer, &mut NoopVMTracer).unwrap()
		};
		(&mut output[..((cmp::min(20, return_data.len())))]).copy_from_slice(&return_data[..(cmp::min(20, return_data.len()))]);

		assert_eq!(result, U256::from(20025));
		// Since transaction errored due to wasm was not activated, result is just empty
		assert_eq!(output[..], [0u8; 20][..]);
	}
}
