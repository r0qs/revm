pub mod analysis;
mod contract;
#[cfg(feature = "serde")]
pub mod serde;
mod shared_memory;
mod stack;

pub use contract::Contract;
pub use shared_memory::{num_words, SharedMemory, EMPTY_SHARED_MEMORY};
pub use stack::{Stack, STACK_LIMIT};

use crate::EOFCreateOutcome;
use crate::{
    gas, push, push_b256, return_ok, return_revert, CallOutcome, CreateOutcome, FunctionStack, Gas,
    Host, InstructionResult, InterpreterAction,
};
use crate::{CallInputs, CallScheme, CallValue};
use core::cmp::min;
use core::ops::Range;
use revm_primitives::{Address, Bytecode, Bytes, Eof, U256};
use std::borrow::ToOwned;

use eth_riscv_interpreter::setup_from_elf;
use rvemu::{emulator::Emulator, exception::Exception};

/// EVM bytecode interpreter.
#[derive(Debug)]
pub struct Interpreter {
    /// The current instruction pointer.
    pub instruction_pointer: *const u8,
    /// The gas state.
    pub gas: Gas,
    /// Contract information and invoking data
    pub contract: Contract,
    /// The execution control flag. If this is not set to `Continue`, the interpreter will stop
    /// execution.
    pub instruction_result: InstructionResult,
    /// Currently run Bytecode that instruction result will point to.
    /// Bytecode is owned by the contract.
    pub bytecode: Bytes,
    /// Whether we are Interpreting the Ethereum Object Format (EOF) bytecode.
    /// This is local field that is set from `contract.is_eof()`.
    pub is_eof: bool,
    /// Is init flag for eof create
    pub is_eof_init: bool,
    /// Shared memory.
    ///
    /// Note: This field is only set while running the interpreter loop.
    /// Otherwise it is taken and replaced with empty shared memory.
    pub shared_memory: SharedMemory,
    /// Stack.
    pub stack: Stack,
    /// EOF function stack.
    pub function_stack: FunctionStack,
    /// The return data buffer for internal calls.
    /// It has multi usage:
    ///
    /// * It contains the output bytes of call sub call.
    /// * When this interpreter finishes execution it contains the output bytes of this contract.
    pub return_data_buffer: Bytes,
    /// Whether the interpreter is in "staticcall" mode, meaning no state changes can happen.
    pub is_static: bool,
    /// Actions that the EVM should do.
    ///
    /// Set inside CALL or CREATE instructions and RETURN or REVERT instructions. Additionally those instructions will set
    /// InstructionResult to CallOrCreate/Return/Revert so we know the reason.
    pub next_action: InterpreterAction,

    pub riscv_emulator: Option<RVEmu>,
}

#[derive(Debug)]
struct RVEmu {
    emu: Emulator,
    returned_data_destiny: Option<Range<u64>>,
}

impl Default for Interpreter {
    fn default() -> Self {
        Self::new(Contract::default(), 0, false)
    }
}

/// The result of an interpreter operation.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(::serde::Serialize, ::serde::Deserialize))]
pub struct InterpreterResult {
    /// The result of the instruction execution.
    pub result: InstructionResult,
    /// The output of the instruction execution.
    pub output: Bytes,
    /// The gas usage information.
    pub gas: Gas,
}

impl Interpreter {
    /// Create new interpreter
    pub fn new(contract: Contract, gas_limit: u64, is_static: bool) -> Self {
        if !contract.bytecode.is_execution_ready() {
            panic!("Contract is not execution ready {:?}", contract.bytecode);
        }
        let is_eof = contract.bytecode.is_eof();
        let bytecode = contract.bytecode.bytecode().clone();

        let riscv_emulator = if bytecode[0] == 0xFF {
            let emu = setup_from_elf(&bytecode[1..], &contract.input);
            Some(RVEmu {
                emu,
                returned_data_destiny: None,
            })
        } else {
            None
        };

        Self {
            instruction_pointer: bytecode.as_ptr(),
            bytecode,
            contract,
            gas: Gas::new(gas_limit),
            instruction_result: InstructionResult::Continue,
            function_stack: FunctionStack::default(),
            is_static,
            is_eof,
            is_eof_init: false,
            return_data_buffer: Bytes::new(),
            shared_memory: EMPTY_SHARED_MEMORY,
            stack: Stack::new(),
            next_action: InterpreterAction::None,
            riscv_emulator,
        }
    }

