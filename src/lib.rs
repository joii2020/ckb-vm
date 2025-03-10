#[macro_use]
extern crate derive_more;

pub mod bits;
pub mod cost_model;
pub mod debugger;
pub mod decoder;
pub mod error;
pub mod instructions;
pub mod machine;
pub mod memory;
pub mod snapshot;
pub mod syscalls;

pub use bytes;
pub use ckb_vm_definitions;

pub use crate::{
    debugger::Debugger,
    instructions::{Instruction, Register},
    machine::{
        trace::TraceMachine, CoreMachine, DefaultCoreMachine, DefaultMachine,
        DefaultMachineBuilder, InstructionCycleFunc, Machine, SupportMachine,
    },
    memory::{flat::FlatMemory, sparse::SparseMemory, wxorx::WXorXMemory, Memory},
    syscalls::Syscalls,
};
pub use bytes::Bytes;

pub use ckb_vm_definitions::{
    registers, DEFAULT_STACK_SIZE, ISA_A, ISA_B, ISA_IMC, ISA_MOP, MEMORY_FRAMES, MEMORY_FRAMESIZE,
    MEMORY_FRAME_SHIFTS, RISCV_GENERAL_REGISTER_NUMBER, RISCV_MAX_MEMORY, RISCV_PAGES,
    RISCV_PAGESIZE, RISCV_PAGE_SHIFTS,
};

pub use error::Error;

pub fn run<R: Register, M: Memory<REG = R>>(
    program: &Bytes,
    args: &[Bytes],
    memory_size: usize,
) -> Result<i8, Error> {
    let core_machine = DefaultCoreMachine::<R, WXorXMemory<M>>::new_with_memory(
        ISA_IMC | ISA_A | ISA_B | ISA_MOP,
        machine::VERSION2,
        u64::max_value(),
        memory_size,
    );
    let mut machine = TraceMachine::new(DefaultMachineBuilder::new(core_machine).build());
    machine.load_program(program, args)?;
    machine.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_max_memory_must_be_multiple_of_pages() {
        assert_eq!(RISCV_MAX_MEMORY % RISCV_PAGESIZE, 0);
    }

    #[test]
    fn test_page_size_be_power_of_2() {
        assert!(RISCV_PAGESIZE.is_power_of_two());
    }
}
