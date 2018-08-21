use super::super::machine::Machine;
use super::super::memory::Memory;
use super::super::Error;
use super::utils::{opcode, funct3, rd, rs1, rs2,
                   btype_immediate, utype_immediate, itype_immediate,
                   stype_immediate, update_register};
use super::{Instruction as GenericInstruction, Instruction::RV32I};

#[derive(Debug)]
pub enum Instruction {
    ECALL,
    // B-type
    BEQ { rs1: usize, rs2: usize, imm: i32 },
    BNE { rs1: usize, rs2: usize, imm: i32 },
    BLT { rs1: usize, rs2: usize, imm: i32 },
    BLTU { rs1: usize, rs2: usize, imm: i32 },
    BGE { rs1: usize, rs2: usize, imm: i32 },
    BGEU { rs1: usize, rs2: usize, imm: i32 },
    // I-type
    ADDI { rd: usize, rs1: usize, imm: i32 },
    ANDI { rd: usize, rs1: usize, imm: i32 },
    JALR { rd: usize, rs1: usize, imm: i32 },
    LBU { rd: usize, rs1: usize, imm: i32 },
    LW { rd: usize, rs1: usize, imm: i32 },
    SLLI { rd: usize, rs1: usize, imm: i32 },
    // R-type
    SLL { rd: usize, rs1: usize, rs2: usize },
    // S-type
    SB { rs1: usize, rs2: usize, imm: i32 },
    SW { rs1: usize, rs2: usize, imm: i32 },
    // U-type
    AUIPC { rd: usize, imm: i32 },
    LUI { rd: usize, imm: i32 },
}