    /// Set set is_eof_init to true, this is used to enable `RETURNCONTRACT` opcode.
    #[inline]
    pub fn set_is_eof_init(&mut self) {
        self.is_eof_init = true;
    }

    #[inline]
    pub fn eof(&self) -> Option<&Eof> {
        self.contract.bytecode.eof()
    }

    /// Test related helper
    #[cfg(test)]
    pub fn new_bytecode(bytecode: Bytecode) -> Self {
        Self::new(
            Contract::new(
                Bytes::new(),
                bytecode,
                None,
                crate::primitives::Address::default(),
                crate::primitives::Address::default(),
                U256::ZERO,
            ),
            0,
            false,
        )
    }

    /// Load EOF code into interpreter. PC is assumed to be correctly set
    pub(crate) fn load_eof_code(&mut self, idx: usize, pc: usize) {
        // SAFETY: eof flag is true only if bytecode is Eof.
        let Bytecode::Eof(eof) = &self.contract.bytecode else {
            panic!("Expected EOF code section")
        };
        let Some(code) = eof.body.code(idx) else {
            panic!("Code not found")
        };
        self.bytecode = code.clone();
        self.instruction_pointer = unsafe { self.bytecode.as_ptr().add(pc) };
    }

    /// Inserts the output of a `create` call into the interpreter.
    ///
    /// This function is used after a `create` call has been executed. It processes the outcome
    /// of that call and updates the state of the interpreter accordingly.
    ///
    /// # Arguments
    ///
    /// * `create_outcome` - A `CreateOutcome` struct containing the results of the `create` call.
    ///
    /// # Behavior
    ///
    /// The function updates the `return_data_buffer` with the data from `create_outcome`.
    /// Depending on the `InstructionResult` indicated by `create_outcome`, it performs one of the following:
    ///
    /// - `Ok`: Pushes the address from `create_outcome` to the stack, updates gas costs, and records any gas refunds.
    /// - `Revert`: Pushes `U256::ZERO` to the stack and updates gas costs.
    /// - `FatalExternalError`: Sets the `instruction_result` to `InstructionResult::FatalExternalError`.
    /// - `Default`: Pushes `U256::ZERO` to the stack.
    ///
    /// # Side Effects
    ///
    /// - Updates `return_data_buffer` with the data from `create_outcome`.
    /// - Modifies the stack by pushing values depending on the `InstructionResult`.
    /// - Updates gas costs and records refunds in the interpreter's `gas` field.
    /// - May alter `instruction_result` in case of external errors.
    pub fn insert_create_outcome(&mut self, create_outcome: CreateOutcome) {
        self.instruction_result = InstructionResult::Continue;

        let instruction_result = create_outcome.instruction_result();
        self.return_data_buffer = if instruction_result.is_revert() {
            // Save data to return data buffer if the create reverted
            create_outcome.output().to_owned()
        } else {
            // Otherwise clear it
            Bytes::new()
        };

        match instruction_result {
            return_ok!() => {
                let address = create_outcome.address;
                push_b256!(self, address.unwrap_or_default().into_word());
                self.gas.erase_cost(create_outcome.gas().remaining());
                self.gas.record_refund(create_outcome.gas().refunded());
            }
            return_revert!() => {
                push!(self, U256::ZERO);
                self.gas.erase_cost(create_outcome.gas().remaining());
            }
            InstructionResult::FatalExternalError => {
                panic!("Fatal external error in insert_create_outcome");
            }
            _ => {
                push!(self, U256::ZERO);
            }
        }
    }

    pub fn insert_eofcreate_outcome(&mut self, create_outcome: EOFCreateOutcome) {
        let instruction_result = create_outcome.instruction_result();

        self.return_data_buffer = if *instruction_result == InstructionResult::Revert {
            // Save data to return data buffer if the create reverted
            create_outcome.output().to_owned()
        } else {
            // Otherwise clear it. Note that RETURN opcode should abort.
            Bytes::new()
        };

        match instruction_result {
            InstructionResult::ReturnContract => {
                push_b256!(self, create_outcome.address.into_word());
                self.gas.erase_cost(create_outcome.gas().remaining());
                self.gas.record_refund(create_outcome.gas().refunded());
            }
            return_revert!() => {
                push!(self, U256::ZERO);
                self.gas.erase_cost(create_outcome.gas().remaining());
            }
            InstructionResult::FatalExternalError => {
                panic!("Fatal external error in insert_eofcreate_outcome");
            }
            _ => {
                push!(self, U256::ZERO);
            }
        }
    }

    /// Inserts the outcome of a call into the virtual machine's state.
    ///
    /// This function takes the result of a call, represented by `CallOutcome`,
    /// and updates the virtual machine's state accordingly. It involves updating
    /// the return data buffer, handling gas accounting, and setting the memory
    /// in shared storage based on the outcome of the call.
    ///
    /// # Arguments
    ///
    /// * `shared_memory` - A mutable reference to the shared memory used by the virtual machine.
    /// * `call_outcome` - The outcome of the call to be processed, containing details such as
    ///   instruction result, gas information, and output data.
    ///
    /// # Behavior
    ///
    /// The function first copies the output data from the call outcome to the virtual machine's
    /// return data buffer. It then checks the instruction result from the call outcome:
    ///
    /// - `return_ok!()`: Processes successful execution, refunds gas, and updates shared memory.
    /// - `return_revert!()`: Handles a revert by only updating the gas usage and shared memory.
    /// - `InstructionResult::FatalExternalError`: Sets the instruction result to a fatal external error.
    /// - Any other result: No specific action is taken.
    pub fn insert_call_outcome(
        &mut self,
        shared_memory: &mut SharedMemory,
        call_outcome: CallOutcome,
    ) {
        self.instruction_result = InstructionResult::Continue;
        self.return_data_buffer.clone_from(call_outcome.output());

        let out_offset = call_outcome.memory_start();
        let out_len = call_outcome.memory_length();

        let target_len = min(out_len, self.return_data_buffer.len());
        match call_outcome.instruction_result() {
            return_ok!() => {
                // return unspend gas.
                let remaining = call_outcome.gas().remaining();
                let refunded = call_outcome.gas().refunded();
                self.gas.erase_cost(remaining);
                self.gas.record_refund(refunded);
                shared_memory.set(out_offset, &self.return_data_buffer[..target_len]);
                push!(self, U256::from(1));
            }
            return_revert!() => {
                self.gas.erase_cost(call_outcome.gas().remaining());
                shared_memory.set(out_offset, &self.return_data_buffer[..target_len]);
                push!(self, U256::ZERO);
            }
            InstructionResult::FatalExternalError => {
                panic!("Fatal external error in insert_call_outcome");
            }
            _ => {
                push!(self, U256::ZERO);
            }
        }
    }

    /// Returns the opcode at the current instruction pointer.
    #[inline]
    pub fn current_opcode(&self) -> u8 {
        unsafe { *self.instruction_pointer }
    }

    /// Returns a reference to the contract.
    #[inline]
    pub fn contract(&self) -> &Contract {
        &self.contract
    }

    /// Returns a reference to the interpreter's gas state.
    #[inline]
    pub fn gas(&self) -> &Gas {
        &self.gas
    }

    /// Returns a reference to the interpreter's stack.
    #[inline]
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// Returns the current program counter.
    #[inline]
    pub fn program_counter(&self) -> usize {
        // SAFETY: `instruction_pointer` should be at an offset from the start of the bytecode.
        // In practice this is always true unless a caller modifies the `instruction_pointer` field manually.
        unsafe { self.instruction_pointer.offset_from(self.bytecode.as_ptr()) as usize }
    }

    /// Executes the instruction at the current instruction pointer.
    ///
    /// Internally it will increment instruction pointer by one.
    #[inline]
    pub(crate) fn step<FN, H: Host + ?Sized>(&mut self, instruction_table: &[FN; 256], host: &mut H)
    where
        FN: Fn(&mut Interpreter, &mut H),
    {
        // Get current opcode.
        let opcode = unsafe { *self.instruction_pointer };

        // SAFETY: In analysis we are doing padding of bytecode so that we are sure that last
        // byte instruction is STOP so we are safe to just increment program_counter bcs on last instruction
        // it will do noop and just stop execution of this contract
        self.instruction_pointer = unsafe { self.instruction_pointer.offset(1) };

        // execute instruction.
        (instruction_table[opcode as usize])(self, host)
    }

    /// Take memory and replace it with empty memory.
    pub fn take_memory(&mut self) -> SharedMemory {
        core::mem::replace(&mut self.shared_memory, EMPTY_SHARED_MEMORY)
    }