impl Instruction {
    pub fn execute<M: Memory>(&self, machine: &mut Machine<M>) -> Result<(), Error> {
        match self {
            Instruction::ADDI { rd, rs1, imm } => {
                let (value, _) = machine.registers[*rs1].overflowing_add(*imm as u32);
                update_register(machine, *rd, value);
            },
            Instruction::ANDI { rd, rs1, imm } => {
                let value = machine.registers[*rs1] & (*imm as u32);
                update_register(machine, *rd, value);
            },
            Instruction::AUIPC { rd, imm } => {
                let (value, _) = machine.pc.overflowing_add(*imm as u32);
                update_register(machine, *rd, value);
            },
            Instruction::BEQ { rs1, rs2, imm } => {
                let rs1_value: u32 = machine.registers[*rs1];
                let rs2_value: u32 = machine.registers[*rs2];
                if rs1_value == rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::BNE { rs1, rs2, imm } => {
                let rs1_value: u32 = machine.registers[*rs1];
                let rs2_value: u32 = machine.registers[*rs2];
                if rs1_value != rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::BGE { rs1, rs2, imm } => {
                let rs1_value: i32 = machine.registers[*rs1] as i32;
                let rs2_value: i32 = machine.registers[*rs2] as i32;
                if rs1_value >= rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::BGEU { rs1, rs2, imm } => {
                let rs1_value: u32 = machine.registers[*rs1];
                let rs2_value: u32 = machine.registers[*rs2];
                if rs1_value >= rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::BLT { rs1, rs2, imm } => {
                let rs1_value: i32 = machine.registers[*rs1] as i32;
                let rs2_value: i32 = machine.registers[*rs2] as i32;
                if rs1_value < rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::BLTU { rs1, rs2, imm } => {
                let rs1_value: u32 = machine.registers[*rs1];
                let rs2_value: u32 = machine.registers[*rs2];
                if rs1_value < rs2_value {
                    let (value, _) = machine.pc.overflowing_add(*imm as u32);
                    machine.pc = value;
                    return Ok(());
                }
            },
            Instruction::ECALL => {
                // The semantic of ECALL is determined by the hardware, which
                // is not part of the spec, hence here the implementation is
                // deferred to the machine. This way custom ECALLs might be
                // provided for different environments.
                return machine.ecall();
            },
            Instruction::JALR { rd, rs1, imm } => {
                let link = machine.pc + 4;
                let (mut value, _) = machine.registers[*rs1].overflowing_add(*imm as u32);
                value &= !(1 as u32);
                machine.pc = value;
                update_register(machine, *rd, link);
                return Ok(());
            },
            Instruction::LUI { rd, imm } => {
                update_register(machine, *rd, *imm as u32);
            },
            Instruction::LBU { rd, rs1, imm } => {
                let (address, _) =  machine.registers[*rs1].overflowing_add(*imm as u32);
                let value = machine.memory.load8(address as usize)?;
                update_register(machine, *rd, value as u32);
            },
            Instruction::LW { rd, rs1, imm } => {
                let (address, _) =  machine.registers[*rs1].overflowing_add(*imm as u32);
                let value = machine.memory.load32(address as usize)?;
                update_register(machine, *rd, value);
            },
            Instruction::SB { rs1, rs2, imm } => {
                let (address, _) =  machine.registers[*rs1].overflowing_add(*imm as u32);
                let value = machine.registers[*rs2] as u8;
                machine.memory.store8(address as usize, value)?;
            },
            Instruction::SW { rs1, rs2, imm } => {
                let (address, _) =  machine.registers[*rs1].overflowing_add(*imm as u32);
                let value = machine.registers[*rs2] as u32;
                machine.memory.store32(address as usize, value)?;
            },
            Instruction::SLL { rd, rs1, rs2 } => {
                let shift_value = machine.registers[*rs2] & 0x1F;
                let value = machine.registers[*rs1] << shift_value;
                update_register(machine, *rd, value);
            },
            Instruction::SLLI { rd, rs1, imm } => {
                let value = machine.registers[*rs1] << imm;
                update_register(machine, *rd, value);
            }
        }
        machine.pc += 4;
        Ok(())
    }
}

pub fn factory(instruction_bits: u32) -> Option<GenericInstruction> {
    match opcode(instruction_bits) {
        0x3 => match funct3(instruction_bits) {
            0x2 => Some(RV32I(Instruction::LW {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                imm: itype_immediate(instruction_bits),
            })),
            0x4 => Some(RV32I(Instruction::LBU {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                imm: itype_immediate(instruction_bits),
            })),
            _ => None,
        },
        0x13 => match funct3(instruction_bits) {
            0x0 => Some(RV32I(Instruction::ADDI {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                imm: itype_immediate(instruction_bits),
            })),
            0x1 => Some(RV32I(Instruction::SLLI {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                // Only lower 5 bits are relevant here
                imm: itype_immediate(instruction_bits) & 0x1f,
            })),
            0x7 => Some(RV32I(Instruction::ANDI {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                imm: itype_immediate(instruction_bits),
            })),
            _ => None,
        },
        0x17 => Some(RV32I(Instruction::AUIPC {
            rd: rd(instruction_bits),
            imm: utype_immediate(instruction_bits),
        })),
        0x23 => match funct3(instruction_bits) {
            0x0 => Some(RV32I(Instruction::SB {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: stype_immediate(instruction_bits),
            })),
            0x2 => Some(RV32I(Instruction::SW {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: stype_immediate(instruction_bits),
            })),
            _ => None,
        },
        0x33 => match funct3(instruction_bits) {
            0x1 => Some(RV32I(Instruction::SLL {
                rd: rd(instruction_bits),
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
            })),
            _ => None,
        },
        0x37 => Some(RV32I(Instruction::LUI {
            rd: rd(instruction_bits),
            imm: utype_immediate(instruction_bits),
        })),
        0x63 => match funct3(instruction_bits) {
            0x0 => Some(RV32I(Instruction::BEQ {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            0x1 => Some(RV32I(Instruction::BNE {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            0x4 => Some(RV32I(Instruction::BLT {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            0x5 => Some(RV32I(Instruction::BGE {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            0x6 => Some(RV32I(Instruction::BLTU {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            0x7 => Some(RV32I(Instruction::BGEU {
                rs1: rs1(instruction_bits),
                rs2: rs2(instruction_bits),
                imm: btype_immediate(instruction_bits),
            })),
            _ => None,
        },
        0x67 => Some(RV32I(Instruction::JALR {
            rd: rd(instruction_bits),
            rs1: rs1(instruction_bits),
            imm: itype_immediate(instruction_bits),
        })),
        0x73 => match funct3(instruction_bits) {
            0 => {
                if itype_immediate(instruction_bits) == 0 {
                    Some(RV32I(Instruction::ECALL))
                } else {
                    // EBREAK
                    None
                }
            },
            _ => None,
        }
        _ => None,
    }
}