    /// Executes the interpreter until it returns or stops.
    pub fn run<FN, H: Host + ?Sized>(
        &mut self,
        shared_memory: SharedMemory,
        instruction_table: &[FN; 256],
        host: &mut H,
    ) -> InterpreterAction
    where
        FN: Fn(&mut Interpreter, &mut H),
    {
        self.next_action = InterpreterAction::None;
        self.shared_memory = shared_memory;

        let mut resize_mem = None;

        if let Some(RVEmu {
            emu,
            returned_data_destiny,
        }) = &mut self.riscv_emulator
        {
            if let Some(destiny) = std::mem::take(returned_data_destiny) {
                let data = emu.cpu.bus.get_dram_slice(destiny).unwrap();
                data.copy_from_slice(self.shared_memory.slice(0, data.len()))
            }

            // Run emulator and capture ecalls
            loop {
                let run_result = emu.start();
                match run_result {
                    Err(Exception::EnvironmentCallFromMMode) => {
                        let t0: u64 = emu.cpu.xregs.read(5);
                        match t0 {
                            0 => {
                                // Syscall::Return
                                let ret_offset: u64 = emu.cpu.xregs.read(10);
                                let ret_size: u64 = emu.cpu.xregs.read(11);
                                let data_bytes = if ret_size != 0 {
                                    emu.cpu
                                        .bus
                                        .get_dram_slice(ret_offset..(ret_offset + ret_size))
                                        .unwrap()
                                } else {
                                    &mut []
                                };
                                self.next_action = InterpreterAction::Return {
                                    result: InterpreterResult {
                                        result: InstructionResult::Return,
                                        output: data_bytes.to_vec().into(),
                                        gas: self.gas, // FIXME: gas is not correct
                                    },
                                };
                                break;
                            }
                            1 => {
                                // Syscall:SLoad
                                let key: u64 = emu.cpu.xregs.read(10);
                                match host.sload(self.contract.target_address, U256::from(key)) {
                                    Some((value, is_cold)) => {
                                        emu.cpu.xregs.write(10, value.as_limbs()[0]);
                                    }
                                    _ => {
                                        self.instruction_result = InstructionResult::Revert;
                                        break;
                                    }
                                }
                            }
                            2 => {
                                // Syscall::SStore
                                let key: u64 = emu.cpu.xregs.read(10);
                                let value: u64 = emu.cpu.xregs.read(11);
                                host.sstore(
                                    self.contract.target_address,
                                    U256::from(key),
                                    U256::from(value),
                                );
                            }
                            3 => {
                                // Syscall::Call
                                let a0: u64 = emu.cpu.xregs.read(10);
                                let address = Address::from_slice(
                                    emu.cpu.bus.get_dram_slice(a0..(a0 + 20)).unwrap(),
                                );
                                let value: u64 = emu.cpu.xregs.read(11);
                                let args_offset: u64 = emu.cpu.xregs.read(12);
                                let args_size: u64 = emu.cpu.xregs.read(13);
                                let ret_offset = emu.cpu.xregs.read(14);
                                let ret_size = emu.cpu.xregs.read(15);

                                *returned_data_destiny = Some(ret_offset..(ret_offset + ret_size));

                                if self.shared_memory.len() < ret_size as usize {
                                    resize_mem = Some(ret_size as usize);
                                }

                                let tx = &host.env().tx;
                                self.next_action = InterpreterAction::Call {
                                    inputs: Box::new(CallInputs {
                                        input: emu
                                            .cpu
                                            .bus
                                            .get_dram_slice(args_offset..(args_offset + args_size))
                                            .unwrap()
                                            .to_vec()
                                            .into(),
                                        gas_limit: tx.gas_limit,
                                        target_address: address,
                                        bytecode_address: address,
                                        caller: self.contract.target_address,
                                        value: CallValue::Transfer(U256::from_le_bytes(
                                            value.to_le_bytes(),
                                        )),
                                        scheme: CallScheme::Call,
                                        is_static: false,
                                        is_eof: false,
                                        return_memory_offset: 0..ret_size as usize,
                                    }),
                                };
                            }
                            4 => {
                                // Syscall::Revert
                                self.next_action = InterpreterAction::Return {
                                    result: InterpreterResult {
                                        result: InstructionResult::Revert,
                                        output: Bytes::from(0u32.to_le_bytes()), //TODO: return revert(0,0)
                                        gas: self.gas, // FIXME: gas is not correct
                                    },
                                };
                                break;
                            }
                            _ => {
                                println!("Unhandled syscall: {:?}", t0);
                                self.instruction_result = InstructionResult::Revert;
                                break;
                            }
                        }
                    }
                    _ => {
                        self.instruction_result = InstructionResult::Revert;
                        break;
                    }
                }
            }
        } else {
            // main loop
            while self.instruction_result == InstructionResult::Continue {
                self.step(instruction_table, host);
            }
        }

        if let Some(new_size) = resize_mem {
            assert!(self.resize_memory(new_size));
        }

        // Return next action if it is some.
        if self.next_action.is_some() {
            return core::mem::take(&mut self.next_action);
        }

        // If not, return action without output as it is a halt.
        InterpreterAction::Return {
            result: InterpreterResult {
                result: self.instruction_result,
                // return empty bytecode
                output: Bytes::new(),
                gas: self.gas, // FIXME: gas is not correct
            },
        }
    }

    /// Resize the memory to the new size. Returns whether the gas was enough to resize the memory.
    #[inline]
    #[must_use]
    pub fn resize_memory(&mut self, new_size: usize) -> bool {
        resize_memory(&mut self.shared_memory, &mut self.gas, new_size)
    }
}

impl InterpreterResult {
    /// Returns whether the instruction result is a success.
    #[inline]
    pub const fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// Returns whether the instruction result is a revert.
    #[inline]
    pub const fn is_revert(&self) -> bool {
        self.result.is_revert()
    }

    /// Returns whether the instruction result is an error.
    #[inline]
    pub const fn is_error(&self) -> bool {
        self.result.is_error()
    }
}

/// Resize the memory to the new size. Returns whether the gas was enough to resize the memory.
#[inline(never)]
#[cold]
#[must_use]
pub fn resize_memory(memory: &mut SharedMemory, gas: &mut Gas, new_size: usize) -> bool {
    let new_words = num_words(new_size as u64);
    let new_cost = gas::memory_gas(new_words);
    let current_cost = memory.current_expansion_cost();
    let cost = new_cost - current_cost;
    let success = gas.record_cost(cost);
    if success {
        memory.resize((new_words as usize) * 32);
    }
    success
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{opcode::InstructionTable, DummyHost};
    use revm_primitives::CancunSpec;
    use std::{fs::File, io::Read};

    #[test]
    fn object_safety() {
        let mut interp = Interpreter::new(Contract::default(), u64::MAX, false);

        let mut host = crate::DummyHost::default();
        let table: InstructionTable<DummyHost> =
            crate::opcode::make_instruction_table::<DummyHost, CancunSpec>();
        let _ = interp.run(EMPTY_SHARED_MEMORY, &table, &mut host);

        let host: &mut dyn Host = &mut host as &mut dyn Host;
        let table: InstructionTable<dyn Host> =
            crate::opcode::make_instruction_table::<dyn Host, CancunSpec>();
        let _ = interp.run(EMPTY_SHARED_MEMORY, &table, host);
    }

    #[test]
    fn riscv_interpreter_return() {
        let mut runtime_bytes = vec![0xFF];
        File::open("../../elf_test/return_example")
            .unwrap()
            .read_to_end(&mut runtime_bytes)
            .unwrap();

        let contract = Contract::new(
            Bytes::new(),
            Bytecode::new_raw(Bytes::from(runtime_bytes)),
            None,
            crate::primitives::Address::default(),
            crate::primitives::Address::default(),
            U256::ZERO,
        );

        let mut interp = Interpreter::new(contract, u64::MAX, false);
        let mut host = crate::DummyHost::default();
        let table: InstructionTable<DummyHost> =
            crate::opcode::make_instruction_table::<DummyHost, CancunSpec>();

        match interp.run(EMPTY_SHARED_MEMORY, &table, &mut host) {
            InterpreterAction::Return { result } => {
                assert!(result.output.len() > 0);
                assert_eq!(result.result, InstructionResult::Return);
                assert_eq!(
                    result.output,
                    Bytes::from(vec![0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
                );
                println!("{result:#?}")
            }
            _ => panic!("Expected return action"),
        }
    }

    #[test]
    fn riscv_interpreter_sstore_and_sload() {
        let mut runtime_bytes = vec![0xFF];
        File::open("../../elf_test/sstore_and_sload_example")
            .unwrap()
            .read_to_end(&mut runtime_bytes)
            .unwrap();

        let contract = Contract::new(
            Bytes::new(),
            Bytecode::new_raw(Bytes::from(runtime_bytes)),
            None,
            crate::primitives::Address::default(),
            crate::primitives::Address::default(),
            U256::ZERO,
        );

        let mut interp = Interpreter::new(contract, u64::MAX, false);
        let mut host = crate::DummyHost::default();
        let table: InstructionTable<DummyHost> =
            crate::opcode::make_instruction_table::<DummyHost, CancunSpec>();

        let result = interp.run(EMPTY_SHARED_MEMORY, &table, &mut host);
        println!("{result:#?}");
    }
}
